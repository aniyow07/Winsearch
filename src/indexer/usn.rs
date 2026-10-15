//! NTFS USN change journal reader (optional; needs administrator rights).
//!
//! When it can be used it is strictly better than directory notifications:
//! changes made while WinSearch was closed are replayed on the next start,
//! and nothing is lost unless the journal wraps around, which is detected
//! (journal ID change or the saved position falling below `FirstUsn`) and
//! answered with a full reconciliation scan.
//!
//! The app never creates or reconfigures a journal; if the volume has none,
//! directory notifications are used instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_DESCRIPTOR,
    FILE_ID_DESCRIPTOR_0, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType, GetFinalPathNameByHandleW, OPEN_EXISTING,
    OpenFileById,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{HRESULT, HSTRING};

use super::changes::Change;
use super::watcher::WatchEvent;
use crate::platform::OwnedHandle;
use crate::util;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;

pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct UsnJournalData {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ReadUsnJournalData {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
}

/// One parsed journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub file_ref: u64,
    pub parent_ref: u64,
    pub usn: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
}

/// Parses the output of FSCTL_READ_USN_JOURNAL: an 8-byte next-USN followed
/// by USN_RECORD_V2/V3 entries. Bounds-checked; malformed tails are ignored.
pub fn parse_records(buf: &[u8]) -> (i64, Vec<UsnRecord>) {
    if buf.len() < 8 {
        return (0, Vec::new());
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
    let mut out = Vec::new();
    let mut off = 8usize;
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    while off + 8 <= buf.len() {
        let len = u32_at(buf, off) as usize;
        if len < 8 || off + len > buf.len() {
            break;
        }
        let rec = &buf[off..off + len];
        let major = u16_at(rec, 4);
        let parsed = match major {
            2 if len >= 60 => {
                let name_len = u16_at(rec, 56) as usize;
                let name_off = u16_at(rec, 58) as usize;
                rec.get(name_off..name_off + name_len).map(|n| UsnRecord {
                    file_ref: u64_at(rec, 8),
                    parent_ref: u64_at(rec, 16),
                    usn: u64_at(rec, 24) as i64,
                    reason: u32_at(rec, 40),
                    attributes: u32_at(rec, 52),
                    name: utf16(n),
                })
            }
            3 if len >= 76 => {
                let name_len = u16_at(rec, 72) as usize;
                let name_off = u16_at(rec, 74) as usize;
                // 128-bit IDs; NTFS only uses the low 64 bits.
                rec.get(name_off..name_off + name_len).map(|n| UsnRecord {
                    file_ref: u64_at(rec, 8),
                    parent_ref: u64_at(rec, 24),
                    usn: u64_at(rec, 40) as i64,
                    reason: u32_at(rec, 56),
                    attributes: u32_at(rec, 68),
                    name: utf16(n),
                })
            }
            _ => None,
        };
        if let Some(r) = parsed {
            out.push(r);
        }
        off += len;
    }
    (next, out)
}

fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

fn open_volume(drive_root: &str) -> windows::core::Result<OwnedHandle> {
    let letter = drive_root.trim_end_matches('\\');
    let device = format!("\\\\.\\{letter}");
    let h = unsafe {
        CreateFileW(
            &HSTRING::from(device),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )?
    };
    Ok(OwnedHandle(h))
}

fn query_journal(vol: &OwnedHandle) -> windows::core::Result<UsnJournalData> {
    let mut data = UsnJournalData::default();
    let mut ret = 0u32;
    unsafe {
        DeviceIoControl(
            vol.0,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut data as *mut _ as *mut _),
            std::mem::size_of::<UsnJournalData>() as u32,
            Some(&mut ret),
            None,
        )?;
    }
    Ok(data)
}

/// Why the USN journal is or is not used for a root, for the UI.
#[derive(Debug, Clone)]
pub enum UsnAvailability {
    Available(UsnJournalData),
    NotDriveRoot,
    NotNtfs(String),
    NeedsAdmin,
    NoJournal(String),
}

pub fn probe(root: &str) -> UsnAvailability {
    if crate::platform::drive_root(root).as_deref() != Some(root) {
        return UsnAvailability::NotDriveRoot;
    }
    let fs = crate::platform::volume_info(root).map(|v| v.fs_name).unwrap_or_default();
    if !fs.eq_ignore_ascii_case("NTFS") && !fs.eq_ignore_ascii_case("ReFS") {
        return UsnAvailability::NotNtfs(fs);
    }
    let vol = match open_volume(root) {
        Ok(v) => v,
        Err(_) => return UsnAvailability::NeedsAdmin,
    };
    match query_journal(&vol) {
        Ok(j) => UsnAvailability::Available(j),
        Err(e) => UsnAvailability::NoJournal(e.message().to_string()),
    }
}

/// Resolves NTFS file references to current paths through the volume
/// handle, with a small cache for parent directories.
struct PathResolver {
    vol: OwnedHandle,
    cache: HashMap<u64, Option<String>>,
}

impl PathResolver {
    fn dir_path(&mut self, file_ref: u64) -> Option<String> {
        if let Some(p) = self.cache.get(&file_ref) {
            return p.clone();
        }
        let p = self.resolve(file_ref);
        if self.cache.len() > 50_000 {
            self.cache.clear();
        }
        self.cache.insert(file_ref, p.clone());
        p
    }

    fn resolve(&self, file_ref: u64) -> Option<String> {
        let desc = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: file_ref as i64 },
        };
        let h = unsafe {
            OpenFileById(
                self.vol.0,
                &desc,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
            .ok()?
        };
        let h = OwnedHandle(h);
        let mut buf = vec![0u16; 1024];
        loop {
            let n = unsafe { GetFinalPathNameByHandleW(h.0, &mut buf, FILE_NAME_NORMALIZED) } as usize;
            if n == 0 {
                return None;
            }
            if n < buf.len() {
                let s = String::from_utf16_lossy(&buf[..n]);
                let s = s.strip_prefix("\\\\?\\").unwrap_or(&s).to_string();
                return Some(util::normalize_dir(&s));
            }
            if buf.len() > 32_768 {
                return None;
            }
            buf.resize(n + 1, 0);
        }
    }

    fn invalidate(&mut self) {
        self.cache.clear();
    }
}

pub struct UsnWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub struct UsnStart {
    pub watcher: UsnWatcher,
    pub journal: UsnJournalData,
    /// The saved position could not be used (first run, journal recreated
    /// or wrapped): a full reconciliation scan is required.
    pub needs_full_scan: bool,
}

impl UsnWatcher {
    /// Starts reading the journal of `root` (a drive root). `resume` is the
    /// saved (journal id, next USN) from the last run.
    pub fn start(
        root: &str,
        resume: Option<(u64, i64)>,
        sink: impl Fn(WatchEvent) + Send + 'static,
    ) -> windows::core::Result<UsnStart> {
        let vol = open_volume(root)?;
        let journal = query_journal(&vol)?;
        let (start_usn, needs_full_scan) = match resume {
            Some((id, next)) if id == journal.journal_id && next >= journal.first_usn && next <= journal.next_usn => {
                (next, false)
            }
            _ => (journal.next_usn, true),
        };
        let resolver_vol = open_volume(root)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let root = root.to_string();
        let thread = std::thread::Builder::new()
            .name(format!("usn {root}"))
            .spawn(move || {
                let mut resolver = PathResolver { vol: resolver_vol, cache: HashMap::new() };
                run(&root, &vol, journal, start_usn, &mut resolver, &stop2, &sink);
            })
            .map_err(|e| windows::core::Error::new(HRESULT(-1), e.to_string()))?;
        Ok(UsnStart {
            watcher: UsnWatcher { stop, thread: Some(thread) },
            journal,
            needs_full_scan,
        })
    }
}

impl Drop for UsnWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    root: &str,
    vol: &OwnedHandle,
    mut journal: UsnJournalData,
    mut usn: i64,
    resolver: &mut PathResolver,
    stop: &AtomicBool,
    sink: &dyn Fn(WatchEvent),
) {
    let mut buf = vec![0u64; 64 * 1024 / 8];
    let mut rename_old: HashMap<u64, String> = HashMap::new();
    while !stop.load(Ordering::Relaxed) {
        let req = ReadUsnJournalData {
            start_usn: usn,
            reason_mask: u32::MAX,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            journal_id: journal.journal_id,
        };
        let mut ret = 0u32;
        let res = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_READ_USN_JOURNAL,
                Some(&req as *const _ as *const _),
                std::mem::size_of::<ReadUsnJournalData>() as u32,
                Some(buf.as_mut_ptr() as *mut _),
                (buf.len() * 8) as u32,
                Some(&mut ret),
                None,
            )
        };
        if let Err(e) = res {
            if e.code() == HRESULT::from_win32(ERROR_JOURNAL_ENTRY_DELETED) {
                // Wrapped while we were behind: resynchronize with a scan.
                match query_journal(vol) {
                    Ok(j) => {
                        journal = j;
                        usn = j.next_usn;
                        sink(WatchEvent::Overflow);
                        continue;
                    }
                    Err(e) => {
                        sink(WatchEvent::Failed(e.message().to_string()));
                        return;
                    }
                }
            }
            sink(WatchEvent::Failed(e.message().to_string()));
            return;
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, ret as usize) };
        let (next, records) = parse_records(bytes);
        if records.is_empty() {
            usn = next.max(usn);
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let mut changes = Vec::new();
        for r in &records {
            records_to_changes(root, r, resolver, &mut rename_old, &mut changes);
        }
        usn = next;
        if !changes.is_empty() {
            sink(WatchEvent::Changes(changes));
        }
        sink(WatchEvent::UsnCheckpoint { journal_id: journal.journal_id, next_usn: usn });
    }
}

fn records_to_changes(
    root: &str,
    r: &UsnRecord,
    resolver: &mut PathResolver,
    rename_old: &mut HashMap<u64, String>,
    out: &mut Vec<Change>,
) {
    let is_dir = r.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_dir && r.reason & (USN_REASON_RENAME_NEW_NAME | USN_REASON_FILE_DELETE) != 0 {
        // Cached directory paths below this one may be stale now.
        resolver.invalidate();
    }
    // A parent that no longer exists (tree deleted) cannot be resolved; the
    // parent's own delete record removes the whole subtree later.
    let Some(parent) = resolver.dir_path(r.parent_ref) else { return };
    if !crate::util::is_under(&parent, root) {
        return;
    }
    let path = util::join(&parent, &r.name);
    if r.reason & USN_REASON_RENAME_OLD_NAME != 0 {
        rename_old.insert(r.file_ref, path);
        return;
    }
    if r.reason & USN_REASON_RENAME_NEW_NAME != 0 {
        match rename_old.remove(&r.file_ref) {
            Some(from) if !from.eq_ignore_ascii_case(&path) => {
                out.push(Change::Rename { from, to: path })
            }
            _ => out.push(Change::Upsert(path)),
        }
        return;
    }
    if r.reason & USN_REASON_FILE_DELETE != 0 {
        out.push(Change::Remove(path));
    } else {
        out.push(Change::Upsert(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_record(file_ref: u64, parent: u64, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let name_units: Vec<u16> = name.encode_utf16().collect();
        let name_len = name_units.len() * 2;
        let len = (60 + name_len).div_ceil(8) * 8;
        let mut v = vec![0u8; len];
        v[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        v[4..6].copy_from_slice(&2u16.to_le_bytes());
        v[8..16].copy_from_slice(&file_ref.to_le_bytes());
        v[16..24].copy_from_slice(&parent.to_le_bytes());
        v[24..32].copy_from_slice(&usn.to_le_bytes());
        v[40..44].copy_from_slice(&reason.to_le_bytes());
        v[52..56].copy_from_slice(&attrs.to_le_bytes());
        v[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
        v[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in name_units.iter().enumerate() {
            v[60 + i * 2..62 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn parses_v2_records() {
        let mut buf = 777i64.to_le_bytes().to_vec();
        buf.extend(v2_record(5, 9, 100, USN_REASON_FILE_CREATE | USN_REASON_CLOSE, 0x20, "a.txt"));
        buf.extend(v2_record(6, 9, 120, USN_REASON_FILE_DELETE, 0x10, "dir"));
        let (next, recs) = parse_records(&buf);
        assert_eq!(next, 777);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].name, "a.txt");
        assert_eq!(recs[0].file_ref, 5);
        assert_eq!(recs[0].parent_ref, 9);
        assert_eq!(recs[1].reason, USN_REASON_FILE_DELETE);
        assert_eq!(recs[1].attributes, 0x10);
    }

    #[test]
    fn malformed_record_lengths_stop_parsing() {
        let mut buf = 1i64.to_le_bytes().to_vec();
        let mut r = v2_record(1, 2, 3, 0, 0, "x");
        r[0..4].copy_from_slice(&5000u32.to_le_bytes());
        buf.extend(r);
        assert!(parse_records(&buf).1.is_empty());
        assert!(parse_records(&[1, 2]).1.is_empty());
    }

    #[test]
    fn probe_reports_admin_requirement_or_availability() {
        // Without elevation the volume cannot be opened; with it, C: has a journal.
        match probe("C:\\") {
            UsnAvailability::NeedsAdmin => assert!(!crate::platform::is_elevated()),
            UsnAvailability::Available(j) => assert!(j.next_usn >= j.first_usn),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(probe("C:\\Windows"), UsnAvailability::NotDriveRoot));
    }
}
