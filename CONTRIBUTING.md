# Contributing / 기여 방법

Issues and PRs in Korean or English are both welcome. 한국어·영어 모두 환영합니다.

## Issues
- 버그/제안은 이슈 템플릿을 사용하세요. 큰 변경은 PR 전에 이슈로 먼저 논의해 주세요.
- `good first issue` 라벨이 입문용입니다.
- 보안 문제는 [SECURITY.md](SECURITY.md)를 따라 비공개로 제보하세요.

## 개발 환경
Windows 10/11, Rust stable(1.85+), MSVC 빌드 도구.

```powershell
cargo build --workspace
cargo run -p fex -- "C:\"
cargo clippy --workspace --all-targets
cargo test -p fex-core
cargo test -p fex --lib
```

`cargo test --workspace`의 `shell-host` 테스트는 대화형 Windows 세션이 필요합니다. 자세한 내용은 [README.md](README.md).

## PR 규칙
1. `main`에서 브랜치를 만들어 작은 단위로 PR을 올립니다. 하나의 PR은 하나의 주제만 다룹니다.
2. CI(fmt, clippy, 테스트)가 통과해야 합니다.
3. 기존 한 줄 스타일을 따르세요(자동 포맷터로 파일 전체를 다시 쓰지 마세요). UI 변경은 스크린샷이나 GIF를 붙여 주세요.
4. 코드 구조·아키텍처 규칙·보안 불변 조건은 [AGENTS.md](AGENTS.md)를 읽어 주세요. 특히 **UI 스레드를 막지 말 것**, **인덱스 파이프 보안 경계를 약화하지 말 것**.
5. UI 문자열은 한국어, 코드 주석은 영어입니다.
6. 기여한 코드는 프로젝트와 같은 [MIT 라이선스](LICENSE)로 배포됩니다.
