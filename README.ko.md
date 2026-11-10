# Fast Explorer (Fex)

한국어 · [English](README.md)

Fast Explorer는 Windows용 파일 탐색기입니다. 큰 폴더를 빠르게 나열하고, 탭·히스토리·셸 통합을 제공하며, NTFS MFT/USN 저널을 이용해 모든 드라이브의 파일 이름을 즉시 검색합니다. Rust와 Slint로 만들었습니다.

**[⬇ 설치 파일 다운로드 (최신 버전)](https://github.com/daper-corp/Fast-Explorer/releases/latest/download/FastExplorer-Setup.exe)** · [모든 릴리스](https://github.com/daper-corp/Fast-Explorer/releases)

## 주요 기능

### 빠른 폴더 나열
- `NtQueryDirectoryFile`과 1 MB 버퍼로 한 번에 읽고, 4096개 단위 배치로 UI에 전달합니다.
- 이름은 Explorer와 같은 자연 정렬(`StrCmpLogicalW`)을 쓰고, 폴더를 항상 먼저 보여 줍니다.
- 크기·수정한 날짜·유형·이름 열 정렬, 열 경계 드래그로 너비 조절, 경계 더블클릭으로 내용에 맞춤.
- 탭, 뒤로/가기 히스토리, 주소 표시줄 경로 버튼(빵부스러기), 자동완성, `%환경변수%` 확장.
- 마우스 뒤로/앞으로 버튼, 백스페이스로 뒤로 가기, 타이핑으로 이름 찾기(type-ahead).

### 검색
- **현재 폴더**: 그 폴더만 나열해 일치하는 항목을 고릅니다.
- **하위 폴더**: 인덱스가 있는 볼륨이면 인덱스를, 아니면 `ignore` 크레이트로 직접 순회합니다.
- **전체 검색**: 인덱스 서비스를 통해 모든 고정 NTFS 드라이브의 파일 이름을 즉시 검색합니다.
- 검색 결과는 폴더 트리로 묶여 표시되고, 트리를 접고 펼칠 수 있습니다. "위치" 열에 실제 폴더가 나옵니다.
- 검색 범위는 검색 상자 옆 콤보 상자에서 고릅니다: `현재 폴더`, `하위 폴더`, `전체 검색`.
- 쿼리에 경로가 들어 있으면 그 경로가 검색 범위가 됩니다.

| 입력 | 의미 |
| --- | --- |
| `report` | 이름 어디에든 `report`가 포함된 항목 (대소문자 무시) |
| `annual report` | 두 단어를 모두 포함한 항목 (공백 AND) |
| `*.txt`, `a?c` | 이름 전체에 대한 와일드카드 |
| `re:^IMG_\d+\.jpg$`, `/^IMG_\d+\.jpg$/` | 정규식 (기본 대소문자 무시) |
| `C:\Work\*.txt`, `sub\report` | 해당 폴더로 범위를 좁힌 검색 |

- 목록에는 최대 2000개 결과를 표시합니다. 더 많은 결과가 있으면 상태 표시줄에 "더 있음, 처음 N개 표시"라고 알려 줍니다.
- 인덱스 서비스는 클라이언트가 폴더를 나열할 권한이 없는 항목(예: `System32\config`, `System Volume Information`)을 결과에서 제외합니다.

### 셸 통합
- 파일 복사·이동·삭제·이름 바꾸기는 `IFileOperation`으로 실행해 Explorer와 같은 진행률, 충돌 처리, 휴지통 동작을 냅니다. `Shift+Delete`는 영구 삭제입니다.
- 파일 탐색기 클립보드(CF_HDROP)를 사용해 다른 프로그램과 잘라내기/복사/붙여넣기가 됩니다.
- OLE 드래그 앤 드롭: 선택 항목을 다른 앱으로 끌어다 놓고, 파일을 목록이나 폴더 행에 떨어뜨릴 수 있습니다.
- 셸 확장 컨텍스트 메뉴는 별도 프로세스(`fex-shell-host`)에서 만들고 실행합니다. 확장이 멈추거나 죽어도 메인 창은 살아 있고, 10초 넘게 응답이 없으면 호스트를 교체합니다. 셸 메뉴가 준비되지 않았으면 "더 많은 옵션…"으로 나중에 띄울 수 있습니다.
- 셸 아이콘은 워커 스레드에서 읽으며, 실행 파일·바로 가기처럼 파일 자체의 아이콘이 필요한 형식도 지원합니다.
- 여러 이름 바꾸기(`Shift+F2`): 찾아 바꾸기(정규식, 대/소문자, 이름/확장자 선택, `{n}` 번호 매기기)를 미리 보며 적용하고 `Ctrl+Z`로 되돌립니다. PowerToys의 PowerRename에 해당합니다.
- 사용 중인 프로세스 찾기(우클릭): 선택한 파일/폴더 안의 파일을 잡고 있는 프로세스를 Restart Manager로 찾아 종료할 수 있습니다. PowerToys의 File Locksmith에 해당하며, 폴더를 작업 디렉터리로만 쓰는 프로세스는 찾지 못합니다.
- 새 폴더(`Ctrl+Shift+N`), 새 텍스트 문서, 속성(`Alt+Enter`), 경로 복사(`Ctrl+Shift+C`)를 제공합니다.

### 창과 뷰
- 프레임 없는 사용자 정의 창(최소화·최대화·닫기, 가장자리 크기 조절, DWM 그림자)과 앱 아이콘.
- 사이드바: 즐겨찾기와 드라이브. 드라이브에는 사용률 막대가 표시됩니다.
- "내 PC" 가상 보기: 드라이브와 사용자 폴더를 전체 용량/여유 공간과 함께 보여 주고, 폴더를 드라이브보다 먼저 묶어 정렬합니다. 주소 표시줄에 `내 PC`, `내PC`, `this pc`를 입력해 열 수 있습니다.
- 현재 폴더를 `ReadDirectoryChangesW`로 감시해 다른 프로그램이 만든 파일을 새로 고침 없이 반영하고, `WM_DEVICECHANGE`로 드라이브 목록도 갱신합니다.
- 숨김 항목/파일 확장자 표시 설정은 세션 사이에 유지되며, 첫 실행에는 Explorer의 설정을 따릅니다.

### 단축키

| 단축키 | 동작 |
| --- | --- |
| `Ctrl+T` / `Ctrl+W` | 새 탭 / 탭 닫기 |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | 다음 / 이전 탭 |
| `Alt+←` / `Alt+→` / `Alt+↑` | 뒤로 / 앞으로 / 상위 폴더 |
| `Backspace` | 뒤로 |
| `Ctrl+L`, `Alt+D`, `F4` | 주소 표시줄로 이동 |
| `Ctrl+F`, `F3` | 검색 상자로 이동 |
| `F5`, `Ctrl+R` | 새로 고침 |
| `Enter` | 열기 |
| `Alt+Enter` | 속성 |
| `F2` | 이름 바꾸기 |
| `Shift+F2` | 여러 이름 바꾸기 (선택 항목 일괄) |
| `Delete` / `Shift+Delete` | 휴지통으로 삭제 / 영구 삭제 |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | 복사 / 잘라내기 / 붙여넣기 |
| `Ctrl+Shift+C` | 경로 복사 |
| `Ctrl+Shift+N` | 새 폴더 |
| `Ctrl+A`, `Ctrl+Space` | 모두 선택, 선택 토글 |
| `Shift+F10`, 메뉴 키 | 컨텍스트 메뉴 |
| `Esc` | 검색 종료, 목록으로 포커스 |

## 구성

Cargo 워크스페이스이며, 실행 파일 세 개와 공용 라이브러리 하나로 이루어집니다.

| 크레이트 | 실행 파일 | 역할 |
| --- | --- | --- |
| `fex-core` | 없음 | UI에 독립적인 엔진: 폴더 나열·정렬·표시 형식, 검색 패턴/순회, 셸 동작, 아이콘, 폴더 감시, 인덱스 클라이언트, 셸 메뉴 클라이언트 |
| `fex` (ui) | `fex.exe` | Slint UI, 창/입력 처리, 탭과 검색 상태, Win32 연동(drag & drop, DWM, 아이콘) |
| `fex-shell-host` | `fex-shell-host.exe` | 셸 확장 컨텍스트 메뉴를 격리 실행하는 호스트 (stdin/stdout 프로토콜) |
| `fex-index` | `fex-index.exe` | NTFS MFT/ USN 저널 색인 Windows 서비스 (이름 검색 파이프) |

## 요구 사항

- Windows 10/11
- Rust stable (edition 2024, Rust 1.85 이상)와 MSVC 빌드 도구
- 전체 검색 인덱스: 관리자 권한, NTFS로 포맷된 고정 드라이브

## 빌드

```powershell
cargo build --release
```

결과물은 `target\release\`에 생깁니다.

- `fex.exe` — 탐색기 본체
- `fex-shell-host.exe` — 셸 메뉴 호스트
- `fex-index.exe` — 인덱스 서비스 (설치용)

배포할 때는 세 실행 파일을 같은 폴더에 두세요. `fex.exe`는 옆에서 `fex-shell-host.exe`를 찾아 실행하고, 인덱스 서비스 설치도 같은 폴더의 `fex-index.exe`를 사용합니다.

## 설치 파일

```powershell
.\installer\build.ps1   # 릴리스 빌드 후 target\installer\FastExplorer-Setup-<버전>.exe 생성 (Inno Setup 6 필요)
```

- `%ProgramFiles%\Fast Explorer`에 설치하며 관리자 권한이 필요합니다(인덱스 서비스 보안 조건).
- 설치 옵션: 바탕 화면 바로 가기, 인덱스 서비스(기본 켜짐), **기본 파일 탐색기로 사용**(기본 꺼짐).
- 기본 탐색기로 사용하면 폴더·드라이브를 여는 기본 동작이 Fast Explorer로 바뀝니다. Win+E와 작업 표시줄의 탐색기 아이콘은 계속 Explorer를 엽니다. 제거하면 원래대로 돌아갑니다.
- 제거하면 인덱스 서비스도 함께 제거됩니다.

## 배포와 업데이트

- 워크스페이스 `Cargo.toml`의 `version`이 유일한 버전입니다. 올린 뒤 같은 이름의 태그(`v0.2.0`)를 push하면 `.github/workflows/release.yml`이 설치 파일과 `latest.json`을 GitHub 릴리스로 올립니다.
- 앱은 하루 한 번 백그라운드에서 `releases/latest/download/latest.json`을 확인하고, 새 버전이 있으면 설치할지 묻습니다. 동의하면 설치 파일을 받아 실행하고 앱을 닫습니다. 묻지 않고 설치하지 않습니다.
- 빈 공간이나 내 PC에서 우클릭 → "업데이트 확인…"으로 직접 확인할 수 있습니다.
- 업데이트 확인은 저장소가 public이어야 동작합니다. 코드 서명이 아직 없어 설치 시 SmartScreen 경고가 뜹니다.

## 실행

```powershell
cargo run -p fex -- "C:\"
```

인자 없이 실행하면 `C:\`에서 시작합니다. `내 PC` 보기로 시작하려면 `"내 PC"`를 인자로 주면 됩니다.

## 인덱스 서비스

전체 검색은 관리자 권한으로 볼륨을 읽어야 하므로 별도 Windows 서비스가 색인을 만듭니다.

```powershell
fex-index.exe install     # 관리자: Program Files에 복사, 서비스 등록 및 시작
fex-index.exe uninstall   # 관리자: 서비스 중지 및 제거
fex-index.exe run         # 관리자: 콘솔에서 직접 실행 (디버깅용)
```

- 서비스 이름은 `FastExplorerIndex`이고 자동 시작입니다.
- 설치 위치는 `%ProgramFiles%\Fast Explorer`입니다. 사용자가 쓸 수 있는 폴더의 실행 파일을 승격 실행하면 관리자 권한이 넘어가므로, 앱은 Program Files 아래에 있을 때만 설치를 제안합니다. 개발 빌드는 위 명령으로 직접 설치하세요.
- 고정 NTFS 볼륨의 MFT에서 이름을 읽어 색인을 만들고, 이후 USN 변경 저널을 따라가며 갱신합니다. 저널을 따라갈 수 없으면(랩어라운드·재생성) 다시 만듭니다.
- 파일 이름만 색인하며 파일 내용은 읽지 않습니다.
- 검색 파이프는 `\\.\pipe\fast-explorer-index`이고, 설치한 사용자와 SYSTEM/Administrators만 접근할 수 있습니다. 클라이언트는 파이프 서버 프로세스가 실제 서비스인지 확인한 뒤 요청합니다.
- 인덱스 서비스가 없으면 범위가 정해진 검색은 직접 순회로 대체되고, 전체 검색은 설치를 안내합니다.

## 테스트

```powershell
cargo test --workspace
```

- `shell-host` 테스트는 실제 호스트 프로세스를 띄워 메뉴를 만들고, 바로 가기를 생성하고, 프로세스를 강제 종료한 뒤 재시작되는지 확인합니다. Explorer 셸이 있는 대화형 Windows 세션이 필요합니다.
- `fex-index`의 실제 서비스 테스트는 `#[ignore]` 처리되어 있습니다. 서비스를 설치한 뒤 다음으로 실행하세요.

```powershell
cargo test -p fex-index --test live -- --ignored
```

- `fex-core` 테스트는 `C:\Windows\System32`를 실제로 나열해 `std::fs`와 개수를 비교합니다. 해당 폴더를 읽을 수 있어야 합니다.

## 설정 파일

사용자 설정은 `%APPDATA%\Fast Explorer\`에 저장됩니다.

- `favorites.txt` — 즐겨찾기 폴더 목록 (한 줄에 하나)
- `view.txt` — 숨김 항목/확장자 표시 설정
- `update.txt` — 마지막 업데이트 확인 시각. `auto=0` 줄이 있으면 자동 확인을 끔

## 라이선스

[MIT](LICENSE). 기여 방법은 [CONTRIBUTING.md](CONTRIBUTING.md), 보안 제보는 [SECURITY.md](SECURITY.md)를 참고하세요.
