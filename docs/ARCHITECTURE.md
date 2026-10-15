# WinSearch architecture and design decisions

## Overview

```
 ┌────────────── winsearch.exe (GUI, egui/eframe) ───────────────┐
 │ UI thread: draws, polls channels, never touches disk or SQL   │
 │   ├─ search worker thread ── read-only SQLite connection      │
 │   ├─ preview workers (2) ─── bounded reads, image/docx/xlsx   │
 │   │      └─ spawns helper: winsearch.exe --render-preview …   │
 │   │         (PDF pages via Windows.Data.Pdf, Shell thumbnails)│
 │   └─ indexer thread ─────── the only writable connection      │
 │          ├─ scanner threads (DirWalkBackend)                  │
 │          └─ change sources: ReadDirectoryChangesW watcher per │
 │             root, or NTFS USN journal reader when elevated    │
 └───────────────────────────────────────────────────────────────┘
          %LOCALAPPDATA%\WinSearch\index.db  (SQLite, WAL mode)
```

The engine is a library (`src/lib.rs`); the GUI (`src/ui`) and the CLI
(`src/bin/wsctl.rs`) are thin front-ends over it.

## Index (src/db.rs)

* `files(id, root_id, path UNIQUE NOCASE, name NOCASE, parent_id, ext,
  is_dir, size, created, modified, attrs, file_ref, seen_gen, listed_gen)`.
  `parent_id` points at the parent directory's row, so renaming a folder
  rewrites descendant paths but keeps every row id.
* `names_fts`: FTS5 `trigram` external-content index on `name`, kept in
  sync by triggers; substring search in 0.2–2 ms on 1.6 M names.
* `roots`: scan generation and state per indexed location, volume serial,
  USN journal id and position.
* Schema version in `PRAGMA user_version`; unknown or newer versions and
  corrupt files are moved aside (`index.corrupt-<time>`) and rebuilt.
* All SQL uses bound parameters; user text never becomes SQL.
* A flag in `meta` records when the trigram index is out of date (bulk
  load or crash during one). Search then falls back to `LIKE` and says so;
  the index is rebuilt on the next start.

## Scanning (src/scanner.rs, src/indexer/scan.rs)

`ScanBackend` is a trait; the shipped backend is a parallel
FindFirstFile/FindNextFile walker (`std::fs::read_dir`). On Windows the
metadata of each entry comes from the directory listing itself, so no
file is opened. Symlinks, junctions and mount points are recorded but
never descended into, which rules out loops. Excluded folders are pruned.

Each scan has a generation number. Entries seen get the new generation;
when a scan completes, older rows under that root are deleted. A
cancelled or crashed scan keeps its generation, and the next scan resumes:
directories fully listed in that generation (`listed_gen`) are not read
again, only descended into. Errors (access denied, vanished folders) are
counted, sampled, logged and shown; the root is marked complete-with-errors,
never silently complete.

Writes are batched (20,000 rows or 500 ms per transaction). For the first
scan of an empty root, trigram maintenance and the name index are
suspended and rebuilt once at the end (75 s → 44 s on 1.6 M entries).

## Keeping the index current (src/indexer)

Three strategies were compared:

| | Reliability | Needs | Cost |
|---|---|---|---|
| **NTFS USN change journal** (`usn.rs`) | Best: ordered, survives restarts, gaps are detectable (journal id change, position below `FirstUsn`, `ERROR_JOURNAL_ENTRY_DELETED`) | Administrator (opening `\\.\C:`), NTFS/ReFS, journal enabled | Low |
| **ReadDirectoryChangesW** (`watcher.rs`) | Good while running; can overflow (reported as `ERROR_NOTIFY_ENUM_DIR` / 0 bytes); sees nothing while the app is closed | Nothing special | Low |
| **Reconciliation scan** | Complete by construction | Nothing | ~45 s of disk reads for 1.6 M entries |

What the app does:

* When elevated and the root is a drive root with an active journal, it
  reads the USN journal from the saved position. If that position is no
  longer valid it runs one reconciliation scan and continues from the
  current journal position. It never creates or resizes a journal.
* Otherwise (the normal, non-admin case) it watches each root with
  ReadDirectoryChangesW and runs a background reconciliation scan at
  startup (changes made while closed) and every 6 hours (configurable).
  Any overflow, full event queue, or watcher failure triggers a
  reconciliation scan of that root. Unavailable drives are retried every
  30 s; a different volume serial behind the same drive letter forces a
  fresh scan.
* Every change is a hint: the indexer re-stats the path and makes the
  index match the disk (`changes.rs`). That makes duplicate, reordered or
  stale notifications harmless. Folder renames move the subtree in one
  `UPDATE`; folders that appear (created or moved in) are enumerated.
* The app's own data folder is always excluded to avoid feedback loops.

The UI reports which mode each root uses and why the journal is not used
(e.g. "needs administrator rights").

## Search (src/search.rs)

Query syntax: space-separated name terms (AND), `"quoted phrases"`,
`ext:pdf,docx` / `*.pdf`, `folder:` / `file:`, `path:x` or any term with a
backslash for full-path matches. Ranking: exact name, then prefix, then
files under the user profile, then earliest match position and shorter
names. Results are limited (5,000 by default, configurable) with a capped
count; the list is virtualized. Scope (search inside one folder) and the
"hide system & app data" filter (AppData, Windows, ProgramData, dot-folders,
node_modules…) are SQL conditions; hidden folders stay indexed.

## Previews (src/preview)

* Only the selected file is previewed, on two worker threads; each request
  supersedes the previous one, and stale work is dropped (helper processes
  are killed when the selection changes).
* Limits: images ≤ 128 MB and ≤ 30,000 px per side with a 768 MB decoder
  allocation cap, scaled to 1,024 px (4,096 px when the user zooms in);
  text reads at most 256 KB; Office XML parts ≤ 64 MB inflated (ZIP-bomb
  guard), at most 4,000 document blocks and 500×60 cells per sheet.
* File contents are untrusted: control characters in text are neutralized,
  XML is parsed without entity expansion beyond the five predefined ones,
  nothing is executed, and online-only cloud files are not downloaded.
* Recently used previews are cached (96 MB / 64 entries, keyed by path,
  size, modified time and render options).

### Isolation of native renderers

PDF pages (Windows.Data.Pdf) and Shell thumbnails (`IShellItemImageFactory`,
which loads whatever thumbnail provider is registered for the file type,
often third-party code) run in a short-lived child process:
`winsearch.exe --render-preview <pdf|thumb> <px> <path> [page]`. The child
returns raw RGBA through a pipe with a checked header; the parent enforces a
15 s timeout and a 4096×4096 pixel ceiling and kills the child on timeout or
when the selection changes. A crash or hang there cannot take down the
search window.

### Shell preview handlers (IPreviewHandler) — investigated, not integrated

Preview handlers render into a child HWND that the host provides, are
activated in-process or in the `prevhost.exe` surrogate depending on
registration, and need the host to forward focus, sizing, accelerators and
`Unload`/`SetRect` calls on its UI thread. Hosting one reliably would mean
embedding a native child window inside the egui/OpenGL surface and pumping
its messages from the UI thread, so a misbehaving handler (many are
third-party) could freeze or crash the main window, which the requirements
rule out. The out-of-process thumbnail path above covers the common case
(a visual of Office, video and design files) with crash isolation; Word
and Excel get dedicated, safe in-process readers instead.
