//! File operations started from the UI. Currently: renaming one item.

use crate::util;

/// Characters Windows does not allow in file and folder names.
const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Checks a new name typed by the user: returns it unchanged when Windows
/// accepts it, or a message saying what is wrong with it.
pub fn validate_name(new_name: &str) -> Result<&str, String> {
    if new_name.trim().is_empty() {
        return Err("The name cannot be empty.".into());
    }
    if let Some(c) = new_name.chars().find(|c| INVALID_CHARS.contains(c) || (*c as u32) < 32) {
        let shown = if (c as u32) < 32 { "control characters".to_string() } else { format!("“{c}”") };
        return Err(format!("A name cannot contain {shown}. Not allowed: < > : \" / \\ | ? *"));
    }
    if new_name == "." || new_name == ".." {
        return Err("That name is reserved by Windows.".into());
    }
    if new_name.ends_with('.') || new_name.ends_with(' ') {
        return Err("A name cannot end with a dot or a space.".into());
    }
    if new_name.starts_with(' ') {
        return Err("A name cannot start with a space.".into());
    }
    let stem = new_name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err(format!("“{stem}” is a device name reserved by Windows."));
    }
    if new_name.encode_utf16().count() > 255 {
        return Err("The name is too long (at most 255 characters).".into());
    }
    Ok(new_name)
}

/// Renames the file or folder at `path` to `new_name` in the same folder.
/// Never replaces an existing item. Returns the new full path.
pub fn rename(path: &str, new_name: &str) -> Result<String, String> {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
        ERROR_INVALID_NAME, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, ERROR_WRITE_PROTECT,
    };
    use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
    use windows::core::HSTRING;

    let new_name = validate_name(new_name)?;
    let Some((parent, _)) = util::split_path(path) else {
        return Err("A drive cannot be renamed here.".into());
    };
    let target = util::join(parent, new_name);
    // No MOVEFILE_REPLACE_EXISTING: an existing item makes the call fail
    // instead of being overwritten. Case-only changes are allowed.
    let res = unsafe { MoveFileExW(&HSTRING::from(path), &HSTRING::from(target.as_str()), MOVE_FILE_FLAGS(0)) };
    match res {
        Ok(()) => Ok(target),
        Err(e) => {
            let code = e.code();
            let msg = if code == ERROR_ALREADY_EXISTS.to_hresult() || code == ERROR_FILE_EXISTS.to_hresult() {
                format!("Something named “{new_name}” already exists in this folder.")
            } else if code == ERROR_ACCESS_DENIED.to_hresult() || code == ERROR_WRITE_PROTECT.to_hresult() {
                "Access denied. You may not have permission to rename this, or it is read-only or in use.".to_string()
            } else if code == ERROR_SHARING_VIOLATION.to_hresult() {
                "The file is open in another program. Close it and try again.".to_string()
            } else if code == ERROR_FILE_NOT_FOUND.to_hresult() || code == ERROR_PATH_NOT_FOUND.to_hresult() {
                "The item no longer exists (it was moved or deleted).".to_string()
            } else if code == ERROR_INVALID_NAME.to_hresult() {
                "Windows does not accept this name.".to_string()
            } else {
                format!("Could not rename: {}", e.message())
            };
            Err(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert!(validate_name("report 2024.pdf").is_ok());
        assert!(validate_name(".gitignore").is_ok());
        assert!(validate_name("COM10.txt").is_ok());
        assert!(validate_name("console.txt").is_ok());
        for bad in ["", "   ", "a<b", "a:b", "a/b", "a\\b", "what?", "star*", "pipe|", "q\"", "tab\t", "end.", "end ", " lead", ".", "..", "CON", "nul.txt", "com1", "LPT9.log", "Aux .txt"] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_name(&"x".repeat(256)).is_err());
        assert!(validate_name(&"x".repeat(255)).is_ok());
    }

    #[test]
    fn renames_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let a = format!("{d}\\a.txt");
        let b = format!("{d}\\b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let err = rename(&a, "b.txt").unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read(&b).unwrap(), b"b"); // untouched

        assert!(rename(&a, "bad?.txt").is_err());
        assert!(std::path::Path::new(&a).exists());

        let c = rename(&a, "c.txt").unwrap();
        assert_eq!(c, format!("{d}\\c.txt"));
        assert!(!std::path::Path::new(&a).exists());
        assert_eq!(std::fs::read(&c).unwrap(), b"a");

        // Changing only the letter case works.
        let upper = rename(&c, "C.TXT").unwrap();
        let listed: Vec<String> =
            std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(listed.contains(&"C.TXT".to_string()), "{listed:?}");
        assert!(upper.ends_with("C.TXT"));

        let gone = rename(&format!("{d}\\missing.txt"), "x.txt").unwrap_err();
        assert!(gone.contains("no longer exists"), "{gone}");
    }
}
