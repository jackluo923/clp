//! Writes a small v0.5 archive containing two primitive schema shapes.

use std::error::Error;
use std::fs::File;
use std::io;

use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterOptions;

fn main() -> Result<(), Box<dyn Error>> {
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_primitive_sfa <output-path>",
        )
    })?;
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_uncompressed_size(152);
    let mut archive = OpenArchive::new(File::create(output_path)?, options);

    let first_metrics = [
        FieldRef::new(b"load", ValueRef::F64(1.25)),
        FieldRef::new(b"ok", ValueRef::Bool(true)),
    ];
    let first = [
        FieldRef::new(b"id", ValueRef::I64(-7)),
        FieldRef::new(b"metrics", ValueRef::Object(&first_metrics)),
        FieldRef::new(b"missing", ValueRef::Null),
    ];
    archive.append_record(RecordRef::new(&first))?;

    let second_metrics = [
        FieldRef::new(b"load", ValueRef::F64(2.5)),
        FieldRef::new(b"ok", ValueRef::Bool(false)),
    ];
    let second = [
        FieldRef::new(b"missing", ValueRef::Null),
        FieldRef::new(b"metrics", ValueRef::Object(&second_metrics)),
        FieldRef::new(b"id", ValueRef::I64(42)),
    ];
    archive.append_record(RecordRef::new(&second))?;

    let third = [
        FieldRef::new(b"id", ValueRef::I64(9)),
        FieldRef::new(b"enabled", ValueRef::Bool(false)),
    ];
    archive.append_record(RecordRef::new(&third))?;
    archive.finish()?;
    Ok(())
}
