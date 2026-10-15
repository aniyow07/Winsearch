//! Applying incremental filesystem changes to the index.
//!
//! Every change is treated as a hint, not as truth: the affected path is
//! re-examined on disk and the index is made to match it. That makes
//! applying a change idempotent and safe when notifications are duplicated,
//! reordered or describe a state that has already changed again.

use std::collections::HashSet;

use anyhow::Result;

use crate::db::Db;
use crate::model::FileRecord;
use crate::util;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    /// Something about this path changed (created, written, attributes...).
    Upsert(String),
    /// This path was deleted.
    Remove(String),
    /// A file or directory was renamed or moved within a watched tree.
    Rename { from: String, to: String },
    /// The direct children of this directory need re-listing.
    RescanDir(String),
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Upsert(p) | Change::Remove(p) | Change::RescanDir(p) => p,
            Change::Rename { to, .. } => to,
        }
    }
}

/// Where a path belongs: the indexed root that contains it, unless the
/// path is inside an excluded folder.
pub struct Scope<'a> {
    pub roots: &'a [(String, i64)],
    pub excludes: &'a [String],
}

impl Scope<'_> {
    pub fn root_for(&self, path: &str) -> Option<i64> {
        if self.excludes.iter().any(|x| util::is_under(path, x)) {
            return None;
        }
        self.roots
            .iter()
            .filter(|(r, _)| util::is_under(path, r) && !path.eq_ignore_ascii_case(r))
            .max_by_key(|(r, _)| r.len())
            .map(|(_, id)| *id)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ApplyStats {
    pub upserted: u64,
    pub removed: u64,
    pub renamed: u64,
}

/// Coalesces a burst of changes: drops exact duplicates while keeping the
/// order of first occurrence (order matters for renames).
pub fn coalesce(changes: Vec<Change>) -> Vec<Change> {
    let mut seen = HashSet::new();
    changes.into_iter().filter(|c| seen.insert(c.clone())).collect()
}

/// Applies changes inside a single transaction.
pub fn apply(db: &mut Db, scope: &Scope<'_>, changes: &[Change]) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    db.conn.execute_batch("BEGIN IMMEDIATE")?;
    let res = (|| -> Result<()> {
        for ch in changes {
            apply_one(db, scope, ch, &mut stats)?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            db.conn.execute_batch("COMMIT")?;
            Ok(stats)
        }
        Err(e) => {
            let _ = db.conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn apply_one(db: &mut Db, scope: &Scope<'_>, ch: &Change, stats: &mut ApplyStats) -> Result<()> {
    match ch {
        Change::Upsert(path) => refresh(db, scope, path, stats),
        Change::Remove(path) => {
            // The path may already exist again (delete + recreate).
            if std::fs::symlink_metadata(path).is_ok() {
                refresh(db, scope, path, stats)
            } else {
                stats.removed += db.remove_tree(path)?;
                Ok(())
            }
        }
        Change::Rename { from, to } => {
            let was_dir = db.get(from)?.map(|r| r.is_dir).unwrap_or(false);
            let to_root = scope.root_for(to);
            if let (true, Some(root_id)) = (was_dir, to_root) {
                stats.renamed += db.rename_tree(from, to, root_id)?;
            } else {
                stats.removed += db.remove_tree(from)?;
            }
            refresh(db, scope, to, stats)
        }
        Change::RescanDir(dir) => rescan_dir(db, scope, dir, stats),
    }
}

/// Makes the index entry for `path` match the disk.
fn refresh(db: &mut Db, scope: &Scope<'_>, path: &str, stats: &mut ApplyStats) -> Result<()> {
    let Some(root_id) = scope.root_for(path) else {
        // Outside every root or excluded: make sure nothing lingers.
        stats.removed += db.remove_tree(path)?;
        return Ok(());
    };
    match FileRecord::from_path(path) {
        Ok(rec) => {
            let known = db.get(path)?;
            db.upsert_one(root_id, &rec)?;
            stats.upserted += 1;
            // A directory that is new to the index (created, or moved in
            // from outside the watched tree) arrives as one notification;
            // its contents must be enumerated.
            if rec.is_dir && !rec.is_reparse_point() && known.is_none() {
                index_subtree(db, scope, root_id, path, stats)?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            stats.removed += db.remove_tree(path)?;
            Ok(())
        }
        Err(e) => {
            // Access denied etc.: keep what we had, it may still be valid.
            tracing::debug!("cannot stat {path}: {e}");
            Ok(())
        }
    }
}

/// Re-lists one directory and reconciles its direct children.
fn rescan_dir(db: &mut Db, scope: &Scope<'_>, dir: &str, stats: &mut ApplyStats) -> Result<()> {
    let Some(_root_id) = scope.root_for(dir).or_else(|| {
        scope.roots.iter().find(|(r, _)| r.eq_ignore_ascii_case(dir)).map(|(_, id)| *id)
    }) else {
        return Ok(());
    };
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            stats.removed += db.remove_tree(dir)?;
            return Ok(());
        }
        Err(e) => {
            tracing::debug!("cannot list {dir}: {e}");
            return Ok(());
        }
    };
    let mut on_disk = HashSet::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = util::join(dir, &name);
        on_disk.insert(path.to_lowercase());
        refresh(db, scope, &path, stats)?;
    }
    for child in db.children(dir)? {
        if !on_disk.contains(&child.path.to_lowercase()) {
            stats.removed += db.remove_tree(&child.path)?;
        }
    }
    Ok(())
}

/// Recursively indexes a directory that was not known before. Does not
/// follow links (same rule as the full scanner).
fn index_subtree(
    db: &mut Db,
    scope: &Scope<'_>,
    root_id: i64,
    dir: &str,
    stats: &mut ApplyStats,
) -> Result<()> {
    let mut stack = vec![dir.to_string()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(md) = entry.metadata() else { continue };
            let rec = FileRecord::from_metadata(&d, &name, &md);
            if scope.root_for(&rec.path).is_none() {
                continue;
            }
            let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(true);
            if rec.is_dir && !is_link {
                stack.push(rec.path.clone());
            }
            db.upsert_one(root_id, &rec)?;
            stats.upserted += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_picks_deepest_root_and_honours_excludes() {
        let roots = vec![("C:\\".to_string(), 1), ("C:\\Data".to_string(), 2)];
        let excludes = vec!["C:\\Data\\tmp".to_string()];
        let s = Scope { roots: &roots, excludes: &excludes };
        assert_eq!(s.root_for("C:\\x.txt"), Some(1));
        assert_eq!(s.root_for("C:\\Data\\a.txt"), Some(2));
        assert_eq!(s.root_for("C:\\Data\\tmp\\a.txt"), None);
        assert_eq!(s.root_for("D:\\a.txt"), None);
    }

    #[test]
    fn coalesce_keeps_first_order() {
        let c = coalesce(vec![
            Change::Upsert("a".into()),
            Change::Remove("b".into()),
            Change::Upsert("a".into()),
        ]);
        assert_eq!(c, vec![Change::Upsert("a".into()), Change::Remove("b".into())]);
    }
}
