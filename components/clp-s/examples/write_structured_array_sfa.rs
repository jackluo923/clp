//! Writes a small heterogeneous structured-array archive through the library API.

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
            "usage: write_structured_array_sfa <output-path>",
        )
    })?;
    let mut archive = OpenArchive::new(
        File::create(output_path)?,
        WriterOptions::default().with_log_order(false),
    );

    let first_object = [
        FieldRef::new(b"name", ValueRef::String(b"first")),
        FieldRef::new(b"value", ValueRef::I64(1)),
    ];
    let second_object = [
        FieldRef::new(b"name", ValueRef::String(b"second")),
        FieldRef::new(b"value", ValueRef::I64(2)),
    ];
    let nested = [ValueRef::Bool(true), ValueRef::Bool(false)];
    let items = [
        ValueRef::Null,
        ValueRef::Object(&first_object),
        ValueRef::Object(&second_object),
        ValueRef::Array(&nested),
    ];
    let fields = [
        FieldRef::new(b"id", ValueRef::I64(0)),
        FieldRef::new(b"items", ValueRef::Array(&items)),
    ];
    archive.append_record(RecordRef::new(&fields))?;
    archive.finish()?;
    Ok(())
}
