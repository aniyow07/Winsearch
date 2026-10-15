# WinSearch measurements

All numbers below were measured on one machine on 2026-10-15. They are
observations, not guarantees: results depend on disk, file count, cache
state and what else the machine is doing.

## Test environment

| | |
|---|---|
| CPU | Intel Core i7-14650HX (24 logical processors) |
| RAM | 15.6 GB |
| Disk | WD PC SN5000S 1 TB NVMe SSD, NTFS |
| GPU | Intel UHD + NVIDIA RTX 5050 Laptop (egui renders through OpenGL) |
| OS | Windows 11 Home Single Language 10.0.26300, x64 |
| Build | `cargo build --release` (Rust 1.93, LTO, opt-level 3) |
| Privileges | Standard user (not elevated) |
| Dataset | Whole `C:\` drive: **1,607,977 entries** (1.28 M files, 328 K folders), 624 folders unreadable (access denied) |

## Initial indexing

Measured with `wsctl index --db <new file> C:\` (same code path as the app).

| Phase | Result |
|---|---|
| Directory enumeration only (`wsctl walk C:\`, 8 threads) | 15.6 s (≈102,000 entries/s) |
| Full initial index, final build | **44.2 s (≈36,400 entries/s)** |
| `optimize` + WAL checkpoint afterwards | 3.1 s |
| Index file size | 796 MB |

What made the difference (same dataset):

| Writer variant | Time |
|---|---|
| Trigram index maintained row by row (triggers active) | 75 s |
| Triggers suspended during the first scan, one `rebuild` at the end | 33 s |
| + case-insensitive name index (for short queries) built during the scan | 85 s |
| + name index dropped during the bulk load and rebuilt at the end | 40.5 s |
| + modified-time index (newest-first browsing), final | 44.2 s |

Rescans of an already indexed root (reconciliation) keep the triggers on;
unchanged names do not touch the trigram index.

Space by structure (from `dbstat`, before the modified-time index): table
431 MB, path index 226 MB, trigram index 122 MB, parent index 176 MB in the
first schema. Replacing the text `parent` column with an integer
`parent_id` cut 220 MB.

## Search latency

`wsctl bench --db <index>`: best of 4 runs after one warm-up, 1,607,977 rows.
Each cell is `time / number of matches`.

| Strategy | `re` | `png` | `chrome` | `readme` | `config.json` | `system32` | `zzqxj` |
|---|---|---|---|---|---|---|---|
| `LIKE '%q%'` full scan (count) | 135 ms / 188,455 | 138 ms / 34,592 | 145 ms / 606 | 131 ms / 5,483 | 144 ms / 616 | 139 ms / 625 | 128 ms / 0 |
| `instr(lower(name))` full scan | 307 ms | 316 ms | 326 ms | 306 ms | 319 ms | 319 ms | 285 ms |
| **FTS5 trigram `MATCH` (count)** | n/a (< 3 chars) | **0.9 ms** | **0.2 ms** | **0.9 ms** | **1.9 ms** | **0.3 ms** | **0.0 ms** |
| FTS5 trigram used through `LIKE … ESCAPE` | 488 ms | 510 ms | 536 ms | 532 ms | 524 ms | 505 ms | 484 ms |
| In-memory name buffer + SIMD `memmem` (count) | 8.2 ms | 4.3 ms | 2.8 ms | 4.7 ms | 4.8 ms | 3.0 ms | 3.0 ms |
| **WinSearch engine: ranked top 500 + capped count** | **72 ms** | **31 ms** | **1.6 ms** | **9.2 ms** | **5.7 ms** | **1.8 ms** | **0.0 ms** |

Earlier, with `detail=none`, the trigram index was smaller (46 MB) but
SQLite refuses phrase queries on it, which substring matching needs.

**Decision.** Names of three or more characters go through the FTS5
trigram index (`detail=full`): 0.2–2 ms to find matches, and the engine's
end-to-end time is dominated by ranking very common terms (`png`: 34,592
matches ranked in 31 ms). One- and two-character queries cannot use
trigrams; they read name prefixes from a `NOCASE` B-tree index first and
fill the rest with a substring scan that stops at the result limit (72 ms
for `re`). The in-memory buffer was faster for short and very common
terms, but it costs ~49 MB of RAM permanently plus a 0.4 s load, and the
requirement was to query the database; the trigram index is already faster
for every query of three or more characters.

## Memory (whole app, `Get-Process` private bytes / working set)

Measured 8–12 s after start with the 1.6 M-entry index, release build.

| State | Private | Working set |
|---|---|---|
| Idle, no query | 131 MB | 123 MB |
| Query `e`, 5,000 results shown | 163 MB | 151 MB |
| `folder:` with the limit raised to 100,000 results | 176 MB | 166 MB |

The result list is virtualized: only visible rows are laid out, so 100,000
results cost about the memory of their strings (~13 MB more than 5,000).
A large part of the idle figure is the OpenGL driver and font atlases;
SQLite page caches are capped (32 MB writer during scans, then shrunk;
16 MB per reader).

## Preview latency

`wsctl preview <file>` (same code as the preview pane), first run and best
of two repeats.

| File | Result | First | Repeat |
|---|---|---|---|
| 12 MP JPEG (4000×3000) | decoded, scaled to 1024×768 | 38 ms | 39 ms |
| PNG 4000×2500 | decoded, scaled to 1024×640 | 41 ms | 41 ms |
| 3-page PDF | page rendered by Windows.Data.Pdf in the helper process, 790×1024 | 325 ms (897 ms first ever) | 323 ms |
| Word .docx | parsed to 8 blocks | 2.2 ms | 0.1 ms |
| Excel .xlsx | 2 sheets parsed | 0.4 ms | 0.2 ms |
| Text, 104 KB (`Cargo.lock`) | decoded | 0.3 ms | 0.2 ms |
| `explorer.exe` | metadata + Shell icon from the helper process | 221 ms | 139 ms |
| Folder (13 items) | listing | 0.2 ms | 0.1 ms |

Repeated selection of the same unchanged file is served from the in-app
preview cache (up to 96 MB / 64 entries) without any disk access.
PDF and Shell thumbnails pay for starting a helper process (~100–150 ms);
that isolation is deliberate (see ARCHITECTURE.md).

## How to reproduce

```powershell
cargo build --release
.\target\release\wsctl.exe walk C:\
.\target\release\wsctl.exe index --db $env:TEMP\bench.db C:\
.\target\release\wsctl.exe bench --db $env:TEMP\bench.db
powershell -ExecutionPolicy Bypass -File scripts\make-samples.ps1
.\target\release\wsctl.exe preview C:\WinSearch\samples\sample-3-pages.pdf C:\WinSearch\samples\sample-photo-12mp.jpg
```

`sample-photo-12mp.jpg` was a random-ellipse 4000×3000 JPEG (quality 90)
generated with System.Drawing for this measurement.
