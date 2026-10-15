//! Query parsing and SQL search over the index.
//!
//! Name terms of three or more characters go through the FTS5 trigram
//! index (substring match, case-insensitive); shorter terms and path terms
//! are applied as LIKE filters. See docs/BENCHMARKS.md for measurements.

use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, ToSql};

use crate::model::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM, SearchHit};
use crate::util;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KindFilter {
    #[default]
    Any,
    FilesOnly,
    FoldersOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    Modified,
    Extension,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// Lower-cased substrings that must all occur in the file name.
    pub name_terms: Vec<String>,
    /// Lower-cased substrings that must occur in the full path.
    pub path_terms: Vec<String>,
    /// Allowed extensions (lower-case, no dot). Empty = any.
    pub exts: Vec<String>,
    pub kind: KindFilter,
    pub sort: SortKey,
    pub descending: bool,
    pub show_hidden: bool,
    /// Only entries inside this folder.
    pub scope: Option<String>,
    /// Folders hidden from results: absolute paths (`C:\Windows`) or
    /// folder names matched anywhere (`AppData`, `node_modules`).
    pub exclude_folders: Vec<String>,
    /// Entries under this folder rank higher (the user's profile).
    pub boost_prefix: Option<String>,
}

/// Count matches up to this many; beyond that the UI shows "N+".
pub const COUNT_CAP: usize = 100_000;

impl Query {
    /// Parses the search box text.
    ///
    /// * `foo bar`        — names containing both "foo" and "bar"
    /// * `"two words"`    — a term containing a space
    /// * `src\main`       — a term with `\` or `/` matches the full path
    /// * `path:proj`      — explicit full-path term
    /// * `ext:pdf,docx` or `*.pdf` — extension filter
    /// * `folder:` / `file:` — only folders / only files
    pub fn parse(text: &str) -> Query {
        let mut q = Query { show_hidden: true, ..Default::default() };
        for tok in tokenize(text) {
            let lower = tok.to_lowercase();
            if let Some(rest) = strip_prefix_ci(&lower, "ext:") {
                q.exts.extend(
                    rest.split([',', ';', '|'])
                        .map(|e| e.trim().trim_start_matches('.').to_string())
                        .filter(|e| !e.is_empty()),
                );
            } else if let Some(rest) = strip_prefix_ci(&lower, "path:") {
                if !rest.is_empty() {
                    q.path_terms.push(rest.replace('/', "\\"));
                }
            } else if lower == "folder:" || lower == "dir:" || lower == "folders:" {
                q.kind = KindFilter::FoldersOnly;
            } else if lower == "file:" || lower == "files:" {
                q.kind = KindFilter::FilesOnly;
            } else if let Some(ext) = lower.strip_prefix("*.") {
                if !ext.is_empty() && !ext.contains(['*', '?', '\\', '/']) {
                    q.exts.push(ext.to_string());
                } else {
                    q.name_terms.push(lower.replace('*', ""));
                }
            } else if lower.contains(['\\', '/']) {
                q.path_terms.push(lower.replace('/', "\\"));
            } else {
                let t = lower.replace('*', "");
                if !t.is_empty() {
                    q.name_terms.push(t);
                }
            }
        }
        q.exts.sort();
        q.exts.dedup();
        q
    }

    pub fn is_empty(&self) -> bool {
        self.name_terms.is_empty() && self.path_terms.is_empty() && self.exts.is_empty()
            && self.kind == KindFilter::Any
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.strip_prefix(prefix)
}

fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in text.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Debug, Default)]
pub struct SearchResults {
    /// The trigram index is being rebuilt; slower LIKE matching was used.
    pub degraded: bool,
    pub hits: Vec<SearchHit>,
    /// Number of matches, capped at COUNT_CAP.
    pub total: usize,
    pub capped: bool,
    pub elapsed: Duration,
}

pub struct SearchEngine {
    conn: Connection,
}

fn fts_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Trigram FTS needs at least 3 characters per term.
fn fts_usable(term: &str) -> bool {
    term.chars().count() >= 3
}

impl SearchEngine {
    pub fn new(conn: Connection) -> SearchEngine {
        SearchEngine { conn }
    }

    pub fn search(&self, q: &Query, limit: usize) -> Result<SearchResults> {
        let started = Instant::now();
        if q.is_empty() {
            return Ok(SearchResults { elapsed: started.elapsed(), ..Default::default() });
        }
        let fts_ok = !crate::db::fts_stale(&self.conn);
        let has_long = fts_ok && q.name_terms.iter().any(|t| fts_usable(t));
        let mut res = if !has_long && !q.name_terms.is_empty() && q.sort == SortKey::Relevance {
            self.search_short(q, limit)?
        } else {
            self.search_sql(q, limit, fts_ok)?
        };
        res.degraded = !fts_ok;
        res.elapsed = started.elapsed();
        Ok(res)
    }

    /// General path: trigram MATCH for long name terms, LIKE for the rest,
    /// ORDER BY + LIMIT in SQLite, and a capped count.
    fn search_sql(&self, q: &Query, limit: usize, fts_ok: bool) -> Result<SearchResults> {
        let mut sql = SqlParts::default();
        let fts_terms: Vec<&String> =
            q.name_terms.iter().filter(|t| fts_ok && fts_usable(t)).collect();
        let from = if fts_terms.is_empty() {
            "files f"
        } else {
            let expr = fts_terms.iter().map(|t| fts_phrase(t)).collect::<Vec<_>>().join(" AND ");
            sql.cond("names_fts MATCH ?#", expr);
            "names_fts JOIN files f ON f.id = names_fts.rowid"
        };
        for t in q.name_terms.iter().filter(|t| !(fts_ok && fts_usable(t))) {
            sql.name_contains(t);
        }
        sql.filters(q);
        let where_sql = sql.where_sql();
        let filter_params = sql.params.len();

        // Relevance: exact name, then prefix, then the user's own files,
        // then earliest/shortest match.
        let boost = if q.name_terms.is_empty() || q.sort != SortKey::Relevance { String::new() } else { sql.boost(q) };
        let order = match q.sort {
            SortKey::Relevance => match q.name_terms.first() {
                Some(first) => {
                    let p = sql.param(first.clone());
                    format!(
                        "CASE WHEN lower(f.name) = ?{p} THEN 0
                              WHEN lower(f.name) LIKE ?{p} || '%' THEN 1
                              ELSE 2 END,
                         {boost} instr(lower(f.name), ?{p}), f.is_dir DESC, length(f.name), length(f.path)"
                    )
                }
                // No name to rank by (type filter only): newest first, which
                // the modified-time index serves without sorting everything.
                None => "f.modified DESC".to_string(),
            },
            SortKey::Name => format!("f.name {}", dir(q.descending)),
            SortKey::Path => format!("f.path {}", dir(q.descending)),
            SortKey::Size => format!("f.size {}", dir(q.descending)),
            SortKey::Modified => format!("f.modified {}", dir(q.descending)),
            SortKey::Extension => format!("f.ext {}, f.name", dir(q.descending)),
        };

        let select = format!("{SELECT_COLS} FROM {from} {where_sql} ORDER BY {order} LIMIT {limit}");
        let refs = sql.refs();
        let hits = self.query_hits(&select, &refs)?;

        let (total, capped) = if hits.len() < limit {
            (hits.len(), false)
        } else {
            // Count separately, bounded so huge result sets stay cheap.
            let count_sql = format!(
                "SELECT count(*) FROM (SELECT 1 FROM {from} {where_sql} LIMIT {})",
                COUNT_CAP + 1
            );
            let n: i64 = self.conn.query_row(&count_sql, &refs[..filter_params], |r| r.get(0))?;
            let n = n as usize;
            (n.min(COUNT_CAP), n > COUNT_CAP)
        };
        Ok(SearchResults { hits, total, capped, ..Default::default() })
    }

    /// One- and two-character name terms cannot use the trigram index. The
    /// best matches for such short input are name prefixes, which the name
    /// index serves directly; remaining slots are filled by a substring scan
    /// that stops as soon as enough rows are found.
    fn search_short(&self, q: &Query, limit: usize) -> Result<SearchResults> {
        let first = &q.name_terms[0];
        let rest = &q.name_terms[1..];

        // Phase 1: prefix range on the NOCASE name index.
        let mut pre = SqlParts::default();
        pre.cond("f.name >= ?#", first.clone());
        pre.cond("f.name < ?#", format!("{first}\u{10FFFF}"));
        for t in rest {
            pre.name_contains(t);
        }
        pre.filters(q);
        let where1 = pre.where_sql();
        let boost = pre.boost(q);
        let p = pre.param(first.clone());
        let sql1 = format!(
            "{SELECT_COLS} FROM files f {where1} ORDER BY lower(f.name) = ?{p} DESC, {boost} f.is_dir DESC, length(f.name), length(f.path) LIMIT {limit}"
        );
        let mut hits = self.query_hits(&sql1, &pre.refs())?;
        let mut capped = hits.len() >= limit;

        // Phase 2: substring scan that stops at the limit. Prefix rows show
        // up again here, so ask for enough to cover them.
        if !capped {
            let mut sub = SqlParts::default();
            sub.name_contains(first);
            for t in rest {
                sub.name_contains(t);
            }
            sub.filters(q);
            let want = limit + hits.len();
            let sql2 = format!("{SELECT_COLS} FROM files f {} LIMIT {want}", sub.where_sql());
            let more = self.query_hits(&sql2, &sub.refs())?;
            capped = more.len() >= want;
            let seen: std::collections::HashSet<i64> = hits.iter().map(|h| h.id).collect();
            let mut extra: Vec<SearchHit> =
                more.into_iter().filter(|h| !seen.contains(&h.id)).collect();
            extra.sort_by_cached_key(|h| {
                let lname = h.name.to_lowercase();
                (lname.find(first.as_str()).unwrap_or(usize::MAX), !h.is_dir, h.name.len())
            });
            extra.truncate(limit - hits.len());
            hits.extend(extra);
        }
        Ok(SearchResults { total: hits.len(), hits, capped, ..Default::default() })
    }

    fn query_hits(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<SearchHit>> {
        let mut st = self.conn.prepare_cached(sql)?;
        let hits = st
            .query_map(params, |r| {
                let path: String = r.get(1)?;
                let parent =
                    util::split_path(&path).map(|(p, _)| p.to_string()).unwrap_or_default();
                Ok(SearchHit {
                    id: r.get(0)?,
                    name: r.get(2)?,
                    parent,
                    ext: r.get(3)?,
                    is_dir: r.get::<_, i64>(4)? != 0,
                    size: r.get::<_, i64>(5)? as u64,
                    created: r.get(6)?,
                    modified: r.get(7)?,
                    path,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hits)
    }
}

const SELECT_COLS: &str =
    "SELECT f.id, f.path, f.name, f.ext, f.is_dir, f.size, f.created, f.modified";

/// Accumulates WHERE conditions with numbered parameters. `?#` in a
/// condition template is replaced by that parameter's number.
#[derive(Default)]
struct SqlParts {
    params: Vec<Box<dyn ToSql>>,
    wheres: Vec<String>,
}

impl SqlParts {
    fn param<T: ToSql + 'static>(&mut self, v: T) -> usize {
        self.params.push(Box::new(v));
        self.params.len()
    }

    fn cond<T: ToSql + 'static>(&mut self, template: &str, v: T) {
        let n = self.param(v);
        self.wheres.push(template.replace("?#", &format!("?{n}")));
    }

    fn name_contains(&mut self, term: &str) {
        self.cond("f.name LIKE ?# ESCAPE '^'", format!("%{}%", util::escape_like(term)));
    }

    /// Path, extension, kind and hidden-attribute filters.
    fn filters(&mut self, q: &Query) {
        if let Some(scope) = &q.scope {
            let s = util::escape_like(scope.trim_end_matches('\\'));
            self.cond("f.path LIKE ?# ESCAPE '^'", format!("{s}\\%"));
        }
        for x in &q.exclude_folders {
            let x = x.trim_end_matches('\\');
            if x == ".*" {
                // Dot-folders (.git, .vscode, .agents...) and dot-files.
                self.wheres.push("f.path NOT LIKE '%\\.%'".into());
            } else if x.len() >= 2 && x.as_bytes()[1] == b':' {
                // Absolute folder: hide it and everything below it.
                self.cond("f.path NOT LIKE ?# ESCAPE '^'", format!("{}\\%", util::escape_like(x)));
                self.cond("f.path <> ?#", x.to_string());
            } else if !x.is_empty() {
                // Folder name: hide any folder with that name, anywhere.
                let n = util::escape_like(x.trim_matches('\\'));
                self.cond("f.path NOT LIKE ?# ESCAPE '^'", format!("%\\{n}\\%"));
                self.cond("f.name <> ?#", x.trim_matches('\\').to_string());
            }
        }
        // LIKE folds ASCII case only; trigram MATCH folds Unicode case.
        for t in &q.path_terms {
            self.cond("f.path LIKE ?# ESCAPE '^'", format!("%{}%", util::escape_like(t)));
        }
        if !q.exts.is_empty() {
            let ph: Vec<String> =
                q.exts.iter().map(|e| format!("?{}", self.param(e.clone()))).collect();
            self.wheres.push(format!("f.ext IN ({})", ph.join(",")));
        }
        match q.kind {
            KindFilter::Any => {}
            KindFilter::FilesOnly => self.wheres.push("f.is_dir = 0".into()),
            KindFilter::FoldersOnly => self.wheres.push("f.is_dir = 1".into()),
        }
        if !q.show_hidden {
            self.wheres
                .push(format!("(f.attrs & {}) = 0", FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM));
        }
    }

    /// ORDER BY fragment ranking entries under `q.boost_prefix` first.
    fn boost(&mut self, q: &Query) -> String {
        match &q.boost_prefix {
            Some(b) => {
                let n = self.param(format!("{}\\%", util::escape_like(b.trim_end_matches('\\'))));
                format!("(f.path LIKE ?{n} ESCAPE '^') DESC,")
            }
            None => String::new(),
        }
    }

    fn where_sql(&self) -> String {
        if self.wheres.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.wheres.join(" AND "))
        }
    }

    fn refs(&self) -> Vec<&dyn ToSql> {
        self.params.iter().map(|b| b.as_ref()).collect()
    }
}

fn dir(desc: bool) -> &'static str {
    if desc { "DESC" } else { "ASC" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_terms_and_filters() {
        let q = Query::parse("Report  ext:PDF,.docx folder:");
        assert_eq!(q.name_terms, vec!["report"]);
        assert_eq!(q.exts, vec!["docx", "pdf"]);
        assert_eq!(q.kind, KindFilter::FoldersOnly);

        let q = Query::parse("*.rs src\\main \"my file\"");
        assert_eq!(q.exts, vec!["rs"]);
        assert_eq!(q.path_terms, vec!["src\\main"]);
        assert_eq!(q.name_terms, vec!["my file"]);

        let q = Query::parse("path:Users/ani file:");
        assert_eq!(q.path_terms, vec!["users\\ani"]);
        assert_eq!(q.kind, KindFilter::FilesOnly);
        assert!(Query::parse("   ").is_empty());
    }
}
