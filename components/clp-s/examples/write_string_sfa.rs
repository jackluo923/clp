//! Writes the milestone-3 string interoperability corpus as a v0.5 archive.

use std::error::Error;
use std::fs::File;
use std::io;

use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterOptions;

// Exact byte size of tests/fixtures/sfa-v0.5.0-strings-cpp-input.jsonl.
const SOURCE_SIZE: u64 = 205;

fn main() -> Result<(), Box<dyn Error>> {
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_string_sfa <output-path>",
        )
    })?;
    let mut archive = OpenArchive::new(
        File::create(output_path)?,
        WriterOptions::default()
            .with_log_order(false)
            .with_uncompressed_size(SOURCE_SIZE),
    );
    let rows: &[(&[u8], &[u8])] = &[
        (b"YScope", b"uid=0 CPU=99.99 user=YScope"),
        (b"a\tb", b"uid=-9223372036854775808 CPU=-00.00 user=face"),
        (b"YScope", b"plain words"),
        (b"YScope", b"literal \\ \x11 \x12 \x13 done"),
    ];
    for (variable, clp) in rows {
        let fields = [
            FieldRef::new(b"v", ValueRef::String(variable)),
            FieldRef::new(b"c", ValueRef::String(clp)),
        ];
        archive.append_record(RecordRef::new(&fields))?;
    }
    archive.finish()?;
    Ok(())
}
