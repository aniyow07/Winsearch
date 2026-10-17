//! Crash reports. A Rust panic on the UI thread, or a native crash anywhere
//! in the process (for example inside a third-party DLL that Windows loaded
//! into WinSearch), writes `winsearch-crash-<time>.txt` next to the
//! executable, or into the log folder when that is not writable. Panics on
//! background threads are written to the regular log; the threads that
//! matter catch them and report the failure in the window.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use parking_lot::Mutex;

static CRASH_DIR: OnceLock<PathBuf> = OnceLock::new();
static WINDOWS_VERSION: OnceLock<String> = OnceLock::new();
/// What the user was doing last (selected file, open dialog...). Included in
/// crash reports so a crash can be tied to a file or an action.
static LAST_ACTION: Mutex<String> = Mutex::new(String::new());

pub fn install() {
    CRASH_DIR.get_or_init(choose_dir);
    WINDOWS_VERSION.get_or_init(windows_version);
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(thread = %name, "panic: {info}\n{backtrace}");
        if name != "main" {
            return;
        }
        let mut text = header("Internal error (Rust panic) on the UI thread");
        let _ = writeln!(text, "Message: {info}\n\nBacktrace:\n{backtrace}");
        let file = write_report(&text);
        let where_ = file.map(|p| format!("A crash report was saved to:\n{}", p.display())).unwrap_or_default();
        crate::show_fatal(&format!(
            "WinSearch hit an internal error and has to close.\n\n{where_}\n\nPlease send that file together with what you were doing when it happened."
        ));
    }));
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;
        SetUnhandledExceptionFilter(Some(native_crash));
    }
}

/// Records the user's current action for crash reports.
pub fn note(action: impl Into<String>) {
    *LAST_ACTION.lock() = action.into();
}

fn header(kind: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "WinSearch {} crash report", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(s, "Time: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z"));
    let _ = writeln!(s, "Kind: {kind}");
    let _ = writeln!(s, "Windows: {}", WINDOWS_VERSION.get().map_or("unknown", String::as_str));
    let last = LAST_ACTION.try_lock().map(|a| a.clone()).unwrap_or_else(|| "(unavailable)".into());
    let _ = writeln!(s, "Last action: {}", if last.is_empty() { "(none)" } else { &last });
    s.push('\n');
    s
}

fn write_report(text: &str) -> Option<PathBuf> {
    let dir = CRASH_DIR.get()?;
    let name = format!("winsearch-crash-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(name);
    std::fs::write(&path, text).ok()?;
    Some(path)
}

/// Next to the executable when the user can write there (portable use),
/// otherwise the log folder in %LOCALAPPDATA%.
fn choose_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)) {
        let probe = dir.join(".winsearch-write-test");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return dir;
        }
    }
    let logs = winsearch::config::log_dir();
    let _ = std::fs::create_dir_all(&logs);
    logs
}

/// "Windows 10.0 build 26100.4652 (24H2)", read once from the registry.
fn windows_version() -> String {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::HSTRING;
    let key = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let read_sz = |name: &str| -> String {
        let mut buf = [0u16; 128];
        let mut len = (buf.len() * 2) as u32;
        let ok = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, &key, &HSTRING::from(name), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len))
        };
        if ok.is_err() {
            return String::new();
        }
        let chars = (len as usize / 2).saturating_sub(1).min(buf.len());
        String::from_utf16_lossy(&buf[..chars])
    };
    let mut ubr = 0u32;
    let mut len = 4u32;
    let _ = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, &key, &HSTRING::from("UBR"), RRF_RT_REG_DWORD, None, Some((&mut ubr as *mut u32).cast()), Some(&mut len))
    };
    format!("10.0 build {}.{ubr} ({})", read_sz("CurrentBuild"), read_sz("DisplayVersion"))
}

/// Last-chance handler for native crashes (access violations and the like).
/// Writes a report naming the module that crashed, then lets Windows
/// continue its normal crash handling.
unsafe extern "system" fn native_crash(
    info: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    let (code, addr) = unsafe {
        match info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
            Some(r) => (r.ExceptionCode.0 as u32, r.ExceptionAddress as usize),
            None => (0, 0),
        }
    };
    let mut text = header("Native crash");
    let _ = writeln!(text, "Exception code: 0x{code:08X} ({})", exception_name(code));
    let _ = writeln!(text, "Address: 0x{addr:016X}");
    let _ = writeln!(text, "Module: {}", module_at(addr));
    let _ = writeln!(
        text,
        "\nIf the module above is not winsearch.exe, the crash happened inside another program's\n\
         component that Windows loaded into WinSearch (for example a shell extension)."
    );
    let _ = write_report(&text);
    EXCEPTION_CONTINUE_SEARCH
}

fn exception_name(code: u32) -> &'static str {
    match code {
        0xC000_0005 => "access violation",
        0xC000_0409 => "stack buffer overrun / fail-fast",
        0xC000_00FD => "stack overflow",
        0xC000_001D => "illegal instruction",
        0xC000_0374 => "heap corruption",
        0xC000_0094 => "integer division by zero",
        0xE06D_7363 => "C++ exception",
        _ => "other",
    }
}

/// Full path of the DLL or EXE containing `addr`, with the offset into it.
fn module_at(addr: usize) -> String {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    use windows::core::PCWSTR;
    let mut module = HMODULE::default();
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut module,
        )
    };
    if found.is_err() || module.is_invalid() {
        return "unknown".into();
    }
    let mut buf = [0u16; 520];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let offset = addr.wrapping_sub(module.0 as usize);
    format!("{} +0x{offset:X}", String::from_utf16_lossy(&buf[..n]))
}
