# Fast Explorer (Fex)

[한국어](README.md) · English

A fast, Windows-only file explorer written in Rust with [Slint](https://slint.dev). The UI is currently in Korean.

- **Fast listing**: `NtQueryDirectoryFile` with large buffers, batched to the UI; natural sort like Explorer.
- **Instant search**: NTFS MFT/USN-journal index across all fixed drives, plus plain/glob/regex queries (`*.txt`, `re:^IMG_\d+`).
- **Tabs, history, breadcrumbs**, type-ahead, drag & drop, folder tree view.
- **Shell integration**: Explorer-style copy/move/delete (`IFileOperation`), context menus with shell extensions isolated in a separate process.
- **Tools**: batch rename (like PowerRename), "which process locks this file" (like File Locksmith).

## Install
Download the installer from [Releases](https://github.com/daper-corp/Fast-Explorer/releases). The installer is not code-signed yet, so SmartScreen may warn.

## Build
Requires Windows 10/11, Rust stable (1.85+) and MSVC build tools.

```powershell
cargo build --release
cargo run -p fex -- "C:\"
```

See the [Korean README](README.md) for the full feature list, shortcuts, and the index service.

## Contributing
Issues and PRs are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities privately via [SECURITY.md](SECURITY.md).

## License
[MIT](LICENSE)
