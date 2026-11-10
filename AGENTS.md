# AGENTS.md

Fast Explorer(내부 이름 Fex)는 Windows 전용 파일 탐색기입니다. Rust 2024 edition 워크스페이스이고 UI는 Slint로 그립니다. 이 문서는 저장소에서 작업하는 에이전트를 위한 안내입니다. 사용자 대상 설명은 `README.md`에 있습니다.

## 빠른 명령

```powershell
cargo build --workspace                 # 디버그 빌드 (fex.exe, fex-shell-host.exe, fex-index.exe)
cargo build --release                   # 배포 빌드 (thin LTO)
cargo run -p fex -- "C:\"               # 앱 실행 (인자: 시작 폴더 또는 "내 PC")
cargo test --workspace                  # 전체 테스트
cargo test -p fex-core                  # 엔진 단위 테스트
cargo clippy --workspace --all-targets  # 린트 (저장소에 clippy 설정은 없음)
cargo fmt                               # 포맷 (rustfmt 설정 파일은 없음)
```

- `cargo test --workspace`는 `shell-host` 통합 테스트를 포함하며, 이 테스트는 실제 프로세스와 Explorer 셸을 사용합니다. 창이 있는 대화형 Windows 세션에서 실행하세요.
- `fex-index`의 `tests/live.rs`는 `#[ignore]`입니다. 서비스 설치 후 `cargo test -p fex-index --test live -- --ignored`로 실행합니다.
- `target/`은 git에서 제외되어 있습니다. `Cargo.lock`은 커밋되어 있으므로 의존성을 바꾸면 함께 갱신하세요.

## 워크스페이스 지도

| 경로 | 역할 |
| --- | --- |
| `core/` (`fex-core`) | UI에 독립적인 엔진. Win32 API는 여기에 모읍니다. |
| `core/src/lib.rs` | `Entry`, `list_dir`(NtQueryDirectoryFile), 정렬, 크기/날짜 포맷 |
| `core/src/search.rs` | 검색 패턴(단어/글롭/정규식), 범위(`Scope`), 쿼리→`Plan` 해석, 폴더 순회 |
| `core/src/index.rs` | MFT/USN 저널, 인메모리 색인, 인덱스 파이프 클라이언트와 프로토콜 |
| `core/src/shell.rs` | IFileOperation, 클립보드, 드라이브/알려진 폴더, 승격 실행, 파일 열기 |
| `core/src/menu.rs` | 셸 메뉴 호스트 클라이언트와 stdio 프로토콜 |
| `core/src/watch.rs` | `ReadDirectoryChangesW` 기반 폴더 감시 |
| `core/src/icons.rs` | 셸 아이콘 → RGBA |
| `core/src/rename.rs` | 일괄 이름 바꾸기 규칙과 미리보기 계획 (순수 로직, 단위 테스트 있음) |
| `core/src/locks.rs` | Restart Manager로 파일을 잡은 프로세스 찾기/종료 |
| `ui/` (`fex`) | Slint 앱. `app.slint` + `src/main.rs`, `tree.rs`, `dnd.rs`, `popup.rs` |
| `ui/src/main.rs` | `App`/`Tab`/`Rows` 상태, 이벤트 배선, 검색·탭·파일 작업, Win32 창 처리 |
| `ui/src/tree.rs` | 검색 결과 폴더 트리와 가상화된 행 순서 |
| `ui/src/dnd.rs` | OLE `IDropSource`/`IDropTarget` |
| `ui/src/popup.rs` | Win32 팝업 메뉴 (자체 명령 + 셸 항목) |
| `ui/src/tools.rs` | 여러 이름 바꾸기 / 사용 중인 프로세스 대화상자 (`RenameWindow`, `LocksWindow`) |
| `shell-host/` | 셸 확장 메뉴를 격리 실행하는 별도 프로세스 |
| `index-service/` | MFT/USN 색인 Windows 서비스와 설치/제거 CLI |
| `ui/src/update.rs` | `latest.json` 업데이트 확인/다운로드 (Windows 내장 `curl.exe`) |
| `installer/` | Inno Setup 스크립트 `fex.iss`와 `build.ps1`. 릴리스는 `.github/workflows/release.yml` |

## 아키텍처 규칙

- **UI 스레드를 막지 마세요.** 디스크 나열, 검색, 셸 작업, 아이콘 로딩, 승격 실행은 모두 워커 스레드에서 하고, 결과는 `post(...)`로 UI 스레드에 돌려보냅니다. `App`은 `APP` 스레드 로컬에 있고 `app()`으로만 접근합니다.
- **오래된 결과를 버리세요.** 탭마다 `epoch`(폴더 나열), `search_seq`(검색), `sort_gen`(정렬)이 있습니다. 새 요청이 시작되면 이전 작업의 결과를 적용하지 않아야 합니다.
- **목록은 가상화되어 있습니다.** `Rows`는 Slint `Model`이며 화면에 보이는 행만 요청받습니다. 행 수가 많아도 모든 행의 UI 객체를 만들지 마세요.
- **Win32는 `windows` 0.62 스타일**을 씁니다. 새 API를 쓰려면 해당 크레이트의 `Cargo.toml`에 feature를 추가해야 합니다. `HSTRING`, `Owned`, `PCWSTR` 패턴을 주변 코드에서 따르세요.
- **blocking 호출은 COM 스레드 규칙을 지키세요.** 아이콘 로딩은 `icons::init_thread()`로 STA를 초기화한 스레드에서, 셸 메뉴는 `fex-shell-host`에서 실행합니다.
- **Slint 파일과 Rust 배선을 함께 바꾸세요.** `app.slint`의 콜백/속성은 `ui/src/main.rs`의 `ui.on_*`/`set_*`과 짝을 이룹니다. `build.rs`가 `app.slint`를 컴파일하고 아이콘/버전 리소스를 넣습니다.

## 보안 불변 조건

- 인덱스 파이프(`\\.\pipe\fast-explorer-index`)의 ACL과 **서버 PID 검증**(`index::query`)을 약화하지 마세요. 다른 프로세스가 인덱스인 척하면 안 됩니다.
- 인덱스 서비스는 LocalSystem으로 실행되며 설치 사용자 SID만 파이프에 허용합니다. 이 경계를 넓히지 마세요.
- 앱은 **Program Files 아래에 있을 때만** 인덱스 서비스 설치를 승격 제안합니다. 사용자가 덮어쓸 수 있는 실행 파일을 관리자로 실행하면 권한 상승 취약점이 됩니다.
- 인덱스 서비스는 클라이언트가 나열할 수 없는 폴더의 결과를 숨깁니다(`visible_to_client`). 이 필터를 우회하는 경로를 만들지 마세요.
- 업데이트는 자기 저장소의 `releases/download/` 주소만 받고, 설치 전에 항상 사용자에게 묻습니다. 무음 설치를 추가하지 마세요.
- 셸 확장은 신뢰할 수 없는 코드입니다. 반드시 `fex-shell-host`에서 실행하고, 응답이 없으면 교체하는 `HANG_LIMIT`(10초) 동작을 유지하세요.

## 프로토콜

- **셸 메뉴**: 호스트 stdin/stdout에 한 줄에 하나의 요청, 탭 구분 필드입니다. 요청/응답 형식은 `core/src/menu.rs` 상단 주석과 `shell-host/src/main.rs`에 있습니다. 항목 줄은 `depth id flags verb icon label` 순서이며, `icon`은 `WxH:` + 16진 BGRA(미리 곱한 알파) 또는 빈 문자열입니다. 양쪽을 함께 바꾸고 `shell-host/tests/protocol.rs`로 검증하세요.
- **인덱스**: 요청 `S\t<limit>\t<scope-kind>\t<scope-path>\t<query>`, 응답 첫 줄 `OK2\t<count>\t<indexing>\t<more>`. 구버전 `OK` 응답은 거부되어 재설치를 안내합니다. 형식을 바꾸면 `REPLY_OK` 버전을 올리고 `index-service/tests/live.rs`를 갱신하세요.
- **인코딩**: 경로는 `menu::enc_path`/`dec_path`로 파이프/stdio에서 안전하게 주고받습니다. 탭·개행이 들어가는 필드는 반드시 이스케이프하세요.

## 코드 관례

- UI 문자열과 사용자에게 보이는 오류 메시지는 **한국어**입니다. 코드 주석은 영어를 씁니다.
- 기존 코드는 간결한 한 줄 구조체/표현식을 선호합니다. 주변 스타일을 따르고, 불필요한 추상화를 만들지 마세요.
- `// ponytail:` 주석은 "지금은 이렇게 두고, 필요해지면 개선한다"는 표시입니다. 관련 최적화를 할 때는 주석도 함께 정리하세요.
- 오류는 `io::Result`/`Result<_, String>`로 전달하고, UI에는 `set_note`로 짧게 보여 줍니다. `unwrap()`은 테스트나 불변식이 확실한 곳에만 쓰세요.
- 새 의존성은 신중히 추가하세요. 이미 `windows`, `slint`, `ignore`, `windows-service`를 사용합니다.
- 릴리스 빌드에서 콘솔 창을 숨기는 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`를 유지하세요.

## 자주 하는 변경

- **새 파일 작업/단축키**: `ui/src/main.rs`의 `App::key`, `context_menu`, `shell::Op`를 확인합니다.
- **새 검색 문법**: `core/src/search.rs`의 `Pattern::compile`/`plan`과 `ui/src/main.rs`의 `search_in`을 함께 수정합니다. 인덱스 검색과 직접 순회가 같은 패턴을 써야 합니다.
- **새 UI 요소**: `ui/app.slint`에 컴포넌트/콜백을 추가하고 `ui/src/main.rs`의 `main()`에서 배선합니다.
- **새 Win32 기능**: 해당 크레이트의 `windows` feature를 추가하고, UI 스레드에서 호출해도 되는지 먼저 판단합니다.
- **인덱스 서비스 변경**: `core/src/index.rs`(공용 로직)와 `index-service/src/main.rs`(서비스)를 함께 봅니다. 설치는 Program Files 복사와 서비스 등록을 포함합니다.

## 검증 체크리스트

변경 후 최소한 다음을 실행하세요.

```powershell
cargo check --workspace
cargo test -p fex-core
cargo test -p fex
```

UI나 셸 통합을 건드렸다면 대화형 세션에서 `cargo test -p fex-shell-host`와 앱 실행(`cargo run -p fex`)까지 확인하세요. 인덱스 프로토콜을 건드렸다면 서비스를 재설치한 뒤 `live` 테스트를 실행하세요.
