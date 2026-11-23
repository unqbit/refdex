// Without this the release build opens a console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde::Serialize;
use tauri::http::{header::CONTENT_TYPE, Request, Response};
use tauri::{Manager, State};
use tauri_plugin_dialog::DialogExt;

// Folders never worth walking into. Skipping before descending is what keeps a big
// folder fast to open.
const SKIP: &[&str] = &[
    ".git", ".hg", ".svn", ".obsidian", "node_modules", "target", "build", "dist",
    "bin", "obj", "vendor", "__pycache__", ".venv", "venv", ".next", ".nuxt",
];

const IMAGES: &[&str] = &["png", "jpg", "jpeg", "gif", "svg", "webp"];

// The folder being read. Empty until someone picks one.
#[derive(Default)]
struct Root(Mutex<Option<PathBuf>>);

// The shells behind the terminal tab, one per project, keyed by the project's folder.
// A pseudo terminal is what makes a shell believe a screen is attached: without one it
// prints no prompt, offers no history and sends no colour.
//
// One per project rather than one for the window, because everything else in the middle
// pane already belongs to the project being read. A single shell would start in the
// folder above the projects, so a command gathered from one project's documents would
// run somewhere that project's paths do not resolve.
#[derive(Default)]
struct Term(Mutex<std::collections::HashMap<String, Shell>>);

struct Shell {
    writer: Box<dyn std::io::Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    // Set when this side replaces the shell on purpose, so its threads stay quiet
    // instead of reporting a death nobody caused.
    replaced: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Serialize)]
struct Doc {
    path: String,
    stamp: u64,
}

#[derive(Serialize)]
struct Index {
    root: String,
    // The whole path, for the one place the screen has to say which folder this is.
    path: String,
    docs: Vec<Doc>,
    assets: Vec<String>,
    graphs: HashMap<String, String>,
}

fn walk(root: &Path) -> Index {
    let mut docs = Vec::new();
    let mut assets = Vec::new();
    let mut graphs = HashMap::new();
    // Depth is counted, because a folder can hold a link back to one of its own parents
    // and walking that is a loop with no end. Nothing anyone files by hand goes this deep.
    const DEEP: usize = 32;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let full = entry.path();
            let tail = match full.strip_prefix(root) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let rel = tail.to_string_lossy().replace('\\', "/");
            let name = entry.file_name().to_string_lossy().to_string();
            if full.is_dir() {
                if SKIP.contains(&name.as_str()) {
                    continue;
                }
                // graphify-out is generated; only its graph page is worth serving.
                if name == "graphify-out" {
                    if full.join("graph.html").is_file() {
                        let owner = rel
                            .strip_suffix("graphify-out")
                            .unwrap_or("")
                            .trim_end_matches('/')
                            .to_string();
                        graphs.insert(owner, format!("{rel}/graph.html"));
                    }
                    continue;
                }
                if depth < DEEP {
                    stack.push((full, depth + 1));
                }
                continue;
            }
            let ext = full
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if ext == "md" {
                let stamp = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                docs.push(Doc { path: rel, stamp });
            } else if IMAGES.contains(&ext.as_str()) {
                assets.push(rel);
            }
        }
    }
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    assets.sort();
    Index {
        root: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: root.to_string_lossy().to_string(),
        docs,
        assets,
        graphs,
    }
}

fn mime_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "md" => "text/markdown; charset=utf-8",
        // Only `graph.html` reaches this arm; no other page is handed over.
        "html" => "text/html; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

fn reply(status: u16, mime: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, mime)
        // The page and this protocol are different origins, so without this the
        // window's own fetch is refused before it ever reaches the folder. Only that
        // one origin is named: a page framed from the folder gets an origin of its own,
        // and a wildcard here would let it read every file this protocol serves.
        .header("Access-Control-Allow-Origin", UI_ORIGIN)
        .header("Cache-Control", "no-store")
        .body(body)
        .unwrap()
}

fn not_here() -> Response<Vec<u8>> {
    reply(404, "text/plain; charset=utf-8", Vec::from("not here"))
}

// A file the screen refuses to hold is a different answer from one that is not there,
// and the pane says so in its own words.
fn too_big() -> Response<Vec<u8>> {
    reply(413, "text/plain; charset=utf-8", Vec::from("too big"))
}

fn serve(root: Option<PathBuf>, raw: &str) -> Response<Vec<u8>> {
    let root = match root {
        Some(r) => r,
        None => return not_here(),
    };
    let path = percent_decode(raw);
    if path == "_index" {
        // A folder that is gone and a folder with nothing to read both walk to an empty
        // list, and the screen cannot tell them apart from the list alone. The first is
        // the end of the session and the second is a folder waiting for its first
        // document, so they must not answer the same.
        if !root.is_dir() {
            return not_here();
        }
        let body = serde_json::to_vec(&walk(&root)).unwrap_or_default();
        return reply(200, "application/json; charset=utf-8", body);
    }
    let full = match inside(&root, &path) {
        Some(f) => f,
        None => return not_here(),
    };
    // Only what the screen draws. Everything else in the folder — a key, an `.env`, a
    // page of someone else's HTML — is not this app's to hand out, and an HTML file
    // handed out here would run at this app's own origin.
    let graph = path.ends_with("graph.html");
    if !graph && !servable(&path) {
        return not_here();
    }
    // Whatever is read lands in memory whole and is handed to the screen whole. A file
    // past this size is not a document anyone reads here, and reading it would take the
    // window down with it.
    const MOST: u64 = 64 * 1024 * 1024;
    match fs::metadata(&full) {
        Ok(m) if m.len() > MOST => return too_big(),
        Ok(_) => {}
        Err(_) => return not_here(),
    }
    match fs::read(&full) {
        Ok(bytes) if graph => reply(200, mime_of(&path), offline_graph(bytes)),
        Ok(bytes) => reply(200, mime_of(&path), bytes),
        Err(_) => not_here(),
    }
}

// The extensions the screen can draw. `graph.html` is named separately — it is one
// known file, not a file type.
fn servable(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    ext == "md" || IMAGES.contains(&ext.as_str())
}

// A path inside the chosen folder, or nothing. Two checks, because either alone lets
// something through: the component filter refuses `..` and drive letters before they
// are joined, and resolving the result refuses a link or junction that points out of
// the folder after the join looked innocent.
fn inside(root: &Path, path: &str) -> Option<PathBuf> {
    let mut full = root.to_path_buf();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(p) => full.push(p),
            _ => return None,
        }
    }
    if !full.is_file() {
        return None;
    }
    let real = fs::canonicalize(&full).ok()?;
    let base = fs::canonicalize(root).ok()?;
    if real.starts_with(&base) {
        Some(real)
    } else {
        None
    }
}

// The generated graph page asks a CDN for its drawing library, and this app does not go
// to the network. The copy kept beside the page is handed over instead, so the graph
// draws offline. Only what is sent changes; the file on disk is left as it is.
// The version is named in full on purpose. A page built against another version keeps
// its own tag rather than being handed a library it was not written for.
// Two spellings, because the page may ask the CDN for the library or ask for a copy laid
// beside itself. Only the first is handed over by this app, so the second is rewritten
// too: the folder's own files are not served, and a page left pointing at one would draw
// nothing here.
const GRAPH_LIB_TAGS: &[&str] = &[
    "<script src=\"https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js\"",
    "<script src=\"vis-network.min.js\"",
];

#[cfg(windows)]
const UI_ORIGIN: &str = "http://tauri.localhost";
#[cfg(not(windows))]
const UI_ORIGIN: &str = "tauri://localhost";

// The page offers Light, Dark and System, and with none of them chosen it stands dark
// whatever the machine is set to. This app follows the machine, so a dark graph inside a
// light window is one app saying two things at once. What is pressed is the page's own
// System button: that is how the page itself asks for the machine's theme, so the colours
// are not rewritten here and a page without that button is handed over untouched. It runs
// last because the page's own script has to have defined that button's handler first.
//
// Pressed again on every change of the machine's theme, and that is not belt and braces.
// This frame is handed over without an origin, so it has no storage; the page keeps the
// choice in storage and its own listener for the theme reads it back before acting, so in
// here that listener finds nothing and does nothing. The frame also reports light for a
// moment before the window's theme reaches it, which is early enough to catch the first
// press. Without the second press a dark window would hold a light graph.
//
// Only while System is the choice showing. Someone who picks Light or Dark on the page has
// said they want that whatever the machine says, and pressing System again under them
// would take it back the next time the machine's theme moved.
const GRAPH_SYSTEM_THEME: &str = "<script>try{(function(){var b=document.querySelector('[data-theme-choice=\"system\"]'),m=window.matchMedia&&window.matchMedia('(prefers-color-scheme: light)');if(!b||!m)return;b.click();m.addEventListener('change',function(){if(b.getAttribute('aria-pressed')==='true')b.click()})})()}catch(e){}</script>";

fn offline_graph(bytes: Vec<u8>) -> Vec<u8> {
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        // Not text this can rewrite. Hand it over exactly as it came.
        Err(e) => return e.into_bytes(),
    };
    system_theme(offline_lib(text)).into_bytes()
}

fn system_theme(text: String) -> String {
    let close = match text.rfind("</body>") {
        Some(i) => i,
        None => return text,
    };
    let mut out = String::with_capacity(text.len() + GRAPH_SYSTEM_THEME.len());
    out.push_str(&text[..close]);
    out.push_str(GRAPH_SYSTEM_THEME);
    out.push_str(&text[close..]);
    out
}

fn offline_lib(text: String) -> String {
    let open = match GRAPH_LIB_TAGS.iter().find_map(|tag| text.find(tag)) {
        Some(i) => i,
        None => return text,
    };
    let close = match text[open..].find("</script>") {
        Some(i) => open + i + "</script>".len(),
        None => return text,
    };
    // The whole tag goes, and its integrity and crossorigin attributes with it. That
    // hash belongs to the copy on the CDN, and asking for CORS on a file this app hands
    // over itself gets the request refused, so the library would never arrive.
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..open]);
    out.push_str("<script src=\"");
    out.push_str(UI_ORIGIN);
    out.push_str("/vis-network.min.js\"></script>");
    out.push_str(&text[close..]);
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Read off the bytes, not the string: a `%` in front of a multi-byte letter puts
        // the end of the slice inside that letter, and slicing a string there is a panic.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let pair = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = pair.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// The folder comes from whoever started the app, so nothing has to be stored anywhere.
// A shortcut carrying the path is the whole setting, and it stays where it can be seen.
fn root_from_args() -> Option<PathBuf> {
    let given = PathBuf::from(std::env::args().nth(1)?);
    if given.is_dir() {
        Some(given)
    } else {
        None
    }
}

// What this app is built from and what it is leaning on, asked rather than written down.
// A version copied into a document is right on the day it is typed and quietly wrong
// after that; the two below are stamped in at build time and the rest are asked for here.
#[derive(Serialize)]
struct About {
    app: &'static str,
    tauri: &'static str,
    // A folder was named at start-up and is not there. Dropping it silently leaves the
    // door standing with nothing said, and the person is left to guess that the
    // shortcut they pressed is pointing at a folder that has moved or gone.
    missing: Option<String>,
    os: &'static str,
    git: Option<String>,
    shell: String,
}

#[tauri::command]
fn about() -> About {
    About {
        app: env!("CARGO_PKG_VERSION"),
        tauri: tauri::VERSION,
        missing: std::env::args()
            .nth(1)
            .filter(|a| !a.starts_with("--") && !Path::new(a).is_dir()),
        os: std::env::consts::OS,
        git: git_out(Path::new("."), &[std::ffi::OsStr::new("--version")])
            .map(|s| s.trim().replace("git version ", "")),
        shell: if cfg!(windows) {
            "powershell.exe".to_string()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        },
    }
}

// A link that leaves the folder is handed to the machine's own browser instead of being
// followed here. This window has no address bar and no way back, and a page opened in it
// would run where this app's own page runs. Only `http` and `https` go, and never a
// `.localhost` host — that is this app's own doorway wearing a web address.
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    let low = url.to_lowercase();
    if !(low.starts_with("http://") || low.starts_with("https://")) {
        return Err("only http and https links open outside".into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("that link has characters a browser would not take".into());
    }
    let host = low
        .split("//")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("");
    if host.ends_with(".localhost") || host == "localhost" {
        return Err("that address belongs to this app".into());
    }
    // The address goes as its own argument. No shell reads it, so nothing in it is a
    // command, however it is spelled.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler").arg(&url);
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(&url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(&url);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn pick(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let chosen = rx.recv().ok()??.into_path().ok()?;
    let state = app.try_state::<Root>()?;
    match state.0.lock() {
        Ok(mut guard) => *guard = Some(chosen.clone()),
        Err(_) => return None,
    }
    Some(chosen.to_string_lossy().to_string())
}

// The shell the person already uses, not one of our choosing.
fn shell_command() -> portable_pty::CommandBuilder {
    #[cfg(windows)]
    {
        let mut c = portable_pty::CommandBuilder::new("powershell.exe");
        c.arg("-NoLogo");
        c
    }
    #[cfg(not(windows))]
    {
        portable_pty::CommandBuilder::new(
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
        )
    }
}

fn size(cols: u16, rows: u16) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[tauri::command]
fn term_open(
    app: tauri::AppHandle,
    project: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let root = app
        .try_state::<Root>()
        .and_then(|s| s.0.lock().ok().and_then(|g| g.clone()))
        .ok_or("no folder")?;
    let dir = project_dir(&root, &project).ok_or("no such project")?;

    // Asked for one that is already running: leave it alone. The screen keeps its own
    // copy of what was printed, so replacing the shell would throw away a session the
    // person can still see.
    if let Some(state) = app.try_state::<Term>() {
        if let Ok(guard) = state.0.lock() {
            if guard.contains_key(&project) {
                return Ok(());
            }
        }
    }

    let pair = portable_pty::native_pty_system()
        .openpty(size(cols, rows))
        .map_err(|e| e.to_string())?;

    let mut cmd = shell_command();
    cmd.cwd(&dir);
    // A window app ignores Ctrl+C, and the shell inherits that state at birth.
    // Without clearing it first, no console event can ever stop a running command.
    #[cfg(windows)]
    unsafe {
        SetConsoleCtrlHandler(None, 0);
    }

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);

    let pid = child.process_id();
    let killer = child.clone_killer();
    let replaced = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    // One thread does nothing but carry bytes from the shell to the screen.
    let sink = app.clone();
    let quiet = replaced.clone();
    let whose = project.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let out = TermOut {
                        project: whose.clone(),
                        data: String::from_utf8_lossy(&buf[..n]).to_string(),
                    };
                    if tauri::Emitter::emit(&sink, "term-out", out).is_err() {
                        break;
                    }
                }
            }
        }
        if !quiet.load(std::sync::atomic::Ordering::Relaxed) {
            forget(&sink, &whose, pid);
            let _ = tauri::Emitter::emit(&sink, "term-end", TermEnd { project: whose });
        }
    });

    // The output pipe never reaches its end while this side holds the master, so the
    // read above cannot tell that the shell quit. Waiting on the shell itself can.
    let gone = app.clone();
    let quiet = replaced.clone();
    let whose = project.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        if !quiet.load(std::sync::atomic::Ordering::Relaxed) {
            forget(&gone, &whose, pid);
            let _ = tauri::Emitter::emit(&gone, "term-end", TermEnd { project: whose });
        }
    });

    if let Some(state) = app.try_state::<Term>() {
        if let Ok(mut guard) = state.0.lock() {
            if let Some(old) = guard.insert(
                project,
                Shell {
                    writer,
                    master: pair.master,
                    killer,
                    pid,
                    replaced,
                },
            ) {
                stop(old);
            }
        }
    }
    Ok(())
}

// A shell that ended on its own is dropped from the map, because term_open treats a
// present key as a running shell and returns without starting one. Left behind, the tab
// would hand back an entry whose shell is gone and the pane would swallow everything
// typed into it. The pid is compared so a shell started since is never thrown away.
fn forget(app: &tauri::AppHandle, project: &str, pid: Option<u32>) {
    if let Some(state) = app.try_state::<Term>() {
        if let Ok(mut guard) = state.0.lock() {
            let same = guard.get(project).map(|sh| sh.pid == pid).unwrap_or(false);
            if same {
                guard.remove(project);
            }
        }
    }
}

fn stop(mut old: Shell) {
    old.replaced
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = old.killer.kill();
}

// A command already running hears no byte on its input. It stops for a console
// control event and nothing else, and that event can only be raised from inside
// the shell's own console, so we step into that console for one call and leave.
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn AttachConsole(pid: u32) -> i32;
    fn FreeConsole() -> i32;
    fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(not(windows))]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(owner: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[cfg(windows)]
#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(window: isize, attr: u32, value: *const u32, size: u32) -> i32;
}

// The bar above the panes is drawn by the window manager, not by the page, so it keeps
// the system colour unless it is handed one. Without this the top of the window is a
// different grey from everything under it.
#[cfg(windows)]
fn paint_frame(window: &tauri::WebviewWindow, dark: bool) {
    const CAPTION: u32 = 35;
    const TEXT: u32 = 36;
    const BORDER: u32 = 34;
    // The pane colour, not the page colour: the panes cover the whole client area, so
    // what the bar actually sits against is `--panel`. A colour here is 0x00BBGGRR, the
    // reverse of what CSS writes, but every one of these is grey, so the three bytes are
    // the same either way.
    let paint = if dark {
        [0x0018_1818u32, 0x00d4_d4d4, 0x002b_2b2b]
    } else {
        [0x00ff_ffffu32, 0x001f_1f1f, 0x00dc_dcdc]
    };
    let Ok(handle) = window.hwnd() else { return };
    let raw = handle.0 as isize;
    for (attr, value) in [CAPTION, TEXT, BORDER].iter().zip(paint.iter()) {
        // Windows 10 refuses these three; the window then keeps the system colour, which
        // is the same as not asking. Nothing to recover from, so the answer is dropped.
        unsafe { DwmSetWindowAttribute(raw, *attr, value, 4) };
    }
}

// When something inside breaks, the window is already unusable: its one page is drawn by
// code that just stopped halfway, and a half-drawn screen that still takes clicks is
// worse than no screen. So the app says what broke, in a box the person has to answer,
// and then ends. Nothing on disk is in play — the app only ever read.
fn fail(what: &str) -> ! {
    #[cfg(windows)]
    unsafe {
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let body = wide(&format!(
            "Refdex has to close.\n\n{what}\n\nNothing was written to your folder."
        ));
        // MB_OK | MB_ICONERROR | MB_SYSTEMMODAL, so it is seen even with no window left.
        MessageBoxW(0, body.as_ptr(), wide("Refdex").as_ptr(), 0x0000_1010);
    }
    #[cfg(not(windows))]
    eprintln!("Refdex has to close.\n\n{what}");
    std::process::exit(1);
}

#[tauri::command]
fn term_interrupt(term: State<Term>, project: String) -> Result<(), String> {
    let pid = {
        let guard = term.0.lock().map_err(|_| "terminal is busy".to_string())?;
        guard
            .get(&project)
            .and_then(|sh| sh.pid)
            .ok_or_else(|| "no shell".to_string())?
    };

    #[cfg(windows)]
    unsafe {
        // The event reaches every process on that console, this one included once
        // it attaches. Without the ignore, stopping a command would close the app.
        SetConsoleCtrlHandler(None, 1);
        if AttachConsole(pid) == 0 {
            // A debug build already owns a console and has to give it up first.
            FreeConsole();
            if AttachConsole(pid) == 0 {
                SetConsoleCtrlHandler(None, 0);
                return Err("the shell console is out of reach".to_string());
            }
        }
        let sent = GenerateConsoleCtrlEvent(0, 0) != 0;
        FreeConsole();
        SetConsoleCtrlHandler(None, 0);
        if !sent {
            return Err("the console refused the event".to_string());
        }
    }

    #[cfg(not(windows))]
    unsafe {
        if kill(-(pid as i32), 2) != 0 {
            return Err("the shell did not take the signal".to_string());
        }
    }

    Ok(())
}

#[derive(Serialize)]
struct Commit {
    sha: String,
    when: String,
    subject: String,
    body: String,
    // What the document was called at that commit. A renamed document keeps its history,
    // and an older change can only be read back under the name it had then.
    name: String,
    // Every document the change touched, and what it did to each. Filled only when the
    // whole project was asked for; one document's own record does not need it.
    touched: Vec<Touch>,
    // When it was made, as a plain number. Only used to put two answers in one order.
    at: u64,
}

// `how` is git's own letter: A put in, D taken out, M altered, R moved. `from` carries
// the old name of a move, which is the only place the two names stand together.
#[derive(Serialize)]
struct Touch {
    how: String,
    name: String,
    from: String,
}

// Splits a path the page asked for into the folder to run git in and the file name to
// hand it. The name never reaches a shell line: it goes as its own argument.
// A project is named by its folder under the root, and the root itself is named by the
// empty string. Checked the same way a served file is: joined a component at a time and
// resolved once more, so a name that climbs out of the root cannot become a shell's cwd.
fn project_dir(root: &Path, key: &str) -> Option<PathBuf> {
    let mut full = root.to_path_buf();
    for part in Path::new(key).components() {
        match part {
            Component::Normal(p) => full.push(p),
            _ => return None,
        }
    }
    if !full.is_dir() {
        return None;
    }
    let real = fs::canonicalize(&full).ok()?;
    let base = fs::canonicalize(root).ok()?;
    if real.starts_with(&base) {
        // The joined path, not the resolved one. Windows resolves to an extended-length
        // form, and a shell started there prints that whole prefix in its prompt.
        Some(full)
    } else {
        None
    }
}

// What the screen is told as a shell writes, and when one ends. The project travels with
// the bytes because several shells report at once and each pane keeps its own screen.
#[derive(Clone, Serialize)]
struct TermOut {
    project: String,
    data: String,
}

#[derive(Clone, Serialize)]
struct TermEnd {
    project: String,
}

fn locate(app: &tauri::AppHandle, path: &str) -> Option<(PathBuf, std::ffi::OsString)> {
    use std::ffi::OsStr;
    let root = app
        .try_state::<Root>()
        .and_then(|s| s.0.lock().ok().and_then(|g| g.clone()))?;
    let full = inside(&root, path)?;
    let dir = full.parent()?.to_path_buf();
    let name = full.file_name()?.to_os_string();
    // git is run where the repository starts, so the names it prints and the names it
    // is handed are the same kind: both counted from the top. Run it in the document's
    // own folder instead and a renamed file answers with a name this side cannot use.
    let prefix = match git_out(&dir, &[OsStr::new("rev-parse"), OsStr::new("--show-prefix")]) {
        Some(p) => p.trim().to_string(),
        None => return Some((dir, name)),
    };
    let mut top = dir.clone();
    for _ in prefix.split('/').filter(|s| !s.is_empty()) {
        top = match top.parent() {
            Some(t) => t.to_path_buf(),
            None => return Some((dir, name)),
        };
    }
    let rel = format!("{}{}", prefix, name.to_string_lossy());
    Some((top, std::ffi::OsString::from(rel)))
}

fn git_out(dir: &Path, args: &[&std::ffi::OsStr]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    // Document names are taken as plain names. Without this a file whose name holds
    // `*` or `?` would be read as a pattern and answer for other files than itself.
    cmd.arg("-C").arg(dir).arg("--literal-pathspecs").args(args);
    #[cfg(windows)]
    {
        // A window app owns no console. Without this every call flashes one open.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    // The messages are written in the same language as the documents, so a byte that is
    // not valid here must not throw the whole history away.
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// The documents keep only what is true now, so the reasons behind what they used to say
// live in the commit messages and nowhere else. The whole message body is carried back,
// not the subject alone, because the body is the part that holds the reason.
// `skip` is how many of the newest commits to pass over, so the pane can ask for the
// next page instead of holding the whole history in one answer.
// `whole` asks for the project's own record rather than one document's: every change,
// including the ones that took a document away. A document that is gone from disk can
// be reached no other way — there is nothing left to open and ask about.
// Which line of work the record below belongs to. `--show-current` is used rather
// than `rev-parse`, because on a repository whose first commit is not made yet the
// latter has no HEAD to resolve while this still names the branch being started.
#[tauri::command]
fn branch(app: tauri::AppHandle, path: String) -> Option<String> {
    use std::ffi::OsStr;
    let (dir, _) = locate(&app, &path)?;
    let out = git_out(&dir, &[OsStr::new("branch"), OsStr::new("--show-current")])?;
    let name = out.trim().to_string();
    // Empty means the work is not on a branch at all: a commit checked out by its own
    // name. Saying so is better than printing nothing, because the record below is
    // still real and still has to belong to something.
    Some(if name.is_empty() {
        String::from("detached")
    } else {
        name
    })
}

#[tauri::command]
fn doc_log(
    app: tauri::AppHandle,
    path: String,
    skip: Option<u32>,
    whole: Option<bool>,
    find: Option<String>,
) -> Option<Vec<Commit>> {
    use std::ffi::OsStr;
    let (dir, name) = locate(&app, &path)?;
    let all = whole.unwrap_or(false);
    // A word, with the whole project asked for, is put to the record two ways: as
    // something a message might say, and as a document the project once had. The second
    // is the one that reaches a document no longer on disk — a name git no longer finds
    // on disk is still a name it knows. The two answers are put together by hash.
    if let Some(word) = find.as_deref().filter(|w| all && !w.trim().is_empty()) {
        let word = word.trim();
        let said = format!("--grep={}", word);
        let mut found: Vec<Commit> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for run in 0..2 {
            let mut args: Vec<&OsStr> = vec![
                OsStr::new("log"),
                OsStr::new("--name-status"),
                OsStr::new("-M"),
                OsStr::new("-n"),
                OsStr::new("30"),
                OsStr::new(FMT),
            ];
            if run == 0 {
                args.push(OsStr::new("-i"));
                args.push(OsStr::new(said.as_str()));
            } else {
                args.push(OsStr::new("--"));
                args.push(OsStr::new(word));
            }
            if let Some(out) = git_out(&dir, &args) {
                for c in read_log(&out, &name.to_string_lossy()) {
                    if !seen.contains(&c.sha) {
                        seen.push(c.sha.clone());
                        found.push(c);
                    }
                }
            }
        }
        // Newest first, the way every other list here stands.
        found.sort_by(|a, b| b.at.cmp(&a.at));
        found.truncate(30);
        return Some(found);
    }
    // Unit and record separators, so a message holding newlines survives the parse.
    let over = format!("--skip={}", skip.unwrap_or(0));
    let mut args: Vec<&OsStr> = vec![OsStr::new("log")];
    if all {
        // What each change did to each document: put in, taken out, moved, altered.
        args.push(OsStr::new("--name-status"));
        args.push(OsStr::new("-M"));
    } else {
        // A renamed document keeps the history it had under its old name, and
        // --name-only prints the name it carried at each of those commits.
        args.push(OsStr::new("--follow"));
        args.push(OsStr::new("--name-only"));
    }
    args.extend_from_slice(&[
        OsStr::new("-n"),
        OsStr::new("30"),
        OsStr::new(over.as_str()),
        OsStr::new(FMT),
    ]);
    if !all {
        args.push(OsStr::new("--"));
        args.push(&name);
    }
    let out = git_out(&dir, &args)?;
    Some(read_log(&out, &name.to_string_lossy()))
}

// Unit and record separators, so a message holding newlines survives the parse. The
// stamp at the end is only for putting two answers in one order.
const FMT: &str = "--format=%h%x1f%ar%x1f%s%x1f%b%x1f%ct%x1e";

fn read_log(out: &str, now: &str) -> Vec<Commit> {
    // That name is printed after the record it belongs to, so every piece of the split
    // holds the tail of one commit and the head of the next.
    let mut list: Vec<Commit> = Vec::new();
    let mut held: Option<(String, String, String, String, u64)> = None;
    for piece in out.split('\u{1e}') {
        // A record always opens with the hash and a unit separator on one line, and a
        // name never holds one. So the record begins at the line carrying the first
        // separator, and whatever stands above that line is the name of the commit
        // before it. Splitting on a blank line instead would cut inside a message.
        let (names, rec) = match piece.find('\u{1f}') {
            Some(u) => {
                let head = piece[..u].rfind('\n').map(|n| n + 1).unwrap_or(0);
                (&piece[..head], &piece[head..])
            }
            None => (piece, ""),
        };
        if let Some((sha, when, subject, body, at)) = held.take() {
            let (was, touched) = read_names(names, now);
            list.push(Commit { sha, when, subject, body, name: was, touched, at });
        }
        let rec = rec.trim_start_matches(['\n', '\r']);
        if rec.trim().is_empty() {
            continue;
        }
        let mut f = rec.split('\u{1f}');
        let sha = f.next().unwrap_or("").to_string();
        let when = f.next().unwrap_or("").to_string();
        let subject = f.next().unwrap_or("").to_string();
        let body = without_trailers(f.next().unwrap_or(""));
        let at = f.next().unwrap_or("").trim().parse::<u64>().unwrap_or(0);
        if !sha.is_empty() {
            held = Some((sha, when, subject, body, at));
        }
    }
    if let Some((sha, when, subject, body, at)) = held.take() {
        list.push(Commit {
            sha,
            when,
            subject,
            body,
            name: now.to_string(),
            touched: Vec::new(),
            at,
        });
    }
    list
}

// The lines git prints under a commit. Without a status letter they are plain names and
// the first one is the name the followed document carried; with one they say what the
// change did to each document it touched.
fn read_names(block: &str, now: &str) -> (String, Vec<Touch>) {
    let mut was = String::new();
    let mut touched = Vec::new();
    for line in block.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let mut cut = line.split('\t');
        let head = cut.next().unwrap_or("");
        match cut.next() {
            Some(first) => {
                let how = head.chars().next().unwrap_or('M').to_string();
                // A move prints the old name and then the new one.
                let (name, from) = match cut.next() {
                    Some(second) => (second.to_string(), first.to_string()),
                    None => (first.to_string(), String::new()),
                };
                if was.is_empty() {
                    was = name.clone();
                }
                touched.push(Touch { how, name, from });
            }
            None => {
                if was.is_empty() {
                    was = head.to_string();
                }
            }
        }
    }
    if was.is_empty() {
        was = now.to_string();
    }
    (was, touched)
}

// A document as it stood at one change. This is the only way to a document that is no
// longer on disk: there is nothing to open and ask about, so the record is asked instead.
// A change that took the document away has nothing under its own hash, and the version
// wanted is the one just before it — so that is tried next, and the answer says which
// of the two it came from.
#[derive(Serialize)]
struct Past {
    text: String,
    gone: bool,
}

#[tauri::command]
fn doc_at(app: tauri::AppHandle, path: String, sha: String, was: String) -> Option<Past> {
    use std::ffi::OsStr;
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (dir, now) = locate(&app, &path)?;
    let name = match was.is_empty() || was == now.to_string_lossy() {
        true => now.to_string_lossy().to_string(),
        false => {
            // The name never touches disk here, so only its shape is checked.
            if !Path::new(&was)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                return None;
            }
            was
        }
    };
    for (spec, gone) in [(format!("{sha}:{name}"), false), (format!("{sha}~1:{name}"), true)] {
        if let Some(text) = git_out(&dir, &[OsStr::new("show"), OsStr::new(spec.as_str())]) {
            return Some(Past { text, gone });
        }
    }
    None
}

// Git's trailers are bookkeeping that every commit carries. The reader is here for
// the reason the document changed, and a signature line is not it. Only a key of
// plain letters counts as one, so a sentence that happens to hold a colon stays.
fn without_trailers(body: &str) -> String {
    let mut lines: Vec<&str> = body.lines().collect();
    while let Some(last) = lines.last() {
        let t = last.trim();
        let drop = t.is_empty()
            || t.split_once(": ").map_or(false, |(key, _)| {
                !key.is_empty()
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if drop {
            lines.pop();
        } else {
            break;
        }
    }
    lines.join("
").trim_end().to_string()
}

#[derive(Serialize)]
struct Hunk {
    head: String,
    rows: Vec<Row>,
}

// A line split into the part it shares with the line that replaced it, the part that is
// its own, and the shared tail. The page draws the middle differently, so the eye lands on
// the words that changed without having to read both lines through.
#[derive(Serialize)]
struct Row {
    add: bool,
    pre: String,
    mid: String,
    post: String,
}

impl Row {
    fn whole(add: bool, text: &str) -> Row {
        Row { add, pre: String::new(), mid: text.to_string(), post: String::new() }
    }

    fn split(add: bool, text: &str, pre: usize, post: usize) -> Row {
        let v: Vec<char> = text.chars().collect();
        let cut = v.len() - post;
        Row {
            add,
            pre: v[..pre].iter().collect(),
            mid: v[pre..cut].iter().collect(),
            post: v[cut..].iter().collect(),
        }
    }
}

// How much two lines share at each end. Backed off to whitespace, so a word is never cut
// in half and what is marked is whole words.
fn common_edges(a: &[char], b: &[char]) -> (usize, usize) {
    let lim = a.len().min(b.len());
    let mut pre = 0;
    while pre < lim && a[pre] == b[pre] {
        pre += 1;
    }
    let mut post = 0;
    while post < lim - pre && a[a.len() - 1 - post] == b[b.len() - 1 - post] {
        post += 1;
    }
    while pre > 0 && !a[pre - 1].is_whitespace() {
        pre -= 1;
    }
    while post > 0 && a.len() > post && !a[a.len() - post - 1].is_whitespace() {
        post -= 1;
    }
    if pre + post > lim {
        post = lim - pre;
    }
    (pre, post)
}

// One stretch of the diff: the lines taken out, then the lines put in. Git lists them in
// that order, so the first out pairs with the first in, and so on down.
fn pair_rows(out: &[String], into: &[String], rows: &mut Vec<Row>) {
    let pairs: Vec<(usize, usize)> = (0..out.len().min(into.len()))
        .map(|i| {
            let a: Vec<char> = out[i].chars().collect();
            let b: Vec<char> = into[i].chars().collect();
            common_edges(&a, &b)
        })
        .collect();
    for (i, text) in out.iter().enumerate() {
        match pairs.get(i) {
            Some(&(pre, post)) => rows.push(Row::split(false, text, pre, post)),
            None => rows.push(Row::whole(false, text)),
        }
    }
    for (i, text) in into.iter().enumerate() {
        match pairs.get(i) {
            Some(&(pre, post)) => rows.push(Row::split(true, text, pre, post)),
            None => rows.push(Row::whole(true, text)),
        }
    }
}

// The lines one commit took out of this document and the lines it put in, gathered under
// the heading each change sits beneath. A line number says nothing to someone reading a
// rendered document — there are no numbers on screen to match it to — but a heading is
// written in the document itself, so it names the place and the page can go there.
#[tauri::command]
fn doc_diff(
    app: tauri::AppHandle,
    path: String,
    sha: String,
    was: Option<String>,
) -> Option<Vec<Hunk>> {
    use std::ffi::OsStr;
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (dir, now) = locate(&app, &path)?;
    // The name the document had at that commit, when the pane knows a different one.
    // It never touches disk, so only its shape is checked: no root, no climbing out.
    let name = match was.filter(|w| !w.is_empty() && w.as_str() != now.to_string_lossy()) {
        Some(w)
            if Path::new(&w)
                .components()
                .all(|c| matches!(c, Component::Normal(_))) =>
        {
            std::ffi::OsString::from(w)
        }
        _ => now,
    };
    let diff = git_out(
        &dir,
        &[
            OsStr::new("show"),
            // The message is already on screen; only the change is wanted here.
            OsStr::new("--format="),
            OsStr::new("--no-color"),
            // No surrounding lines. A paragraph is a line here, so a line of context is a
            // whole paragraph that did not change.
            OsStr::new("-U0"),
            OsStr::new(sha.as_str()),
            OsStr::new("--"),
            &name,
        ],
    )?;
    // The document as this commit left it, read only to find the heading above each hunk.
    let mut spec = std::ffi::OsString::from(format!("{}:./", sha));
    spec.push(&name);
    let after = git_out(&dir, &[OsStr::new("show"), &spec]).unwrap_or_default();
    let lines: Vec<&str> = after.lines().collect();
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut into: Vec<String> = Vec::new();
    for l in diff.lines() {
        if l.starts_with("@@") {
            if let Some(h) = hunks.last_mut() {
                pair_rows(&out, &into, &mut h.rows);
            }
            out.clear();
            into.clear();
            let at = l
                .split(" +")
                .nth(1)
                .and_then(|r| r.split([',', ' ']).next())
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0);
            let head = heading_above(&lines, at);
            // Two changes under one heading are one place to go, so they are one entry.
            if hunks.last().map_or(true, |h| h.head != head) {
                hunks.push(Hunk { head, rows: Vec::new() });
            }
        } else if !hunks.is_empty() {
            // Before the first hunk sit git's own --- and +++ lines, which these two
            // arms would otherwise read as a removal and an addition.
            if let Some(rest) = l.strip_prefix('+') {
                into.push(rest.to_string());
            } else if let Some(rest) = l.strip_prefix('-') {
                out.push(rest.to_string());
            }
        }
    }
    if let Some(h) = hunks.last_mut() {
        pair_rows(&out, &into, &mut h.rows);
    }
    hunks.retain(|h| !h.rows.is_empty());
    Some(hunks)
}

// The nearest heading at or above a line. Empty when the change sits before the first one.
fn heading_above(lines: &[&str], line: usize) -> String {
    let mut i = line.min(lines.len());
    while i > 0 {
        let t = lines[i - 1].trim_start();
        if t.starts_with('#') {
            return t.trim_start_matches('#').trim().to_string();
        }
        i -= 1;
    }
    String::new()
}

#[tauri::command]
fn term_write(term: State<Term>, project: String, data: String) {
    if let Ok(mut guard) = term.0.lock() {
        if let Some(sh) = guard.get_mut(&project) {
            use std::io::Write;
            let _ = sh.writer.write_all(data.as_bytes());
            let _ = sh.writer.flush();
        }
    }
}

#[tauri::command]
fn term_resize(term: State<Term>, project: String, cols: u16, rows: u16) {
    if let Ok(guard) = term.0.lock() {
        if let Some(sh) = guard.get(&project) {
            let _ = sh.master.resize(size(cols, rows));
        }
    }
}

fn main() {
    // A panic anywhere — a command, the thread reading the shell, the setup — otherwise
    // either dies quietly in its own thread or tears the process down with no word at
    // all. Either way the person is left with a window that no longer answers.
    std::panic::set_hook(Box::new(|info| {
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Something went wrong inside the app.".to_string());
        let place = info
            .location()
            .map(|l| format!("\n{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        fail(&format!("{what}{place}"));
    }));

    // Only in a debug build: a way to see the box above without waiting for a real fault.
    #[cfg(debug_assertions)]
    if std::env::args().any(|a| a == "--fail-now") {
        panic!("this is the warning you get when something breaks");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Root::default())
        .manage(Term::default())
        .invoke_handler(tauri::generate_handler![
            about,
            open_external,
            pick,
            branch,
            doc_log,
            doc_at,
            doc_diff,
            term_open,
            term_write,
            term_interrupt,
            term_resize
        ])
        .register_uri_scheme_protocol("refdex", |ctx, request: Request<Vec<u8>>| {
            let root = ctx
                .app_handle()
                .try_state::<Root>()
                .and_then(|s| s.0.lock().ok().and_then(|g| g.clone()));
            let path = request.uri().path().trim_start_matches('/').to_string();
            serve(root, &path)
        })
        .setup(|app| {
            if let Some(given) = root_from_args() {
                if let Ok(mut guard) = app.state::<Root>().0.lock() {
                    *guard = Some(given);
                }
            }
            // The window is built here rather than described in the config, because the
            // latch below can only be tied on while it is being built. Nothing takes this
            // window off its own page: there is no address bar and no way back, so a page
            // it wandered onto would be the end of the session — and a page from the
            // chosen folder would run where this app's own page runs.
            // The window it wants and the floor it keeps, both held inside the screen it
            // opens on. A window wider than the monitor hangs off the edge with no way to
            // pull it back, and a floor taller than the monitor cannot be met at all.
            let (want_w, want_h, least_w, least_h) = app
                .primary_monitor()
                .ok()
                .flatten()
                .map(|m| {
                    let a = m.work_area().size.to_logical::<f64>(m.scale_factor());
                    let w = (a.width - 40.0).max(360.0);
                    let h = (a.height - 60.0).max(300.0);
                    (w.min(1400.0), h.min(900.0), w.min(820.0), h.min(520.0))
                })
                .unwrap_or((1400.0, 900.0, 820.0, 520.0));
            let window = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::default(),
            )
            .title("Refdex")
            .inner_size(want_w, want_h)
            .min_inner_size(least_w, least_h)
            .resizable(true)
            .on_navigation(|url| url.as_str().starts_with(UI_ORIGIN))
            .build()?;
            // The page follows the system's light or dark through a stylesheet query; the
            // frame has to be told, and told again whenever that setting changes.
            #[cfg(windows)]
            {
                let dark = |t: tauri::Theme| matches!(t, tauri::Theme::Dark);
                paint_frame(&window, window.theme().map(dark).unwrap_or(true));
                let framed = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::ThemeChanged(theme) = event {
                        paint_frame(&framed, dark(*theme));
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start the refdex window")
        // The shell is a child of this process, and a child does not always go when its
        // parent does. Closing the window must not leave one behind holding the folder.
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<Term>() {
                    if let Ok(mut guard) = state.0.lock() {
                        for (_, old) in guard.drain() {
                            stop(old);
                        }
                    }
                }
            }
        });
}

// The parts that decide what leaves this process, and the parts that turn one text into
// another. They take plain values, so they can be checked without a window: `cargo test`.
// Nothing here is compiled into the app.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_documents_and_pictures_are_servable() {
        assert!(servable("docs/USING.md"));
        assert!(servable("DOCS/Using.MD"));
        assert!(servable("art/cover.png"));
        assert!(!servable("secrets/.env"));
        assert!(!servable("ui/index.html"));
        assert!(!servable("notes.md.txt"));
        assert!(!servable("no-extension"));
    }

    #[test]
    fn a_path_may_not_leave_the_folder() {
        let root = std::env::temp_dir().join("refdex-inside-test");
        let deep = root.join("one");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("a.md"), "x").unwrap();
        std::fs::write(std::env::temp_dir().join("refdex-outside.md"), "x").unwrap();

        assert!(inside(&root, "one/a.md").is_some());
        assert!(inside(&root, "../refdex-outside.md").is_none());
        assert!(inside(&root, "one/../../refdex-outside.md").is_none());
        assert!(inside(&root, "/one/a.md").is_none());
        assert!(inside(&root, "one/missing.md").is_none());
        assert!(inside(&root, "one").is_none());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(std::env::temp_dir().join("refdex-outside.md")).ok();
    }

    #[test]
    fn percent_pairs_are_read_as_bytes() {
        assert_eq!(percent_decode("docs/a%20b.md"), "docs/a b.md");
        assert_eq!(percent_decode("%ED%95%9C"), "한");
        // A stray percent is a percent, and a half-written pair at the end stays as it is.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn the_graph_library_is_served_from_this_app() {
        let page = b"<script src=\"https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js\" integrity=\"sha384-x\"></script>".to_vec();
        let out = String::from_utf8(offline_graph(page)).unwrap();
        assert!(out.contains(UI_ORIGIN));
        assert!(!out.contains("unpkg.com"));
        // The integrity hash belongs to the copy on the CDN and goes with the tag.
        assert!(!out.contains("integrity"));
        // A page that names no such tag keeps its own tags.
        let plain = b"<p>no library here</p>".to_vec();
        assert_eq!(offline_graph(plain.clone()), plain);
    }

    #[test]
    fn the_graph_is_asked_for_the_machines_theme() {
        let page = b"<html><body><button data-theme-choice=\"system\"></button></body></html>".to_vec();
        let out = String::from_utf8(offline_graph(page)).unwrap();
        // The line runs after everything the page itself puts in the body.
        let at = out.find("data-theme-choice").unwrap();
        assert!(out.find(GRAPH_SYSTEM_THEME).unwrap() > at);
        assert!(out.ends_with("</body></html>"));
        // Nothing to add it to, so nothing is added.
        let bare = b"<p>not a page</p>".to_vec();
        assert_eq!(offline_graph(bare.clone()), bare);
    }

    #[test]
    fn mime_is_given_for_what_is_served_and_bytes_otherwise() {
        assert_eq!(mime_of("a.md"), "text/markdown; charset=utf-8");
        assert_eq!(mime_of("a.png"), "image/png");
        assert_eq!(mime_of("a.zip"), "application/octet-stream");
    }

    #[test]
    fn a_signature_line_is_not_part_of_the_reason() {
        let body = "Why this was done.\n\nSigned-off-by: Someone <a@b.c>\nCo-Authored-By: X <y@z>\n";
        assert_eq!(without_trailers(body), "Why this was done.");
        // A line that merely holds a colon is prose, not a trailer.
        let keep = "It broke here: the second call.";
        assert_eq!(without_trailers(keep), keep);
    }

    #[test]
    fn the_heading_a_line_sits_under_is_the_one_above_it() {
        let lines = vec!["# Top", "a", "## Second", "b", "c"];
        assert_eq!(heading_above(&lines, 2), "Top");
        assert_eq!(heading_above(&lines, 5), "Second");
        assert_eq!(heading_above(&["no heading", "at all"], 2), "");
    }

    #[test]
    fn the_changed_part_of_a_line_stops_at_a_word_edge() {
        let a: Vec<char> = "the quick brown fox".chars().collect();
        let b: Vec<char> = "the slow brown fox".chars().collect();
        let (pre, post) = common_edges(&a, &b);
        assert_eq!(&a[..pre].iter().collect::<String>(), "the ");
        assert_eq!(&a[a.len() - post..].iter().collect::<String>(), "brown fox");
    }

    #[test]
    fn two_sides_of_a_change_are_paired_in_order() {
        let out = vec!["one".to_string(), "two".to_string()];
        let into = vec!["ONE".to_string()];
        let mut rows = Vec::new();
        pair_rows(&out, &into, &mut rows);
        // Every line on both sides stands, paired where there is a partner.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().filter(|r| r.add).count() == 1);
    }
}
