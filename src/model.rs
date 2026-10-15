//! Plain data types shared between the index, the scanners and the UI.

use crate::util;

/// One file or directory as stored in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub name: String,
    pub parent: String,
    pub ext: String,
    pub is_dir: bool,
    pub size: u64,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub attrs: u32,
    /// NTFS file reference number when known (USN journal or a handle query).
    pub file_ref: Option<u64>,
}

pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

impl FileRecord {
    /// Builds a record from std metadata. On Windows, metadata that comes from
    /// a directory listing is served from the FindNextFile data, so this does
    /// not touch the file itself.
    pub fn from_metadata(parent: &str, name: &str, md: &std::fs::Metadata) -> FileRecord {
        use std::os::windows::fs::MetadataExt;
        let is_dir = md.is_dir();
        FileRecord {
            path: util::join(parent, name),
            name: name.to_string(),
            parent: parent.to_string(),
            ext: util::ext_of(name, is_dir),
            is_dir,
            size: if is_dir { 0 } else { md.file_size() },
            created: util::filetime_to_unix(md.creation_time()),
            modified: util::filetime_to_unix(md.last_write_time()),
            attrs: md.file_attributes(),
            file_ref: None,
        }
    }

    /// Stats a single path (without following a final symlink/junction).
    pub fn from_path(path: &str) -> std::io::Result<FileRecord> {
        let md = std::fs::symlink_metadata(path)?;
        let (parent, name) = util::split_path(path).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?;
        let mut rec = FileRecord::from_metadata(parent, name, &md);
        rec.file_ref = crate::platform::file_reference(path);
        Ok(rec)
    }

    pub fn is_reparse_point(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

/// A search hit as shown in the result list.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub parent: String,
    pub ext: String,
    pub is_dir: bool,
    pub size: u64,
    pub created: Option<i64>,
    pub modified: Option<i64>,
}
