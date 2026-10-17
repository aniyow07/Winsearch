//! Index coordinator: owns the writable database on a background thread,
//! runs scans, keeps the index current from change sources and publishes
//! status for the UI. The UI talks to it only through [`IndexerHandle`].

pub mod changes;
pub mod scan;
pub mod usn;
pub mod watcher;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use parking_lot::Mutex;

use crate::config::AppConfig;
use crate::db::Db;
use crate::scanner::{DirWalkBackend, ScanBackend, ScanConfig, ScanOutcome, ScanProgress};
use crate::util;
use changes::{Change, Scope};
use watcher::{DirWatcher, WatchEvent};

pub enum Command {
    /// New settings: roots/excludes may have changed.
    Reconfigure(AppConfig),
    /// Scan every root (resuming interrupted scans unless `force_new`).
    ScanAll { force_new: bool },
    /// Delete the index and scan everything again.
    Rebuild,
    /// Run an integrity check of the database.
    Verify,
    /// An event from a change source.
    Watch { root: String, event: WatchEvent },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Starting,
    Idle,
    Scanning,
    Applying,
    Rebuilding,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveMode {
    /// NTFS change journal: survives restarts, no lost events.
    Usn,
    /// ReadDirectoryChangesW plus reconciliation scans.
    Notifications,
    /// No live updates for this root right now (reason).
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub struct RootStatus {
    pub path: String,
    /// Database scan state: new, scanning (interrupted/in progress),
    /// complete, unavailable.
    pub state: String,
    pub live: LiveMode,
    pub last_complete_scan: Option<i64>,
    pub errors: i64,
    pub last_error: Option<String>,
    /// Why the USN journal is not used, if it is not.
    pub usn_note: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StatusSnapshot {
    pub phase: Phase,
    pub current_root: Option<String>,
    pub roots: Vec<RootStatus>,
    pub total_entries: u64,
    /// Most recent scan/apply errors, newest last.
    pub recent_errors: VecDeque<String>,
    pub message: String,
    /// Persistent notices (database recovered, journal unavailable...).
    pub notices: Vec<String>,
    pub changes_applied: u64,
    pub scan_rate: f64,
}

impl StatusSnapshot {
    /// True when every root finished a full scan, live updates run for all
    /// of them, and nothing is pending.
    pub fn up_to_date(&self) -> bool {
        self.phase == Phase::Idle
            && !self.roots.is_empty()
            && self
                .roots
                .iter()
                .all(|r| r.state == "complete" && !matches!(r.live, LiveMode::Unavailable(_)))
    }
}

#[derive(Default)]
pub struct IndexStatus {
    snap: Mutex<StatusSnapshot>,
    pub progress: ScanProgress,
    pub cancel: AtomicBool,
    /// Incremented whenever the index content changes, so views can refresh.
    pub version: AtomicU64,
}

impl IndexStatus {
    pub fn snapshot(&self) -> StatusSnapshot {
        self.snap.lock().clone()
    }

    fn update(&self, f: impl FnOnce(&mut StatusSnapshot)) {
        f(&mut self.snap.lock());
    }

    fn error(&self, line: String) {
        self.update(|s| {
            if s.recent_errors.len() >= 200 {
                s.recent_errors.pop_front();
            }
            s.recent_errors.push_back(line);
        });
    }
}

pub struct IndexerHandle {
    tx: Sender<Command>,
    pub status: Arc<IndexStatus>,
    thread: Option<JoinHandle<()>>,
}

impl IndexerHandle {
    pub fn start(db_path: PathBuf, cfg: AppConfig) -> IndexerHandle {
        let (tx, rx) = crossbeam_channel::bounded::<Command>(COMMAND_QUEUE);
        let status = Arc::new(IndexStatus::default());
        let st = status.clone();
        let tx2 = tx.clone();
        let thread = std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || {
                let st2 = st.clone();
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let mut ix = Indexer::new(db_path, cfg, st, tx2, rx);
                    ix.run();
                }));
                if run.is_err() {
                    // The panic itself is in the log (see main.rs). Searching
                    // keeps working on the existing index.
                    st2.update(|s| {
                        s.phase = Phase::Failed(
                            "Index updates stopped after an internal error. Restart WinSearch; details are in the log.".into(),
                        )
                    });
                }
            })
            .expect("spawn indexer thread");
        IndexerHandle { tx, status, thread: Some(thread) }
    }

    pub fn send(&self, cmd: Command) {
        if self.tx.send_timeout(cmd, Duration::from_secs(2)).is_err() {
            tracing::warn!("indexer command queue is full; command dropped");
        }
    }

    pub fn cancel_scan(&self) {
        self.status.cancel.store(true, Ordering::Relaxed);
    }

    /// Tells the indexer about a rename the app itself just made, so the
    /// index is updated right away instead of waiting for the change source.
    pub fn notify_renamed(&self, from: String, to: String) {
        let root = to.clone();
        self.send(Command::Watch { root, event: WatchEvent::Changes(vec![Change::Rename { from, to }]) });
    }
}

impl Drop for IndexerHandle {
    fn drop(&mut self) {
        self.cancel_scan();
        let _ = self.tx.send_timeout(Command::Shutdown, Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const COMMAND_QUEUE: usize = 4096;
const MAX_CHANGES_PER_TX: usize = 20_000;
const RETRY_EVERY: Duration = Duration::from_secs(30);

enum Live {
    Notify(#[allow(dead_code)] DirWatcher),
    Usn(#[allow(dead_code)] usn::UsnWatcher),
}

struct Indexer {
    db_path: PathBuf,
    db: Option<Db>,
    cfg: AppConfig,
    status: Arc<IndexStatus>,
    tx: Sender<Command>,
    rx: Receiver<Command>,
    live: HashMap<String, Live>,
    live_mode: HashMap<String, LiveMode>,
    usn_notes: HashMap<String, String>,
    queue: VecDeque<(String, bool)>,
    /// Roots whose change events were dropped because the queue was full.
    overflowed: Arc<Mutex<Vec<String>>>,
    last_retry: Instant,
    backend: Box<dyn ScanBackend>,
}

impl Indexer {
    fn new(
        db_path: PathBuf,
        cfg: AppConfig,
        status: Arc<IndexStatus>,
        tx: Sender<Command>,
        rx: Receiver<Command>,
    ) -> Indexer {
        Indexer {
            db_path,
            db: None,
            cfg,
            status,
            tx,
            rx,
            live: HashMap::new(),
            live_mode: HashMap::new(),
            usn_notes: HashMap::new(),
            queue: VecDeque::new(),
            overflowed: Arc::new(Mutex::new(Vec::new())),
            last_retry: Instant::now(),
            backend: Box::new(DirWalkBackend),
        }
    }

    fn excludes(&self) -> Vec<String> {
        let mut x = self.cfg.excludes.clone();
        // Never index our own database and logs (would feed back changes).
        x.push(util::normalize_dir(&crate::config::data_dir().to_string_lossy()));
        x
    }

    fn run(&mut self) {
        match Db::open(&self.db_path) {
            Ok((db, report)) => {
                if let Some(b) = report.recovered_from {
                    self.status.update(|s| {
                        s.notices.push(format!(
                            "The index database was damaged and has been rebuilt. The old file was kept as {}.",
                            b.display()
                        ))
                    });
                }
                self.db = Some(db);
            }
            Err(e) => {
                let msg = format!("Cannot open the index database: {e:#}");
                tracing::error!("{msg}");
                self.status.update(|s| s.phase = Phase::Failed(msg));
                // Stay alive so the UI can still shut us down cleanly.
                while let Ok(cmd) = self.rx.recv() {
                    if matches!(cmd, Command::Shutdown) {
                        return;
                    }
                }
                return;
            }
        }
        if let Err(e) = self.startup() {
            self.fail(e);
        }
        loop {
            if let Some((root, force_new)) = self.queue.pop_front() {
                if let Err(e) = self.scan(&root, force_new) {
                    self.fail(e);
                }
                continue;
            }
            self.set_phase(Phase::Idle);
            match self.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Command::Shutdown) => break,
                Ok(cmd) => {
                    if let Err(e) = self.handle(cmd) {
                        self.fail(e);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.housekeeping();
        }
        self.live.clear();
        if let Some(db) = &self.db {
            db.checkpoint();
        }
        tracing::info!("indexer stopped");
    }

    fn db(&mut self) -> &mut Db {
        self.db.as_mut().expect("database open")
    }

    fn fail(&mut self, e: anyhow::Error) {
        let corrupt = e
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error_code())
            .map(|c| {
                matches!(c, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
            })
            .unwrap_or(false);
        let msg = if corrupt {
            format!("The index database is damaged ({e}). Use Settings > Rebuild index.")
        } else {
            format!("Indexing error: {e:#}")
        };
        tracing::error!("{msg}");
        self.status.error(msg.clone());
        self.status.update(|s| s.message = msg);
    }

    fn set_phase(&self, p: Phase) {
        self.status.update(|s| {
            if s.phase != p {
                s.phase = p;
            }
        });
    }

    /// Publishes per-root state and totals.
    fn publish(&mut self) {
        let roots = self.db().roots().unwrap_or_default();
        let total = self.db().count().unwrap_or(0);
        let modes = self.live_mode.clone();
        let notes = self.usn_notes.clone();
        self.status.update(|s| {
            s.total_entries = total;
            s.roots = roots
                .into_iter()
                .map(|r| RootStatus {
                    live: modes
                        .get(&r.path.to_lowercase())
                        .cloned()
                        .unwrap_or(LiveMode::Unavailable("not started".into())),
                    usn_note: notes.get(&r.path.to_lowercase()).cloned(),
                    path: r.path,
                    state: r.state,
                    last_complete_scan: r.last_complete_scan,
                    errors: r.error_count,
                    last_error: r.last_error,
                })
                .collect();
        });
        self.status.version.fetch_add(1, Ordering::Relaxed);
    }

    fn startup(&mut self) -> anyhow::Result<()> {
        self.sync_roots()?;
        let roots = self.cfg.roots.clone();
        for root in &roots {
            let needs_full = self.start_live(root);
            let row = self.db().root(root)?;
            let serial_now = crate::platform::volume_info(root).map(|v| v.serial);
            let serial_changed = matches!((row.as_ref().and_then(|r| r.volume_serial), serial_now),
                (Some(a), Some(b)) if a != b);
            match row {
                Some(r) if r.state == "scanning" && !serial_changed => self.enqueue(root, false),
                Some(r) if r.state == "complete" && !serial_changed => {
                    let mode = self.live_mode.get(&root.to_lowercase()).cloned();
                    let reconcile = match mode {
                        Some(LiveMode::Usn) => needs_full,
                        _ => self.cfg.reconcile_on_startup,
                    };
                    if reconcile {
                        self.enqueue(root, true);
                    }
                }
                _ => self.enqueue(root, true),
            }
        }
        if self.queue.is_empty() && self.db().fts_stale()? {
            self.set_phase(Phase::Rebuilding);
            self.db().rebuild_fts()?;
        }
        self.publish();
        Ok(())
    }

    /// Makes the database's root list match the settings.
    fn sync_roots(&mut self) -> anyhow::Result<()> {
        let wanted: Vec<String> = self.cfg.roots.iter().map(|r| r.to_lowercase()).collect();
        for r in self.db().roots()? {
            if !wanted.contains(&r.path.to_lowercase()) {
                self.set_phase(Phase::Rebuilding);
                self.status.update(|s| s.message = format!("Removing {} from the index", r.path));
                let n = self.db().remove_root(r.id)?;
                tracing::info!(root = %r.path, removed = n, "root removed");
            }
        }
        let roots = self.cfg.roots.clone();
        for r in &roots {
            self.db().ensure_root(r)?;
        }
        Ok(())
    }

    fn enqueue(&mut self, root: &str, force_new: bool) {
        if let Some(e) = self.queue.iter_mut().find(|(r, _)| r.eq_ignore_ascii_case(root)) {
            e.1 |= force_new;
        } else {
            self.queue.push_back((root.to_string(), force_new));
        }
    }

    /// Starts the best available change source for `root`. Returns true if
    /// the USN journal position could not be resumed (full scan needed).
    fn start_live(&mut self, root: &str) -> bool {
        let key = root.to_lowercase();
        self.live.remove(&key);
        let tx = self.tx.clone();
        let overflowed = self.overflowed.clone();
        let root_s = root.to_string();
        let sink = move |event: WatchEvent| {
            match tx.try_send(Command::Watch { root: root_s.clone(), event }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => overflowed.lock().push(root_s.clone()),
                Err(TrySendError::Disconnected(_)) => {}
            }
        };

        match usn::probe(root) {
            usn::UsnAvailability::Available(_) => {
                let resume = self
                    .db()
                    .root(root)
                    .ok()
                    .flatten()
                    .and_then(|r| Some((r.usn_journal_id?, r.usn_next?)));
                match usn::UsnWatcher::start(root, resume, sink.clone()) {
                    Ok(start) => {
                        if let Ok(Some(r)) = self.db().root(root) {
                            let _ = self.db().set_root_usn(
                                r.id,
                                Some(start.journal.journal_id),
                                Some(if start.needs_full_scan { start.journal.next_usn } else { resume.map(|x| x.1).unwrap_or(start.journal.next_usn) }),
                            );
                        }
                        self.live.insert(key.clone(), Live::Usn(start.watcher));
                        self.live_mode.insert(key.clone(), LiveMode::Usn);
                        self.usn_notes.remove(&key);
                        return start.needs_full_scan;
                    }
                    Err(e) => {
                        self.usn_notes.insert(key.clone(), format!("USN journal could not be read: {}", e.message()));
                    }
                }
            }
            usn::UsnAvailability::NeedsAdmin => {
                self.usn_notes.insert(
                    key.clone(),
                    "Change journal needs administrator rights; using change notifications and reconciliation scans.".into(),
                );
            }
            usn::UsnAvailability::NotNtfs(fs) => {
                self.usn_notes.insert(key.clone(), format!("{fs} volume has no change journal; using change notifications."));
            }
            usn::UsnAvailability::NoJournal(m) => {
                self.usn_notes.insert(key.clone(), format!("No change journal on this volume ({m}); using change notifications."));
            }
            usn::UsnAvailability::NotDriveRoot => {
                self.usn_notes.insert(key.clone(), "Folder roots use change notifications.".into());
            }
        }
        match DirWatcher::start(root, sink) {
            Ok(w) => {
                self.live.insert(key.clone(), Live::Notify(w));
                self.live_mode.insert(key, LiveMode::Notifications);
            }
            Err(e) => {
                self.live_mode.insert(key, LiveMode::Unavailable(e.message().to_string()));
            }
        }
        false
    }

    fn scope_roots(&mut self) -> Vec<(String, i64)> {
        self.db().roots().unwrap_or_default().into_iter().map(|r| (r.path, r.id)).collect()
    }

    fn scan(&mut self, root: &str, force_new: bool) -> anyhow::Result<()> {
        self.status.cancel.store(false, Ordering::Relaxed);
        let p = &self.status.progress;
        for a in [&p.files, &p.dirs, &p.errors, &p.skipped_dirs] {
            a.store(0, Ordering::Relaxed);
        }
        self.set_phase(Phase::Scanning);
        self.status.update(|s| {
            s.current_root = Some(root.to_string());
            s.message = format!("Scanning {root}");
        });
        self.publish();
        let cfg = ScanConfig { excludes: self.excludes(), threads: self.cfg.scan_threads };
        let status = self.status.clone();
        let backend = std::mem::replace(&mut self.backend, Box::new(DirWalkBackend));
        let res = scan::scan_root(
            self.db.as_mut().expect("db"),
            backend.as_ref(),
            root,
            &cfg,
            force_new,
            &status.cancel,
            &status.progress,
            |path, msg| status.error(format!("{path}: {msg}")),
        );
        self.backend = backend;
        let rep = res?;
        let secs = rep.elapsed.as_secs_f64().max(0.001);
        let msg = match &rep.outcome {
            ScanOutcome::Completed => format!(
                "{root}: {} entries in {:.1}s, {} unreadable, {} removed",
                util::format_count(rep.files),
                secs,
                rep.errors,
                rep.removed
            ),
            ScanOutcome::Cancelled => {
                self.queue.clear();
                format!("{root}: scan paused after {} entries; it resumes on the next scan", util::format_count(rep.files))
            }
            ScanOutcome::RootUnavailable(e) => format!("{root} is not available: {e}"),
        };
        tracing::info!("{msg}");
        let rate = rep.files as f64 / secs;
        self.status.update(|s| {
            s.current_root = None;
            s.message = msg;
            s.scan_rate = rate;
        });
        self.db().checkpoint();
        self.publish();
        Ok(())
    }

    fn handle(&mut self, cmd: Command) -> anyhow::Result<()> {
        match cmd {
            Command::Shutdown => {}
            Command::ScanAll { force_new } => {
                for r in self.cfg.roots.clone() {
                    self.enqueue(&r, force_new);
                }
            }
            Command::Rebuild => {
                self.queue.clear();
                self.set_phase(Phase::Rebuilding);
                self.status.update(|s| s.message = "Clearing the index".into());
                self.db().clear_all()?;
                self.publish();
                for r in self.cfg.roots.clone() {
                    self.start_live(&r);
                    self.enqueue(&r, true);
                }
            }
            Command::Verify => {
                self.status.update(|s| s.message = "Checking index integrity...".into());
                let msg = match self.db().quick_check()? {
                    None => "Index integrity check passed.".to_string(),
                    Some(p) => format!("Index integrity problem: {p}. Use Rebuild index."),
                };
                self.status.update(|s| s.message = msg);
            }
            Command::Reconfigure(new_cfg) => self.reconfigure(new_cfg)?,
            Command::Watch { root, event } => self.on_watch(root, event)?,
        }
        Ok(())
    }

    fn reconfigure(&mut self, new_cfg: AppConfig) -> anyhow::Result<()> {
        let old = std::mem::replace(&mut self.cfg, new_cfg);
        let lower = |v: &[String]| v.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
        let (old_roots, new_roots) = (lower(&old.roots), lower(&self.cfg.roots));
        for r in &old_roots {
            if !new_roots.contains(r) {
                self.live.remove(r);
                self.live_mode.remove(r);
            }
        }
        self.sync_roots()?;
        let excludes_changed = lower(&old.excludes) != lower(&self.cfg.excludes);
        for r in self.cfg.roots.clone() {
            if !old_roots.contains(&r.to_lowercase()) {
                self.start_live(&r);
                self.enqueue(&r, true);
            } else if excludes_changed {
                // Newly excluded entries disappear and newly included ones
                // appear through a reconciliation scan.
                self.enqueue(&r, true);
            }
        }
        self.publish();
        Ok(())
    }

    fn on_watch(&mut self, root: String, event: WatchEvent) -> anyhow::Result<()> {
        match event {
            WatchEvent::Changes(first) => {
                // Drain whatever else is queued so bursts become one transaction.
                let mut batch = first;
                let mut deferred = Vec::new();
                while batch.len() < MAX_CHANGES_PER_TX {
                    match self.rx.try_recv() {
                        Ok(Command::Watch { event: WatchEvent::Changes(more), .. }) => batch.extend(more),
                        Ok(other) => {
                            deferred.push(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                self.apply(batch)?;
                for cmd in deferred {
                    self.handle(cmd)?;
                }
            }
            WatchEvent::Overflow => {
                tracing::warn!(%root, "change events were lost; reconciling");
                self.status.update(|s| s.message = format!("Change events were lost on {root}; rescanning to catch up"));
                self.enqueue(&root, true);
            }
            WatchEvent::Failed(msg) => {
                tracing::warn!(%root, "live updates stopped: {msg}");
                self.live.remove(&root.to_lowercase());
                self.live_mode.insert(root.to_lowercase(), LiveMode::Unavailable(msg));
                self.publish();
            }
            WatchEvent::UsnCheckpoint { journal_id, next_usn } => {
                if let Some(r) = self.db().root(&root)? {
                    self.db().set_root_usn(r.id, Some(journal_id), Some(next_usn))?;
                }
            }
        }
        Ok(())
    }

    fn apply(&mut self, batch: Vec<Change>) -> anyhow::Result<()> {
        let batch = changes::coalesce(batch);
        if batch.is_empty() {
            return Ok(());
        }
        self.set_phase(Phase::Applying);
        let roots = self.scope_roots();
        let excludes = self.excludes();
        let scope = Scope { roots: &roots, excludes: &excludes };
        let stats = changes::apply(self.db(), &scope, &batch)?;
        tracing::debug!(?stats, n = batch.len(), "applied changes");
        let total = self.db().count().unwrap_or(0);
        self.status.update(|s| {
            s.changes_applied += batch.len() as u64;
            s.total_entries = total;
        });
        self.status.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Periodic work: overflow recovery, retrying unavailable roots and
    /// periodic reconciliation for roots without a change journal.
    fn housekeeping(&mut self) {
        let over: Vec<String> = std::mem::take(&mut *self.overflowed.lock());
        for r in over {
            self.enqueue(&r, true);
        }
        if self.last_retry.elapsed() < RETRY_EVERY {
            return;
        }
        self.last_retry = Instant::now();
        let roots = self.cfg.roots.clone();
        let mut changed = false;
        for r in &roots {
            let key = r.to_lowercase();
            if matches!(self.live_mode.get(&key), Some(LiveMode::Unavailable(_)) | None)
                && crate::platform::volume_info(&crate::platform::drive_root(r).unwrap_or_default()).is_some()
                && std::fs::metadata(r).is_ok()
            {
                tracing::info!(root = %r, "root is available again");
                self.start_live(r);
                self.enqueue(r, true);
                changed = true;
            }
        }
        if self.cfg.reconcile_interval_hours > 0 {
            let max_age = self.cfg.reconcile_interval_hours as i64 * 3600;
            let now = util::now_unix();
            for row in self.db().roots().unwrap_or_default() {
                let usn = matches!(self.live_mode.get(&row.path.to_lowercase()), Some(LiveMode::Usn));
                if !usn && row.state == "complete" && row.last_complete_scan.map(|t| now - t > max_age).unwrap_or(false) {
                    self.enqueue(&row.path, true);
                }
            }
        }
        if changed {
            self.publish();
        }
    }
}
