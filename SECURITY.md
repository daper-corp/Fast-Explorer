# Security Policy

Fast Explorer ships a Windows service (`fex-index`) that runs as LocalSystem, so security reports matter.

**Do not open a public issue for vulnerabilities.** Use
[private vulnerability reporting](https://github.com/daper-corp/Fast-Explorer/security/advisories/new).
보안 취약점은 공개 이슈 대신 위 링크로 비공개 제보해 주세요.

Of particular interest: the index pipe ACL / server-PID check, the privilege-elevation flow, the update download path, and shell-extension isolation (see "보안 불변 조건" in [AGENTS.md](AGENTS.md)).

Only the latest release is supported.
