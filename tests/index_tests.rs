//! Integration tests for scanning, incremental changes, search and recovery.
//! They run against real temporary directories on the local NTFS volume.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use winsearch::db::Db;
use winsearch::indexer::changes::{self, Change, Scope};
use winsearch::indexer::scan::scan_root;
use winsearch::scanner::{
    DirWalkBackend, ResumeState, ScanBackend, ScanConfig, ScanMsg, ScanOutcome, ScanProgress,
};
use winsearch::search::{KindFilter, Query, SearchEngine};
use winsearch::util;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: String,
    db_path: PathBuf,
    db: Db,
}

fn norm(p: &Path) -> String {
    // Resolve 8.3 short names (C:\Users\RUNNER~1) so paths match what the
    // scanner produces from directory listings.
    let canon = std::fs::canonicalize(p).unwrap();
    let s = canon.to_string_lossy().to_string();
    util::normalize_dir(s.strip_prefix(r"\\?\").unwrap_or(&s))
}

fn write(p: &Path, content: &str) {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

/// Builds a small tree and indexes it.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("tree");
    write(&base.join("Report 2024.pdf"), "%PDF");
    write(&base.join("docs").join("readme.md"), "# hi");
    write(&base.join("docs").join("Report-final.docx"), "x");
    write(&base.join("src").join("main.rs"), "fn main() {}");
    write(&base.join("src").join("lib").join("main.rs"), "// duplicate name");
    write(&base.join("photos").join("Ärger am Strand.jpg"), "jpg");
    write(&base.join("skip").join("secret.txt"), "excluded");
    let root = norm(&base);
    let db_path = tmp.path().join("index.db");
    let (mut db, _) = Db::open(&db_path).unwrap();
    let cfg = ScanConfig { excludes: vec![format!("{root}\\skip")], threads: 4 };
    let rep = scan_root(
        &mut db,
        &DirWalkBackend,
        &root,
        &cfg,
        true,
        &AtomicBool::new(false),
        &ScanProgress::default(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(rep.outcome, ScanOutcome::Completed);
    Fixture { _tmp: tmp, root, db_path, db }
}

fn search(f: &Fixture, text: &str) -> Vec<String> {
    let engine = SearchEngine::new(Db::open_reader(&f.db_path).unwrap());
    engine.search(&Query::parse(text), 1000).unwrap().hits.into_iter().map(|h| h.path).collect()
}

fn rescan(f: &mut Fixture) -> winsearch::indexer::scan::ScanReport {
    let cfg = ScanConfig { excludes: vec![format!("{}\\skip", f.root)], threads: 4 };
    scan_root(
        &mut f.db,
        &DirWalkBackend,
        &f.root,
        &cfg,
        true,
        &AtomicBool::new(false),
        &ScanProgress::default(),
        |_, _| {},
    )
    .unwrap()
}

fn apply(f: &mut Fixture, ch: Vec<Change>) {
    let roots = vec![(f.root.clone(), f.db.root(&f.root).unwrap().unwrap().id)];
    let excludes = vec![format!("{}\\skip", f.root)];
    let scope = Scope { roots: &roots, excludes: &excludes };
    changes::apply(&mut f.db, &scope, &ch).unwrap();
}

#[test]
fn indexes_tree_with_metadata_and_honours_excludes() {
    let f = fixture();
    // 6 files + 4 dirs (docs, src, src\lib, photos); "skip" is excluded entirely.
    assert_eq!(f.db.count().unwrap(), 10);
    let rec = f.db.get(&format!("{}\\src\\main.rs", f.root)).unwrap().unwrap();
    assert_eq!(rec.name, "main.rs");
    assert_eq!(rec.ext, "rs");
    assert_eq!(rec.size, 12);
    assert!(rec.modified.is_some() && rec.created.is_some());
    assert!(search(&f, "secret").is_empty());
    assert!(search(&f, "skip").is_empty());
}

#[test]
fn substring_case_insensitive_and_ranked() {
    let f = fixture();
    let hits = search(&f, "REPORT");
    assert_eq!(hits.len(), 2);
    // Name starting with the term ranks first.
    assert!(hits[0].ends_with("Report 2024.pdf") || hits[0].ends_with("Report-final.docx"));
    assert_eq!(search(&f, "port 20").len(), 1, "quoted-free multiword terms are ANDed");
    assert_eq!(search(&f, "\"report 2024\"").len(), 1);
    // Non-ASCII case folding through the trigram index.
    assert_eq!(search(&f, "ärger").len(), 1);
    assert_eq!(search(&f, "ÄRGER").len(), 1);
}

#[test]
fn filters_extensions_kinds_and_paths() {
    let f = fixture();
    assert_eq!(search(&f, "ext:pdf").len(), 1);
    assert_eq!(search(&f, "*.docx").len(), 1);
    assert_eq!(search(&f, "report ext:docx").len(), 1);
    let folders = search(&f, "folder: o");
    assert!(folders.iter().all(|p| std::path::Path::new(p).is_dir()), "{folders:?}");
    assert_eq!(search(&f, "main src\\lib").len(), 1);
    assert_eq!(search(&f, "main path:lib").len(), 1);
}

#[test]
fn duplicate_names_in_different_folders_are_both_found() {
    let f = fixture();
    let hits = search(&f, "main.rs");
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|p| p.ends_with("src\\main.rs")));
    assert!(hits.iter().any(|p| p.ends_with("lib\\main.rs")));
}

#[test]
fn short_queries_use_prefix_then_substring() {
    let f = fixture();
    let hits = search(&f, "ma");
    assert_eq!(hits.len(), 2, "{hits:?}");
    let hits = search(&f, "d");
    assert!(hits.iter().any(|p| p.ends_with("docs")));
    assert!(hits.iter().any(|p| p.ends_with("readme.md")));
}

#[test]
fn rescan_reconciles_deletions_and_additions() {
    let mut f = fixture();
    let base = PathBuf::from(&f.root);
    std::fs::remove_file(base.join("docs").join("readme.md")).unwrap();
    std::fs::remove_dir_all(base.join("src").join("lib")).unwrap();
    write(&base.join("new folder").join("added.txt"), "new");
    let rep = rescan(&mut f);
    assert_eq!(rep.outcome, ScanOutcome::Completed);
    assert!(rep.removed >= 3, "readme, lib and lib\\main.rs: {}", rep.removed);
    assert!(search(&f, "readme").is_empty());
    assert_eq!(search(&f, "main.rs").len(), 1);
    assert_eq!(search(&f, "added").len(), 1);
}

#[test]
fn incremental_file_rename_and_delete() {
    let mut f = fixture();
    let from = format!("{}\\docs\\readme.md", f.root);
    let to = format!("{}\\docs\\guide.md", f.root);
    std::fs::rename(&from, &to).unwrap();
    apply(&mut f, vec![Change::Rename { from: from.clone(), to: to.clone() }]);
    assert!(search(&f, "readme").is_empty());
    assert_eq!(search(&f, "guide"), vec![to.clone()]);

    std::fs::remove_file(&to).unwrap();
    apply(&mut f, vec![Change::Remove(to)]);
    assert!(search(&f, "guide").is_empty());
}

#[test]
fn in_app_rename_updates_the_index() {
    let mut f = fixture();
    let from = format!("{}\\docs\\readme.md", f.root);
    let to = winsearch::fileops::rename(&from, "notes.md").unwrap();
    apply(&mut f, vec![Change::Rename { from, to: to.clone() }]);
    assert!(search(&f, "readme").is_empty());
    assert_eq!(search(&f, "notes"), vec![to]);
}

#[test]
fn case_only_directory_rename_keeps_descendants() {
    let mut f = fixture();
    let from = format!("{}\\src", f.root);
    let to = winsearch::fileops::rename(&from, "SRC").unwrap();
    assert_eq!(to, format!("{}\\SRC", f.root));
    apply(&mut f, vec![Change::Rename { from, to: to.clone() }]);
    let rec = f.db.get(&to).unwrap().unwrap();
    assert_eq!(rec.name, "SRC");
    let hits = search(&f, "main.rs");
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert!(hits.iter().all(|p| p.starts_with(&to)), "{hits:?}");
    assert_eq!(f.db.children(&to).unwrap().len(), 2, "main.rs and lib");
}

#[test]
fn incremental_directory_rename_moves_descendants() {
    let mut f = fixture();
    let from = format!("{}\\src", f.root);
    let to = format!("{}\\code", f.root);
    let old_id = f.db.conn.query_row("SELECT id FROM files WHERE path=?1", [&from], |r| r.get::<_, i64>(0)).unwrap();
    std::fs::rename(&from, &to).unwrap();
    apply(&mut f, vec![Change::Rename { from: from.clone(), to: to.clone() }]);
    let hits = search(&f, "main.rs");
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|p| p.starts_with(&to)), "{hits:?}");
    assert!(f.db.get(&from).unwrap().is_none());
    // The directory keeps its row id, so children still point at it.
    let new_id = f.db.conn.query_row("SELECT id FROM files WHERE path=?1", [&to], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(old_id, new_id);
    let kids = f.db.children(&to).unwrap();
    assert_eq!(kids.len(), 2, "main.rs and lib");
    // A full rescan agrees with the incremental result.
    let before = f.db.count().unwrap();
    let rep = rescan(&mut f);
    assert_eq!(rep.removed, 0);
    assert_eq!(f.db.count().unwrap(), before);
}

#[test]
fn incremental_new_directory_is_indexed_recursively() {
    let mut f = fixture();
    let dir = PathBuf::from(&f.root).join("moved in");
    write(&dir.join("deep").join("nested.txt"), "x");
    let dir_s = norm(&dir);
    apply(&mut f, vec![Change::Upsert(dir_s.clone())]);
    assert_eq!(search(&f, "nested").len(), 1);
    // Removing the directory removes its whole subtree.
    std::fs::remove_dir_all(&dir).unwrap();
    apply(&mut f, vec![Change::Remove(dir_s)]);
    assert!(search(&f, "nested").is_empty());
    assert!(search(&f, "deep").is_empty());
}

#[test]
fn notifications_for_vanished_paths_are_harmless() {
    let mut f = fixture();
    let ghost = format!("{}\\does-not-exist.txt", f.root);
    let root = f.root.clone();
    apply(&mut f, vec![Change::Upsert(ghost.clone()), Change::Remove(ghost), Change::RescanDir(root)]);
    assert_eq!(f.db.count().unwrap(), 10);
}

#[test]
fn junctions_are_indexed_but_not_followed() {
    let f = fixture();
    let link = PathBuf::from(&f.root).join("loop");
    // A junction back to the root would loop forever if followed.
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&f.root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("mklink /J unavailable; skipping");
        return;
    }
    let mut f = f;
    let rep = rescan(&mut f);
    assert_eq!(rep.outcome, ScanOutcome::Completed);
    assert_eq!(search(&f, "loop").len(), 1);
    assert_eq!(search(&f, "main.rs").len(), 2, "nothing indexed through the junction");
    std::fs::remove_dir(&link).unwrap();
}

#[test]
fn unreadable_folders_are_reported_not_fatal() {
    let mut f = fixture();
    let locked = PathBuf::from(&f.root).join("locked");
    write(&locked.join("hidden.txt"), "x");
    let user = std::env::var("USERNAME").unwrap();
    let deny = std::process::Command::new("icacls")
        .arg(&locked)
        .args(["/deny", &format!("{user}:(RD)")])
        .output()
        .unwrap();
    assert!(deny.status.success());
    let mut errors = Vec::new();
    let cfg = ScanConfig { excludes: vec![], threads: 2 };
    let rep = scan_root(
        &mut f.db,
        &DirWalkBackend,
        &f.root,
        &cfg,
        true,
        &AtomicBool::new(false),
        &ScanProgress::default(),
        |p, m| errors.push(format!("{p}: {m}")),
    )
    .unwrap();
    let _ = std::process::Command::new("icacls").arg(&locked).args(["/remove:d", &user]).output();
    assert_eq!(rep.outcome, ScanOutcome::Completed);
    assert!(rep.errors >= 1, "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("locked")), "{errors:?}");
    assert_eq!(search(&f, "locked").len(), 1, "the folder itself is listed by its parent");
    assert!(search(&f, "hidden.txt").is_empty());
    let root = f.db.root(&f.root).unwrap().unwrap();
    assert_eq!(root.state, "complete");
    assert!(root.error_count >= 1);
}

#[test]
fn missing_root_is_reported_and_keeps_entries() {
    let mut f = fixture();
    let ghost = format!("{}\\nope", f.root);
    let rep = scan_root(
        &mut f.db,
        &DirWalkBackend,
        &ghost,
        &ScanConfig::default(),
        true,
        &AtomicBool::new(false),
        &ScanProgress::default(),
        |_, _| {},
    )
    .unwrap();
    assert!(matches!(rep.outcome, ScanOutcome::RootUnavailable(_)));
    assert_eq!(f.db.root(&ghost).unwrap().unwrap().state, "unavailable");
    assert_eq!(f.db.count().unwrap(), 10);
}

/// Lists only the root and its direct subfolders, then reports
/// cancellation, like a user pressing Cancel early in a scan.
struct PartialBackend;

impl ScanBackend for PartialBackend {
    fn name(&self) -> &'static str {
        "partial"
    }
    fn scan(
        &self,
        root: &str,
        cfg: &ScanConfig,
        _resume: &ResumeState,
        tx: &crossbeam_channel::Sender<ScanMsg>,
        cancel: &AtomicBool,
        progress: &ScanProgress,
    ) -> ScanOutcome {
        for dir in std::iter::once(root.to_string()).chain(
            std::fs::read_dir(root)
                .unwrap()
                .flatten()
                .filter(|e| e.file_type().unwrap().is_dir())
                .map(|e| util::join(root, &e.file_name().to_string_lossy()))
                .filter(|d| !cfg.excludes.iter().any(|x| util::is_under(d, x))),
        ) {
            let mut records = Vec::new();
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let md = e.metadata().unwrap();
                records.push(winsearch::model::FileRecord::from_metadata(&dir, &e.file_name().to_string_lossy(), &md));
                progress.files.fetch_add(1, Ordering::Relaxed);
            }
            tx.send(ScanMsg::Entries { dir, records, complete: true }).unwrap();
        }
        cancel.store(true, Ordering::Relaxed);
        ScanOutcome::Cancelled
    }
}

#[test]
fn cancelled_scan_resumes_without_relisting_finished_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("r");
    for i in 0..5 {
        write(&base.join(format!("d{i}")).join("sub").join(format!("f{i}.txt")), "x");
    }
    let root = norm(&base);
    let db_path = tmp.path().join("i.db");
    let (mut db, _) = Db::open(&db_path).unwrap();
    let cancel = AtomicBool::new(false);
    let rep = scan_root(&mut db, &PartialBackend, &root, &ScanConfig::default(), true, &cancel, &ScanProgress::default(), |_, _| {}).unwrap();
    assert_eq!(rep.outcome, ScanOutcome::Cancelled);
    assert_eq!(db.root(&root).unwrap().unwrap().state, "scanning", "not marked complete");

    let progress = ScanProgress::default();
    let rep = scan_root(&mut db, &DirWalkBackend, &root, &ScanConfig::default(), false, &AtomicBool::new(false), &progress, |_, _| {}).unwrap();
    assert!(rep.resumed);
    assert_eq!(rep.outcome, ScanOutcome::Completed);
    assert_eq!(progress.skipped_dirs.load(Ordering::Relaxed), 5, "d0..d4 were already listed");
    // 5 d* + 5 sub + 5 files
    assert_eq!(db.count().unwrap(), 15);
    assert_eq!(db.root(&root).unwrap().unwrap().state, "complete");
}

#[test]
fn search_works_while_name_index_is_rebuilt() {
    let f = fixture();
    f.db.suspend_fts().unwrap();
    let engine = SearchEngine::new(Db::open_reader(&f.db_path).unwrap());
    let r = engine.search(&Query::parse("report"), 100).unwrap();
    assert!(r.degraded);
    assert_eq!(r.hits.len(), 2);
    f.db.rebuild_fts().unwrap();
    let r = engine.search(&Query::parse("report"), 100).unwrap();
    assert!(!r.degraded);
    assert_eq!(r.hits.len(), 2);
}

#[test]
fn corrupt_database_is_moved_aside_and_recreated() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("index.db");
    std::fs::write(&p, vec![0x42u8; 8192]).unwrap();
    let (db, report) = Db::open(&p).unwrap();
    let backup = report.recovered_from.expect("recovered");
    assert!(backup.exists());
    assert_eq!(db.count().unwrap(), 0);
    assert!(db.quick_check().unwrap().is_none());
}

#[test]
fn scope_and_hidden_folder_filters() {
    let f = fixture();
    let engine = SearchEngine::new(Db::open_reader(&f.db_path).unwrap());
    let mut q = Query::parse("main");
    q.scope = Some(format!("{}\\src\\lib", f.root));
    assert_eq!(engine.search(&q, 100).unwrap().hits.len(), 1);
    let mut q = Query::parse("main");
    q.exclude_folders = vec!["lib".into()];
    assert_eq!(engine.search(&q, 100).unwrap().hits.len(), 1);
    let mut q = Query::parse("main");
    q.exclude_folders = vec![format!("{}\\src", f.root)];
    assert!(engine.search(&q, 100).unwrap().hits.is_empty());
    let mut q = Query::parse("o");
    q.kind = KindFilter::FoldersOnly;
    q.exclude_folders = vec![".*".into()];
    assert!(engine.search(&q, 100).unwrap().hits.iter().all(|h| !h.path.contains("\\.")));
}

#[test]
fn directory_watcher_reports_create_rename_delete() {
    use winsearch::indexer::watcher::{DirWatcher, WatchEvent};
    let tmp = tempfile::tempdir().unwrap();
    let root = norm(tmp.path());
    let (tx, rx) = crossbeam_channel::unbounded();
    let _w = DirWatcher::start(&root, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let a = format!("{root}\\a.txt");
    let b = format!("{root}\\b.txt");
    std::fs::write(&a, "x").unwrap();
    std::fs::rename(&a, &b).unwrap();
    std::fs::remove_file(&b).unwrap();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(WatchEvent::Changes(c)) = rx.recv_timeout(Duration::from_millis(200)) {
            seen.extend(c);
        }
        if seen.iter().any(|c| matches!(c, Change::Remove(p) if *p == b)) {
            break;
        }
    }
    assert!(seen.contains(&Change::Upsert(a.clone())), "{seen:?}");
    assert!(seen.contains(&Change::Rename { from: a, to: b.clone() }), "{seen:?}");
    assert!(seen.contains(&Change::Remove(b)), "{seen:?}");
}

#[test]
fn indexer_end_to_end_with_live_updates() {
    use winsearch::config::AppConfig;
    use winsearch::indexer::{IndexerHandle, Phase};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("live");
    write(&base.join("first.txt"), "x");
    let root = norm(&base);
    let db_path = tmp.path().join("index.db");
    let cfg = AppConfig { roots: vec![root.clone()], reconcile_on_startup: false, ..AppConfig::default() };
    let ix = IndexerHandle::start(db_path.clone(), cfg);
    let wait = |cond: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    };
    assert!(wait(&|| {
        let s = ix.status.snapshot();
        s.phase == Phase::Idle && s.roots.iter().all(|r| r.state == "complete") && !s.roots.is_empty()
    }));
    let engine = SearchEngine::new(Db::open_reader(&db_path).unwrap());
    assert_eq!(engine.search(&Query::parse("first"), 10).unwrap().hits.len(), 1);
    write(&base.join("second file.txt"), "x");
    assert!(
        wait(&|| engine.search(&Query::parse("second"), 10).map(|r| r.hits.len() == 1).unwrap_or(false)),
        "live update did not arrive"
    );
    std::fs::remove_file(base.join("first.txt")).unwrap();
    assert!(wait(&|| engine.search(&Query::parse("first"), 10).map(|r| r.hits.is_empty()).unwrap_or(false)));
    let s = ix.status.snapshot();
    assert!(s.up_to_date(), "{s:?}");
}
