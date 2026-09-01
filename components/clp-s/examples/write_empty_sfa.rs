//! Writes one canonical empty v0.5 structured single-file archive.

use std::error::Error;
use std::fs::File;
use std::io;

use clp_s::writer::OpenArchive;
use clp_s::writer::WriterOptions;

fn main() -> Result<(), Box<dyn Error>> {
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_empty_sfa <output-path>",
        )
    })?;
    let output = File::create(output_path)?;
    OpenArchive::new(output, WriterOptions::default().with_log_order(false)).finish()?;
    Ok(())
}
