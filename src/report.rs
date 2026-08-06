//! Decoding App Store Connect analytics report segments.
//!
//! The Analytics Reports API hands out pre-signed URLs, not data: a segment is a
//! gzipped delimited-text file sitting on Apple's CDN. Listing segments and
//! stopping there leaves the one question people actually ask — how did the app
//! do last month — unanswerable without leaving the conversation.
//!
//! This module turns those bytes into JSON rows. Apple emits tab-separated data
//! today but documents the segments as CSV, so the delimiter is detected from
//! the header rather than assumed.

use serde_json::{json, Map, Value};

use crate::error::AscError;

/// Refuse to decompress beyond this, so a malformed or hostile segment can't
/// exhaust memory. Comfortably above any real report segment.
const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

/// Hard ceiling on rows returned in one call, whatever the caller asks for.
pub const MAX_ROWS_LIMIT: usize = 5_000;

/// Default number of rows returned when the caller doesn't say.
pub const DEFAULT_MAX_ROWS: usize = 100;

/// A parsed report segment.
#[derive(Debug, PartialEq)]
pub struct Report {
    /// Column names, in file order.
    pub columns: Vec<String>,
    /// Rows as objects keyed by column name, capped at the requested limit.
    pub rows: Vec<Value>,
    /// Total data rows in the file, whether or not they were returned.
    pub total_rows: usize,
}

impl Report {
    /// Render as the tool result document.
    pub fn to_json(&self) -> Value {
        json!({
            "columns": self.columns,
            "totalRows": self.total_rows,
            "rowsReturned": self.rows.len(),
            "truncated": self.rows.len() < self.total_rows,
            "rows": self.rows,
        })
    }
}

/// Decompress a segment if it is gzipped, otherwise pass the bytes through.
///
/// Segments arrive gzipped, but the URL is opaque and Apple has changed
/// encodings before; sniffing the magic number costs nothing and means an
/// already-decompressed body still works.
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>, AscError> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_DECOMPRESSED_BYTES)
        .read_to_end(&mut out)
        .map_err(|e| AscError::Parse(format!("could not gunzip the report segment: {e}")))?;
    if out.len() as u64 >= MAX_DECOMPRESSED_BYTES {
        return Err(AscError::Parse(format!(
            "report segment expands beyond the {MAX_DECOMPRESSED_BYTES}-byte decompression limit"
        )));
    }
    Ok(out)
}

/// Parse delimited report text into rows keyed by column name.
///
/// Returns at most `max_rows` rows but always counts them all, so a caller can
/// tell a 20-row sample from a 20-row report.
pub fn parse(text: &str, max_rows: usize) -> Result<Report, AscError> {
    let max_rows = max_rows.min(MAX_ROWS_LIMIT);
    let delimiter = detect_delimiter(text);

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .from_reader(text.as_bytes());

    let columns: Vec<String> = reader
        .headers()
        .map_err(|e| AscError::Parse(format!("could not read the report header row: {e}")))?
        .iter()
        .map(str::to_string)
        .collect();

    let mut rows = Vec::new();
    let mut total_rows = 0usize;
    for record in reader.records() {
        let record =
            record.map_err(|e| AscError::Parse(format!("could not read a report row: {e}")))?;
        total_rows += 1;
        if rows.len() < max_rows {
            let mut row = Map::new();
            for (index, field) in record.iter().enumerate() {
                let key = columns
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("column{index}"));
                row.insert(key, Value::String(field.to_string()));
            }
            rows.push(Value::Object(row));
        }
    }

    Ok(Report {
        columns,
        rows,
        total_rows,
    })
}

/// Pick the delimiter from the header line: tab if it has one, else comma.
fn detect_delimiter(text: &str) -> u8 {
    let header = text.lines().next().unwrap_or_default();
    if header.contains('\t') {
        b'\t'
    } else {
        b','
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    const TSV: &str = "Date\tApp Name\tUnits\n2026-07-01\tMy App\t142\n2026-07-02\tMy App\t97\n";

    #[test]
    fn gzipped_segments_round_trip() {
        assert_eq!(decompress(&gzip(TSV.as_bytes())).unwrap(), TSV.as_bytes());
    }

    #[test]
    fn plain_segments_pass_through_untouched() {
        assert_eq!(decompress(TSV.as_bytes()).unwrap(), TSV.as_bytes());
    }

    #[test]
    fn corrupt_gzip_reports_a_parse_error() {
        let mut corrupt = gzip(TSV.as_bytes());
        corrupt.truncate(corrupt.len() / 2);
        let err = decompress(&corrupt).unwrap_err();
        assert!(err.to_string().contains("gunzip"), "{err}");
    }

    #[test]
    fn tab_separated_reports_parse_into_keyed_rows() {
        let report = parse(TSV, 10).unwrap();
        assert_eq!(report.columns, ["Date", "App Name", "Units"]);
        assert_eq!(report.total_rows, 2);
        assert_eq!(report.rows[0]["App Name"], "My App");
        assert_eq!(report.rows[1]["Units"], "97");
    }

    #[test]
    fn comma_separated_reports_parse_too() {
        let report = parse("Date,Units\n2026-07-01,142\n", 10).unwrap();
        assert_eq!(report.columns, ["Date", "Units"]);
        assert_eq!(report.rows[0]["Units"], "142");
    }

    #[test]
    fn quoted_commas_stay_inside_their_field() {
        let report = parse("Name,Units\n\"Widgets, Deluxe\",7\n", 10).unwrap();
        assert_eq!(report.rows[0]["Name"], "Widgets, Deluxe");
        assert_eq!(report.rows[0]["Units"], "7");
    }

    #[test]
    fn a_header_only_segment_yields_no_rows() {
        let report = parse("Date\tUnits\n", 10).unwrap();
        assert_eq!(report.total_rows, 0);
        assert!(report.rows.is_empty());
        assert_eq!(report.columns, ["Date", "Units"]);
    }

    #[test]
    fn row_cap_limits_the_sample_but_not_the_count() {
        let mut text = String::from("Date\tUnits\n");
        for i in 0..500 {
            text.push_str(&format!("2026-07-{:02}\t{i}\n", i % 28 + 1));
        }
        let report = parse(&text, 5).unwrap();
        assert_eq!(report.rows.len(), 5);
        assert_eq!(report.total_rows, 500, "row count must reflect the file");
        assert_eq!(report.to_json()["truncated"], true);
    }

    #[test]
    fn the_row_cap_cannot_be_raised_past_the_hard_limit() {
        let mut text = String::from("Units\n");
        for i in 0..(MAX_ROWS_LIMIT + 10) {
            text.push_str(&format!("{i}\n"));
        }
        let report = parse(&text, usize::MAX).unwrap();
        assert_eq!(report.rows.len(), MAX_ROWS_LIMIT);
        assert_eq!(report.total_rows, MAX_ROWS_LIMIT + 10);
    }

    #[test]
    fn ragged_rows_do_not_abort_the_parse() {
        // Apple pads some report families unevenly; a short row must not fail.
        let report = parse("A\tB\tC\n1\t2\n3\t4\t5\t6\n", 10).unwrap();
        assert_eq!(report.total_rows, 2);
        assert_eq!(report.rows[0]["A"], "1");
        assert!(report.rows[0].get("C").is_none());
        assert_eq!(report.rows[1]["column3"], "6");
    }

    #[test]
    fn json_rendering_reports_completeness() {
        let full = parse(TSV, 10).unwrap().to_json();
        assert_eq!(full["truncated"], false);
        assert_eq!(full["rowsReturned"], 2);
        assert_eq!(full["totalRows"], 2);
    }
}
