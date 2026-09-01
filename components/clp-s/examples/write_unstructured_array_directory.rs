//! Writes the milestone-8 unstructured-array corpus as a canonical directory archive.

use std::error::Error;
use std::io;

use clp_s::writer::FieldRef;
use clp_s::writer::FsDirectoryArchiveSink;
use clp_s::writer::OpenDirectoryArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::UnstructuredArrayRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterOptions;

// Exact byte size of tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl.
const SOURCE_SIZE: u64 = 284;
const ARRAY_LEXEMES: &[&[u8]] = &[
    b"[]",
    br#"[1,true,null,"x",{"k":"v"},[2,3]]"#,
    br#"[2,false,null,"y",{"k":"w"},[4,5]]"#,
    br#"[ -7, 12.50 , "user=face", {"n": 9} ]"#,
    br#"["slash\\\\marker","\u0011\u0012\u0013"]"#,
    br#"[[],{},[{"x":[]}]]"#,
];

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let target = arguments.next().ok_or_else(usage)?;
    let staging = arguments.next().ok_or_else(usage)?;
    if arguments.next().is_some() {
        return Err(usage().into());
    }

    let options = WriterOptions::default()
        .with_log_order(false)
        .with_uncompressed_size(SOURCE_SIZE);
    let mut archive = OpenDirectoryArchive::new(options);
    for (kind, raw_json) in (0_i64..).zip(ARRAY_LEXEMES.iter().copied()) {
        let fields = [
            FieldRef::new(
                b"array",
                ValueRef::UnstructuredArray(UnstructuredArrayRef::new(raw_json)),
            ),
            FieldRef::new(b"kind", ValueRef::I64(kind)),
        ];
        archive.append_record(RecordRef::new(&fields))?;
    }
    let _target = archive
        .finish_to(FsDirectoryArchiveSink::new(target, staging))?
        .into_inner();
    Ok(())
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: write_unstructured_array_directory <target-directory> <staging-directory>",
    )
}
