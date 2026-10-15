//! Change notifications via ReadDirectoryChangesW (works without admin
//! rights on local volumes).
//!
//! Notifications are lossy by design: the kernel buffer can overflow, and
//! nothing is delivered while the app is not running. Overflow is reported
//! as [`WatchEvent::Overflow`] so the indexer can reconcile the whole root,
//! and startup/periodic reconciliation scans cover the rest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResultEx, OVERLAPPED};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{HRESULT, HSTRING};

use super::changes::Change;
use crate::platform::OwnedHandle;
use crate::util;

#[derive(Debug)]
pub enum WatchEvent {
    Changes(Vec<Change>),
    /// Events were lost; the root must be reconciled by a scan.
    Overflow,
    /// The watch stopped (volume removed, access lost...).
    Failed(String),
    /// USN journal position after the preceding changes (persist once applied).
    UsnCheckpoint { journal_id: u64, next_usn: i64 },
}

pub struct DirWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DirWatcher {
    /// Starts watching `root` recursively. `sink` is called from the
    /// watcher thread with batched events.
    pub fn start(
        root: &str,
        sink: impl Fn(WatchEvent) + Send + 'static,
    ) -> windows::core::Result<DirWatcher> {
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(root),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )?
        };
        let handle = OwnedHandle(handle);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let root = root.to_string();
        let thread = std::thread::Builder::new()
            .name(format!("watch {root}"))
            .spawn(move || {
                if let Err(e) = run(&root, handle, &stop2, &sink) {
                    if !stop2.load(Ordering::Relaxed) {
                        sink(WatchEvent::Failed(e.message().to_string()));
                    }
                }
            })
            .map_err(|e| windows::core::Error::new(HRESULT(-1), e.to_string()))?;
        Ok(DirWatcher { stop, thread: Some(thread) })
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 64 KB is the documented maximum for network shares and plenty locally
/// when the buffer is drained promptly.
const BUF_BYTES: usize = 64 * 1024;
const FLUSH_EVERY: Duration = Duration::from_millis(250);

fn run(
    root: &str,
    handle: OwnedHandle,
    stop: &AtomicBool,
    sink: &dyn Fn(WatchEvent),
) -> windows::core::Result<()> {
    let event = OwnedHandle(unsafe { CreateEventW(None, true, false, None)? });
    let mut buf = vec![0u32; BUF_BYTES / 4]; // DWORD-aligned as required
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION;
    let mut pending: Vec<Change> = Vec::new();
    let mut rename_from: Option<String> = None;
    let mut last_flush = Instant::now();

    loop {
        let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
        unsafe {
            ReadDirectoryChangesW(
                handle.0,
                buf.as_mut_ptr() as *mut _,
                BUF_BYTES as u32,
                true,
                filter,
                None,
                Some(&mut ov),
                None,
            )?;
        }
        let mut bytes = 0u32;
        loop {
            match unsafe { GetOverlappedResultEx(handle.0, &ov, &mut bytes, 200, false) } {
                Ok(()) => break,
                Err(e) if e.code() == HRESULT::from_win32(WAIT_TIMEOUT.0) => {
                    if !pending.is_empty() && last_flush.elapsed() >= FLUSH_EVERY {
                        sink(WatchEvent::Changes(std::mem::take(&mut pending)));
                        last_flush = Instant::now();
                    }
                    if stop.load(Ordering::Relaxed) {
                        unsafe {
                            let _ = CancelIoEx(handle.0, Some(&ov));
                            // Wait for the cancellation so `buf`/`ov` outlive the I/O.
                            let _ = GetOverlappedResultEx(handle.0, &ov, &mut bytes, u32::MAX, false);
                        }
                        return Ok(());
                    }
                }
                Err(e) if e.code() == HRESULT::from_win32(ERROR_NOTIFY_ENUM_DIR.0) => {
                    bytes = 0;
                    break;
                }
                Err(e) if e.code() == HRESULT::from_win32(ERROR_OPERATION_ABORTED.0) => {
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        if bytes == 0 {
            // Buffer overflowed: the kernel discarded the events.
            pending.clear();
            rename_from = None;
            sink(WatchEvent::Overflow);
            continue;
        }
        let bytes_slice =
            unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, bytes as usize) };
        parse_notifications(root, bytes_slice, &mut pending, &mut rename_from);
        if pending.len() >= 4096 || last_flush.elapsed() >= FLUSH_EVERY {
            sink(WatchEvent::Changes(std::mem::take(&mut pending)));
            last_flush = Instant::now();
        }
    }
}

const ACTION_ADDED: u32 = 1;
const ACTION_REMOVED: u32 = 2;
const ACTION_MODIFIED: u32 = 3;
const ACTION_RENAMED_OLD: u32 = 4;
const ACTION_RENAMED_NEW: u32 = 5;

/// Parses a FILE_NOTIFY_INFORMATION chain. Treats the buffer as untrusted:
/// all offsets are bounds-checked.
pub fn parse_notifications(
    root: &str,
    data: &[u8],
    out: &mut Vec<Change>,
    rename_from: &mut Option<String>,
) {
    let mut off = 0usize;
    loop {
        if off + 12 > data.len() {
            return;
        }
        let rd = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
        let next = rd(off) as usize;
        let action = rd(off + 4);
        let name_len = rd(off + 8) as usize;
        let name_start = off + 12;
        let Some(name_bytes) = data.get(name_start..name_start + name_len) else { return };
        let units: Vec<u16> =
            name_bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let rel = String::from_utf16_lossy(&units);
        let path = util::join(root, &rel);
        match action {
            ACTION_ADDED | ACTION_MODIFIED => out.push(Change::Upsert(path)),
            ACTION_REMOVED => out.push(Change::Remove(path)),
            ACTION_RENAMED_OLD => {
                if let Some(prev) = rename_from.replace(path) {
                    out.push(Change::Remove(prev));
                }
            }
            ACTION_RENAMED_NEW => match rename_from.take() {
                Some(from) => out.push(Change::Rename { from, to: path }),
                None => out.push(Change::Upsert(path)),
            },
            _ => {}
        }
        if next == 0 {
            return;
        }
        off += next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = Vec::new();
        v.extend_from_slice(&next.to_le_bytes());
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_chain_with_rename_pair() {
        let a = record(0, ACTION_ADDED, "dir\\new.txt");
        let b = record(0, ACTION_RENAMED_OLD, "old");
        let c = record(0, ACTION_RENAMED_NEW, "renamed");
        let mut data = Vec::new();
        for (i, r) in [&a, &b, &c].iter().enumerate() {
            let mut r = (*r).clone();
            if i < 2 {
                let len = r.len() as u32;
                r[0..4].copy_from_slice(&len.to_le_bytes());
            }
            data.extend(r);
        }
        let mut out = Vec::new();
        let mut from = None;
        parse_notifications("C:\\w", &data, &mut out, &mut from);
        assert_eq!(
            out,
            vec![
                Change::Upsert("C:\\w\\dir\\new.txt".into()),
                Change::Rename { from: "C:\\w\\old".into(), to: "C:\\w\\renamed".into() },
            ]
        );
    }

    #[test]
    fn truncated_buffer_is_ignored() {
        let mut r = record(0, ACTION_ADDED, "abc");
        r[8..12].copy_from_slice(&1000u32.to_le_bytes());
        let mut out = Vec::new();
        parse_notifications("C:\\", &r, &mut out, &mut None);
        assert!(out.is_empty());
    }
}
