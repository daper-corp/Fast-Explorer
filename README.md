# Fast Explorer (Fex)

English · [한국어](README.ko.md)

Fast Explorer is a Windows file explorer. It lists large folders quickly, offers tabs, history and shell integration, and searches file names on every drive instantly using the NTFS MFT/USN journal. Built with Rust and [Slint](https://slint.dev). The UI is currently in Korean only.

**[⬇ Download the latest installer](https://github.com/daper-corp/Fast-Explorer/releases/latest/download/FastExplorer-Setup.exe)** · [All releases](https://github.com/daper-corp/Fast-Explorer/releases)

The installer is not code-signed yet, so SmartScreen may show a warning.

## Features

### Fast folder listing
- Reads with `NtQueryDirectoryFile` and a 1 MB buffer, and hands entries to the UI in batches of 4096.
- Natural name sort like Explorer (`StrCmpLogicalW`), folders always first.
- Sort by size, date modified, type or name; drag column borders to resize, double-click a border to fit contents.
- Tabs, back/forward history, breadcrumb address bar, autocomplete, `%ENV%` expansion.
- Mouse back/forward buttons, Backspace to go back, type-ahead to jump to a name.

### Search
- **Current folder**: lists only that folder and selects matches.
- **Subfolders**: uses the index when the volume has one, otherwise walks the tree directly (`ignore` crate).
- **Everywhere**: instantly searches file names on all fixed NTFS drives through the index service.
- Results are grouped in a collapsible folder tree; the "Location" column shows the real folder.
- Pick the scope in the combo box next to the search box: current folder, subfolders, everywhere.
- A path inside the query becomes the search scope.

| Input | Meaning |
| --- | --- |
| `report` | Names containing `report` (case-insensitive) |
| `annual report` | Names containing both words (space = AND) |
| `*.txt`, `a?c` | Wildcard over the whole name |
| `re:^IMG_\d+\.jpg$`, `/^IMG_\d+\.jpg$/` | Regular expression (case-insensitive by default) |
| `C:\Work\*.txt`, `sub\report` | Search narrowed to that folder |

- At most 2000 results are shown; if there are more, the status bar says so ("more available, showing first N").
- The index service hides entries the client is not allowed to list (e.g. `System32\config`, `System Volume Information`).

### Shell integration
- Copy, move, delete and rename run through `IFileOperation`, giving Explorer's progress, conflict handling and Recycle Bin behavior. `Shift+Delete` deletes permanently.
- Uses the Explorer clipboard (CF_HDROP), so cut/copy/paste works with other programs.
- OLE drag & drop: drag selections to other apps, and drop files onto the list or onto folder rows.
- Shell-extension context menus are built and run in a separate process (`fex-shell-host`). If an extension hangs or crashes the main window survives, and the host is replaced after 10 seconds without a response. If the shell menu is not ready yet, "More options…" opens it later.
- Shell icons are loaded on a worker thread, including per-file icons for executables and shortcuts.
- Batch rename (`Shift+F2`): find & replace (regex, case, name/extension, `{n}` numbering) with preview, undo with `Ctrl+Z`. Comparable to PowerToys PowerRename.
- Find processes using a file (right-click): finds processes holding files inside the selection via Restart Manager and can end them. Comparable to PowerToys File Locksmith; it cannot find processes that merely use a folder as their working directory.
- New folder (`Ctrl+Shift+N`), new text document, properties (`Alt+Enter`), copy path (`Ctrl+Shift+C`).

### Window and views
- Custom frameless window (minimize/maximize/close, edge resize, DWM shadow) with app icon.
- Sidebar with favorites and drives; drives show a usage bar.
- "This PC" virtual view: drives and user folders with total/free space, folders grouped before drives. Open it by typing `This PC` (or the Korean `내 PC`) in the address bar.
- The current folder is watched with `ReadDirectoryChangesW`, so files created by other programs appear without refreshing; `WM_DEVICECHANGE` refreshes the drive list.
- Hidden-items and file-extension settings persist between sessions and follow Explorer's settings on first run.

### Shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl+T` / `Ctrl+W` | New tab / close tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Alt+←` / `Alt+→` / `Alt+↑` | Back / forward / up |
| `Backspace` | Back |
| `Ctrl+L`, `Alt+D`, `F4` | Focus address bar |
| `Ctrl+F`, `F3` | Focus search box |
| `F5`, `Ctrl+R` | Refresh |
| `Enter` | Open |
| `Alt+Enter` | Properties |
| `F2` | Rename |
| `Shift+F2` | Batch rename (selection) |
| `Delete` / `Shift+Delete` | Delete to Recycle Bin / delete permanently |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste |
| `Ctrl+Shift+C` | Copy path |
| `Ctrl+Shift+N` | New folder |
| `Ctrl+A`, `Ctrl+Space` | Select all, toggle selection |
| `Shift+F10`, Menu key | Context menu |
| `Esc` | End search, focus the list |

## Layout

A Cargo workspace with three executables and one shared library.

| Crate | Executable | Role |
| --- | --- | --- |
| `fex-core` | none | UI-independent engine: folder listing, sorting, formatting, search patterns/traversal, shell operations, icons, folder watching, index client, shell-menu client |
| `fex` (ui) | `fex.exe` | Slint UI, window/input handling, tab and search state, Win32 glue (drag & drop, DWM, icons) |
| `fex-shell-host` | `fex-shell-host.exe` | Host that isolates shell-extension context menus (stdin/stdout protocol) |
| `fex-index` | `fex-index.exe` | Windows service indexing the NTFS MFT / USN journal (name-search pipe) |

## Requirements

- Windows 10/11
- Rust stable (edition 2024, Rust 1.85+) and MSVC build tools
- Everywhere-search index: administrator rights and fixed NTFS drives

## Build

```powershell
cargo build --release
```

Output is in `target\release\`:

- `fex.exe` — the explorer
- `fex-shell-host.exe` — shell menu host
- `fex-index.exe` — index service (for installation)

Keep the three executables in the same folder when distributing. `fex.exe` looks for `fex-shell-host.exe` next to itself, and installing the index service uses the `fex-index.exe` in the same folder.

## Installer

```powershell
.\installer\build.ps1   # release build, then target\installer\FastExplorer-Setup-<version>.exe (requires Inno Setup 6)
```

- Installs to `%ProgramFiles%\Fast Explorer` and requires administrator rights (a security requirement of the index service).
- Options: desktop shortcut, index service (on by default), **use as default file explorer** (off by default).
- Using it as the default explorer makes opening folders and drives launch Fast Explorer. Win+E and the taskbar Explorer icon still open Explorer. Uninstalling restores the original behavior.
- Uninstalling also removes the index service.

## Releases and updates

- The workspace `Cargo.toml` `version` is the only version. Bump it, then push a tag of the same name (`v0.2.1`); `.github/workflows/release.yml` publishes the installer and `latest.json` as a GitHub release. The installer is attached both as `FastExplorer-Setup-<version>.exe` and as `FastExplorer-Setup.exe`, which keeps the "latest installer" link above stable.
- Once a day the app checks `releases/latest/download/latest.json` in the background and asks whether to install a new version. If you agree it downloads the installer, runs it and closes the app. It never installs without asking.
- Right-click an empty area or This PC → "Check for updates…" to check manually.
- Update checks require the repository to be public. Without code signing, SmartScreen warns on install.

## Run

```powershell
cargo run -p fex -- "C:\"
```

Without arguments it starts in `C:\`. To start in the This PC view, pass `"내 PC"` as the argument.

## Index service

Everywhere-search needs to read volumes with administrator rights, so a separate Windows service builds the index.

```powershell
fex-index.exe install     # admin: copy to Program Files, register and start the service
fex-index.exe uninstall   # admin: stop and remove the service
fex-index.exe run         # admin: run in the console (debugging)
```

- The service is named `FastExplorerIndex` and starts automatically.
- It installs to `%ProgramFiles%\Fast Explorer`. Elevating an executable from a user-writable folder would hand out administrator rights, so the app only offers installation when it runs from under Program Files. For development builds, install with the commands above.
- It reads names from the MFT of fixed NTFS volumes, then keeps the index current by following the USN change journal, rebuilding if the journal can't be followed (wraparound, recreation).
- Only file names are indexed; file contents are never read.
- The search pipe is `\\.\pipe\fast-explorer-index`, accessible only to the installing user and SYSTEM/Administrators. Clients verify that the pipe server process is the real service before sending requests.
- Without the index service, scoped searches fall back to direct traversal, and everywhere-search prompts you to install it.

## Tests

```powershell
cargo test --workspace
```

- The `shell-host` tests start a real host process, build menus, create a shortcut, kill the process and check that it restarts. They need an interactive Windows session with the Explorer shell.
- The real-service tests of `fex-index` are `#[ignore]`d. Install the service, then run:

```powershell
cargo test -p fex-index --test live -- --ignored
```

- The `fex-core` tests list `C:\Windows\System32` and compare the count with `std::fs`, so that folder must be readable.

## Settings files

User settings are stored in `%APPDATA%\Fast Explorer\`:

- `favorites.txt` — favorite folders, one per line
- `view.txt` — hidden-items / file-extension settings
- `update.txt` — time of the last update check; an `auto=0` line turns automatic checks off

## Contributing and license

Issues and PRs are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities privately via [SECURITY.md](SECURITY.md).

[MIT](LICENSE)
