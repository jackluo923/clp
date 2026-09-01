//! Writes the milestone-6 exact timestamp interoperability corpus as a v0.5 archive.

use std::error::Error;
use std::fs::File;
use std::io;

use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::TimestampRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterOptions;

// Exact byte size of tests/fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl.
const SOURCE_SIZE: u64 = 144;
const RANGE_KEY: &str = "ts";
const DATE_PATTERN: &str = r#""\Y-\m-\dT\H:\M:\S.\3""#;

fn main() -> Result<(), Box<dyn Error>> {
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_timestamp_sfa <output-path>",
        )
    })?;
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_uncompressed_size(SOURCE_SIZE);
    let mut archive = OpenArchive::new(File::create(output_path)?, options);

    append(
        &mut archive,
        TimestampRef::new(
            1_422_752_523_004_000_000,
            r#""2015-02-01T01:02:03.004""#,
            DATE_PATTERN,
            RANGE_KEY,
        ),
        1,
    )?;
    append(
        &mut archive,
        TimestampRef::new(1_700_000_000_123_000_000, "1700000000123", r"\L", RANGE_KEY),
        2,
    )?;
    append(
        &mut archive,
        TimestampRef::new(
            1_422_752_524_004_000_000,
            r#""2015-02-01T01:02:04.004""#,
            DATE_PATTERN,
            RANGE_KEY,
        ),
        3,
    )?;
    append(
        &mut archive,
        TimestampRef::new(1_700_000_001_123_000_000, "1700000001123", r"\L", RANGE_KEY),
        4,
    )?;
    archive.finish()?;
    Ok(())
}

fn append<W>(
    archive: &mut OpenArchive<W>,
    timestamp: TimestampRef<'_>,
    kind: i64,
) -> Result<(), clp_s::writer::AppendError> {
    let fields = [
        FieldRef::new(b"ts", ValueRef::Timestamp(timestamp)),
        FieldRef::new(b"kind", ValueRef::I64(kind)),
    ];
    archive.append_record(RecordRef::new(&fields))
}
