//! Extracts one CLP-S single-file archive in canonical log-event order.

use std::env;
use std::error::Error;
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::path::PathBuf;

use clp_s::ExtractionMode;
use clp_s::ExtractionOptions;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::extract_jsonl;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let archive_path = arguments.next().map(PathBuf::from).ok_or_else(usage)?;
    let output_path = arguments.next().map(PathBuf::from).ok_or_else(usage)?;
    if arguments.next().is_some() {
        return Err(usage().into());
    }

    let source = File::open(archive_path)?;
    let mut reader = SingleFileArchiveReader::open(source)?;
    let mut output = BufWriter::new(File::create(output_path)?);
    extract_jsonl(
        &mut reader,
        &mut output,
        ExtractionOptions::new(ExtractionMode::LogOrder),
    )?;
    output.flush()?;
    Ok(())
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: cargo run -p clp-s --example extract_sfa_ordered -- ARCHIVE OUTPUT",
    )
}
