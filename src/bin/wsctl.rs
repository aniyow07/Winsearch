//! Command-line companion for WinSearch: index, search and benchmark
//! without the GUI. Used for measurements and troubleshooting.
//!
//!   wsctl index  --db <file> <root>...      scan roots into the index
//!   wsctl search --db <file> <query...>     run one query
//!   wsctl bench  --db <file>                compare substring strategies
//!   wsctl check  --db <file>                integrity check

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use winsearch::db::Db;
use winsearch::scanner::{DirWalkBackend, ScanConfig, ScanProgress};
use winsearch::util;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        bail!("usage: wsctl <index|search|bench|check> --db <file> [args]");
    }
    let cmd = args.remove(0);
    let mut db_path: Option<PathBuf> = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--db" {
            db_path = Some(PathBuf::from(it.next().context("--db needs a path")?));
        } else {
            rest.push(a);
        }
    }
    let db_path = db_path.unwrap_or_else(winsearch::config::default_db_path);
    match cmd.as_str() {
        "index" => index(&db_path, &rest),
        "search" => search(&db_path, &rest.join(" ")),
        "bench" => bench(&db_path),
        "walk" => {
            // Enumeration speed without any database work.
            for root in &rest {
                let (tx, rx) = crossbeam_channel::bounded(256);
                let progress = ScanProgress::default();
                let cancel = AtomicBool::new(false);
                let t = Instant::now();
                let drain = std::thread::spawn(move || rx.iter().count());
                let out = winsearch::scanner::ScanBackend::scan(
                    &DirWalkBackend,
                    &util::normalize_dir(root),
                    &ScanConfig::default(),
                    &Default::default(),
                    &tx,
                    &cancel,
                    &progress,
                );
                drop(tx);
                drain.join().ok();
                let files = progress.files.load(std::sync::atomic::Ordering::Relaxed);
                let secs = t.elapsed().as_secs_f64();
                println!("{root}: {out:?} {files} entries in {secs:.2}s ({:.0}/s)", files as f64 / secs);
            }
            Ok(())
        }
        "preview" => {
            // Preview generation latency per file (cold first, then warm).
            for p in &rest {
                let mut times = Vec::new();
                let mut label = String::new();
                for _ in 0..3 {
                    let t = Instant::now();
                    let pv = winsearch::preview::generate(
                        0,
                        p,
                        std::path::Path::new(p).is_dir(),
                        Default::default(),
                        &|| false,
                    );
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                    label = match &pv.content {
                        Ok(c) => format!("{} ok ({})", pv.kind_label, content_kind(c)),
                        Err(e) => format!("{} error: {e}", pv.kind_label),
                    };
                }
                println!(
                    "{:>8.1} ms first, {:>7.1} ms repeat  {label}  {p}",
                    times[0],
                    times[1].min(times[2])
                );
            }
            Ok(())
        }
        "exec" => {
            let conn = rusqlite::Connection::open(&db_path)?;
            let t = Instant::now();
            conn.execute_batch(&rest.join(" "))?;
            println!("done in {:.2}s", t.elapsed().as_secs_f64());
            Ok(())
        }
        "sql" => {
            // Ad-hoc timing of one SQL statement against the index.
            let conn = Db::open_reader(&db_path)?;
            let t = Instant::now();
            let sql = rest.join(" ");
            let mut st = conn.prepare(&sql)?;
            let mut rows = st.query([])?;
            let mut n = 0;
            while let Some(r) = rows.next()? {
                if n < 40 {
                    let cols: Vec<String> = (0..r.as_ref().column_count())
                        .map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap()))
                        .collect();
                    println!("{}", cols.join(" | "));
                }
                n += 1;
            }
            println!("{n} rows in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
            Ok(())
        }
        "check" => {
            let (db, _) = Db::open(&db_path)?;
            match db.quick_check()? {
                None => println!("ok"),
                Some(e) => println!("problem: {e}"),
            }
            Ok(())
        }
        _ => bail!("unknown command {cmd}"),
    }
}

fn index(db_path: &PathBuf, roots: &[String]) -> Result<()> {
    let (mut db, _) = Db::open(db_path)?;
    let cfg = ScanConfig::default();
    for root in roots {
        let root = util::normalize_dir(root);
        let progress = ScanProgress::default();
        let cancel = AtomicBool::new(false);
        let rep = winsearch::indexer::scan::scan_root(
            &mut db,
            &DirWalkBackend,
            &root,
            &cfg,
            true,
            &cancel,
            &progress,
            |_, _| {},
        )?;
        let secs = rep.elapsed.as_secs_f64();
        println!(
            "{root}: {:?} — {} entries, {} dirs, {} errors, {} removed in {:.2}s ({:.0} entries/s)",
            rep.outcome,
            rep.files,
            rep.dirs,
            rep.errors,
            rep.removed,
            secs,
            rep.files as f64 / secs.max(0.001)
        );
        for e in rep.error_samples.iter().take(5) {
            println!("  error: {e}");
        }
    }
    let t = Instant::now();
    db.optimize();
    db.checkpoint();
    println!("optimize+checkpoint: {:.2}s", t.elapsed().as_secs_f64());
    println!("total rows: {}", db.count()?);
    let size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    println!("database size: {}", util::format_size(size));
    Ok(())
}

fn search(db_path: &PathBuf, q: &str) -> Result<()> {
    let conn = Db::open_reader(db_path)?;
    let engine = winsearch::search::SearchEngine::new(conn);
    let query = winsearch::search::Query::parse(q);
    let t = Instant::now();
    let res = engine.search(&query, 20)?;
    println!(
        "{} matches (showing {}) in {:.1} ms",
        res.total,
        res.hits.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );
    for h in res.hits {
        println!("  {}", h.path);
    }
    Ok(())
}

/// Compares substring-matching strategies on the current index.
fn bench(db_path: &PathBuf) -> Result<()> {
    let conn = Db::open_reader(db_path)?;
    let rows: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;
    println!("rows: {rows}");
    let queries = ["re", "png", "chrome", "readme", "config.json", "system32", "zzqxj"];
    let strategies: [(&str, &str); 4] = [
        ("LIKE scan (count)", "SELECT count(*) FROM files WHERE name LIKE ?1 ESCAPE '^'"),
        ("instr(lower) scan (count)", "SELECT count(*) FROM files WHERE instr(lower(name), ?2) > 0"),
        ("FTS5 trigram MATCH (count)", "SELECT count(*) FROM names_fts WHERE names_fts MATCH ?3"),
        ("FTS5 trigram LIKE (count)", "SELECT count(*) FROM names_fts WHERE name LIKE ?1 ESCAPE '^'"),
    ];
    println!("{:<30} {}", "strategy \\ query", queries.join(" | "));
    for (label, sql) in strategies {
        let mut st = conn.prepare(sql)?;
        let mut cells = Vec::new();
        for q in queries {
            let like = format!("%{}%", util::escape_like(q));
            let fts = format!("\"{}\"", q.replace('"', "\"\""));
            let lower = q.to_lowercase();
            // Warm once, then take the best of 3.
            let mut best = f64::MAX;
            let mut n = 0i64;
            for _ in 0..4 {
                let t = Instant::now();
                n = match st.query_row(
                    rusqlite::params_from_iter(
                        [&like, &lower, &fts].iter().take(st.parameter_count()),
                    ),
                    |r| r.get(0),
                ) {
                    Ok(v) => v,
                    Err(_) => -1,
                };
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            cells.push(format!("{best:.1}ms/{n}"));
        }
        println!("{label:<30} {}", cells.join(" | "));
    }

    // In-memory alternative: all lower-cased names in one buffer, scanned
    // with a SIMD substring finder.
    let t = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut starts: Vec<u32> = Vec::new();
    {
        let mut st = conn.prepare("SELECT name FROM files")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let n: String = r.get(0)?;
            starts.push(buf.len() as u32);
            buf.extend_from_slice(n.to_lowercase().as_bytes());
            buf.push(0);
        }
    }
    println!(
        "in-memory load: {:.0} ms, {} of names",
        t.elapsed().as_secs_f64() * 1000.0,
        util::format_size(buf.len() as u64 + starts.len() as u64 * 4)
    );
    let mut cells = Vec::new();
    for q in queries {
        let finder = memchr::memmem::Finder::new(q.as_bytes());
        let mut best = f64::MAX;
        let mut n = 0;
        for _ in 0..4 {
            let t = Instant::now();
            n = 0;
            let mut last_entry = usize::MAX;
            for pos in finder.find_iter(&buf) {
                let entry = starts.partition_point(|&s| s as usize <= pos) - 1;
                if entry != last_entry {
                    n += 1;
                    last_entry = entry;
                }
            }
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        cells.push(format!("{best:.1}ms/{n}"));
    }
    println!("{:<30} {}", "in-memory memmem (count)", cells.join(" | "));

    // Engine end-to-end (ranked top 500 + total count).
    let engine = winsearch::search::SearchEngine::new(Db::open_reader(db_path)?);
    let mut cells = Vec::new();
    for q in queries {
        let query = winsearch::search::Query::parse(q);
        let mut best = f64::MAX;
        let mut n = 0;
        for _ in 0..4 {
            let t = Instant::now();
            let r = engine.search(&query, 500)?;
            n = r.total;
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        cells.push(format!("{best:.1}ms/{n}"));
    }
    println!("{:<30} {}", "engine ranked top-500", cells.join(" | "));
    Ok(())
}

fn content_kind(c: &winsearch::preview::Content) -> String {
    use winsearch::preview::Content;
    match c {
        Content::Image { image, original, .. } => {
            format!("image {}x{} from {}x{}", image.width, image.height, original.0, original.1)
        }
        Content::Pdf { page, page_count, .. } => format!("pdf page {}x{}, {page_count} pages", page.width, page.height),
        Content::Text { text, truncated, .. } => format!("text {} bytes{}", text.len(), if *truncated { ", truncated" } else { "" }),
        Content::Document { blocks, .. } => format!("docx {} blocks", blocks.len()),
        Content::Sheets { sheets } => format!("xlsx {} sheets", sheets.len()),
        Content::Folder { total, .. } => format!("folder {total} items"),
        Content::Info { icon, note } => format!("info{}{}", if icon.is_some() { " +shell icon" } else { "" }, note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default()),
    }
}
