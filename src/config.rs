//! User settings stored as JSON next to the index in %LOCALAPPDATA%\WinSearch.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    /// Folders or drive roots to index (normalized paths).
    pub roots: Vec<String>,
    /// Folders skipped during indexing, including everything below them.
    pub excludes: Vec<String>,
    /// Maximum rows fetched per query (the list is virtualized).
    pub result_limit: usize,
    /// Without the USN journal, changes made while the app was closed can
    /// only be found by rescanning. Runs in the background.
    pub reconcile_on_startup: bool,
    /// Periodic reconciliation rescan, in hours (0 disables).
    pub reconcile_interval_hours: u32,
    /// Show entries with the hidden or system attribute in results.
    pub show_hidden: bool,
    pub scan_threads: usize,
    /// Hide system, app-data and build/cache folders from results (they
    /// stay indexed; the toggle beside the search box shows them again).
    pub hide_clutter: bool,
    /// Folders treated as clutter: absolute paths or folder names.
    pub clutter_folders: Vec<String>,
    /// Folders recently picked as search scope (most recent first).
    pub recent_scopes: Vec<String>,
}

pub fn default_clutter_folders() -> Vec<String> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let progdata = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    let mut v = vec![
        ".*".to_string(),
        "AppData".to_string(),
        util::normalize_dir(&windir),
        util::normalize_dir(&progdata),
        "$Recycle.Bin".into(),
        "System Volume Information".into(),
        "node_modules".into(),
        "__pycache__".into(),
        "site-packages".into(),
    ];
    if let Some(root) = crate::platform::drive_root(&windir) {
        v.push(format!("{root}Windows.old"));
        v.push(format!("{root}$WinREAgent"));
        v.push(format!("{root}Recovery"));
    }
    v
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            roots: Vec::new(),
            excludes: Vec::new(),
            result_limit: 5000,
            reconcile_on_startup: true,
            reconcile_interval_hours: 6,
            show_hidden: true,
            scan_threads: crate::scanner::default_threads(),
            hide_clutter: true,
            clutter_folders: default_clutter_folders(),
            recent_scopes: Vec::new(),
        }
    }
}

impl AppConfig {
    /// First-run defaults: every ready local fixed drive, minus folders that
    /// are never useful search results.
    pub fn first_run() -> AppConfig {
        let mut cfg = AppConfig::default();
        for d in crate::platform::list_drives() {
            if d.kind == crate::platform::DriveKind::Fixed && d.ready {
                cfg.excludes.push(format!("{}$Recycle.Bin", d.root));
                cfg.excludes.push(format!("{}System Volume Information", d.root));
                cfg.roots.push(d.root);
            }
        }
        cfg
    }

    pub fn load() -> AppConfig {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<AppConfig>(&text) {
                Ok(mut cfg) => {
                    cfg.normalize();
                    cfg
                }
                Err(e) => {
                    tracing::error!("settings file {} is invalid ({e}); using defaults", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("json.bad"));
                    AppConfig::first_run()
                }
            },
            Err(_) => AppConfig::first_run(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    pub fn normalize(&mut self) {
        self.recent_scopes.truncate(8);
        for list in [&mut self.roots, &mut self.excludes] {
            for p in list.iter_mut() {
                *p = util::normalize_dir(p);
            }
            list.retain(|p| !p.is_empty());
            list.sort_by_key(|p| p.to_lowercase());
            list.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        }
        self.result_limit = self.result_limit.clamp(100, 100_000);
        self.scan_threads = self.scan_threads.clamp(1, 32);
    }
}

/// Data directory: `%WINSEARCH_DATA_DIR%` if set, else `%LOCALAPPDATA%\WinSearch`.
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("WINSEARCH_DATA_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("WinSearch")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn default_db_path() -> PathBuf {
    data_dir().join("index.db")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Human-readable label for an entry of the clutter list.
pub fn clutter_label(entry: &str) -> String {
    if entry == ".*" {
        "Dot-folders and dot-files (.git, .vscode, .agents…)".to_string()
    } else if entry.contains(':') {
        entry.to_string()
    } else {
        format!("Any folder named “{entry}”")
    }
}
