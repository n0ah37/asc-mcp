//! Decoding App Store Connect analytics report segments.
//!
//! The Analytics Reports API hands out pre-signed URLs, not data: a segment is a
//! gzipped delimited-text file sitting on Apple's CDN. Listing segments and
//! stopping there leaves the one question people actually ask — how did the app
//! do last month — unanswerable without leaving the conversation.
//!
//! This module turns those bytes into JSON rows, **streaming**: the gzip decoder
//! feeds the CSV reader directly, so the decompressed text is never held in
//! memory. That matters more than it sounds — a 1.2 MB segment expands to 10.6 MB,
//! and materializing it cost about ten times the download in resident memory.
//! Streaming makes peak memory the compressed buffer plus a parser window,
//! whatever the report's size.
//!
//! Apple emits tab-separated data today but documents the segments as CSV, so
//! the delimiter is sniffed from the header rather than assumed.

use std::io::{BufRead, BufReader, Read};

use serde_json::{json, Map, Value};

use crate::error::AscError;

/// Refuse to decompress beyond this, so a malformed or hostile segment can't
/// occupy the parser indefinitely. Comfortably above any real report segment.
const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

/// How much of the stream to buffer, and how much of it the delimiter sniff sees.
const READ_BUFFER: usize = 64 * 1024;

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

/// Parse a downloaded segment — gzipped or not — into rows keyed by column name.
///
/// Returns at most `max_rows` rows but always counts them all, so a caller can
/// tell a 20-row sample from a 20-row report.
///
/// This is synchronous, CPU-bound work (measured at ~28 ms for 300k rows, and it
/// scales linearly), so callers on an async runtime must hand it to
/// `spawn_blocking` rather than stall a worker thread with it.
pub fn parse_segment(bytes: &[u8], max_rows: usize) -> Result<Report, AscError> {
    parse_segment_within(bytes, max_rows, MAX_DECOMPRESSED_BYTES)
}

/// [`parse_segment`] with an explicit decompression ceiling, so the limit is
/// testable without building a quarter-gigabyte fixture.
fn parse_segment_within(bytes: &[u8], max_rows: usize, limit: u64) -> Result<Report, AscError> {
    let raw: Box<dyn Read + '_> = if bytes.starts_with(&[0x1f, 0x8b]) {
        // Sniffing the magic number rather than trusting the URL means an
        // already-decompressed body still works.
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else {
        Box::new(bytes)
    };

    let mut reader = BufReader::with_capacity(READ_BUFFER, LimitReader::new(raw, limit));
    let delimiter = sniff_delimiter(&mut reader)?;
    parse_delimited(reader, delimiter, max_rows)
}

/// A reader that **fails** past a byte limit rather than truncating.
///
/// `Read::take` would end the stream silently, and a report that quietly loses
/// its tail is worse than one that refuses to load: the row count would look
/// authoritative while being wrong.
struct LimitReader<R> {
    inner: R,
    /// Bytes still permitted, seeded one over the limit so a stream of exactly
    /// the limit still reaches a clean EOF.
    remaining: u64,
}

impl<R: Read> LimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit.saturating_add(1),
        }
    }
}

impl<R: Read> Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "report segment is larger than the decompression limit",
            ));
        }
        let cap = buf.len().min(self.remaining as usize);
        let read = self.inner.read(&mut buf[..cap])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// Pick the delimiter from the buffered header: tab if it has one, else comma.
///
/// Peeks with `fill_buf`, which does not consume, so the header still reaches
/// the CSV reader.
fn sniff_delimiter<R: Read>(reader: &mut BufReader<R>) -> Result<u8, AscError> {
    let buffered = reader
        .fill_buf()
        .map_err(|e| AscError::Parse(format!("could not read the report segment: {e}")))?;
    let header_end = buffered
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(buffered.len());
    Ok(if buffered[..header_end].contains(&b'\t') {
        b'\t'
    } else {
        b','
    })
}

fn parse_delimited<R: Read>(reader: R, delimiter: u8, max_rows: usize) -> Result<Report, AscError> {
    let max_rows = max_rows.min(MAX_ROWS_LIMIT);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .from_reader(reader);

    let columns: Vec<String> = reader
        .headers()
        .map_err(|e| AscError::Parse(format!("could not read the report header row: {e}")))?
        .iter()
        .map(str::to_string)
        .collect();

    let mut rows = Vec::new();
    let mut total_rows = 0usize;
    // Reused across rows so counting a large report doesn't allocate per row.
    let mut record = csv::StringRecord::new();
    while reader
        .read_record(&mut record)
        .map_err(|e| AscError::Parse(format!("could not read a report row: {e}")))?
    {
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
    fn gzipped_segments_parse() {
        let report = parse_segment(&gzip(TSV.as_bytes()), 10).unwrap();
        assert_eq!(report.columns, ["Date", "App Name", "Units"]);
        assert_eq!(report.total_rows, 2);
        assert_eq!(report.rows[0]["App Name"], "My App");
        assert_eq!(report.rows[1]["Units"], "97");
    }

    #[test]
    fn plain_segments_parse_too() {
        let report = parse_segment(TSV.as_bytes(), 10).unwrap();
        assert_eq!(report.total_rows, 2);
        assert_eq!(report.rows[0]["Date"], "2026-07-01");
    }

    #[test]
    fn corrupt_gzip_reports_a_parse_error() {
        let mut corrupt = gzip(TSV.as_bytes());
        corrupt.truncate(corrupt.len() / 2);
        let err = parse_segment(&corrupt, 10).unwrap_err();
        assert!(matches!(err, AscError::Parse(_)), "{err:?}");
    }

    #[test]
    fn comma_separated_reports_parse() {
        let report = parse_segment(b"Date,Units\n2026-07-01,142\n", 10).unwrap();
        assert_eq!(report.columns, ["Date", "Units"]);
        assert_eq!(report.rows[0]["Units"], "142");
    }

    #[test]
    fn quoted_commas_stay_inside_their_field() {
        let report = parse_segment(b"Name,Units\n\"Widgets, Deluxe\",7\n", 10).unwrap();
        assert_eq!(report.rows[0]["Name"], "Widgets, Deluxe");
        assert_eq!(report.rows[0]["Units"], "7");
    }

    #[test]
    fn a_tab_in_the_header_wins_over_commas_in_the_data() {
        let report = parse_segment(b"Name\tUnits\nWidgets, Deluxe\t7\n", 10).unwrap();
        assert_eq!(report.columns, ["Name", "Units"]);
        assert_eq!(report.rows[0]["Name"], "Widgets, Deluxe");
    }

    #[test]
    fn a_header_only_segment_yields_no_rows() {
        let report = parse_segment(b"Date\tUnits\n", 10).unwrap();
        assert_eq!(report.total_rows, 0);
        assert!(report.rows.is_empty());
        assert_eq!(report.columns, ["Date", "Units"]);
    }

    #[test]
    fn an_empty_segment_does_not_panic() {
        let report = parse_segment(b"", 10).unwrap();
        assert_eq!(report.total_rows, 0);
        assert!(report.columns.is_empty());
    }

    #[test]
    fn row_cap_limits_the_sample_but_not_the_count() {
        let mut text = String::from("Date\tUnits\n");
        for i in 0..500 {
            text.push_str(&format!("2026-07-{:02}\t{i}\n", i % 28 + 1));
        }
        let report = parse_segment(text.as_bytes(), 5).unwrap();
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
        let report = parse_segment(text.as_bytes(), usize::MAX).unwrap();
        assert_eq!(report.rows.len(), MAX_ROWS_LIMIT);
        assert_eq!(report.total_rows, MAX_ROWS_LIMIT + 10);
    }

    #[test]
    fn a_report_larger_than_the_read_buffer_parses_whole() {
        // Spans many buffer refills, so a bug in the streaming loop shows here.
        let mut text = String::from("Date\tUnits\tPadding\n");
        for i in 0..20_000 {
            text.push_str(&format!(
                "2026-07-01\t{i}\tpadding padding padding padding\n"
            ));
        }
        assert!(text.len() > READ_BUFFER * 10);
        let report = parse_segment(&gzip(text.as_bytes()), 3).unwrap();
        assert_eq!(report.total_rows, 20_000);
        assert_eq!(report.rows.len(), 3);
    }

    #[test]
    fn a_segment_past_the_decompression_limit_fails_rather_than_truncating() {
        // Silently dropping the tail would report a confident, wrong row count.
        let mut text = String::from("Units\n");
        for i in 0..1_000 {
            text.push_str(&format!("{i}\n"));
        }
        let err = parse_segment_within(text.as_bytes(), 10, 256).unwrap_err();
        assert!(matches!(err, AscError::Parse(_)), "{err:?}");
        assert!(err.to_string().contains("decompression limit"), "{err}");
    }

    #[test]
    fn a_segment_exactly_at_the_limit_still_parses() {
        let text = "Units\n1\n2\n";
        let report = parse_segment_within(text.as_bytes(), 10, text.len() as u64).unwrap();
        assert_eq!(report.total_rows, 2);
    }

    #[test]
    fn ragged_rows_do_not_abort_the_parse() {
        // Apple pads some report families unevenly; a short row must not fail.
        let report = parse_segment(b"A\tB\tC\n1\t2\n3\t4\t5\t6\n", 10).unwrap();
        assert_eq!(report.total_rows, 2);
        assert_eq!(report.rows[0]["A"], "1");
        assert!(report.rows[0].get("C").is_none());
        assert_eq!(report.rows[1]["column3"], "6");
    }

    #[test]
    fn json_rendering_reports_completeness() {
        let full = parse_segment(TSV.as_bytes(), 10).unwrap().to_json();
        assert_eq!(full["truncated"], false);
        assert_eq!(full["rowsReturned"], 2);
        assert_eq!(full["totalRows"], 2);
    }
}
