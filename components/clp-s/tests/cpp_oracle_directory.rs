//! Public-API reconstruction of the committed C++ oracle through directory members.
//!
//! C++ writes the same eight byte sequences for a directory archive that it concatenates into an
//! SFA. The fixed ranges below expose the pinned SFA as its byte-identical canonical directory
//! layout without committing a second copy of the fixture.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_s::ExtractionMode;
use clp_s::ExtractionOptions;
use clp_s::ExtractionPlan;
use clp_s::ExtractionPlanLimits;
use clp_s::RecordLimits;
use clp_s::RecordProgram;
use clp_s::archive::ArchiveCatalogLimits;
use clp_s::archive::ColumnLimits;
use clp_s::archive::DirectoryArchiveMember;
use clp_s::archive::DirectoryArchiveReader;
use clp_s::archive::FsDirectoryArchiveSource;
use clp_s::archive::MetadataLimits;
use clp_s::archive::PackedStreamLimits;
use clp_s::extract_jsonl;

const CPP_SFA: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp.bin");
const MEMBER_RANGES: [(DirectoryArchiveMember, std::ops::Range<usize>); 8] = [
    (DirectoryArchiveMember::Header, 0..363),
    (DirectoryArchiveMember::SchemaTree, 363..471),
    (DirectoryArchiveMember::SchemaIds, 471..510),
    (DirectoryArchiveMember::TableMetadata, 510..541),
    (DirectoryArchiveMember::VariableDictionary, 541..570),
    (DirectoryArchiveMember::LogTypeDictionary, 570..609),
    (DirectoryArchiveMember::ArrayDictionary, 609..617),
    (DirectoryArchiveMember::PackedStreams, 617..654),
];

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn materialize_cpp_oracle() -> Self {
        let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-cpp-directory-oracle-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated C++ directory fixture");
        for (member, range) in MEMBER_RANGES {
            fs::write(path.join(member.file_name()), &CPP_SFA[range])
                .expect("write one byte-identical C++ directory member");
        }
        // C++ requests only canonical member names and tolerates unrelated directory entries.
        fs::write(path.join("unrelated-member"), b"ignored")
            .expect("write unrelated compatibility entry");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "failed to remove temporary directory fixture {}: {error}",
                self.0.display()
            );
        }
    }
}

#[test]
fn reconstructs_cpp_json_through_the_filesystem_directory_adapter() {
    let fixture = TempDirectory::materialize_cpp_oracle();
    let source = FsDirectoryArchiveSource::new(fixture.path());
    let mut archive = DirectoryArchiveReader::open(source, MetadataLimits::default())
        .expect("open pinned C++ directory layout");
    let catalog = archive
        .read_catalog(ArchiveCatalogLimits::default())
        .expect("load the shared checked catalog");
    let stream = archive
        .read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            0,
            PackedStreamLimits::default(),
        )
        .expect("read only the requested packed stream");
    let mut tables = catalog
        .schema_tables(0, &stream, ColumnLimits::default())
        .expect("create lazy table iterator");
    let table = tables
        .next()
        .expect("one schema table")
        .expect("decode one schema table");
    assert!(tables.next().is_none());

    let plan = ExtractionPlan::compile(
        table.schema(),
        catalog.schema_tree(),
        ExtractionPlanLimits::default(),
    )
    .expect("compile C++ schema extraction plan");
    let program = RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default())
        .expect("compile reusable record program");
    let mut writer = program
        .writer(table.table(), catalog.timestamp_patterns())
        .expect("bind record writer");
    let mut output = Vec::new();
    assert!(
        writer
            .append_next_record(&mut output)
            .expect("reconstruct the C++ fixture record")
    );
    output.push(b'\n');
    assert!(!writer.append_next_record(&mut output).expect("writer EOF"));

    assert_eq!(
        include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl").as_slice(),
        output
    );

    let mut high_level_output = Vec::new();
    let stats = extract_jsonl(
        &mut archive,
        &mut high_level_output,
        ExtractionOptions::new(ExtractionMode::Unordered),
    )
    .expect("extract the directory through the format-independent high-level reader");
    assert_eq!(1, stats.streams());
    assert_eq!(1, stats.tables());
    assert_eq!(1, stats.records());
    assert_eq!(57, stats.decoded_bytes());
    assert_eq!(
        include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl").as_slice(),
        high_level_output
    );
}
