//! Thin wrappers around the Win32 calls the application needs.
//! Everything here is safe to call from any thread and never panics on
//! Win32 failures; errors are returned or mapped to `None`.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetDriveTypeW, GetFileInformationByHandle, GetLogicalDrives,
    GetVolumeInformationW, OPEN_EXISTING,
};
use windows::core::HSTRING;

/// Owned Win32 handle closed on drop.
pub struct OwnedHandle(pub HANDLE);

// Kernel handles may be used and closed from any thread.
unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Opens a file or directory for attribute queries only, without following
/// a final symlink/junction and without needing read access to its content.
pub fn open_for_attributes(path: &str) -> windows::core::Result<OwnedHandle> {
    let h = unsafe {
        CreateFileW(
            &HSTRING::from(path),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )?
    };
    Ok(OwnedHandle(h))
}

/// NTFS file reference number (64-bit file index) of a path, if obtainable.
pub fn file_reference(path: &str) -> Option<u64> {
    let h = open_for_attributes(path).ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(h.0, &mut info).ok()? };
    Some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    CdRom,
    RamDisk,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub root: String,
    pub kind: DriveKind,
    pub label: String,
    pub fs_name: String,
    pub serial: Option<u32>,
    pub ready: bool,
}

impl DriveInfo {
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            DriveKind::Fixed => "Local disk",
            DriveKind::Removable => "Removable",
            DriveKind::Network => "Network",
            DriveKind::CdRom => "Optical",
            DriveKind::RamDisk => "RAM disk",
            DriveKind::Unknown => "Drive",
        };
        if !self.ready {
            return format!("{} ({kind}, not ready)", self.root);
        }
        let label = if self.label.is_empty() { kind.to_string() } else { self.label.clone() };
        format!("{} {label} [{}]", self.root, self.fs_name)
    }
}

pub struct VolumeInfo {
    pub serial: u32,
    pub fs_name: String,
    pub label: String,
}

pub fn volume_info(root: &str) -> Option<VolumeInfo> {
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    unsafe {
        GetVolumeInformationW(
            &HSTRING::from(root),
            Some(&mut label),
            Some(&mut serial),
            None,
            None,
            Some(&mut fs),
        )
        .ok()?;
    }
    Some(VolumeInfo {
        serial,
        fs_name: wide_to_string(&fs),
        label: wide_to_string(&label),
    })
}

/// The drive root ("C:\") of a path, if it has a drive letter.
pub fn drive_root(path: &str) -> Option<String> {
    let b = path.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        Some(format!("{}:\\", (b[0] as char).to_ascii_uppercase()))
    } else {
        None
    }
}

pub fn list_drives() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let kind = match unsafe { GetDriveTypeW(&HSTRING::from(root.as_str())) } {
            2 => DriveKind::Removable,
            3 => DriveKind::Fixed,
            4 => DriveKind::Network,
            5 => DriveKind::CdRom,
            6 => DriveKind::RamDisk,
            _ => DriveKind::Unknown,
        };
        let vol = volume_info(&root);
        out.push(DriveInfo {
            root,
            kind,
            ready: vol.is_some(),
            label: vol.as_ref().map(|v| v.label.clone()).unwrap_or_default(),
            fs_name: vol.as_ref().map(|v| v.fs_name.clone()).unwrap_or_default(),
            serial: vol.map(|v| v.serial),
        });
    }
    out
}

pub fn is_elevated() -> bool {
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Initializes COM (STA) for the current thread for the lifetime of the guard.
pub struct ComGuard(bool);

impl ComGuard {
    pub fn sta() -> ComGuard {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
        };
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        ComGuard(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Opens a file with its default application (same as double-clicking it in
/// Explorer). Runs synchronously; call from a background thread.
pub fn open_with_default_app(path: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let _com = ComGuard::sta();
    let r = unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &HSTRING::from(path),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows could not open the file (code {})", r.0 as isize))
    }
}

/// Opens Explorer on the containing folder with the item selected.
pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};
    let _com = ComGuard::sta();
    unsafe {
        let pidl = ILCreateFromPathW(&HSTRING::from(path));
        if pidl.is_null() {
            return Err("the item no longer exists or cannot be shown".into());
        }
        let res = SHOpenFolderAndSelectItems(pidl, None, 0);
        ILFree(Some(pidl));
        res.map_err(|e| e.message().to_string())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryUsage {
    pub working_set: u64,
    pub private_bytes: u64,
}

pub fn process_memory() -> MemoryUsage {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_err() {
        return MemoryUsage::default();
    }
    MemoryUsage { working_set: c.WorkingSetSize as u64, private_bytes: c.PrivateUsage as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_root_parsing() {
        assert_eq!(drive_root("c:\\x\\y").as_deref(), Some("C:\\"));
        assert_eq!(drive_root("\\\\server\\share"), None);
    }

    #[test]
    fn file_ref_of_temp_file() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ws_ref_{}.tmp", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        let r1 = file_reference(p.to_str().unwrap());
        assert!(r1.is_some());
        let p2 = dir.join(format!("ws_ref2_{}.tmp", std::process::id()));
        std::fs::rename(&p, &p2).unwrap();
        // A rename keeps the NTFS file identity.
        assert_eq!(file_reference(p2.to_str().unwrap()), r1);
        std::fs::remove_file(p2).unwrap();
    }
}
