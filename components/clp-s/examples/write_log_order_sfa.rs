//! Writes the milestone-5 multi-schema log-order interoperability corpus as a v0.5 archive.

use std::error::Error;
use std::fs::File;
use std::io;

use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterOptions;

// Exact byte size of tests/fixtures/sfa-v0.5.0-log-order-cpp-input.jsonl.
const SOURCE_SIZE: u64 = 60;

fn main() -> Result<(), Box<dyn Error>> {
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_log_order_sfa <output-path>",
        )
    })?;
    let options = WriterOptions::default().with_uncompressed_size(SOURCE_SIZE);
    let mut archive = OpenArchive::new(File::create(output_path)?, options);

    append(&mut archive, b"a", ValueRef::I64(10))?;
    append(&mut archive, b"b", ValueRef::Bool(true))?;
    append(&mut archive, b"a", ValueRef::I64(20))?;
    append(&mut archive, b"c", ValueRef::String(b"x"))?;
    append(&mut archive, b"b", ValueRef::Bool(false))?;
    append(&mut archive, b"a", ValueRef::I64(30))?;
    archive.finish()?;
    Ok(())
}

fn append<W>(
    archive: &mut OpenArchive<W>,
    key: &[u8],
    value: ValueRef<'_>,
) -> Result<(), clp_s::writer::AppendError> {
    let fields = [FieldRef::new(key, value)];
    archive.append_record(RecordRef::new(&fields))
}
