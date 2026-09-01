#![cfg(feature = "cli")]

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_s::archive::DirectoryArchiveMember;

const LOG_ORDER_ARCHIVE: &str = "sfa-v0.5.0-log-order-cpp.bin";
const LOG_ORDER_JSONL: &str = "sfa-v0.5.0-log-order-cpp-input.jsonl";
const MINIMAL_ARCHIVE: &str = "sfa-v0.5.0-minimal-cpp.bin";
const MINIMAL_JSONL: &str = "sfa-v0.5.0-minimal-cpp-input.jsonl";
const STRINGS_ARCHIVE: &str = "sfa-v0.5.0-strings-cpp.bin";
const STRINGS_JSONL: &str = "sfa-v0.5.0-strings-cpp-input.jsonl";
const MINIMAL_SFA: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp.bin");
const MINIMAL_DIRECTORY_RANGES: [(DirectoryArchiveMember, std::ops::Range<usize>); 8] = [
    (DirectoryArchiveMember::Header, 0..363),
    (DirectoryArchiveMember::SchemaTree, 363..471),
    (DirectoryArchiveMember::SchemaIds, 471..510),
    (DirectoryArchiveMember::TableMetadata, 510..541),
    (DirectoryArchiveMember::VariableDictionary, 541..570),
    (DirectoryArchiveMember::LogTypeDictionary, 570..609),
    (DirectoryArchiveMember::ArrayDictionary, 609..617),
    (DirectoryArchiveMember::PackedStreams, 617..654),
];

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-rust-cli-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique CLI test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn clp_s(arguments: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clp-s"))
        .args(arguments)
        .output()
        .expect("run Rust clp-s binary")
}

fn materialize_minimal_directory_archive(path: &Path) {
    fs::create_dir(path).expect("create directory archive");
    for (member, range) in MINIMAL_DIRECTORY_RANGES {
        fs::write(path.join(member.file_name()), &MINIMAL_SFA[range])
            .expect("write canonical directory member");
    }
}

#[test]
fn unordered_extraction_appends_exact_cpp_jsonl() {
    let temporary = TestDirectory::new("unordered");
    let output_dir = temporary.path().join("output");
    fs::create_dir(&output_dir).expect("create output directory");
    fs::write(output_dir.join("original"), b"seed\n").expect("seed append destination");
    let archive = fixture(MINIMAL_ARCHIVE);

    let first = clp_s(&["x".as_ref(), archive.as_os_str(), output_dir.as_os_str()]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(b"", first.stdout.as_slice());

    let second = clp_s(&["x".as_ref(), archive.as_os_str(), output_dir.as_os_str()]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(b"", second.stdout.as_slice());

    let expected_record = fs::read(fixture(MINIMAL_JSONL)).expect("read expected JSONL");
    let mut expected = b"seed\n".to_vec();
    expected.extend_from_slice(&expected_record);
    expected.extend_from_slice(&expected_record);
    assert_eq!(expected, fs::read(output_dir.join("original")).unwrap());
}

#[test]
fn ordered_extraction_rotates_only_after_complete_records_and_prints_stats() {
    let temporary = TestDirectory::new("ordered");
    let output_dir = temporary.path().join("output");
    let archive = fixture(LOG_ORDER_ARCHIVE);
    let result = clp_s(&[
        "x".as_ref(),
        archive.as_os_str(),
        output_dir.as_os_str(),
        "--ordered".as_ref(),
        "--target-ordered-chunk-size".as_ref(),
        "20".as_ref(),
        "--print-ordered-chunk-stats".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());

    let ordered = fs::read_to_string(fixture(LOG_ORDER_JSONL)).expect("read ordered oracle");
    let records = ordered.split_inclusive('\n').collect::<Vec<_>>();
    let chunk_specs = [
        (0_u64, 2_u64, records[..2].concat()),
        (2, 5, records[2..5].concat()),
        (5, 6, records[5..].concat()),
    ];
    let mut expected_stats = String::new();
    for (begin, end, expected) in chunk_specs {
        let path = output_dir.join(format!("{LOG_ORDER_ARCHIVE}_{begin}_{end}.jsonl"));
        assert_eq!(expected.as_bytes(), fs::read(&path).unwrap());
        writeln!(&mut expected_stats, "{{\"path\":\"{}\"}}", path.display())
            .expect("write expected chunk statistic");
    }
    assert_eq!(expected_stats.as_bytes(), result.stdout);
    assert!(!output_dir.join("original").exists());
    assert!(!output_dir.join(LOG_ORDER_ARCHIVE).exists());
}

#[test]
fn missing_order_metadata_falls_back_without_touching_the_temporary_basename() {
    let temporary = TestDirectory::new("fallback");
    let output_dir = temporary.path().join("output");
    fs::create_dir(&output_dir).expect("create output directory");
    let sentinel = output_dir.join(STRINGS_ARCHIVE);
    fs::write(&sentinel, b"sentinel").expect("seed basename sentinel");
    let archive = fixture(STRINGS_ARCHIVE);

    let result = clp_s(&[
        "x".as_ref(),
        archive.as_os_str(),
        output_dir.as_os_str(),
        "--ordered".as_ref(),
        "--target-ordered-chunk-size".as_ref(),
        "1".as_ref(),
        "--print-ordered-chunk-stats".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert!(String::from_utf8_lossy(&result.stderr).contains("falling back to physical order"));
    assert_eq!(b"sentinel", fs::read(sentinel).unwrap().as_slice());
    assert_eq!(
        fs::read(fixture(STRINGS_JSONL)).unwrap(),
        fs::read(output_dir.join("original")).unwrap()
    );
}

#[test]
fn archive_id_selects_one_direct_directory_archive() {
    let temporary = TestDirectory::new("archive-id");
    let archives = temporary.path().join("archives");
    fs::create_dir(&archives).expect("create archive container");
    materialize_minimal_directory_archive(&archives.join("selected"));
    fs::copy(fixture(LOG_ORDER_ARCHIVE), archives.join("ignored")).expect("copy ignored archive");
    let output_dir = temporary.path().join("output");

    let result = clp_s(&[
        "x".as_ref(),
        archives.as_os_str(),
        output_dir.as_os_str(),
        "--archive-id".as_ref(),
        "selected".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(fixture(MINIMAL_JSONL)).unwrap(),
        fs::read(output_dir.join("original")).unwrap()
    );
}

#[test]
fn invalid_archive_creates_no_output_file() {
    let temporary = TestDirectory::new("invalid-archive");
    let archive = temporary.path().join("not-an-archive");
    fs::write(&archive, b"not CLP-S").expect("write invalid archive");
    let output_dir = temporary.path().join("output");

    let result = clp_s(&["x".as_ref(), archive.as_os_str(), output_dir.as_os_str()]);
    assert!(!result.status.success());
    assert!(output_dir.is_dir());
    assert!(!output_dir.join("original").exists());
}

#[test]
fn validates_order_only_options() {
    let temporary = TestDirectory::new("validation");
    let archive = fixture(MINIMAL_ARCHIVE);
    let output_dir = temporary.path().join("output");
    let invalid = clp_s(&[
        "x".as_ref(),
        archive.as_os_str(),
        output_dir.as_os_str(),
        "--target-ordered-chunk-size".as_ref(),
        "1".as_ref(),
    ]);
    assert!(!invalid.status.success());
    assert!(!output_dir.exists());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("must be used with ordered"));
}
