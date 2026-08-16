//! Reads a spreadsheet into JSON rows, entirely in the guest.
//!
//! XLSX, XLSM and XLS open as `kind: "binary"` with zero characters through
//! the text path, because they are not text — an XLSX is a zip of XML, and
//! an XLS is a compound binary document. A corpus where the numeric outcome
//! series live in workbooks is therefore unreadable by any amount of
//! `slice`/`page_text`, which is exactly the situation the Medicaid 1115
//! field log describes: 1,059 of 8,185 files, and not incidental ones.
//!
//! # Why this is a Rust block and not a Rhai builtin
//!
//! The pattern established by the binary and image toolkits is that *header
//! parsing* belongs in the shared interpreter, because it is small, and
//! *decoding* belongs elsewhere, because it is not. A sheet reader is
//! decoding: `calamine` brings a zip reader, an XML parser, a shared-string
//! table, a date serial converter and the compound-file reader for legacy
//! XLS. Putting that in the interpreter would tax every scripted job in the
//! system for a capability most of them never use.
//!
//! It stays in the guest rather than becoming a host feature flag for a
//! different reason: `calamine` compiles to `wasm32-unknown-unknown` with
//! default features off, so nothing here needs native code. A capability
//! that needs no feature flag is one nobody has to discover they lack.
//!
//! # Memory
//!
//! The whole workbook is pulled into guest memory before parsing, because
//! `calamine` needs `Read + Seek` over the complete file — a zip's central
//! directory is at the end. That is a real ceiling and the reason
//! [`MAX_BYTES`] exists: a workbook past it fails as one item with a clear
//! message rather than trapping the guest and taking the whole fan-out item
//! with it.

use cuttlefish_sdk::{export_block, Block, Command, Event, MediaKind, Signature};

/// Largest workbook this block will hold in guest memory.
///
/// Chosen against the corpus that motivated the block: CMS budget-neutrality
/// workbooks run to a few megabytes, so 64 MiB is generous by roughly an
/// order of magnitude while still bounding the failure. Exceeding it is an
/// ordinary item failure, not a trap.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// How much to ask for per `SliceBytes`.
///
/// The host base64-encodes each window, so a very large window costs a third
/// again in transfer for no benefit. A megabyte keeps the round-trip count
/// low without making any single message enormous.
const WINDOW: u64 = 1024 * 1024;

#[derive(Default)]
struct SheetExtract {
    path: String,
    handle: u32,
    len: u64,
    /// The workbook, accumulated across windows.
    bytes: Vec<u8>,
    /// How many rows of each sheet to return, from the job input.
    max_rows: usize,
}

/// Rows a sheet returns when the caller does not say.
///
/// Not unbounded: a single sheet in this corpus can carry tens of thousands
/// of rows, and a fan-out item returning all of them puts the whole thing
/// through the ledger and into `results.jsonl`. A caller that wants
/// everything can ask for it explicitly.
const DEFAULT_MAX_ROWS: usize = 1000;

impl Block for SheetExtract {
    fn signature() -> Signature {
        // Stated concretely rather than `json -> json`: a `json` seam
        // typechecks unconditionally, which is the same as not checking it.
        Signature {
            input: "{path: text}".parse().expect("a literal type"),
            output: "{path: text, sheets: json, schema: json, truncated: json}"
                .parse()
                .expect("a literal type"),
        }
    }

    fn start(&mut self, input: serde_json::Value) -> Command {
        self.max_rows = input
            .get("max_rows")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_ROWS);

        match input.get("path").and_then(|v| v.as_str()) {
            Some(path) => {
                self.path = path.to_string();
                Command::Open {
                    path: path.to_string(),
                }
            }
            // Fail rather than panic: a panic is an opaque wasm trap, while
            // this reaches the caller as a code they can act on.
            None => fail(
                "schema_validation_failed",
                "input must have a string `path` field",
            ),
        }
    }

    fn step(&mut self, event: Event) -> Command {
        match event {
            Event::Opened { handle, len, kind } => {
                // A workbook is `Binary` to the host, which has no sheet
                // reader — that is the whole reason this block exists. Text
                // and documents are somebody else's job, and saying so is
                // more useful than reading them badly.
                if !matches!(kind, MediaKind::Binary) {
                    return fail(
                        "unsupported",
                        &format!(
                            "`{}` opened as {kind:?}, not a binary workbook. This block reads \
                             XLSX/XLSM/XLS; use `page_text` or `slice` for text and documents.",
                            self.path
                        ),
                    );
                }
                if len > MAX_BYTES {
                    return fail(
                        "unsupported",
                        &format!(
                            "workbook is {len} bytes, over this block's {MAX_BYTES}-byte ceiling. \
                             The whole file must be in guest memory to be parsed, since a zip's \
                             central directory is at the end."
                        ),
                    );
                }
                self.handle = handle;
                self.len = len;
                self.bytes = Vec::with_capacity(len as usize);
                self.next_window()
            }

            Event::SlicedBytes { bytes_base64, .. } => {
                use base64::Engine as _;
                match base64::engine::general_purpose::STANDARD.decode(&bytes_base64) {
                    Ok(mut chunk) => self.bytes.append(&mut chunk),
                    Err(e) => return fail("unsupported", &format!("undecodable window: {e}")),
                }
                if (self.bytes.len() as u64) < self.len {
                    return self.next_window();
                }
                self.parse()
            }

            other => fail(
                "unsupported",
                &format!("this block issues only Open and SliceBytes; got {other:?}"),
            ),
        }
    }
}

impl SheetExtract {
    fn next_window(&self) -> Command {
        let offset = self.bytes.len() as u64;
        Command::SliceBytes {
            handle: self.handle,
            offset,
            len: WINDOW.min(self.len - offset),
        }
    }

    /// Parse the assembled bytes and finish.
    fn parse(&mut self) -> Command {
        use calamine::Reader;

        let cursor = std::io::Cursor::new(std::mem::take(&mut self.bytes));
        let mut workbook = match calamine::open_workbook_auto_from_rs(cursor) {
            Ok(w) => w,
            // A workbook that will not open is a data-quality fact about one
            // file, not an authoring error, so it fails this item and lets a
            // fan-out run continue.
            Err(e) => {
                return fail(
                    "unsupported",
                    &format!("`{}` is not a readable workbook: {e}", self.path),
                )
            }
        };

        let mut sheets = serde_json::Map::new();
        let mut schema = serde_json::Map::new();
        let mut truncated = serde_json::Map::new();

        for name in workbook.sheet_names().to_owned() {
            let Ok(range) = workbook.worksheet_range(&name) else {
                // One unreadable sheet does not condemn the workbook: record
                // it and keep the others, the same way one bad item does not
                // condemn a fan-out.
                sheets.insert(name.clone(), serde_json::Value::Null);
                schema.insert(name.clone(), serde_json::Value::Null);
                continue;
            };

            let total = range.rows().count();
            // Schema is computed over the *whole* range, before truncation.
            // Inferring a column's type from the first thousand rows of a
            // fifty-thousand-row sheet is how a column that turns numeric
            // halfway down gets reported as text.
            schema.insert(name.clone(), describe(&range));

            let rows: Vec<serde_json::Value> = range
                .rows()
                .take(self.max_rows)
                .map(|row| serde_json::Value::Array(row.iter().map(cell_to_json).collect()))
                .collect();

            if total > rows.len() {
                // Reported, never silent. A caller summing a column has to
                // know the column was cut off, or the total is wrong and
                // looks right.
                truncated.insert(
                    name.clone(),
                    serde_json::json!({ "returned": rows.len(), "total": total }),
                );
            }
            sheets.insert(name, serde_json::Value::Array(rows));
        }

        Command::Done {
            result: serde_json::json!({
                "path": self.path,
                "sheets": serde_json::Value::Object(sheets),
                "schema": serde_json::Value::Object(schema),
                "truncated": serde_json::Value::Object(truncated),
            }),
        }
    }
}

/// One cell as JSON.
///
/// Numbers stay numbers and strings stay strings, so a downstream node can do
/// arithmetic without reparsing. Dates and times are rendered as strings via
/// calamine's own conversion rather than left as the raw serial number: a
/// bare `45292.0` in a report is indistinguishable from a quantity, which is
/// exactly the kind of silently-wrong value this codebase tries not to
/// produce.
fn cell_to_json(cell: &calamine::Data) -> serde_json::Value {
    use calamine::Data;
    match cell {
        Data::Empty => serde_json::Value::Null,
        Data::String(s) => serde_json::Value::String(s.clone()),
        Data::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            // NaN and infinity have no JSON representation; a string keeps
            // the fact rather than dropping the cell.
            .unwrap_or_else(|| serde_json::Value::String(f.to_string())),
        Data::Int(i) => serde_json::Value::Number((*i).into()),
        Data::Bool(b) => serde_json::Value::Bool(*b),
        Data::DateTime(d) => serde_json::Value::String(
            d.as_datetime()
                .map(|dt| dt.to_string())
                .unwrap_or_else(|| d.as_f64().to_string()),
        ),
        Data::DateTimeIso(s) => serde_json::Value::String(s.clone()),
        Data::DurationIso(s) => serde_json::Value::String(s.clone()),
        // An error cell (#REF!, #DIV/0!) is a fact worth carrying, not a
        // blank: a blank reads as "no data", which is a different claim.
        Data::Error(e) => serde_json::Value::String(format!("#ERROR:{e:?}")),
    }
}

fn fail(code: &str, message: &str) -> Command {
    Command::Fail {
        code: code.into(),
        message: message.into(),
    }
}

export_block!(SheetExtract);

/// Where the table actually is, and what its columns hold.
///
/// Real workbooks rarely put a header at A1. A CMS budget-neutrality
/// workbook opens with a title, a blank row, an "as of" date, maybe a merged
/// banner, and only then the column names — and the data may be inset from
/// column A as well. A caller that assumes row 0 is the header gets a header
/// of `["Demonstration Budget Neutrality", "", "", ""]` and column names
/// that are really data.
///
/// So this reports the offsets rather than assuming them, and reports the
/// *evidence* alongside, because the detection is a heuristic and a Rhai
/// block cleaning this up for a knowledge base needs to be able to disagree
/// with it. Everything here is descriptive: nothing is dropped or rewritten,
/// and the rows are still returned verbatim.
fn describe(range: &calamine::Range<calamine::Data>) -> serde_json::Value {
    let rows: Vec<&[calamine::Data]> = range.rows().collect();
    // `start` is where the sheet's used range begins in the real grid —
    // a table inset from A1 would otherwise report column indices that do
    // not match the spreadsheet a human is looking at.
    let (row_offset, col_offset) = range.start().unwrap_or((0, 0));

    let header_row = detect_header_row(&rows);
    let data_start = header_row.map(|h| h + 1).unwrap_or(0);

    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let columns: Vec<serde_json::Value> = (0..width)
        .map(|c| {
            let name = header_row
                .and_then(|h| rows.get(h))
                .and_then(|r| r.get(c))
                .map(cell_to_text)
                .filter(|s| !s.is_empty());

            let cells: Vec<&calamine::Data> = rows
                .iter()
                .skip(data_start)
                .filter_map(|r| r.get(c))
                .collect();
            let filled = cells
                .iter()
                .filter(|c| !matches!(c, calamine::Data::Empty))
                .count();

            serde_json::json!({
                "index": c,
                // The spreadsheet's own column letter, so a finding can be
                // pointed at in Excel without arithmetic.
                "letter": column_letter(col_offset as usize + c),
                "name": name,
                "type": infer_type(&cells),
                "filled": filled,
                "of": cells.len(),
                // A sample is worth more than a type name when the type is
                // "mixed" and somebody has to decide what to do about it.
                "sample": cells
                    .iter()
                    .find(|c| !matches!(c, calamine::Data::Empty))
                    .map(|c| cell_to_json(c))
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();

    serde_json::json!({
        "header_row": header_row,
        "data_start_row": data_start,
        "rows": rows.len(),
        "columns": columns,
        // Where the used range sits in the real grid, so reported indices
        // can be translated back to what a person sees.
        "grid_origin": { "row": row_offset, "col": col_offset },
    })
}

/// Guess which row holds the column names.
///
/// The heuristic: among the first [`HEADER_SEARCH_DEPTH`] rows, prefer the
/// one with the most non-empty *text* cells that is also followed by a row
/// of comparable width. Titles and banners lose because they are one wide
/// cell; a header row loses to nothing because it is many narrow ones.
///
/// Returns `None` when no row looks like a header — a sheet that is pure
/// numbers, or empty. `None` is a real answer and better than pointing at
/// row 0 and calling it names.
fn detect_header_row(rows: &[&[calamine::Data]]) -> Option<usize> {
    const HEADER_SEARCH_DEPTH: usize = 25;

    let mut best: Option<(usize, usize)> = None;
    for (i, row) in rows.iter().enumerate().take(HEADER_SEARCH_DEPTH) {
        let texty = row
            .iter()
            .filter(|c| matches!(c, calamine::Data::String(s) if !s.trim().is_empty()))
            .count();
        if texty < 2 {
            // One text cell is a title, not a header.
            continue;
        }
        // The row beneath must carry something, or this is a trailing label
        // block rather than the top of a table.
        let below_filled = rows
            .get(i + 1)
            .map(|r| {
                r.iter()
                    .filter(|c| !matches!(c, calamine::Data::Empty))
                    .count()
            })
            .unwrap_or(0);
        if below_filled == 0 {
            continue;
        }
        if best.is_none_or(|(_, score)| texty > score) {
            best = Some((i, texty));
        }
    }
    best.map(|(i, _)| i)
}

/// What a column holds, across its data rows.
///
/// `mixed` is reported rather than resolved. A column of numbers with three
/// stray "N/A" strings is a fact the caller needs; silently calling it text
/// (or worse, numeric) is how a total ends up wrong and plausible.
fn infer_type(cells: &[&calamine::Data]) -> &'static str {
    use calamine::Data;
    let (mut num, mut text, mut date, mut boolean, mut err) = (0, 0, 0, 0, 0);
    for cell in cells {
        match cell {
            Data::Empty => {}
            Data::Int(_) | Data::Float(_) => num += 1,
            Data::String(_) => text += 1,
            Data::DateTime(_) | Data::DateTimeIso(_) | Data::DurationIso(_) => date += 1,
            Data::Bool(_) => boolean += 1,
            Data::Error(_) => err += 1,
        }
    }
    let present = num + text + date + boolean + err;
    if present == 0 {
        return "empty";
    }
    match (num, text, date, boolean, err) {
        (n, 0, 0, 0, 0) if n > 0 => "number",
        (0, t, 0, 0, 0) if t > 0 => "text",
        (0, 0, d, 0, 0) if d > 0 => "date",
        (0, 0, 0, b, 0) if b > 0 => "bool",
        _ => "mixed",
    }
}

/// Zero-based column index to its spreadsheet letter: 0 -> A, 26 -> AA.
fn column_letter(mut index: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

fn cell_to_text(cell: &calamine::Data) -> String {
    match cell {
        calamine::Data::String(s) => s.trim().to_string(),
        calamine::Data::Empty => String::new(),
        other => cell_to_json(other).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_letters_match_the_spreadsheet() {
        // So a finding can be pointed at in Excel without arithmetic.
        assert_eq!(column_letter(0), "A");
        assert_eq!(column_letter(25), "Z");
        assert_eq!(column_letter(26), "AA");
        assert_eq!(column_letter(27), "AB");
        assert_eq!(column_letter(51), "AZ");
        assert_eq!(column_letter(52), "BA");
    }

    #[test]
    fn a_column_of_numbers_with_stray_text_is_mixed_not_number() {
        use calamine::Data;
        let clean = [Data::Float(1.0), Data::Float(2.0), Data::Empty];
        let cells: Vec<&Data> = clean.iter().collect();
        assert_eq!(infer_type(&cells), "number");

        // The case that matters: three "N/A"s in a numeric column. Calling
        // this "number" is how a total comes out wrong and plausible.
        let dirty = [Data::Float(1.0), Data::String("N/A".into())];
        let cells: Vec<&Data> = dirty.iter().collect();
        assert_eq!(infer_type(&cells), "mixed");

        let none: Vec<&Data> = Vec::new();
        assert_eq!(infer_type(&none), "empty");
    }

    #[test]
    fn a_title_row_does_not_win_over_the_real_header() {
        use calamine::Data;
        // Shaped like a real workbook: title, blank, then the header.
        let r0 = vec![Data::String("Demonstration Budget Neutrality".into())];
        let r1 = vec![Data::Empty, Data::Empty];
        let r2 = vec![
            Data::String("member_months".into()),
            Data::String("cost".into()),
        ];
        let r3 = vec![Data::Float(1200.0), Data::Float(4567.89)];
        let rows: Vec<&[Data]> = vec![&r0, &r1, &r2, &r3];

        assert_eq!(
            detect_header_row(&rows),
            Some(2),
            "a one-cell title is not a header; the two-name row is"
        );
    }

    #[test]
    fn a_sheet_with_no_header_says_so_rather_than_naming_row_zero() {
        use calamine::Data;
        let r0 = vec![Data::Float(1.0), Data::Float(2.0)];
        let r1 = vec![Data::Float(3.0), Data::Float(4.0)];
        let rows: Vec<&[Data]> = vec![&r0, &r1];
        assert_eq!(
            detect_header_row(&rows),
            None,
            "pure numbers have no header, and guessing one invents column names"
        );
    }
}
