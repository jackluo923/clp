//! Writes the milestone-4 retained-float interoperability corpus as a v0.5 archive.

use std::error::Error;
use std::fs::File;
use std::io;
use std::str;

use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::RetainedFloatRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterOptions;

// Exact byte size of tests/fixtures/sfa-v0.5.0-retained-floats-cpp-input.jsonl.
const SOURCE_SIZE: u64 = 381;

fn main() -> Result<(), Box<dyn Error>> {
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_retained_float_sfa <output-path>",
        )
    })?;
    let mut archive = OpenArchive::new(
        File::create(output_path)?,
        WriterOptions::default()
            .with_log_order(false)
            .with_uncompressed_size(SOURCE_SIZE),
    );
    let rows: &[(&[u8], &[u8])] = &[
        (b"-0.00", b"123456789.123456789"),
        (b"123456789.000", b"123456789.123456700"),
        (
            b"0.00000000000000000001234567891234567",
            b"123456789.123456789",
        ),
        (b"1.234567891234567E+0009", b"12.345e6"),
        (b"4.9406564584124654e-324", b"1.0e00000"),
        (b"1.7976931348623157E308", b"1.2345678912345679e+13"),
    ];
    for (formatted, fallback) in rows {
        let formatted_value = parse_finite(formatted)?;
        let fallback_value = parse_finite(fallback)?;
        let fields = [
            FieldRef::new(
                b"formatted",
                ValueRef::RetainedFloat(RetainedFloatRef::new(formatted_value, formatted)),
            ),
            FieldRef::new(
                b"fallback",
                ValueRef::RetainedFloat(RetainedFloatRef::new(fallback_value, fallback)),
            ),
        ];
        archive.append_record(RecordRef::new(&fields))?;
    }
    archive.finish()?;
    Ok(())
}

fn parse_finite(source: &[u8]) -> Result<f64, Box<dyn Error>> {
    let value = str::from_utf8(source)?.parse::<f64>()?;
    if !value.is_finite() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "non-finite fixture value").into());
    }
    Ok(value)
}
