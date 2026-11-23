# 명령

[README](../README.md) / `docs` — [ARCHITECTURE](ARCHITECTURE.md) · [USING](USING.md) · [COMMANDS](COMMANDS.md) · [VENDOR](VENDOR.md)

Refdex를 실행하고, 빌드하고, 배포 파일을 만드는 명령을 모아 둔다. 각 명령이 무엇을 하는지는 그 기능의 문서에 있다.

**COMMANDS 패널에 서는 순서는 이 문서에 적힌 순서다.** 패널은 프로젝트의 문서를 경로 순으로 읽고 각 문서 안에서는 위에서 아래로 블록을 모은다. 그러니 자주 쓰는 것을 위에 적으면 패널에서도 위에 선다.

**터미널은 열어 둔 프로젝트 폴더에서 시작한다.** Refdex의 터미널 탭은 지금 선택한 프로젝트의 폴더에서 셸을 띄우므로, 이 문서의 명령들은 그대로 돌아간다. 다른 경로에서 돌린다면 저장소 폴더로 먼저 이동한다 — `cd refdex`.

아래 블록의 **실행 버튼**은 터미널에 입력만 하고 **마지막 Enter는 누르지 않는다.** 무엇이 실행될지 읽고 네가 누르면 된다.

## Refdex 실행

빌드하고 창까지 띄운다. 코드를 고친 뒤 바로 확인할 때 쓴다.

```powershell
cargo run --manifest-path src-tauri/Cargo.toml
```

처음 실행할 때는 크레이트를 내려받느라 몇 분 걸리고, 그다음부터는 몇 초다.

**이미 실행 중인 Refdex가 있으면 먼저 닫는다.** 실행 중인 앱이 자기 실행 파일을 잠그기 때문에, 열어 둔 채로 빌드하면 `액세스가 거부되었습니다`로 멈춘다. `ui/index.html`을 고쳤을 때도 다시 빌드해야 한다 — 화면이 실행 파일 안에 포함되기 때문이다.

## 다른 폴더로 열기

설치된 Refdex로 다른 폴더를 **새 창**에서 연다. 따옴표 안의 경로만 바꾸면 된다.

```powershell
& "C:\Program Files\Refdex\refdex.exe" "D:\Projects"
```

창은 여러 개를 함께 띄울 수 있다. 늘 같은 폴더로 열고 싶으면 이 줄을 바로가기의 대상에 적어 두면 된다 — [USING](USING.md)에 있다.

## Refdex 빌드

실행 파일만 만든다.

```powershell
cargo build --release --manifest-path src-tauri/Cargo.toml
```

`src-tauri/target/release/refdex.exe`가 나온다. 릴리스 빌드라 콘솔 창이 뜨지 않는다. 이 파일 하나가 그대로 **포터블 버전**이다 — 화면이 실행 파일 안에 있어서 옆에 둘 파일이 없다.

## 테스트 검사

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
```

터미널에 `10 passed; 0 failed` 형식으로 결과가 나온다. 하나라도 실패하면 어디가 왜 틀렸는지 함께 출력된다. 창을 띄우지 않고 함수에 값을 넣어 결과만 확인하는 테스트 10개다 — 경로가 폴더를 벗어나는지, 어떤 파일을 내보내는지, `%` 디코딩, 그래프 라이브러리 교체, 그래프 테마 요청, 파일 형식 판별, 커밋 메시지의 서명 줄 제거, 바뀐 줄이 속한 제목 찾기, 바뀐 줄 짝짓기, 바뀐 부분을 단어 중간에서 자르지 않기.

앱에 포함되는 코드가 아니다 — `#[cfg(test)]` 안에 있어서 빌드된 실행 파일에는 들어가지 않는다.

## 설치 파일 생성

`.msi` 설치 파일을 만든다.

```powershell
cargo tauri build
```

`src-tauri/target/release/bundle/msi/Refdex_1.0.0_x64_en-US.msi`가 나온다 — 3.80MiB, 안에 든 파일은 `refdex.exe` 하나다(2026-11-23). 파일 이름의 버전은 `Cargo.toml`에서 가져온다(`D31`).

**처음 한 번은 준비가 필요하다.** `cargo install tauri-cli --version "^2" --locked`로 Tauri CLI를 설치하면, 그 CLI가 실제로 `.msi`를 만드는 WiX Toolset을 `%LOCALAPPDATA%\tauri\WixTools314`에 내려받는다. 그래서 첫 실행에는 인터넷이 필요하고, 그다음부터는 내려받은 것을 쓴다.

설치하면 `C:\Program Files\Refdex`에 들어가고 관리자 권한을 묻는다. 설치 마법사는 영어다. 제거할 때 뜨는 확인 창만 OS 언어를 따르는데, 그건 Windows가 띄우는 것이다([D35](ARCHITECTURE.md)).

## 해시 조회

배포할 파일의 `SHA-256`을 구한다. **실제로 배포하는 그 파일**에서 구해야 한다.

```powershell
Get-FileHash .\Refdex_1.0.0_x64_en-US.msi -Algorithm SHA256
```

나온 64자리를 내려받는 페이지에 파일과 **나란히** 적는다. 받는 사람이 같은 명령으로 자기 파일을 검사해서 비교하면, 전송 중 손상되거나 변조되지 않았는지 직접 확인할 수 있다. 서명을 붙이지 않는 이유는 [D35](ARCHITECTURE.md)에 있다.

**다시 빌드하면 값이 바뀐다.** 같은 코드라도 빌드 시각이 실행 파일에 기록되기 때문이다(`D35`). 배포할 파일을 먼저 정하고, 그 파일에서 해시를 구해서, 그 파일과 함께 올린다.
