//! Directory-enumeration backend for the initial (and reconciliation) scan.
//!
//! Scanning is behind the [`ScanBackend`] trait so a different strategy
//! (for example NTFS MFT enumeration when elevated) can be added without
//! touching the indexer or UI.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use crate::model::FileRecord;
use crate::util;

/// Messages produced by a scan, consumed by the index writer.
#[derive(Debug)]
pub enum ScanMsg {
    /// Children of `dir`. `complete` is true when `dir` was enumerated to
    /// the end (large directories arrive in several chunks).
    Entries { dir: String, records: Vec<FileRecord>, complete: bool },
    /// A directory or entry could not be read.
    Error { path: String, message: String },
}

#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub errors: AtomicU64,
    pub skipped_dirs: AtomicU64,
}

/// Directories already enumerated by an interrupted scan of the same
/// generation (lower-cased path) and their child directories.
#[derive(Default, Debug)]
pub struct ResumeState {
    pub listed: HashSet<String>,
    pub children: HashMap<String, Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// Normalized folders that are not indexed (nor descended into).
    pub excludes: Vec<String>,
    pub threads: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig { excludes: Vec::new(), threads: default_threads() }
    }
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanOutcome {
    Completed,
    Cancelled,
    /// The root itself could not be read (drive missing, access denied...).
    RootUnavailable(String),
}

pub trait ScanBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn scan(
        &self,
        root: &str,
        cfg: &ScanConfig,
        resume: &ResumeState,
        tx: &Sender<ScanMsg>,
        cancel: &AtomicBool,
        progress: &ScanProgress,
    ) -> ScanOutcome;
}

/// Parallel FindFirstFile/FindNextFile walker (via `std::fs::read_dir`).
/// Never follows symlinks, junctions or mount points, so it cannot loop.
pub struct DirWalkBackend;

const CHUNK: usize = 2048;

impl ScanBackend for DirWalkBackend {
    fn name(&self) -> &'static str {
        "directory walk"
    }

    fn scan(
        &self,
        root: &str,
        cfg: &ScanConfig,
        resume: &ResumeState,
        tx: &Sender<ScanMsg>,
        cancel: &AtomicBool,
        progress: &ScanProgress,
    ) -> ScanOutcome {
        if let Err(e) = std::fs::read_dir(root) {
            return ScanOutcome::RootUnavailable(e.to_string());
        }
        let excludes: Vec<String> = cfg.excludes.iter().map(|e| util::normalize_dir(e)).collect();
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<String>();
        let pending = Arc::new(AtomicUsize::new(1));
        work_tx.send(root.to_string()).ok();

        std::thread::scope(|s| {
            for _ in 0..cfg.threads.max(1) {
                let work_tx = work_tx.clone();
                let work_rx = work_rx.clone();
                let pending = pending.clone();
                let excludes = &excludes;
                s.spawn(move || {
                    worker(root, &work_tx, &work_rx, &pending, excludes, resume, tx, cancel, progress)
                });
            }
        });

        if cancel.load(Ordering::Relaxed) { ScanOutcome::Cancelled } else { ScanOutcome::Completed }
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    root: &str,
    work_tx: &Sender<String>,
    work_rx: &Receiver<String>,
    pending: &AtomicUsize,
    excludes: &[String],
    resume: &ResumeState,
    tx: &Sender<ScanMsg>,
    cancel: &AtomicBool,
    progress: &ScanProgress,
) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let dir = match work_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(d) => d,
            Err(_) => {
                if pending.load(Ordering::Acquire) == 0 {
                    return;
                }
                continue;
            }
        };
        let key = dir.to_lowercase();
        let subdirs = if dir != root && resume.listed.contains(&key) {
            // Already enumerated by the interrupted scan: only descend.
            progress.skipped_dirs.fetch_add(1, Ordering::Relaxed);
            resume.children.get(&key).cloned().unwrap_or_default()
        } else {
            list_dir(&dir, excludes, tx, progress)
        };
        for sub in subdirs {
            pending.fetch_add(1, Ordering::AcqRel);
            if work_tx.send(sub).is_err() {
                pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Lists one directory, sends its entries, and returns subdirectories to
/// descend into.
fn list_dir(
    dir: &str,
    excludes: &[String],
    tx: &Sender<ScanMsg>,
    progress: &ScanProgress,
) -> Vec<String> {
    let mut subdirs = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(ScanMsg::Error { path: dir.to_string(), message: e.to_string() });
            return subdirs;
        }
    };
    progress.dirs.fetch_add(1, Ordering::Relaxed);
    let mut batch = Vec::with_capacity(64);
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(ScanMsg::Error { path: dir.to_string(), message: e.to_string() });
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(ScanMsg::Error {
                    path: util::join(dir, &name),
                    message: e.to_string(),
                });
                continue;
            }
        };
        let rec = FileRecord::from_metadata(dir, &name, &md);
        if rec.is_dir && !excludes.is_empty() && excludes.iter().any(|x| util::is_under(&rec.path, x)) {
            continue;
        }
        // Symlinks, junctions and mount points are indexed as entries but
        // never descended into: they can point back up the tree.
        let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(true);
        if rec.is_dir && !is_link {
            subdirs.push(rec.path.clone());
        }
        progress.files.fetch_add(1, Ordering::Relaxed);
        batch.push(rec);
        if batch.len() >= CHUNK {
            let records = std::mem::replace(&mut batch, Vec::with_capacity(CHUNK));
            if tx.send(ScanMsg::Entries { dir: dir.to_string(), records, complete: false }).is_err() {
                return Vec::new();
            }
        }
    }
    let _ = tx.send(ScanMsg::Entries { dir: dir.to_string(), records: batch, complete: true });
    subdirs
}
