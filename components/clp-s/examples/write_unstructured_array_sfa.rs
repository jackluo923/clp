//! Writes the milestone-7 default-mode unstructured-array interoperability corpus.

use std::error::Error;
use std::fs::File;
use std::io;

use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
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
    let output_path = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: write_unstructured_array_sfa <output-path>",
        )
    })?;
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_uncompressed_size(SOURCE_SIZE);
    let mut archive = OpenArchive::new(File::create(output_path)?, options);
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
    archive.finish()?;
    Ok(())
}
