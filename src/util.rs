//! Small path, time and formatting helpers shared by the engine and the UI.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01 (Unix epoch).
const FILETIME_UNIX_DIFF_SECS: i64 = 11_644_473_600;

/// Converts a Windows FILETIME (100ns ticks since 1601) to Unix seconds.
/// Returns `None` for the zero FILETIME, which Windows uses for "unknown".
pub fn filetime_to_unix(ft: u64) -> Option<i64> {
    if ft == 0 {
        return None;
    }
    Some((ft / 10_000_000) as i64 - FILETIME_UNIX_DIFF_SECS)
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Normalizes a user-supplied directory path to the form stored in the index:
/// backslash separators, no trailing separator except for drive roots (`C:\`),
/// and an upper-case drive letter.
pub fn normalize_dir(p: &str) -> String {
    let mut s: String = p.trim().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    if s.len() == 2 && s.as_bytes()[1] == b':' {
        s.push('\\');
    }
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        let first = s[..1].to_ascii_uppercase();
        s.replace_range(..1, &first);
    }
    s
}

/// Joins a directory path and a child name using a single backslash.
pub fn join(dir: &str, name: &str) -> String {
    let mut s = String::with_capacity(dir.len() + name.len() + 1);
    s.push_str(dir);
    if !dir.ends_with('\\') {
        s.push('\\');
    }
    s.push_str(name);
    s
}

/// Splits a stored path into (parent, name). A drive root has no parent.
pub fn split_path(path: &str) -> Option<(&str, &str)> {
    let idx = path.rfind('\\')?;
    let name = &path[idx + 1..];
    if name.is_empty() {
        return None;
    }
    // Keep the trailing backslash for drive roots: parent of "C:\x" is "C:\".
    let parent = if idx == 2 && path.as_bytes()[1] == b':' {
        &path[..3]
    } else {
        &path[..idx]
    };
    Some((parent, name))
}

/// Lower-cased extension without the dot. Directories and dot-files such as
/// `.gitignore` have no extension.
pub fn ext_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) if i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// Case-insensitive "path is equal to or inside prefix" check on normalized paths.
pub fn is_under(path: &str, prefix: &str) -> bool {
    let p = path.to_lowercase();
    let q = prefix.to_lowercase();
    if !p.starts_with(&q) {
        return false;
    }
    let rest = &p[q.len()..];
    rest.is_empty() || q.ends_with('\\') || rest.starts_with('\\')
}

/// Escapes `%`, `_` and the escape character itself for a SQL `LIKE ... ESCAPE '^'`.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '^') {
            out.push('^');
        }
        out.push(c);
    }
    out
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Formats Unix seconds as local "YYYY-MM-DD HH:MM".
pub fn format_time(unix: Option<i64>) -> String {
    use chrono::{Local, TimeZone};
    match unix.and_then(|t| Local.timestamp_opt(t, 0).single()) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => String::from("-"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_split() {
        assert_eq!(normalize_dir("c:"), "C:\\");
        assert_eq!(normalize_dir("c:/Users/x/"), "C:\\Users\\x");
        assert_eq!(split_path("C:\\a"), Some(("C:\\", "a")));
        assert_eq!(split_path("C:\\a\\b.txt"), Some(("C:\\a", "b.txt")));
        assert_eq!(split_path("C:\\"), None);
        assert_eq!(join("C:\\", "a"), "C:\\a");
        assert_eq!(join("C:\\a", "b"), "C:\\a\\b");
    }

    #[test]
    fn extensions() {
        assert_eq!(ext_of("Report.PDF", false), "pdf");
        assert_eq!(ext_of(".gitignore", false), "");
        assert_eq!(ext_of("archive.tar.gz", false), "gz");
        assert_eq!(ext_of("trailingdot.", false), "");
        assert_eq!(ext_of("folder.d", true), "");
    }

    #[test]
    fn under() {
        assert!(is_under("C:\\Windows\\System32", "C:\\windows"));
        assert!(is_under("C:\\Windows", "C:\\Windows"));
        assert!(!is_under("C:\\WindowsApps", "C:\\Windows"));
        assert!(is_under("C:\\x", "C:\\"));
    }

    #[test]
    fn like_escape() {
        assert_eq!(escape_like("50%_a^b"), "50^%^_a^^b");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(10), "10 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_count(1234567), "1,234,567");
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), Some(0));
        assert_eq!(filetime_to_unix(0), None);
    }
}
