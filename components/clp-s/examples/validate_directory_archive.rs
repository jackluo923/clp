//! Validates and reconstructs CLP-S directory archives through the public library API.

use std::env;
use std::error::Error;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use clp_s::ExtractionMode;
use clp_s::ExtractionOptions;
use clp_s::archive::DirectoryArchiveReader;
use clp_s::archive::FsDirectoryArchiveSource;
use clp_s::archive::MetadataLimits;
use clp_s::extract_jsonl;

fn main() -> Result<(), Box<dyn Error>> {
    let paths = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: cargo run -p clp-s --example validate_directory_archive -- ARCHIVE_DIR...",
        )
        .into());
    }

    for path in paths {
        validate(&path)?;
    }
    Ok(())
}

fn validate(path: &Path) -> Result<(), Box<dyn Error>> {
    let source = FsDirectoryArchiveSource::new(path);
    let mut reader = DirectoryArchiveReader::open(source, MetadataLimits::default())?;
    let stats = extract_jsonl(
        &mut reader,
        &mut io::sink(),
        ExtractionOptions::new(ExtractionMode::Unordered),
    )?;
    println!(
        "{}: {} streams, {} tables, {} records, {} decoded bytes",
        path.display(),
        stats.streams(),
        stats.tables(),
        stats.records(),
        stats.decoded_bytes()
    );
    Ok(())
}
