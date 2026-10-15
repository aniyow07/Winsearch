//! Minimal, safe readers for Word (.docx) and Excel (.xlsx) previews.
//!
//! Office Open XML files are ZIP archives of XML parts. Only the parts
//! needed for a readable preview are parsed, every read is size-limited
//! (protects against ZIP bombs), and output is capped. Nothing is
//! executed and no Office installation is required.

use std::collections::HashMap;
use std::io::Read;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// Largest XML part we are willing to inflate.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BLOCKS: usize = 4000;
pub const MAX_SHEET_ROWS: usize = 500;
pub const MAX_SHEET_COLS: usize = 60;
const MAX_SHEETS: usize = 12;

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DocBlock {
    /// `level` 0 = body text, 1..=6 = heading level.
    Paragraph { runs: Vec<Run>, level: u8, list: bool },
    Table { rows: Vec<Vec<String>> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sheet {
    pub name: String,
    /// Dense grid starting at A1, trimmed to the used range (capped).
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
}

fn open(path: &str) -> Result<zip::ZipArchive<std::fs::File>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("Cannot open the file: {e}"))?;
    zip::ZipArchive::new(f).map_err(|_| "The file is damaged or not an Office Open XML document.".to_string())
}

fn read_part(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Result<Option<Vec<u8>>, String> {
    let mut entry = match zip.by_name(name) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("Cannot read {name}: {e}")),
    };
    if entry.size() > MAX_PART_BYTES {
        return Err("The document is too large to preview.".into());
    }
    let mut buf = Vec::with_capacity(entry.size().min(8 * 1024 * 1024) as usize);
    (&mut entry)
        .take(MAX_PART_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Cannot read {name}: {e}"))?;
    if buf.len() as u64 > MAX_PART_BYTES {
        return Err("The document is too large to preview.".into());
    }
    Ok(Some(buf))
}

fn local(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

#[allow(deprecated)]
fn attr(e: &BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes().flatten().find(|a| local(a.key.as_ref()) == key).and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

/// Appends the text of a Text/GeneralRef event.
fn push_text(ev: &Event<'_>, out: &mut String) {
    match ev {
        Event::Text(t) => out.push_str(&t.xml10_content()),
        Event::CData(t) => out.push_str(&t[..]),
        Event::GeneralRef(r) => {
            if let Ok(Some(c)) = r.resolve_char_ref() {
                out.push(c);
            } else if let Some(c) = entity(&r.xml10_content()) {
                out.push(c);
            }
        }
        _ => {}
    }
}

/// True when a toggle property (`<w:b/>`, `<w:b w:val="0"/>`) is on.
fn toggle_on(e: &BytesStart<'_>) -> bool {
    !matches!(attr(e, "val").as_deref(), Some("0") | Some("false") | Some("none"))
}

// ---- Word -----------------------------------------------------------------

pub fn parse_docx(path: &str) -> Result<(Vec<DocBlock>, bool), String> {
    let mut zip = open(path)?;
    let xml = read_part(&mut zip, "word/document.xml")?
        .ok_or("This file has no Word document body (word/document.xml).")?;
    parse_document_xml(&xml)
}

/// Parses WordprocessingML body text, headings, simple lists and tables.
pub fn parse_document_xml(xml: &[u8]) -> Result<(Vec<DocBlock>, bool), String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut blocks = Vec::new();
    let mut truncated = false;

    // Paragraph state.
    let mut runs: Vec<Run> = Vec::new();
    let mut level = 0u8;
    let mut list = false;
    let (mut bold, mut italic) = (false, false);
    let mut in_text = false;
    let mut cur = String::new();
    // Table state (tables can nest; only the outermost is rendered).
    let mut table_depth = 0usize;
    let mut table: Vec<Vec<String>> = Vec::new();
    let mut cell = String::new();
    let mut in_run_props = false;

    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| format!("The document XML is malformed: {e}"))?;
        match &ev {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                match local(e.name().as_ref()) {
                    "tbl" if !empty => {
                        table_depth += 1;
                        if table_depth == 1 {
                            table.clear();
                        }
                    }
                    "tr" if table_depth == 1 && !empty => table.push(Vec::new()),
                    "tc" if table_depth == 1 && !empty => cell.clear(),
                    "p" if !empty => {
                        runs.clear();
                        level = 0;
                        list = false;
                    }
                    "pStyle" => {
                        if let Some(v) = attr(e, "val") {
                            let v = v.to_lowercase();
                            if let Some(n) = v.strip_prefix("heading").and_then(|n| n.trim().parse::<u8>().ok()) {
                                level = n.clamp(1, 6);
                            } else if v == "title" {
                                level = 1;
                            } else if v == "subtitle" {
                                level = 2;
                            } else if v.contains("list") {
                                list = true;
                            }
                        }
                    }
                    "numPr" => list = true,
                    "r" if !empty => {
                        bold = false;
                        italic = false;
                    }
                    "rPr" if !empty => in_run_props = true,
                    "b" if in_run_props => bold = toggle_on(e),
                    "i" if in_run_props => italic = toggle_on(e),
                    "t" if !empty => {
                        in_text = true;
                        cur.clear();
                    }
                    "tab" => cur_push(&mut runs, "\t", bold, italic, table_depth, &mut cell),
                    "br" | "cr" => cur_push(&mut runs, "\n", bold, italic, table_depth, &mut cell),
                    _ => {}
                }
            }
            Event::Text(_) | Event::GeneralRef(_) | Event::CData(_) if in_text => push_text(&ev, &mut cur),
            Event::End(e) => match local(e.name().as_ref()) {
                "t" => {
                    in_text = false;
                    let t = std::mem::take(&mut cur);
                    cur_push(&mut runs, &t, bold, italic, table_depth, &mut cell);
                }
                "rPr" => in_run_props = false,
                "p" => {
                    if table_depth > 0 {
                        if !cell.is_empty() && !cell.ends_with('\n') {
                            cell.push('\n');
                        }
                    } else if blocks.len() < MAX_BLOCKS {
                        blocks.push(DocBlock::Paragraph { runs: std::mem::take(&mut runs), level, list });
                    } else {
                        truncated = true;
                    }
                }
                "tc" if table_depth == 1 => {
                    if let Some(row) = table.last_mut() {
                        row.push(cell.trim_end().to_string());
                    }
                }
                "tbl" => {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 {
                        if blocks.len() < MAX_BLOCKS {
                            blocks.push(DocBlock::Table { rows: std::mem::take(&mut table) });
                        } else {
                            truncated = true;
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
        if truncated {
            break;
        }
    }
    Ok((blocks, truncated))
}

fn cur_push(runs: &mut Vec<Run>, text: &str, bold: bool, italic: bool, table_depth: usize, cell: &mut String) {
    if text.is_empty() {
        return;
    }
    if table_depth > 0 {
        cell.push_str(text);
        return;
    }
    match runs.last_mut() {
        Some(r) if r.bold == bold && r.italic == italic => r.text.push_str(text),
        _ => runs.push(Run { text: text.to_string(), bold, italic }),
    }
}

// ---- Excel ----------------------------------------------------------------

pub fn parse_xlsx(path: &str) -> Result<Vec<Sheet>, String> {
    let mut zip = open(path)?;
    let workbook = read_part(&mut zip, "xl/workbook.xml")?.ok_or("This file has no Excel workbook (xl/workbook.xml).")?;
    let rels = read_part(&mut zip, "xl/_rels/workbook.xml.rels")?.unwrap_or_default();
    let shared = match read_part(&mut zip, "xl/sharedStrings.xml")? {
        Some(x) => parse_shared_strings(&x)?,
        None => Vec::new(),
    };
    let targets = parse_rels(&rels)?;
    let mut sheets = Vec::new();
    for (name, rid) in parse_workbook_sheets(&workbook)?.into_iter().take(MAX_SHEETS) {
        let Some(target) = targets.get(&rid) else { continue };
        let part = if let Some(abs) = target.strip_prefix('/') {
            abs.to_string()
        } else {
            format!("xl/{}", target.trim_start_matches("./"))
        };
        match read_part(&mut zip, &part)? {
            Some(xml) => {
                let (rows, truncated) = parse_sheet_xml(&xml, &shared)?;
                sheets.push(Sheet { name, rows, truncated });
            }
            None => sheets.push(Sheet { name, rows: Vec::new(), truncated: false }),
        }
    }
    if sheets.is_empty() {
        return Err("The workbook has no readable worksheets.".into());
    }
    Ok(sheets)
}

fn parse_rels(xml: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| e.to_string())? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "Relationship" => {
                if let (Some(id), Some(t)) = (attr(&e, "Id"), attr(&e, "Target")) {
                    out.insert(id, t);
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn parse_workbook_sheets(xml: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| e.to_string())? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) if local(e.name().as_ref()) == "sheet" => {
                let name = attr(&e, "name").unwrap_or_else(|| format!("Sheet{}", out.len() + 1));
                if let Some(rid) = attr(&e, "id") {
                    out.push((name, rid));
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

pub fn parse_shared_strings(xml: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut cur = String::new();
    let mut in_t = false;
    let mut in_phonetic = false;
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        match &ev {
            Event::Eof => break,
            Event::Start(e) => match local(e.name().as_ref()) {
                "si" => cur.clear(),
                "t" => in_t = true,
                "rPh" => in_phonetic = true,
                _ => {}
            },
            Event::Empty(e) if local(e.name().as_ref()) == "si" => out.push(String::new()),
            Event::Text(_) | Event::GeneralRef(_) | Event::CData(_) if in_t && !in_phonetic => push_text(&ev, &mut cur),
            Event::End(e) => match local(e.name().as_ref()) {
                "t" => in_t = false,
                "rPh" => in_phonetic = false,
                "si" => out.push(std::mem::take(&mut cur)),
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// "BC12" -> (row 11, col 54), zero-based.
pub fn cell_ref(r: &str) -> Option<(usize, usize)> {
    let letters: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits = &r[letters.len()..];
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }
    let col = letters.bytes().fold(0usize, |acc, b| acc * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize) - 1;
    let row: usize = digits.parse().ok()?;
    Some((row.checked_sub(1)?, col))
}

pub fn col_name(mut c: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

pub fn parse_sheet_xml(xml: &[u8], shared: &[String]) -> Result<(Vec<Vec<String>>, bool), String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    let mut row_idx = 0usize;
    let mut next_col = 0usize;
    let mut cell_pos: Option<(usize, usize)> = None;
    let mut cell_type = String::new();
    let mut value = String::new();
    let mut in_value = false;
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| format!("The worksheet XML is malformed: {e}"))?;
        match &ev {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                match local(e.name().as_ref()) {
                    "row" => {
                        row_idx = attr(e, "r").and_then(|r| r.parse::<usize>().ok()).map(|r| r.saturating_sub(1)).unwrap_or(row_idx);
                        next_col = 0;
                        if row_idx >= MAX_SHEET_ROWS {
                            truncated = true;
                            break;
                        }
                    }
                    "c" => {
                        let pos = attr(e, "r").and_then(|r| cell_ref(&r)).unwrap_or((row_idx, next_col));
                        next_col = pos.1 + 1;
                        cell_type = attr(e, "t").unwrap_or_default();
                        value.clear();
                        cell_pos = if empty { None } else { Some(pos) };
                    }
                    "v" | "t" if !empty => in_value = true,
                    _ => {}
                }
            }
            Event::Text(_) | Event::GeneralRef(_) | Event::CData(_) if in_value => push_text(&ev, &mut value),
            Event::End(e) => match local(e.name().as_ref()) {
                "v" | "t" => in_value = false,
                "c" => {
                    if let Some((r, c)) = cell_pos.take() {
                        if c >= MAX_SHEET_COLS {
                            truncated = true;
                        } else if r < MAX_SHEET_ROWS {
                            let text = match cell_type.as_str() {
                                "s" => value.trim().parse::<usize>().ok().and_then(|i| shared.get(i).cloned()).unwrap_or_default(),
                                "b" => if value.trim() == "1" { "TRUE".into() } else { "FALSE".into() },
                                "e" => value.clone(),
                                _ => format_number(&value),
                            };
                            if !text.is_empty() {
                                if grid.len() <= r {
                                    grid.resize(r + 1, Vec::new());
                                }
                                let row = &mut grid[r];
                                if row.len() <= c {
                                    row.resize(c + 1, String::new());
                                }
                                row[c] = text;
                            }
                        }
                    }
                }
                "row" => row_idx += 1,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    // Make the grid rectangular for display.
    let width = grid.iter().map(|r| r.len()).max().unwrap_or(0);
    for r in &mut grid {
        r.resize(width, String::new());
    }
    Ok((grid, truncated))
}

/// Shortens float noise such as 0.30000000000000004 for display.
fn format_number(v: &str) -> String {
    let v = v.trim();
    match v.parse::<f64>() {
        Ok(f) if v.contains(['.', 'E', 'e']) => {
            let s = format!("{:.10}", f);
            let s = s.trim_end_matches('0').trim_end_matches('.');
            s.to_string()
        }
        _ => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_refs() {
        assert_eq!(cell_ref("A1"), Some((0, 0)));
        assert_eq!(cell_ref("Z3"), Some((2, 25)));
        assert_eq!(cell_ref("AA10"), Some((9, 26)));
        assert_eq!(cell_ref("1A"), None);
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(701), "ZZ");
    }

    #[test]
    fn parses_word_body() {
        let xml = br#"<w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title &amp; more</w:t></w:r></w:p>
            <w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Bold</w:t></w:r><w:r><w:t xml:space="preserve"> plain</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
        </w:body></w:document>"#;
        let (blocks, truncated) = parse_document_xml(xml).unwrap();
        assert!(!truncated);
        assert_eq!(blocks.len(), 3);
        match &blocks[0] {
            DocBlock::Paragraph { runs, level, .. } => {
                assert_eq!(*level, 1);
                assert_eq!(runs[0].text, "Title & more");
            }
            other => panic!("{other:?}"),
        }
        match &blocks[1] {
            DocBlock::Paragraph { runs, .. } => {
                assert_eq!(runs.len(), 2);
                assert!(runs[0].bold);
                assert_eq!(runs[1].text, " plain");
                assert!(!runs[1].bold);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(blocks[2], DocBlock::Table { rows: vec![vec!["a".into(), "b".into()]] });
    }

    #[test]
    fn parses_sheet_with_shared_strings() {
        let shared = parse_shared_strings(br#"<sst><si><t>Name</t></si><si><r><t>Ri</t></r><r><t>ch</t></r></si></sst>"#).unwrap();
        assert_eq!(shared, vec!["Name", "Rich"]);
        let xml = br#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1"><v>0.30000000000000004</v></c></row>
            <row r="3"><c r="B3" t="s"><v>1</v></c><c r="D3" t="b"><v>1</v></c><c r="E3" t="inlineStr"><is><t>x</t></is></c></row>
        </sheetData></worksheet>"#;
        let (rows, truncated) = parse_sheet_xml(xml, &shared).unwrap();
        assert!(!truncated);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], "Name");
        assert_eq!(rows[0][2], "0.3");
        assert_eq!(rows[2][1], "Rich");
        assert_eq!(rows[2][3], "TRUE");
        assert_eq!(rows[2][4], "x");
        assert!(rows[1].iter().all(|c| c.is_empty()));
    }

    #[test]
    fn malformed_archives_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fake.docx");
        std::fs::write(&p, b"not a zip").unwrap();
        assert!(parse_docx(p.to_str().unwrap()).is_err());
        assert!(parse_xlsx(p.to_str().unwrap()).is_err());
    }
}
