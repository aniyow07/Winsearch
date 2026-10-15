//! Runs one root scan: walker threads feed a bounded channel, this thread
//! batches the results into SQLite transactions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::db::Db;
use crate::model::FileRecord;
use crate::scanner::{ResumeState, ScanBackend, ScanConfig, ScanMsg, ScanOutcome, ScanProgress};

/// Rows per write transaction. Larger batches amortize fsync and FTS
/// segment merges; the time bound keeps progress visible on slow disks.
const BATCH_ROWS: usize = 20_000;
const BATCH_MAX_AGE: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct ScanReport {
    pub outcome: ScanOutcome,
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub removed: u64,
    pub resumed: bool,
    pub elapsed: Duration,
    /// A sample of error messages (path: message), capped.
    pub error_samples: Vec<String>,
}

pub const MAX_ERROR_SAMPLES: usize = 200;

/// Scans `root` into `db`. `on_error` sees every error message as it
/// arrives (for live display). The scan resumes an interrupted generation
/// unless `force_new` is set.
#[allow(clippy::too_many_arguments)]
pub fn scan_root(
    db: &mut Db,
    backend: &dyn ScanBackend,
    root: &str,
    cfg: &ScanConfig,
    force_new: bool,
    cancel: &AtomicBool,
    progress: &ScanProgress,
    mut on_error: impl FnMut(&str, &str),
) -> Result<ScanReport> {
    let started = Instant::now();
    let root_id = db.ensure_root(root)?;
    if let Some(v) = crate::platform::volume_info(root) {
        db.set_root_volume(root_id, Some(v.serial), Some(&v.fs_name))?;
    }
    let (gen_, resumed) = db.begin_scan(root_id, force_new)?;
    db.set_bulk_mode(true);
    // Filling an empty root: maintaining the trigram index row by row costs
    // more than the scan itself, so suspend it and rebuild once at the end
    // (measured 75 s vs 44 s for 1.6M entries; docs/BENCHMARKS.md).
    if db.root_row_count(root_id)? == 0 && !db.fts_stale()? {
        db.suspend_fts()?;
    }
    let resume = if resumed { db.resume_state(root_id, gen_)? } else { ResumeState::default() };
    if resumed {
        tracing::info!(root, gen_, listed = resume.listed.len(), "resuming interrupted scan");
    }

    let (tx, rx) = crossbeam_channel::bounded::<ScanMsg>(256);
    let mut error_samples = Vec::new();
    let mut last_error: Option<String> = None;

    let outcome = std::thread::scope(|s| -> Result<ScanOutcome> {
        let walker = s.spawn(|| {
            let tx = tx; // dropped when the walk ends, closing the channel
            backend.scan(root, cfg, &resume, &tx, cancel, progress)
        });

        let mut records: Vec<FileRecord> = Vec::with_capacity(BATCH_ROWS);
        let mut listed: Vec<String> = Vec::new();
        let mut last_flush = Instant::now();
        let mut write_err: Option<anyhow::Error> = None;
        loop {
            let msg = rx.recv_timeout(Duration::from_millis(100));
            match msg {
                Ok(ScanMsg::Entries { dir, records: mut recs, complete }) => {
                    records.append(&mut recs);
                    if complete && dir != root {
                        listed.push(dir);
                    }
                }
                Ok(ScanMsg::Error { path, message }) => {
                    on_error(&path, &message);
                    let line = format!("{path}: {message}");
                    if error_samples.len() < MAX_ERROR_SAMPLES {
                        error_samples.push(line.clone());
                    }
                    last_error = Some(line);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            let due = records.len() >= BATCH_ROWS
                || (!records.is_empty() && last_flush.elapsed() >= BATCH_MAX_AGE);
            if due && write_err.is_none() {
                if let Err(e) = db.upsert_batch(root_id, gen_, &records, &listed) {
                    // Stop the walk; the error is reported below.
                    cancel.store(true, Ordering::Relaxed);
                    write_err = Some(e);
                }
                records.clear();
                listed.clear();
                last_flush = Instant::now();
            }
        }
        let outcome = walker.join().unwrap_or(ScanOutcome::Cancelled);
        if let Some(e) = write_err {
            return Err(e);
        }
        if !records.is_empty() || !listed.is_empty() {
            db.upsert_batch(root_id, gen_, &records, &listed)?;
        }
        Ok(outcome)
    })?;

    if db.fts_stale()? {
        db.rebuild_fts()?;
    }
    db.set_bulk_mode(false);
    let errors = progress.errors.load(Ordering::Relaxed);
    let removed = match &outcome {
        ScanOutcome::Completed => db.finish_scan(root_id, gen_, errors, last_error.as_deref())?,
        ScanOutcome::Cancelled => {
            // Keep state 'scanning' so the next start resumes this generation.
            0
        }
        ScanOutcome::RootUnavailable(msg) => {
            // Keep the existing entries (drive may come back) but say so.
            db.set_root_state(root_id, "unavailable", Some(msg))?;
            0
        }
    };
    Ok(ScanReport {
        outcome,
        files: progress.files.load(Ordering::Relaxed),
        dirs: progress.dirs.load(Ordering::Relaxed),
        errors,
        removed,
        resumed,
        elapsed: started.elapsed(),
        error_samples,
    })
}
