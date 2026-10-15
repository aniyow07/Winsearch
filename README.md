# WinSearch

A fast, local filename search for Windows 10 and 11 with a built-in preview
pane. It keeps its own index of file and folder names in SQLite, so
searching never scans the disk: typical queries over 1.6 million entries
answer in 1–30 ms (see [docs/BENCHMARKS.md](docs/BENCHMARKS.md)).

Local-only: no cloud services, telemetry or accounts. Runs without
administrator rights.

## Features

* Instant substring search on names (case-insensitive, Unicode), with
  extension, file/folder and full-path filters.
* Search scope: everywhere, your Desktop/Documents/Downloads…, any indexed
  location or any folder you pick. Right-click a result for "Search in
  this folder" or "Hide this folder from results".
* "Hiding system & app data" (on by default) keeps AppData, Windows,
  ProgramData, dot-folders such as `.git`/`.vscode`, `node_modules` and
  similar out of results. They stay indexed; one click shows them again.
* Preview pane, rendered in the background for the selected file only:
  * images, zoomable (wheel, Ctrl +/−, drag to pan, double-click to fit)
    with a sharper rendering loaded when you zoom in;
  * PDF pages, zoomable, with page navigation (Windows' built-in PDF engine,
    run in an isolated helper process);
  * Word (.docx) and Excel (.xlsx) documents without Office installed,
    zoomable, with sheet tabs;
  * text and source code, folders, and file details plus the Windows
    thumbnail/icon for everything else.
* Live index updates while the app runs, background reconciliation for
  changes made while it was closed, resumable initial indexing, and a
  status bar that says whether the index is up to date.
* Dark, keyboard-first interface.

## Keyboard

| Key | Action |
|---|---|
| type | search immediately |
| ↑ / ↓, PageUp / PageDown, Ctrl+Home / Ctrl+End | move the selection |
| Enter | open with the default app |
| Ctrl+Enter or Ctrl+E | show in Explorer |
| Ctrl+C | copy the selected path (or the selected text in the search box) |
| Ctrl + / Ctrl − / Ctrl 0 | zoom the preview / reset |
| Ctrl+PageDown / Ctrl+PageUp | next / previous PDF page |
| F5 | refresh results |
| Ctrl+, | settings |
| Esc | clear the search; close the window when it is already empty |

## Search syntax

| Example | Meaning |
|---|---|
| `report 2024` | names containing both words |
| `"annual report"` | a term with a space |
| `*.pdf` or `ext:pdf,docx` | extension filter |
| `folder:` / `file:` | only folders / only files |
| `src\main` or `path:projects` | match anywhere in the full path |

## Requirements

* Windows 10 or 11, x64.
* To build: stable Rust (tested with 1.93) with the MSVC toolchain
  (`rustup default stable-x86_64-pc-windows-msvc`) and the Visual Studio
  Build Tools (C++ workload, for the linker). SQLite is compiled from the
  bundled sources; nothing else needs installing.

## Build, run, test, package

```powershell
cargo build --release
.\target\release\winsearch.exe
cargo test --release
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

The release executable is `target\release\winsearch.exe`; the packaged zip
is written to `dist\WinSearch-<version>-win-x64.zip` and is portable (no
installer, copy anywhere).

`winsearch.exe --query "text"` starts with a search filled in;
`--scope C:\path` starts inside a folder.

Sample files for trying the previews:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\make-samples.ps1
```

### Command-line companion

```powershell
.\target\release\wsctl.exe index  --db C:\temp\idx.db C:\Users   # build an index
.\target\release\wsctl.exe search --db C:\temp\idx.db report ext:pdf
.\target\release\wsctl.exe bench  --db C:\temp\idx.db            # compare search strategies
.\target\release\wsctl.exe walk   C:\                            # enumeration speed only
.\target\release\wsctl.exe preview C:\path\file.pdf               # preview latency
.\target\release\wsctl.exe check  --db C:\temp\idx.db            # integrity check
```

Without `--db`, `wsctl` uses the app's own index.

## First run

On first start WinSearch indexes every ready local fixed drive (skipping
`$Recycle.Bin` and `System Volume Information`). You can search while it
runs; the status bar shows progress, unreadable folders and a Cancel
button (a cancelled scan picks up where it stopped on the next start, or from Settings > Index status). Change locations,
exclusions and options in **Settings** (gear icon or Ctrl+,).

## Where data lives

| | |
|---|---|
| Index | `%LOCALAPPDATA%\WinSearch\index.db` (+ `-wal`/`-shm`) |
| Settings | `%LOCALAPPDATA%\WinSearch\config.json` |
| Logs | `%LOCALAPPDATA%\WinSearch\logs\winsearch.<date>.log` (7 days kept; set `WINSEARCH_LOG=debug` for more) |

Set `WINSEARCH_DATA_DIR` to use a different folder. Deleting the folder
resets everything; Settings > Index status > Rebuild does the same for
the index only.

## Administrator rights and file systems

Nothing requires elevation. Without it, live updates come from Windows
change notifications and a background reconciliation scan at startup
(and every 6 hours) catches anything missed. When WinSearch is run as
administrator, drive roots on NTFS/ReFS use the NTFS change journal
instead: changes made while the app was closed are replayed without a
rescan. The app never creates, resizes or deletes a change journal and
never changes file-system settings. Settings > Locations shows which mode
each location uses and why.

WinSearch never modifies, moves, renames or deletes your files; it only
reads directory listings and, for the selected file, its contents.

## Design and measurements

* [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): index schema, scanning,
  change tracking (USN vs notifications vs rescans), preview isolation,
  why Shell preview handlers are not hosted in-process.
* [docs/BENCHMARKS.md](docs/BENCHMARKS.md): indexing speed, search latency
  by strategy, memory, preview latency, with the test machine and dataset.

## Known limitations

* The NTFS change journal is only used when running elevated and only for
  whole drives; folder roots and non-admin sessions rely on notifications
  plus reconciliation scans (about 45 s of background disk reads for 1.6 M
  entries).
* The USN journal reader is covered by unit tests of its record parser and
  availability checks, but was not exercised end to end on the development
  machine (no elevation available there).
* One- and two-character searches use name prefixes plus a bounded
  substring scan, so their result count is approximate ("5,000+").
* Office previews show text, headings, lists and tables only (no images,
  styling or formulas; Excel shows stored values, so dates appear as serial
  numbers). Legacy .doc/.xls files fall back to the Windows thumbnail.
* PDF previews take ~0.3 s because each render starts an isolated helper
  process; password-protected PDFs are not previewed.
* File names that are not valid UTF-16 (unpaired surrogates) are stored in
  a lossy form.
* Network drives are scanned like local ones; change notifications on them
  depend on the server.
