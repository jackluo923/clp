#![cfg(feature = "cli")]

use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_s::archive::DirectoryArchiveMember;
use clp_s::archive::MetadataLimits;
use clp_s::archive::RangeIndexValue;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::archive::TimestampBounds;
use flate2::Compression;
use flate2::write::GzEncoder;
use uuid::Uuid;

const INPUT: &[u8] = include_bytes!("fixtures/compression-cli-v1-input.json");
const CANONICAL_JSONL: &[u8] =
    b"{\"id\":0,\"kind\":\"a\"}\n{\"id\":1,\"kind\":\"b\"}\n{\"id\":2,\"kind\":\"c\"}\n";
const EXPECTED_SFA_HEX: &str = include_str!("fixtures/compression-cli-v1-sfa.hex");
const TIMESTAMP_INPUT: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-timestamps-cpp-input.jsonl");
const TIMESTAMP_SFA: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-timestamps-cpp.bin");
const STRUCTURED_ARRAY_INPUT: &[u8] =
    include_bytes!("fixtures/sfa-v0.5.0-structured-arrays-cpp-input.jsonl");
const STRUCTURED_ARRAY_SFA_HEX: &str =
    include_str!("fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");
const FOUR_BYTE_KV_IR_HEX: &str = include_str!("fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
const EIGHT_BYTE_KV_IR_HEX: &str = include_str!("fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");
const FOUR_BYTE_KV_TIMESTAMP_HEX: &str =
    include_str!("fixtures/kv-ir-v0.1.0-timestamps-four-cpp.hex");
const EIGHT_BYTE_KV_TIMESTAMP_HEX: &str =
    include_str!("fixtures/kv-ir-v0.1.0-timestamps-eight-cpp.hex");
const FOUR_BYTE_KV_TIMESTAMP_SPLIT_HEX: &str =
    include_str!("fixtures/kv-ir-v0.1.0-timestamp-split-four-cpp.hex");
const EXPECTED_KV_IR_JSONL: &[u8] = concat!(
    "{\"empty\":{},\"message\":\"task 42 done\",\"none\":null,",
    "\"ok\":true,\"ratio\":1.25}\n"
)
.as_bytes();


fn assert_same_archive_records(expected: &[u8], actual: &[u8]) {
    fn records(bytes: &[u8]) -> Vec<String> {
        let mut reader = SingleFileArchiveReader::open(std::io::Cursor::new(bytes)).unwrap();
        let mut output = Vec::new();
        clp_s::extract_jsonl(&mut reader, &mut output, clp_s::ExtractionOptions::default()).unwrap();
        let mut rows: Vec<_> = std::str::from_utf8(&output).unwrap().lines().map(str::to_owned).collect();
        rows.sort();
        rows
    }
    assert_eq!(records(expected), records(actual));
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-rust-cli-compress-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique CLI compression test directory");
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

fn clp_s(arguments: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clp-s"))
        .args(arguments)
        .output()
        .expect("run Rust clp-s binary")
}

fn only_entry(path: &Path) -> PathBuf {
    let mut entries = fs::read_dir(path).expect("read archive output directory");
    let entry = entries
        .next()
        .expect("one archive output")
        .expect("read archive output entry")
        .path();
    assert!(entries.next().is_none(), "expected exactly one archive");
    entry
}

fn decode_hex(source: &str) -> Vec<u8> {
    let digits: Vec<u8> = source.bytes().filter(u8::is_ascii_hexdigit).collect();
    let (pairs, remainder) = digits.as_chunks::<2>();
    assert!(remainder.is_empty(), "hex fixture has an even length");
    pairs
        .iter()
        .map(|pair| (decode_nibble(pair[0]) << 4) | decode_nibble(pair[1]))
        .collect()
}

fn semantic_json_records(jsonl: &[u8]) -> Vec<serde_json::Value> {
    let mut records: Vec<serde_json::Value> = jsonl
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut value = serde_json::from_slice(line).expect("extraction is JSONL");
            canonicalize_json_object_order(&mut value);
            value
        })
        .collect();
    records.sort_unstable_by_key(serde_json::Value::to_string);
    records
}

fn canonicalize_json_object_order(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for value in object.values_mut() {
                canonicalize_json_object_order(value);
            }
            object.sort_keys();
        }
        serde_json::Value::Array(array) => {
            for value in array {
                canonicalize_json_object_order(value);
            }
        }
        _ => {}
    }
}

fn assert_timestamp_range(
    archive_path: &Path,
    expected_key: &str,
    expected_start: i64,
    expected_end: i64,
) {
    let archive = File::open(archive_path).expect("open timestamp archive");
    let mut reader = SingleFileArchiveReader::open(archive).expect("open timestamp SFA");
    let metadata = reader
        .read_metadata(MetadataLimits::default())
        .expect("read timestamp metadata");
    let [range] = metadata.timestamp_dictionary().ranges() else {
        panic!("expected one authoritative timestamp range")
    };
    assert_eq!(expected_key, range.key());
    assert_eq!(1, range.column_ids().len());
    assert_eq!(
        TimestampBounds::Epoch {
            start: expected_start,
            end: expected_end,
        },
        range.bounds()
    );
}

fn gzip(source: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(source).expect("encode gzip CLI input");
    encoder.finish().expect("finish gzip CLI input")
}

enum UstarMember<'a> {
    Regular { path: &'a [u8], contents: &'a [u8] },
    Directory { path: &'a [u8] },
    Symlink { path: &'a [u8], target: &'a [u8] },
}

fn write_ustar_bytes(header: &mut [u8; 512], offset: usize, width: usize, value: &[u8]) {
    assert!(value.len() <= width, "ustar field value fits");
    header[offset..offset + value.len()].copy_from_slice(value);
}

fn write_ustar_octal(field: &mut [u8], value: u64) {
    let digits = format!("{value:o}");
    assert!(digits.len() < field.len(), "ustar octal value fits");
    let digits_start = field.len() - digits.len() - 1;
    let terminator = field.len() - 1;
    field[..digits_start].fill(b'0');
    field[digits_start..terminator].copy_from_slice(digits.as_bytes());
    field[terminator] = 0;
}

fn tar_gzip(members: &[UstarMember<'_>]) -> Vec<u8> {
    let mut tar = Vec::new();
    for member in members {
        let (path, contents, type_flag, link_target, mode) = match member {
            UstarMember::Regular { path, contents } => {
                (*path, *contents, b'0', b"".as_slice(), 0o644)
            }
            UstarMember::Directory { path } => (*path, b"".as_slice(), b'5', b"".as_slice(), 0o755),
            UstarMember::Symlink { path, target } => (*path, b"".as_slice(), b'2', *target, 0o777),
        };

        let mut header = [0_u8; 512];
        write_ustar_bytes(&mut header, 0, 100, path);
        write_ustar_octal(&mut header[100..108], mode);
        write_ustar_octal(&mut header[108..116], 0);
        write_ustar_octal(&mut header[116..124], 0);
        write_ustar_octal(
            &mut header[124..136],
            u64::try_from(contents.len()).expect("member length fits u64"),
        );
        write_ustar_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = type_flag;
        write_ustar_bytes(&mut header, 157, 100, link_target);
        write_ustar_bytes(&mut header, 257, 6, b"ustar\0");
        write_ustar_bytes(&mut header, 263, 2, b"00");

        let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
        let checksum = format!("{checksum:06o}\0 ");
        assert_eq!(8, checksum.len(), "ustar checksum field width");
        header[148..156].copy_from_slice(checksum.as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(contents);
        let padding = (512 - contents.len() % 512) % 512;
        tar.resize(tar.len() + padding, 0);
    }
    tar.resize(tar.len() + 1024, 0);
    gzip(&tar)
}

fn zstd(source: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(source, 1).expect("encode zstd CLI input")
}

fn four_nested_wrappers(source: &[u8]) -> Vec<u8> {
    let inner_zstd = zstd(source);
    let inner_gzip = gzip(&inner_zstd);
    let outer_zstd = zstd(&inner_gzip);
    gzip(&outer_zstd)
}

const fn decode_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("invalid hex fixture"),
    }
}

fn archive_bytes(path: &Path, single_file: bool) -> Vec<u8> {
    if single_file {
        return fs::read(path).expect("read generated SFA");
    }

    let mut bytes = Vec::new();
    for member in DirectoryArchiveMember::ALL {
        bytes.extend_from_slice(
            &fs::read(path.join(member.file_name())).expect("read generated archive member"),
        );
    }
    bytes
}

fn extract_original(archive_path: &Path, output_path: &Path) -> Vec<u8> {
    let extraction = clp_s(&[
        "x".as_ref(),
        archive_path.as_os_str(),
        output_path.as_os_str(),
    ]);
    assert!(
        extraction.status.success(),
        "{}",
        String::from_utf8_lossy(&extraction.stderr)
    );
    assert_eq!(b"", extraction.stdout.as_slice());
    assert_eq!(b"", extraction.stderr.as_slice());
    fs::read(output_path.join("original")).expect("read extracted original stream")
}

fn single_source_filename(archives_path: &Path) -> String {
    let archive_path = only_entry(archives_path);
    let mut archive = SingleFileArchiveReader::open(
        File::open(archive_path).expect("open generated source-path archive"),
    )
    .expect("open generated source-path SFA envelope");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read generated source-path metadata");
    let [range] = metadata
        .range_index()
        .expect("source-path archive has range metadata")
        .entries()
    else {
        panic!("one input produces one source range")
    };
    range
        .field("_filename")
        .and_then(RangeIndexValue::as_str)
        .expect("source range has a filename")
        .to_owned()
}

#[test]
fn source_path_options_match_cpp_transform_order_and_empty_prefix_noop() {
    let temporary = TestDirectory::new("source-path-options");
    let prefix = temporary.path().join("prefix");
    let nested = prefix.join("nested");
    fs::create_dir_all(&nested).expect("create nested input directory");
    let input_path = nested.join("input.json");
    fs::write(&input_path, b"{\"id\":1}\n").expect("write source-path input");

    let normalized_archives = temporary.path().join("normalized-archives");
    let lexical_input = nested.join("..").join("nested").join("input.json");
    let normalized = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--normalize-paths".as_ref(),
        "--remove-path-prefix".as_ref(),
        prefix.as_os_str(),
        "--remove-leading-slash".as_ref(),
        normalized_archives.as_os_str(),
        lexical_input.as_os_str(),
    ]);
    assert!(
        normalized.status.success(),
        "{}",
        String::from_utf8_lossy(&normalized.stderr)
    );
    assert_eq!(b"", normalized.stdout.as_slice());
    assert_eq!(
        "nested/input.json",
        single_source_filename(&normalized_archives)
    );

    let empty_prefix_archives = temporary.path().join("empty-prefix-archives");
    let empty_prefix = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--remove-path-prefix".as_ref(),
        "".as_ref(),
        empty_prefix_archives.as_os_str(),
        lexical_input.as_os_str(),
    ]);
    assert!(
        empty_prefix.status.success(),
        "{}",
        String::from_utf8_lossy(&empty_prefix.stderr)
    );
    assert_eq!(
        lexical_input.to_str().expect("temporary path is UTF-8"),
        single_source_filename(&empty_prefix_archives)
    );

    let disabled_archives = temporary.path().join("disabled-archives");
    let disabled = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--disable-log-order".as_ref(),
        "--normalize-paths".as_ref(),
        "--remove-path-prefix".as_ref(),
        prefix.as_os_str(),
        "--remove-leading-slash".as_ref(),
        disabled_archives.as_os_str(),
        lexical_input.as_os_str(),
    ]);
    assert!(
        disabled.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert!(only_entry(&disabled_archives).is_file());
}

#[test]
fn transformed_source_context_is_used_for_kv_ir_without_changing_its_open_path() {
    let temporary = TestDirectory::new("source-path-kv-ir");
    let prefix = temporary.path().join("prefix");
    fs::create_dir(&prefix).expect("create KV-IR prefix");
    let input_path = prefix.join("input.kvir");
    fs::write(&input_path, decode_hex(FOUR_BYTE_KV_IR_HEX)).expect("write KV-IR input");
    let archives_path = temporary.path().join("archives");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--remove-path-prefix".as_ref(),
        prefix.as_os_str(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!("/input.kvir", single_source_filename(&archives_path));
}

#[test]
fn every_source_path_is_validated_before_the_archive_directory_is_created() {
    let temporary = TestDirectory::new("source-path-validation");
    let inside = temporary.path().join("inside");
    let outside = temporary.path().join("outside");
    fs::create_dir(&inside).expect("create prefix directory");
    fs::create_dir(&outside).expect("create outside directory");
    let inside_input = inside.join("inside.json");
    let outside_input = outside.join("outside.json");
    fs::write(&inside_input, b"{\"id\":1}\n").expect("write inside input");
    fs::write(&outside_input, b"{\"id\":2}\n").expect("write outside input");

    let missing_prefix = temporary.path().join("missing-prefix");
    let missing_output = temporary.path().join("missing-output");
    let missing = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--remove-path-prefix".as_ref(),
        missing_prefix.as_os_str(),
        missing_output.as_os_str(),
        inside_input.as_os_str(),
    ]);
    assert!(!missing.status.success());
    assert_eq!(b"", missing.stdout.as_slice());
    assert!(!missing_output.exists());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does not exist"));

    let file_prefix_output = temporary.path().join("file-prefix-output");
    let file_prefix = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--remove-path-prefix".as_ref(),
        inside_input.as_os_str(),
        file_prefix_output.as_os_str(),
        inside_input.as_os_str(),
    ]);
    assert!(!file_prefix.status.success());
    assert_eq!(b"", file_prefix.stdout.as_slice());
    assert!(!file_prefix_output.exists());
    assert!(String::from_utf8_lossy(&file_prefix.stderr).contains("not a directory"));

    let mismatch_output = temporary.path().join("mismatch-output");
    let mismatch = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--remove-path-prefix".as_ref(),
        inside.as_os_str(),
        mismatch_output.as_os_str(),
        inside_input.as_os_str(),
        outside_input.as_os_str(),
    ]);
    assert!(!mismatch.status.success());
    assert_eq!(b"", mismatch.stdout.as_slice());
    assert!(!mismatch_output.exists());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("does not begin"));

    let normalize_output = temporary.path().join("normalize-output");
    let missing_input = temporary.path().join("missing-input.json");
    let normalize = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--normalize-paths".as_ref(),
        normalize_output.as_os_str(),
        missing_input.as_os_str(),
    ]);
    assert!(!normalize.status.success());
    assert_eq!(b"", normalize.stdout.as_slice());
    assert!(!normalize_output.exists());
    assert!(String::from_utf8_lossy(&normalize.stderr).contains("normalize input path"));
}

#[cfg(unix)]
#[test]
fn non_utf8_transformed_metadata_fails_before_output_creation() {
    let temporary = TestDirectory::new("source-path-non-utf8");
    let input_name = OsString::from_vec(b"input-\xff.json".to_vec());
    let input_path = temporary.path().join(input_name);
    fs::write(&input_path, b"{\"id\":1}\n").expect("write non-UTF8 input path");
    let archives_path = temporary.path().join("archives");
    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(!archives_path.exists());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not valid UTF-8"));
}

#[test]
fn both_layouts_preserve_cpp_records_and_extract() {
    let expected_archive = decode_hex(EXPECTED_SFA_HEX);
    assert_eq!(387, expected_archive.len());

    for single_file in [false, true] {
        let temporary = TestDirectory::new(if single_file { "sfa" } else { "directory" });
        let input_path = temporary.path().join("input.json");
        let archives_path = temporary.path().join("archives");
        fs::write(&input_path, INPUT).expect("write parse-many fixture");

        // Mixed integer archives use version 0.10. Compare reconstructed records
        // with the C++ 0.5.0 oracle while retaining its compression options.
        let mut arguments = vec![
            "c".as_ref(),
            "--no-adaptive-numeric-columns".as_ref(),
            "--dictionary-compression-level".as_ref(),
            "3".as_ref(),
            "--no-dictionary-long-distance-matching".as_ref(),
            "--disable-log-order".as_ref(),
        ];
        if single_file {
            arguments.push("--single-file-archive".as_ref());
        }
        arguments.extend([
            "--print-archive-stats".as_ref(),
            archives_path.as_os_str(),
            input_path.as_os_str(),
        ]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(b"", result.stderr.as_slice());

        let archive_path = only_entry(&archives_path);
        let archive_id = archive_path.file_name().expect("archive UUID filename");
        Uuid::parse_str(&archive_id.to_string_lossy()).expect("archive filename is UUIDv4");
        assert_eq!(single_file, archive_path.is_file());
        let actual_bytes = archive_bytes(&archive_path, single_file);
        assert_same_archive_records(&expected_archive, &actual_bytes);
        let expected_stats = format!(
            "{{\"begin_timestamp\":0,\"end_timestamp\":0,\"id\":\"{}\",\"is_split\":false,\"\
             range_index\":[],\"size\":{},\"uncompressed_size\":66}}\n",
            archive_id.to_string_lossy(), actual_bytes.len()
        );
        assert_eq!(expected_stats.as_bytes(), result.stdout);

        let extracted_path = temporary.path().join("extracted");
        let extraction = clp_s(&[
            "x".as_ref(),
            archive_path.as_os_str(),
            extracted_path.as_os_str(),
        ]);
        assert!(
            extraction.status.success(),
            "{}",
            String::from_utf8_lossy(&extraction.stderr)
        );
        assert_eq!(
            CANONICAL_JSONL,
            fs::read(extracted_path.join("original"))
                .expect("read extracted records")
                .as_slice()
        );
    }
}

#[test]
fn local_inputs_auto_decode_by_magic_and_account_final_plaintext_bytes() {
    let four_layers = four_nested_wrappers(INPUT);
    let cases = [
        ("raw-misleading", "raw.json.gz", INPUT.to_vec()),
        ("gzip-no-suffix", "gzip.json", gzip(INPUT)),
        ("zstd-no-suffix", "zstd.json", zstd(INPUT)),
        ("four-nested", "nested.data", four_layers),
    ];

    for (label, file_name, encoded) in cases {
        let temporary = TestDirectory::new(label);
        let input_path = temporary.path().join(file_name);
        let archives_path = temporary.path().join("archives");
        fs::write(&input_path, encoded).expect("write wrapped CLI input");

        let result = clp_s(&[
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            "--print-archive-stats".as_ref(),
            archives_path.as_os_str(),
            input_path.as_os_str(),
        ]);
        assert!(
            result.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(b"", result.stderr.as_slice());

        let stats: serde_json::Value =
            serde_json::from_slice(&result.stdout).expect("wrapped-input stats are valid JSON");
        assert_eq!(
            Some(u64::try_from(INPUT.len()).unwrap()),
            stats["uncompressed_size"].as_u64(),
            "{label}"
        );

        let archive_path = only_entry(&archives_path);
        let mut archive = SingleFileArchiveReader::open(
            File::open(&archive_path).expect("open wrapped-input archive"),
        )
        .expect("open wrapped-input SFA envelope");
        let metadata = archive
            .read_metadata(MetadataLimits::default())
            .expect("read wrapped-input archive metadata");
        let [range] = metadata
            .range_index()
            .expect("wrapped input records source metadata")
            .entries()
        else {
            panic!("one wrapped input produces one source range")
        };
        assert_eq!(
            input_path.to_str(),
            range.field("_filename").and_then(RangeIndexValue::as_str),
            "{label}"
        );
        assert_eq!(
            CANONICAL_JSONL,
            extract_original(&archive_path, &temporary.path().join("wrapped-extracted")).as_slice(),
            "{label}"
        );
    }
}

#[test]
fn tar_gzip_regular_members_are_independent_sources_with_exact_member_paths() {
    let temporary = TestDirectory::new("tar-gzip-members");
    let prefix = temporary.path().join("prefix");
    let nested = prefix.join("outer");
    fs::create_dir_all(&nested).expect("create container input directory");
    let input_path = nested.join("input.tar.gz");
    fs::write(
        &input_path,
        tar_gzip(&[
            UstarMember::Regular {
                path: b"first.json",
                contents: b"{\"member\":\"first\"}\n",
            },
            UstarMember::Directory { path: b"nested/" },
            UstarMember::Regular {
                path: b"nested/second.json",
                contents: b"{\"member\":\"second\"}\n",
            },
            UstarMember::Regular {
                path: b"empty.json",
                contents: b"",
            },
            UstarMember::Symlink {
                path: b"alias.json",
                target: b"first.json",
            },
        ]),
    )
    .expect("write tar-gzip input");
    let lexical_input = nested.join("..").join("outer").join("input.tar.gz");
    let archives_path = temporary.path().join("archives");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--normalize-paths".as_ref(),
        "--remove-path-prefix".as_ref(),
        prefix.as_os_str(),
        "--remove-leading-slash".as_ref(),
        archives_path.as_os_str(),
        lexical_input.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let archive_path = only_entry(&archives_path);
    let mut archive = SingleFileArchiveReader::open(
        File::open(&archive_path).expect("open container-member archive"),
    )
    .expect("open container-member SFA envelope");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read container-member metadata");
    let ranges = metadata
        .range_index()
        .expect("container members record source ranges")
        .entries();
    assert_eq!(3, ranges.len());
    assert_eq!(0..1, ranges[0].range());
    assert_eq!(1..2, ranges[1].range());
    assert_eq!(2..2, ranges[2].range());
    assert_eq!(
        ["first.json", "nested/second.json", "empty.json"],
        ranges
            .iter()
            .map(|range| {
                range
                    .field("_filename")
                    .and_then(RangeIndexValue::as_str)
                    .expect("container member range has a filename")
            })
            .collect::<Vec<_>>()
            .as_slice(),
    );
    assert_eq!(
        b"{\"member\":\"first\"}\n{\"member\":\"second\"}\n",
        extract_original(
            &archive_path,
            &temporary.path().join("container-members-extracted"),
        )
        .as_slice(),
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_tar_member_path_fails_without_lossy_source_metadata() {
    let temporary = TestDirectory::new("tar-member-non-utf8");
    let input_path = temporary.path().join("input.tar.gz");
    fs::write(
        &input_path,
        tar_gzip(&[UstarMember::Regular {
            path: b"bad-\xff.json",
            contents: b"{\"id\":1}\n",
        }]),
    )
    .expect("write non-UTF8 tar member input");
    let archives_path = temporary.path().join("archives");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    let diagnostic =
        std::str::from_utf8(&result.stderr).expect("member-path diagnostic remains valid UTF-8");
    assert!(diagnostic.contains("not valid UTF-8"), "{diagnostic}");
    assert!(!diagnostic.contains('\u{fffd}'), "{diagnostic}");

    let archive_path = only_entry(&archives_path);
    let mut archive = SingleFileArchiveReader::open(
        File::open(archive_path).expect("open empty archive after member-path failure"),
    )
    .expect("open empty SFA after member-path failure");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read metadata after member-path failure");
    assert!(
        metadata
            .range_index()
            .is_none_or(|index| index.entries().is_empty()),
        "invalid member path must not create lossy source metadata",
    );
}

#[test]
fn local_input_wrapper_depth_matches_the_four_layer_cpp_ceiling() {
    let temporary = TestDirectory::new("five-nested");
    let input_path = temporary.path().join("five-nested.json");
    let archives_path = temporary.path().join("archives");
    let five_layers = zstd(&four_nested_wrappers(INPUT));
    fs::write(&input_path, five_layers).expect("write five-layer CLI input");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(
        diagnostic.contains("5 input compression layers exceeds limit 4"),
        "{diagnostic}"
    );
}

#[test]
fn local_input_rejects_truncated_and_trailing_compression_data() {
    let mut truncated_gzip = gzip(INPUT);
    truncated_gzip.pop();
    let mut truncated_zstd = zstd(INPUT);
    truncated_zstd.pop();
    let mut trailing_gzip = gzip(INPUT);
    trailing_gzip.push(b'x');
    let mut trailing_zstd = zstd(INPUT);
    trailing_zstd.push(b'x');
    let cases = [
        (
            "truncated-gzip",
            truncated_gzip,
            "gzip decoder at compression layer 1",
            "truncated compressed data",
        ),
        (
            "truncated-zstd",
            truncated_zstd,
            "zstd decoder at compression layer 1",
            "truncated compressed data",
        ),
        (
            "trailing-gzip",
            trailing_gzip,
            "gzip decoder at compression layer 1",
            "truncated compressed data",
        ),
        (
            "trailing-zstd",
            trailing_zstd,
            "zstd decoder at compression layer 1",
            "invalid compressed data",
        ),
    ];

    for (label, encoded, decoder, failure) in cases {
        let temporary = TestDirectory::new(label);
        let input_path = temporary.path().join(format!("{label}.json"));
        let archives_path = temporary.path().join("archives");
        fs::write(&input_path, encoded).expect("write malformed wrapped input");
        let result = clp_s(&[
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            archives_path.as_os_str(),
            input_path.as_os_str(),
        ]);
        assert!(!result.status.success(), "{label}");
        assert_eq!(b"", result.stdout.as_slice());
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        assert!(diagnostic.contains(decoder), "{label}: {diagnostic}");
        assert!(diagnostic.contains(failure), "{label}: {diagnostic}");
    }
}

#[test]
fn compressed_kv_ir_is_probed_after_wrapper_decoding() {
    let temporary = TestDirectory::new("wrapped-kv-ir");
    let input_path = temporary.path().join("wrapped-kv-ir.json");
    let archives_path = temporary.path().join("archives");
    let fixture = decode_hex(FOUR_BYTE_KV_IR_HEX);
    fs::write(&input_path, zstd(&gzip(&fixture))).expect("write wrapped KV-IR input");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    let stats: serde_json::Value =
        serde_json::from_slice(&result.stdout).expect("wrapped KV-IR stats are valid JSON");
    assert_eq!(
        Some(u64::try_from(fixture.len()).unwrap()),
        stats["uncompressed_size"].as_u64()
    );
    assert_eq!(
        EXPECTED_KV_IR_JSONL,
        extract_original(
            &only_entry(&archives_path),
            &temporary.path().join("wrapped-kv-ir-extracted")
        )
        .as_slice()
    );
}

#[test]
fn iso_offset_timestamp_key_round_trips_and_filters_in_utc() {
    let temporary = TestDirectory::new("iso-offsets");
    let input_path = temporary.path().join("input.json");
    let archives_path = temporary.path().join("archives");
    let source = concat!(
        "{\"t\":{\"$date\":\"2023-03-21T23:34:54.576-04:00\"},\"id\":1}\n",
        "{\"t\":{\"$date\":\"2023-03-22T09:04:54.576+05:30\"},\"id\":2}\n",
        "{\"t\":{\"$date\":\"2023-03-22T03:34:55.576Z\"},\"id\":3}\n",
    );
    fs::write(&input_path, source).unwrap();
    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--timestamp-key".as_ref(),
        "t.$date".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let archive_path = only_entry(&archives_path);
    assert_timestamp_range(
        &archive_path,
        "t.$date",
        1_679_456_094_576,
        1_679_456_095_576,
    );
    let all = clp_s(&["s".as_ref(), archive_path.as_os_str(), "*".as_ref()]);
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    assert_eq!(
        semantic_json_records(source.as_bytes()),
        semantic_json_records(&all.stdout)
    );
    let bounded = clp_s(&[
        "s".as_ref(),
        archive_path.as_os_str(),
        "*".as_ref(),
        "--tge".as_ref(),
        "1679456094576".as_ref(),
        "--tle".as_ref(),
        "1679456094576".as_ref(),
    ]);
    assert!(
        bounded.status.success(),
        "{}",
        String::from_utf8_lossy(&bounded.stderr)
    );
    let records = semantic_json_records(&bounded.stdout);
    assert_eq!(2, records.len());
    assert_eq!(serde_json::json!(1), records[0]["id"]);
    assert_eq!(serde_json::json!(2), records[1]["id"]);
}

#[test]
fn named_timezone_timestamp_key_round_trips_and_filters_in_utc() {
    let temporary = TestDirectory::new("named-timezones");
    let input_path = temporary.path().join("input.json");
    let archives_path = temporary.path().join("archives");
    let source = concat!(
        "{\"timestamp\":\"2023-03-27 00:26:35.719 EDT\",\"id\":1}\n",
        "{\"timestamp\":\"2023-03-26 23:26:35.719 EST\",\"id\":2}\n",
        "{\"timestamp\":\"2023-03-27 09:56:35.719 Asia/Kolkata\",\"id\":3}\n",
        "{\"timestamp\":\"2023-03-27 04:26:36.719 UTC\",\"id\":4}\n",
    );
    fs::write(&input_path, source).unwrap();
    let result = clp_s(&["c".as_ref(), "--single-file-archive".as_ref(),
        "--timestamp-key".as_ref(), "timestamp".as_ref(), archives_path.as_os_str(), input_path.as_os_str()]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let archive_path = only_entry(&archives_path);
    assert_timestamp_range(&archive_path, "timestamp", 1_679_891_195_719, 1_679_891_196_719);
    assert_eq!(source.as_bytes(), extract_original(&archive_path, &temporary.path().join("extracted")).as_slice());
    let bounded = clp_s(&["s".as_ref(), archive_path.as_os_str(), "*".as_ref(),
        "--tge".as_ref(), "1679891195719".as_ref(), "--tle".as_ref(), "1679891195719".as_ref()]);
    assert!(bounded.status.success(), "{}", String::from_utf8_lossy(&bounded.stderr));
    let records = semantic_json_records(&bounded.stdout);
    assert_eq!(3, records.len());
    for (index, record) in records.iter().enumerate() {
        assert_eq!(serde_json::json!(index + 1), record["id"]);
    }
}

#[test]
fn timestamp_key_preserves_cpp_records() {
    let temporary = TestDirectory::new("timestamps");
    let input_path = temporary.path().join("input.json");
    let archives_path = temporary.path().join("archives");
    fs::write(&input_path, TIMESTAMP_INPUT).expect("write timestamp fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--no-adaptive-numeric-columns".as_ref(),
        "--disable-log-order".as_ref(),
        "--single-file-archive".as_ref(),
        "--timestamp-key".as_ref(),
        "ts".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_same_archive_records(TIMESTAMP_SFA, &fs::read(only_entry(&archives_path)).unwrap());
}

#[test]
fn nested_timestamp_key_drives_search_bounds_and_invalid_values_fail_explicitly() {
    let temporary = TestDirectory::new("nested-timestamp");
    let input_path = temporary.path().join("input.json");
    let archives_path = temporary.path().join("archives");
    let input = concat!(
        r#"{"outer":{"ts":1700000000123},"kind":"first"}"#,
        "\n",
        r#"{"outer":{"ts":1700000001123},"kind":"second"}"#,
        "\n",
        r#"{"outer":{"other":2},"kind":"missing"}"#,
        "\n",
    );
    fs::write(&input_path, input).expect("write nested timestamp fixture");

    let compression = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--timestamp-key".as_ref(),
        "outer.ts:precision=ms".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        compression.status.success(),
        "{}",
        String::from_utf8_lossy(&compression.stderr)
    );
    let archive_path = only_entry(&archives_path);
    let archive_id = archive_path.file_name().expect("archive UUID filename");
    let bounded = clp_s(&[
        "s".as_ref(),
        archive_path.as_os_str(),
        "kind:*".as_ref(),
        "--tge".as_ref(),
        "1700000000123".as_ref(),
        "--tle".as_ref(),
        "1700000000123".as_ref(),
        "--count".as_ref(),
    ]);
    assert!(
        bounded.status.success(),
        "{}",
        String::from_utf8_lossy(&bounded.stderr)
    );
    assert_eq!(
        format!(
            "{{\"archive_id\":\"{}\",\"count\":1}}\n",
            archive_id.to_string_lossy()
        )
        .as_bytes(),
        bounded.stdout
    );

    let invalid_input_path = temporary.path().join("invalid.json");
    let invalid_archives_path = temporary.path().join("invalid-archives");
    fs::write(
        &invalid_input_path,
        br#"{"outer":{"ts":"not-a-time"},"kind":"invalid"}"#,
    )
    .expect("write invalid timestamp fixture");
    let invalid = clp_s(&[
        "c".as_ref(),
        "--timestamp-key".as_ref(),
        "outer.ts:precision=ms".as_ref(),
        invalid_archives_path.as_os_str(),
        invalid_input_path.as_os_str(),
    ]);
    assert!(!invalid.status.success());
    assert_eq!(b"", invalid.stdout.as_slice());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("timestamp"));
}

#[test]
fn default_log_order_reports_cpp_compatible_ranges_with_one_creator_per_invocation() {
    let temporary = TestDirectory::new("source-ranges");
    let first_input_path = temporary.path().join("first.json");
    let second_input_path = temporary.path().join("second.json");
    let archives_path = temporary.path().join("archives");
    let first_input = b"{\"source\":\"first\"}\n";
    let second_input = b"{\"source\":\"second\"}\n";
    fs::write(&first_input_path, first_input).expect("write first source fixture");
    fs::write(&second_input_path, second_input).expect("write second source fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        archives_path.as_os_str(),
        first_input_path.as_os_str(),
        second_input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());

    let archive_path = only_entry(&archives_path);
    let archive_id = archive_path.file_name().expect("archive UUID filename");
    Uuid::parse_str(&archive_id.to_string_lossy()).expect("archive filename is UUIDv4");
    let archive_size = fs::metadata(&archive_path)
        .expect("read generated archive metadata")
        .len();
    let mut archive = SingleFileArchiveReader::open(
        File::open(&archive_path).expect("open generated source-range archive"),
    )
    .expect("open generated SFA envelope");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read generated archive metadata");
    let range_index = metadata
        .range_index()
        .expect("default compression records source ranges");
    let [first_range, second_range] = range_index.entries() else {
        panic!("two input files produce two source ranges")
    };
    assert_eq!(0..1, first_range.range());
    assert_eq!(1..2, second_range.range());

    let creator_id = first_range
        .field("_archive_creator_id")
        .and_then(RangeIndexValue::as_str)
        .expect("first source has an archive creator ID");
    Uuid::parse_str(creator_id).expect("archive creator ID is UUIDv4");
    assert_eq!(
        Some(creator_id),
        second_range
            .field("_archive_creator_id")
            .and_then(RangeIndexValue::as_str)
    );
    assert_eq!(
        Some(0),
        first_range
            .field("_file_split_number")
            .and_then(RangeIndexValue::as_u64)
    );
    assert_eq!(
        Some(0),
        second_range
            .field("_file_split_number")
            .and_then(RangeIndexValue::as_u64)
    );
    assert_eq!(
        first_input_path.to_str(),
        first_range
            .field("_filename")
            .and_then(RangeIndexValue::as_str)
    );
    assert_eq!(
        second_input_path.to_str(),
        second_range
            .field("_filename")
            .and_then(RangeIndexValue::as_str)
    );

    let uncompressed_size = first_input.len() + second_input.len();
    let expected_stats = format!(
        "{{\"begin_timestamp\":0,\"end_timestamp\":0,\"id\":\"{}\",\"is_split\":false,\"\
         range_index\":[{{\"e\":1,\"f\":{{\"_archive_creator_id\":\"{creator_id}\",\"\
         _file_split_number\":0,\"_filename\":\"{}\"}},\"s\":0}},{{\"e\":2,\"f\":{{\"\
         _archive_creator_id\":\"{creator_id}\",\"_file_split_number\":0,\"_filename\":\"{}\"}},\"\
         s\":1}}],\"size\":{archive_size},\"uncompressed_size\":{uncompressed_size}}}\n",
        archive_id.to_string_lossy(),
        first_input_path.display(),
        second_input_path.display(),
    );
    assert_eq!(expected_stats.as_bytes(), result.stdout);
}

#[test]
fn both_current_kv_ir_encodings_auto_route_with_exact_source_metadata_and_stats() {
    for (label, fixture, expected_size) in [
        ("four-byte", FOUR_BYTE_KV_IR_HEX, 345_u64),
        ("eight-byte", EIGHT_BYTE_KV_IR_HEX, 349_u64),
    ] {
        let temporary = TestDirectory::new(label);
        let input_path = temporary.path().join(format!("{label}.bin"));
        let archives_path = temporary.path().join("archives");
        let bytes = decode_hex(fixture);
        assert_eq!(expected_size, u64::try_from(bytes.len()).unwrap());
        fs::write(&input_path, &bytes).expect("write committed C++ KV-IR fixture");

        let result = clp_s(&[
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            "--print-archive-stats".as_ref(),
            archives_path.as_os_str(),
            input_path.as_os_str(),
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(b"", result.stderr.as_slice());

        let archive_path = only_entry(&archives_path);
        let mut archive =
            SingleFileArchiveReader::open(File::open(&archive_path).expect("open KV-IR archive"))
                .expect("open KV-IR SFA envelope");
        let metadata = archive
            .read_metadata(MetadataLimits::default())
            .expect("read KV-IR archive metadata");
        let [range] = metadata
            .range_index()
            .expect("KV-IR compression records source metadata")
            .entries()
        else {
            panic!("one KV-IR stream produces one source range")
        };
        assert_eq!(0..1, range.range());
        assert_eq!(
            input_path.to_str(),
            range.field("_filename").and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(0),
            range
                .field("_file_split_number")
                .and_then(RangeIndexValue::as_u64)
        );
        assert_eq!(
            Some("rust-kv-ir-reader-v1"),
            range.field("fixture").and_then(RangeIndexValue::as_str)
        );
        let creator_id = range
            .field("_archive_creator_id")
            .and_then(RangeIndexValue::as_str)
            .expect("KV-IR range has the invocation creator ID");
        Uuid::parse_str(creator_id).expect("KV-IR creator ID is UUIDv4");

        let stats: serde_json::Value =
            serde_json::from_slice(&result.stdout).expect("KV-IR stats are valid JSON");
        assert_eq!(Some(expected_size), stats["uncompressed_size"].as_u64());
        assert_eq!(Some(0), stats["range_index"][0]["s"].as_u64());
        assert_eq!(Some(1), stats["range_index"][0]["e"].as_u64());
        assert_eq!(
            input_path.to_str(),
            stats["range_index"][0]["f"]["_filename"].as_str()
        );
        assert_eq!(
            Some(creator_id),
            stats["range_index"][0]["f"]["_archive_creator_id"].as_str()
        );
        assert_eq!(
            Some("rust-kv-ir-reader-v1"),
            stats["range_index"][0]["f"]["fixture"].as_str()
        );
        assert_eq!(
            EXPECTED_KV_IR_JSONL,
            extract_original(&archive_path, &temporary.path().join("extracted")).as_slice()
        );
    }
}

#[test]
fn concatenated_current_kv_ir_streams_form_adjacent_source_ranges() {
    let temporary = TestDirectory::new("concatenated-kv-ir");
    let input_path = temporary.path().join("concatenated.bin");
    let archives_path = temporary.path().join("archives");
    let mut bytes = decode_hex(FOUR_BYTE_KV_IR_HEX);
    bytes.extend_from_slice(&decode_hex(EIGHT_BYTE_KV_IR_HEX));
    assert_eq!(694, bytes.len());
    fs::write(&input_path, &bytes).expect("write concatenated KV-IR fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    let archive_path = only_entry(&archives_path);

    let mut archive = SingleFileArchiveReader::open(
        File::open(&archive_path).expect("open concatenated KV-IR archive"),
    )
    .expect("open concatenated KV-IR SFA envelope");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read concatenated KV-IR metadata");
    let [first, second] = metadata
        .range_index()
        .expect("concatenated streams record ranges")
        .entries()
    else {
        panic!("each concatenated KV-IR stream produces one source range")
    };
    assert_eq!(0..1, first.range());
    assert_eq!(1..2, second.range());
    let creator_id = first
        .field("_archive_creator_id")
        .and_then(RangeIndexValue::as_str)
        .expect("first KV-IR stream has creator ID");
    for range in [first, second] {
        assert_eq!(
            input_path.to_str(),
            range.field("_filename").and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(creator_id),
            range
                .field("_archive_creator_id")
                .and_then(RangeIndexValue::as_str)
        );
        assert_eq!(
            Some(0),
            range
                .field("_file_split_number")
                .and_then(RangeIndexValue::as_u64)
        );
        assert_eq!(
            Some("rust-kv-ir-reader-v1"),
            range.field("fixture").and_then(RangeIndexValue::as_str)
        );
    }

    let stats: serde_json::Value =
        serde_json::from_slice(&result.stdout).expect("concatenated KV-IR stats are valid JSON");
    assert_eq!(Some(694), stats["uncompressed_size"].as_u64());
    assert_eq!(Some(2), stats["range_index"].as_array().map(Vec::len));
    let mut expected = EXPECTED_KV_IR_JSONL.to_vec();
    expected.extend_from_slice(EXPECTED_KV_IR_JSONL);
    assert_eq!(
        expected,
        extract_original(&archive_path, &temporary.path().join("extracted"))
    );
}

#[test]
fn kv_ir_representation_options_ignore_json_only_switches_and_accept_timestamps() {
    let temporary = TestDirectory::new("kv-ir-options");
    let kv_input_path = temporary.path().join("input.bin");
    let kv_archives_path = temporary.path().join("kv-archives");
    fs::write(&kv_input_path, decode_hex(FOUR_BYTE_KV_IR_HEX)).expect("write KV-IR option fixture");

    let compatible = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--no-retain-float-format".as_ref(),
        "--structurize-arrays".as_ref(),
        kv_archives_path.as_os_str(),
        kv_input_path.as_os_str(),
    ]);
    assert!(
        compatible.status.success(),
        "{}",
        String::from_utf8_lossy(&compatible.stderr)
    );
    assert_eq!(b"", compatible.stdout.as_slice());
    assert_eq!(b"", compatible.stderr.as_slice());
    assert_eq!(
        EXPECTED_KV_IR_JSONL,
        extract_original(
            &only_entry(&kv_archives_path),
            &temporary.path().join("compatible-extracted")
        )
        .as_slice()
    );

    let timestamp_archives_path = temporary.path().join("timestamp-archives");
    let timestamp = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--timestamp-key".as_ref(),
        "@seq:precision=s".as_ref(),
        timestamp_archives_path.as_os_str(),
        kv_input_path.as_os_str(),
    ]);
    assert!(
        timestamp.status.success(),
        "{}",
        String::from_utf8_lossy(&timestamp.stderr)
    );
    assert_eq!(b"", timestamp.stdout.as_slice());
    assert_eq!(b"", timestamp.stderr.as_slice());
    assert_timestamp_range(&only_entry(&timestamp_archives_path), "@seq", 7_000, 7_000);
}

fn seconds_timestamp_fixture(fixture: &str) -> Vec<u8> {
    let mut input = decode_hex(fixture);
    let original = 1_700_000_000_123_i64.to_be_bytes();
    let offset = input.windows(8).position(|w| w == original).expect("integer timestamp");
    input[offset..offset + 8].copy_from_slice(&1_700_000_000_i64.to_be_bytes());
    let original = b"1700000000125";
    let offset = input.windows(original.len()).position(|w| w == original).expect("quoted timestamp");
    input[offset..offset + original.len()].copy_from_slice(b"1700000000.12");
    input
}

#[test]
fn both_kv_ir_widths_promote_timestamp_scalars_with_declared_seconds() {
    for (label, fixture, expected_size) in [
        ("four", FOUR_BYTE_KV_TIMESTAMP_HEX, 377_u64),
        ("eight", EIGHT_BYTE_KV_TIMESTAMP_HEX, 389_u64),
    ] {
        let temporary = TestDirectory::new(&format!("kv-timestamp-{label}"));
        let input_path = temporary.path().join("input.bin");
        let archives_path = temporary.path().join("archives");
        let input = seconds_timestamp_fixture(fixture);
        assert_eq!(expected_size, u64::try_from(input.len()).unwrap());
        fs::write(&input_path, &input).expect("write KV timestamp fixture");

        let result = clp_s(&[
            "c".as_ref(),
            "--disable-log-order".as_ref(),
            "--single-file-archive".as_ref(),
            "--print-archive-stats".as_ref(),
            "--timestamp-key".as_ref(),
            "ts:precision=s".as_ref(),
            archives_path.as_os_str(),
            input_path.as_os_str(),
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(b"", result.stderr.as_slice());

        let archive_path = only_entry(&archives_path);
        let stats: serde_json::Value =
            serde_json::from_slice(&result.stdout).expect("timestamp stats are valid JSON");
        assert_eq!(Some(1_700_000_000_000), stats["begin_timestamp"].as_i64());
        assert_eq!(Some(1_700_000_000_000), stats["end_timestamp"].as_i64());
        assert_eq!(Some(false), stats["is_split"].as_bool());
        assert_eq!(Some(expected_size), stats["uncompressed_size"].as_u64());
        assert_eq!(Some(0), stats["range_index"].as_array().map(Vec::len));
        assert_eq!(
            archive_path.file_name().and_then(OsStr::to_str),
            stats["id"].as_str(),
        );
        assert_eq!(
            fs::metadata(&archive_path)
                .map(|metadata| metadata.len())
                .ok(),
            stats["size"].as_u64(),
        );
        assert_timestamp_range(&archive_path, "ts", 1_700_000_000_000, 1_700_000_000_000);
        let extracted =
            extract_original(&archive_path, &temporary.path().join("timestamp-extracted"));
        assert_eq!(
            { let expected = vec![serde_json::json!({"ts":1700000000_i64,"kind":"int"}),
                serde_json::json!({"ts":1700000000.0,"kind":"float"}),
                serde_json::json!({"ts":"1700000000.00","kind":"plain"}),
                serde_json::json!({"ts":"2023-11-14 22:13:20.000","kind":"encoded"}),
                serde_json::json!({"kind":"missing"})]; semantic_json_records(expected.iter().map(|r| r.to_string() + "\n").collect::<String>().as_bytes()) },
            semantic_json_records(&extracted)
        );
    }
}

#[test]
fn zero_target_reports_archive_local_quantized_negative_timestamp_bounds() {
    let temporary = TestDirectory::new("kv-timestamp-split");
    let input_path = temporary.path().join("input.bin");
    let archives_path = temporary.path().join("archives");
    let input = decode_hex(FOUR_BYTE_KV_TIMESTAMP_SPLIT_HEX);
    assert_eq!(251, input.len());
    fs::write(&input_path, input).expect("write split timestamp fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--disable-log-order".as_ref(),
        "--single-file-archive".as_ref(),
        "--target-encoded-size".as_ref(),
        "0".as_ref(),
        "--print-archive-stats".as_ref(),
        "--timestamp-key".as_ref(),
        "ts:precision=s".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    let stdout = String::from_utf8(result.stdout).expect("timestamp stats are UTF-8");
    let stats = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid stats JSON"))
        .collect::<Vec<_>>();
    assert_eq!(3, stats.len());

    for (index, (actual, expected)) in stats
        .iter()
        .zip([
            (-2_000, -2_000, true, 239_u64),
            (2_000, 2_000, true, 11),
            (0, 0, false, 1),
        ])
        .enumerate()
    {
        assert_eq!(Some(expected.0), actual["begin_timestamp"].as_i64());
        assert_eq!(Some(expected.1), actual["end_timestamp"].as_i64());
        assert_eq!(Some(expected.2), actual["is_split"].as_bool());
        assert_eq!(Some(expected.3), actual["uncompressed_size"].as_u64());
        assert_eq!(Some(0), actual["range_index"].as_array().map(Vec::len));
        let archive_id = actual["id"].as_str().expect("stats contain archive ID");
        Uuid::parse_str(archive_id).expect("archive ID is UUIDv4");
        let archive_path = archives_path.join(archive_id);
        assert_eq!(
            fs::metadata(&archive_path)
                .map(|metadata| metadata.len())
                .ok(),
            actual["size"].as_u64(),
        );
        if index < 2 {
            assert_timestamp_range(&archive_path, "ts", expected.0, expected.1);
        }
    }
    assert_eq!(
        3,
        fs::read_dir(&archives_path).expect("read archives").count()
    );
    assert_eq!(
        b"{\"ts\":-2.000000000}\n",
        extract_original(
            &archives_path.join(stats[0]["id"].as_str().expect("first archive ID")),
            &temporary.path().join("first-extracted"),
        )
        .as_slice(),
    );
    assert_eq!(
        b"{\"ts\":2.000000000}\n",
        extract_original(
            &archives_path.join(stats[1]["id"].as_str().expect("second archive ID")),
            &temporary.path().join("second-extracted"),
        )
        .as_slice(),
    );
}

#[test]
fn malformed_kv_timestamp_fails_after_only_complete_records() {
    let temporary = TestDirectory::new("invalid-kv-timestamp");
    let input_path = temporary.path().join("input.bin");
    let archives_path = temporary.path().join("archives");
    let mut input = seconds_timestamp_fixture(FOUR_BYTE_KV_TIMESTAMP_HEX);
    let original = b"1700000000.12";
    let replacement = b"not-a-time___";
    let offset = input
        .windows(original.len())
        .position(|window| window == original)
        .expect("plain timestamp exists in fixture");
    input[offset..offset + original.len()].copy_from_slice(replacement);
    fs::write(&input_path, input).expect("write malformed KV timestamp fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        "--timestamp-key".as_ref(),
        "ts:precision=s".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(!result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(diagnostic.contains("authoritative timestamp"));
    assert!(diagnostic.contains("user-generated node"));
    let stats: serde_json::Value =
        serde_json::from_slice(&result.stdout).expect("committed-prefix stats are valid JSON");
    assert_eq!(Some(1_700_000_000_000), stats["begin_timestamp"].as_i64());
    assert_eq!(Some(1_700_000_000_000), stats["end_timestamp"].as_i64());
    assert_eq!(Some(282), stats["uncompressed_size"].as_u64());
    assert_eq!(Some(0), stats["range_index"][0]["s"].as_u64());
    assert_eq!(Some(2), stats["range_index"][0]["e"].as_u64());
    assert_eq!(
        concat!(
            "{\"kind\":\"int\",\"ts\":1700000000}\n",
            "{\"kind\":\"float\",\"ts\":1700000000.000000000}\n",
        )
        .as_bytes(),
        extract_original(
            &only_entry(&archives_path),
            &temporary.path().join("prefix-extracted"),
        )
        .as_slice(),
    );
}

#[test]
fn auto_namespace_timestamp_is_a_json_noop_in_either_mixed_input_order() {
    for kv_first in [false, true] {
        let temporary = TestDirectory::new(if kv_first {
            "mixed-kv-json-timestamp"
        } else {
            "mixed-json-kv-timestamp"
        });
        let json_path = temporary.path().join("input.jsonl");
        let kv_path = temporary.path().join("input.bin");
        let archives_path = temporary.path().join("archives");
        fs::write(&json_path, INPUT).expect("write mixed JSON input");
        fs::write(&kv_path, decode_hex(FOUR_BYTE_KV_IR_HEX)).expect("write mixed KV input");
        let ordered_inputs = if kv_first {
            [kv_path.as_os_str(), json_path.as_os_str()]
        } else {
            [json_path.as_os_str(), kv_path.as_os_str()]
        };

        let result = clp_s(&[
            "c".as_ref(),
            "--disable-log-order".as_ref(),
            "--single-file-archive".as_ref(),
            "--timestamp-key".as_ref(),
            "@seq:precision=s".as_ref(),
            archives_path.as_os_str(),
            ordered_inputs[0],
            ordered_inputs[1],
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(b"", result.stdout.as_slice());
        assert_eq!(b"", result.stderr.as_slice());
        let archive_path = only_entry(&archives_path);
        assert_timestamp_range(&archive_path, "@seq", 7_000, 7_000);

        let extracted = extract_original(&archive_path, &temporary.path().join("mixed-extracted"));
        let mut expected = CANONICAL_JSONL.to_vec();
        expected.extend_from_slice(EXPECTED_KV_IR_JSONL);
        assert_eq!(
            semantic_json_records(&expected),
            semantic_json_records(&extracted)
        );
    }
}

#[test]
fn structurize_arrays_preserves_cpp_records() {
    let temporary = TestDirectory::new("structured-arrays");
    let input_path = temporary.path().join("input.jsonl");
    let archives_path = temporary.path().join("archives");
    fs::write(&input_path, STRUCTURED_ARRAY_INPUT).expect("write structured-array fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--no-adaptive-numeric-columns".as_ref(),
        "--dictionary-compression-level".as_ref(),
        "3".as_ref(),
        "--no-dictionary-long-distance-matching".as_ref(),
        "--single-file-archive".as_ref(),
        "--disable-log-order".as_ref(),
        "--structurize-arrays".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let archive_path = only_entry(&archives_path);
    assert_same_archive_records(&decode_hex(STRUCTURED_ARRAY_SFA_HEX), &fs::read(archive_path).unwrap());
}

#[test]
fn shape_projection_writes_clp_strings_as_logtype_templates() {
    // As the C++ `shape(...)` projection: each variable becomes `%int%`, `%float%` or `%str%`, a
    // literal `%` is doubled, and a field that is not a CLP string keeps its value.
    let input = concat!(
        r#"{"id":1,"msg":"took 15 ms for user alice42","kind":"plain"}"#,
        "\n",
        r#"{"id":2,"msg":"ratio 0.5 at 100% of 10.0.0.1","kind":"plain"}"#,
        "\n",
        r#"{"id":3,"msg":"no variables here at all","kind":"plain"}"#,
        "\n",
        r#"{"id":4,"msg":"escaped \\ backslash 7","kind":"x"}"#,
        "\n",
    );
    let temporary = TestDirectory::new("shape-projection");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, input).expect("write shape fixture");
    let archives_path = temporary.path().join("archives");
    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let archive = only_entry(&archives_path);
    let search = |projection: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec![
            "s".as_ref(),
            archive.as_os_str(),
            "id > 0".as_ref(),
            "--projection".as_ref(),
        ];
        arguments.extend(projection.iter().map(|path| OsStr::new(*path)));
        clp_s(&arguments)
    };
    let templates = concat!(
        r#"{"msg":"took %int% ms for user %str%"}"#,
        "\n",
        r#"{"msg":"ratio %float% at %int%%% of %str%"}"#,
        "\n",
        r#"{"msg":"no variables here at all"}"#,
        "\n",
        r#"{"msg":"escaped \\ backslash %int%"}"#,
        "\n",
    );
    let result = search(&["shape(msg)"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(templates, String::from_utf8_lossy(&result.stdout));
    // Selected both ways, the template wins, as in C++.
    assert_eq!(templates.as_bytes(), search(&["shape(msg)", "msg"]).stdout);
    // A field that is not a CLP string is written as its value.
    let result = search(&["shape(kind)"]);
    assert_eq!(
        concat!(
            r#"{"kind":"plain"}"#,
            "\n",
            r#"{"kind":"plain"}"#,
            "\n",
            r#"{"kind":"plain"}"#,
            "\n",
            r#"{"kind":"x"}"#,
            "\n",
        ),
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(!search(&["shape(msg)", "shape(msg)"]).status.success());
}

#[test]
fn searches_through_structured_arrays_read_only_what_they_need_and_answer_exactly() {
    // Three record shapes, each its own table stored one frame per column. A filter that passes
    // through the structured array reaches a Boolean inside it, a lookup matches one table of
    // many, and an object projection is written only from the tables a filter can match.
    let mut input = String::new();
    for index in 0..60 {
        let record = match index % 3 {
            0 => format!(
                "{{\"id\":{index},\"uuid\":\"u-{index}\",\"result\":\"failed {index}\",\
                 \"content\":[{{\"type\":\"tool_result\",\"is_error\":{},\
                 \"content\":\"result text {index}\"}}]}}",
                0 == index % 4
            ),
            1 => format!(
                "{{\"id\":{index},\"uuid\":\"u-{index}\",\
                 \"content\":[{{\"type\":\"text\",\"text\":\"assistant says {index}\"}}]}}"
            ),
            _ => format!(
                "{{\"id\":{index},\"uuid\":\"u-{index}\",\"isApiError\":true,\
                 \"content\":[{{\"type\":\"text\",\"text\":\"API Error {}\"}}]}}",
                index % 4
            ),
        };
        input.push_str(&record);
        input.push('\n');
    }
    let temporary = TestDirectory::new("structured-array-search");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, &input).expect("write structured-array fixture");
    let archives_path = temporary.path().join("archives");
    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let archive = only_entry(&archives_path);
    let records: Vec<serde_json::Value> = input
        .lines()
        .map(|line| serde_json::from_str(line).expect("fixture is JSONL"))
        .collect();
    let expected = |keep: &dyn Fn(&serde_json::Value) -> bool, fields: &[&str]| {
        let mut lines = Vec::new();
        for record in records.iter().filter(|record| keep(record)) {
            let value = if fields.is_empty() {
                record.clone()
            } else {
                let mut projected = serde_json::Map::new();
                for field in fields {
                    projected.insert((*field).to_owned(), record[*field].clone());
                }
                serde_json::Value::Object(projected)
            };
            lines.extend_from_slice(value.to_string().as_bytes());
            lines.push(b'\n');
        }
        semantic_json_records(&lines)
    };
    let search = |query: &str, projection: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str(), query.as_ref()];
        if !projection.is_empty() {
            arguments.push("--projection".as_ref());
            arguments.extend(projection.iter().map(|path| OsStr::new(*path)));
        }
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        semantic_json_records(&result.stdout)
    };
    let is_error = |record: &serde_json::Value| record["content"][0]["is_error"] == true;
    let is_api_error = |record: &serde_json::Value| record["isApiError"] == true;
    let lookup = |record: &serde_json::Value| record["uuid"] == "u-7";
    for (query, keep, projection) in [
        (
            "content.is_error:true",
            &is_error as &dyn Fn(&serde_json::Value) -> bool,
            &["result"][..],
        ),
        ("content.is_error:true", &is_error, &[][..]),
        ("isApiError:true", &is_api_error, &["content"][..]),
        ("uuid:\"u-7\"", &lookup, &["id"][..]),
        ("uuid:\"u-7\"", &lookup, &[][..]),
    ] {
        let wanted = expected(keep, projection);
        assert!(!wanted.is_empty(), "{query} matches the fixture");
        assert_eq!(wanted, search(query, projection), "{query} {projection:?}");
    }
}

#[test]
fn searches_read_the_scalar_elements_a_path_through_an_array_resolves_to() {
    // The query compiler resolves `content.text` to the object element's `text` and to the bare
    // message beside it, so a search must load both columns and their logtypes.
    let temporary = TestDirectory::new("array-scalar-elements");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(
        &input_path,
        "{\"content\":[{\"text\":\"took 7 ms\"},\"note x\"]}\n\
         {\"content\":[{\"text\":\"took 8 ms\"},\"note y\"]}\n",
    )
    .expect("write array fixture");
    for layout in [&[][..], &["--single-file-archive"][..]] {
        let archives_path = temporary.path().join(format!("archives{}", layout.len()));
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref()];
        arguments.extend(layout.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let archive = only_entry(&archives_path);
        for query in ["content.text:\"took 7*\"", "content.text:\"took 7 ms\""] {
            let result = clp_s(&["s".as_ref(), archive.as_os_str(), query.as_ref()]);
            assert!(
                result.status.success(),
                "{query}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                "{\"content\":[{\"text\":\"took 7 ms\"},\"note x\"]}\n",
                String::from_utf8_lossy(&result.stdout),
                "{query}"
            );
        }
    }
}

#[test]
fn adaptive_string_columns_read_back_exactly_and_answer_searches_the_same() {
    // Tables holding a number are stored one frame per column, so their string and CLP-string
    // columns are encoded adaptively: a few repeated strings (dictionary IDs that suit the
    // dictionary or run-length schemes), unique ones, and messages whose variables vary.
    let mut input = String::new();
    for index in 0..300 {
        input.push_str(&format!(
            "{{\"n\":{index},\"level\":\"{}\",\"id\":\"req-{index}\",\
             \"msg\":\"took {} ms for user u{}\",\"const\":\"same\"}}\n",
            ["info", "warn", "error"][index % 3],
            index * 7 % 50,
            index % 11
        ));
    }
    let temporary = TestDirectory::new("adaptive-strings");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, &input).expect("write adaptive-string fixture");
    let compress = |name: &str, extra: &[&str]| {
        let archives_path = temporary.path().join(name);
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref(), "--single-file-archive".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        only_entry(&archives_path)
    };
    let plain = compress("plain", &["--no-adaptive-string-columns"]);
    let adaptive = compress("adaptive", &[]);
    assert_ne!(
        fs::read(&plain).expect("read plain archive"),
        fs::read(&adaptive).expect("read adaptive archive")
    );
    assert_eq!(
        extract_original(&plain, &temporary.path().join("plain-extracted")),
        extract_original(&adaptive, &temporary.path().join("adaptive-extracted"))
    );
    let search = |archive: &Path, query: &str, projection: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str(), query.as_ref()];
        if !projection.is_empty() {
            arguments.push("--projection".as_ref());
            arguments.extend(projection.iter().map(|path| OsStr::new(*path)));
        }
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    };
    for (query, projection) in [
        ("level:warn", &["n", "id"][..]),
        ("id:\"req-17\"", &[][..]),
        ("msg:\"*user u3*\"", &["msg", "const"][..]),
        ("const:same AND n > 290", &["msg"][..]),
    ] {
        let expected = search(&plain, query, projection);
        assert!(!expected.is_empty(), "{query} matches the fixture");
        assert_eq!(
            expected,
            search(&adaptive, query, projection),
            "{query} {projection:?}"
        );
    }
    let version = |archive: &Path| fs::read(archive).expect("read archive")[4..8].to_vec();
    // Mixed JSON integers require archive version 0.10.
    assert_eq!(vec![0, 0, 15, 0], version(&adaptive));
}

#[test]
fn verbatim_text_fields_read_back_exactly_and_answer_searches_and_shapes_the_same() {
    // `msg` reuses a few templates like log messages; `note` brings a new one with nearly every
    // string, as prose does, so its strings are stored whole once there are enough to judge, or,
    // with a low enough length cutoff, because their templates are long. Some notes hold marker
    // bytes, which a verbatim string must escape.
    let words = [
        "alpha", "beta", "gamma", "delta", "kappa", "omega", "sigma", "theta",
    ];
    let mut input = String::new();
    for index in 0..200_usize {
        let note: Vec<String> = (0..=(index % 6))
            .map(|position| words[(index * 7 + position * 3 + index / 8) % 8].to_owned())
            .collect();
        let marker = if 0 == index % 10 { "\\u0011 \\\\ " } else { "" };
        input.push_str(&format!(
            "{{\"n\":{index},\"msg\":\"request {index} took {} ms\",\
             \"note\":\"{marker}step {index}: {} then file_{}.rs line {}\"}}\n",
            index * 3 % 50,
            note.join(" "),
            index % 13,
            index * 11
        ));
    }
    let temporary = TestDirectory::new("verbatim-text-fields");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, &input).expect("write verbatim-text fixture");
    let compress = |name: &str, extra: &[&str]| {
        let archives_path = temporary.path().join(name);
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        only_entry(&archives_path)
    };
    let ordered = |archive: &Path, name: &str| {
        let output = temporary.path().join(name);
        let result = clp_s(&[
            "x".as_ref(),
            "--ordered".as_ref(),
            archive.as_os_str(),
            output.as_os_str(),
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut files: Vec<PathBuf> = fs::read_dir(&output)
            .expect("list extraction")
            .map(|entry| entry.expect("extraction entry").path())
            .collect();
        files.sort();
        files
            .into_iter()
            .flat_map(|file| fs::read(file).expect("read extraction"))
            .collect::<Vec<u8>>()
    };
    let search = |archive: &Path, query: &str, projection: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str(), query.as_ref()];
        if !projection.is_empty() {
            arguments.push("--projection".as_ref());
            arguments.extend(projection.iter().map(|path| OsStr::new(*path)));
        }
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut lines: Vec<String> = String::from_utf8(result.stdout)
            .expect("search output is UTF-8")
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    };
    let templated = compress("templated", &["--no-verbatim-text-fields"]);
    let templated_log = fs::read(templated.join("log.dict")).expect("read templated log.dict");
    let templated_records = ordered(&templated, "templated-extracted");
    let by_reuse = compress("by-reuse", &["--verbatim-template-bytes", "0"]);
    let by_length = compress(
        "by-length",
        &[
            "--verbatim-text-min-strings",
            "0",
            "--verbatim-template-bytes",
            "40",
        ],
    );
    // The defaults judge reuse only from 64 strings and length only from 256 bytes, which no
    // note reaches, so they store the same notes whole as reuse alone.
    let defaults = compress("defaults", &[]);
    for (name, archive) in [
        ("by-reuse", &by_reuse),
        ("by-length", &by_length),
        ("defaults", &defaults),
    ] {
        assert_ne!(
            templated_log,
            fs::read(archive.join("log.dict")).expect("read log.dict"),
            "{name} stores notes whole"
        );
        assert_eq!(
            templated_records,
            ordered(archive, &format!("{name}-extracted")),
            "{name}"
        );
    }
    // `shape(...)` writes what the dictionary holds: a note's template where notes are templated,
    // its text where it is stored whole, as a long note is under the 40-byte rule.
    // `shape(..., all)` parses that text into the template it would have had.
    let long_note = |archive: &Path, projection: &str| {
        search(archive, "n:197", &[projection])
            .pop()
            .expect("note 197 matches")
    };
    let note = input.lines().nth(197).expect("fixture has note 197");
    let note: serde_json::Value = serde_json::from_str(note).expect("fixture is JSONL");
    let stored_whole = long_note(&by_length, "shape(note)");
    assert_eq!(
        serde_json::json!({ "note": note["note"].as_str().expect("note text").replace('%', "%%") }),
        serde_json::from_str::<serde_json::Value>(&stored_whole).expect("shape is JSON"),
        "a note stored whole is its own shape"
    );
    assert_eq!(
        long_note(&templated, "shape(note)"),
        long_note(&by_length, "shape(note, all)"),
        "parsed, it is the template it would have had"
    );
    assert_ne!(stored_whole, long_note(&templated, "shape(note)"));
    for verbatim in [&by_reuse, &by_length, &defaults] {
        for (query, projection) in [
            ("note:\"*file_7.rs*\"", &["n", "note"][..]),
            ("note:\"*gamma*\"", &[][..]),
            ("msg:\"request 4* took *\"", &["msg"][..]),
            ("n > 100", &["shape(note, all)", "shape(msg, all)"][..]),
            ("note:*", &["shape(note, all)"][..]),
        ] {
            let expected = search(&templated, query, projection);
            assert!(!expected.is_empty(), "{query} matches the fixture");
            assert_eq!(
                expected,
                search(verbatim, query, projection),
                "{query} {projection:?}"
            );
        }
    }
}

#[test]
fn floats_are_stored_as_values_unless_asked_and_written_as_asked() {
    // A float is stored as its binary64 value unless `--retain-float-format` keeps its spelling.
    // `x` and `s` write it as spelled where kept (`original`, the default), and otherwise, or for
    // any other notation or a precision, from its value.
    let temporary = TestDirectory::new("float-formats");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(
        &input_path,
        "{\"a\":0.1,\"b\":1e-7,\"c\":2.50,\"d\":452.07357499999995}\n",
    )
    .expect("write float fixture");
    let compress = |name: &str, extra: &[&str]| {
        let archives_path = temporary.path().join(name);
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        only_entry(&archives_path)
    };
    let search = |archive: &Path, extra: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str(), "*".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).expect("UTF-8 output")
    };
    let values = compress("values", &[]);
    let spelled = compress("spelled", &["--retain-float-format"]);
    let legacy = compress("legacy", &["--no-retain-float-format"]);
    let shortest = "{\"a\":0.1,\"b\":1e-7,\"c\":2.5,\"d\":452.07357499999995}\n";
    assert_eq!(shortest, search(&values, &[]));
    assert_eq!(shortest, search(&legacy, &[]), "the old flag is accepted");
    assert_eq!(
        "{\"a\":0.1,\"b\":1e-7,\"c\":2.50,\"d\":452.07357499999995}\n",
        search(&spelled, &[]),
        "a kept spelling is written as spelled"
    );
    for archive in [&values, &spelled] {
        assert_eq!(shortest, search(archive, &["--float-notation", "auto"]));
        assert_eq!(
            "{\"a\":0.1,\"b\":0.0000001,\"c\":2.5,\"d\":452.07357499999995}\n",
            search(archive, &["--float-notation", "plain"])
        );
        assert_eq!(
            "{\"a\":1e-1,\"b\":1e-7,\"c\":2.5e0,\"d\":4.5207357499999995e2}\n",
            search(archive, &["--float-notation", "scientific"])
        );
        assert_eq!(
            "{\"a\":0.100000,\"b\":0.000000,\"c\":2.500000,\"d\":452.073575}\n",
            search(
                archive,
                &["--float-notation", "plain", "--float-precision", "6"]
            )
        );
        assert_eq!(
            "{\"a\":0.10,\"b\":1.00e-7,\"c\":2.50,\"d\":452.07}\n",
            search(archive, &["--float-precision", "2"]),
            "a precision rewrites even a kept spelling"
        );
    }
    let output = temporary.path().join("extracted");
    let result = clp_s(&[
        "x".as_ref(),
        "--float-notation".as_ref(),
        "scientific".as_ref(),
        spelled.as_os_str(),
        output.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let extracted: Vec<u8> = fs::read_dir(&output)
        .expect("list extraction")
        .flat_map(|entry| fs::read(entry.expect("entry").path()).expect("read extraction"))
        .collect();
    assert_eq!(
        b"{\"a\":1e-1,\"b\":1e-7,\"c\":2.5e0,\"d\":4.5207357499999995e2}\n".as_slice(),
        extracted.as_slice()
    );
    let too_precise = clp_s(&[
        "s".as_ref(),
        values.as_os_str(),
        "*".as_ref(),
        "--float-precision".as_ref(),
        "341".as_ref(),
    ]);
    assert!(!too_precise.status.success());
}

#[test]
fn grouped_array_tables_read_back_exactly_and_answer_searches_the_same() {
    // Records whose arrays differ only in length and element order share a grouped stream: mixed
    // element types, objects whose fields vary, nested arrays, and CLP-string messages inside.
    let mut input = String::new();
    for index in 0..240_u32 {
        let elements: Vec<String> = (0..=(index % 5))
            .map(|position| match (index + position) % 4 {
                0 => format!("{{\"type\":\"text\",\"text\":\"step {position} took {index} ms\"}}"),
                1 => format!(
                    "{{\"type\":\"tool\",\"id\":\"t{index}\",\"is_error\":{}}}",
                    0 == index % 3
                ),
                2 => format!("\"note {position}\""),
                _ => format!("[{position},{index}]"),
            })
            .collect();
        input.push_str(&format!(
            "{{\"n\":{index},\"kind\":\"{}\",\"content\":[{}]}}\n",
            ["a", "b"][index as usize % 2],
            elements.join(",")
        ));
    }
    let temporary = TestDirectory::new("grouped-array-tables");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, &input).expect("write grouped-table fixture");
    let compress = |name: &str, extra: &[&str]| {
        let archives_path = temporary.path().join(name);
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        only_entry(&archives_path)
    };
    let ordered = |archive: &Path, name: &str| {
        let output = temporary.path().join(name);
        let result = clp_s(&[
            "x".as_ref(),
            "--ordered".as_ref(),
            archive.as_os_str(),
            output.as_os_str(),
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut bytes = Vec::new();
        let mut files: Vec<PathBuf> = fs::read_dir(&output)
            .expect("list extraction")
            .map(|entry| entry.expect("extraction entry").path())
            .collect();
        files.sort();
        for file in files {
            bytes.extend(fs::read(file).expect("read extraction"));
        }
        bytes
    };
    let search = |archive: &Path, query: &str, projection: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str(), query.as_ref()];
        if !projection.is_empty() {
            arguments.push("--projection".as_ref());
            arguments.extend(projection.iter().map(|path| OsStr::new(*path)));
        }
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut lines: Vec<String> = String::from_utf8(result.stdout)
            .expect("search output is UTF-8")
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    };
    for layout in [&[][..], &["--single-file-archive"][..]] {
        let separate: Vec<&str> = layout
            .iter()
            .copied()
            .chain(["--no-grouped-array-tables"])
            .collect();
        let name = if layout.is_empty() { "dir" } else { "sfa" };
        let plain = compress(&format!("{name}-plain"), &separate);
        let grouped = compress(&format!("{name}-grouped"), layout);
        assert_eq!(
            ordered(&plain, &format!("{name}-plain-extracted")),
            ordered(&grouped, &format!("{name}-grouped-extracted")),
            "{name}"
        );
        for (query, projection) in [
            ("content.is_error:true", &["n", "content"][..]),
            ("content.text:\"*took 7*\"", &["n"][..]),
            ("kind:b AND n > 200", &[][..]),
            ("content.type:tool", &["content"][..]),
        ] {
            let expected = search(&plain, query, projection);
            assert!(!expected.is_empty(), "{query} matches the fixture");
            assert_eq!(
                expected,
                search(&grouped, query, projection),
                "{name} {query} {projection:?}"
            );
        }
    }
}

#[test]
fn binary_uuids_read_back_exactly_and_answer_searches_the_same() {
    // Canonical UUIDs in their own fields and inside messages are stored as bytes; an uppercase
    // one and a near miss stay text. Every search must answer as it does with UUIDs as text.
    // Version-4 UUIDs are random but for their version and variant bits; splitmix64 stands in.
    let uuid = |seed: u64| {
        let mix = |mut value: u64| {
            value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
            value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            value ^ (value >> 31)
        };
        let (high, low) = (mix(seed), mix(seed ^ 0xFFFF));
        format!(
            "{:08x}-{:04x}-4{:03x}-{:x}{:03x}-{:012x}",
            high >> 32,
            (high >> 16) & 0xFFFF,
            high & 0xFFF,
            8 + (low >> 62),
            (low >> 48) & 0xFFF,
            low & 0xFFFF_FFFF_FFFF
        )
    };
    let mut input = String::new();
    for index in 0..200_u32 {
        let id = uuid(u64::from(index));
        let parent = uuid(u64::from(index / 2));
        input.push_str(&format!(
            "{{\"n\":{index},\"uuid\":\"{id}\",\"parentUuid\":\"{parent}\",\
             \"msg\":\"request {id} failed after {} ms\",\"upper\":\"{}\",\
             \"near\":\"{}\"}}\n",
            index % 13,
            uuid(u64::from(index) + 1000).to_uppercase(),
            &uuid(u64::from(index) + 2000)[..35]
        ));
    }
    let temporary = TestDirectory::new("binary-uuids");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, &input).expect("write binary-UUID fixture");
    let compress = |name: &str, extra: &[&str]| {
        let archives_path = temporary.path().join(name);
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref(), "--single-file-archive".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        only_entry(&archives_path)
    };
    let text = compress("text", &["--no-binary-uuids"]);
    let binary = compress("binary", &[]);
    assert!(
        fs::metadata(&binary).expect("stat binary archive").len()
            < fs::metadata(&text).expect("stat text archive").len()
    );
    assert_eq!(
        extract_original(&text, &temporary.path().join("text-extracted")),
        extract_original(&binary, &temporary.path().join("binary-extracted"))
    );
    let search = |archive: &Path, arguments: &[&str]| {
        let mut all: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str()];
        all.extend(arguments.iter().map(|argument| OsStr::new(*argument)));
        let result = clp_s(&all);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    };
    let id = uuid(17);
    for arguments in [
        vec![format!("uuid:\"{id}\""), "--projection".into(), "n".into()],
        vec![format!("parentUuid:\"{id}\"")],
        vec![
            format!("msg:\"*{id}*\""),
            "--projection".into(),
            "msg".into(),
        ],
        vec![
            format!("uuid:\"{}*\"", &id[..4]),
            "--projection".into(),
            "uuid".into(),
        ],
        vec![
            format!("upper:\"{}\"", uuid(1017).to_uppercase()),
            "--projection".into(),
            "n".into(),
        ],
        vec![
            format!("uuid:\"{}\"", id.to_uppercase()),
            "--ignore-case".into(),
        ],
        vec![format!("near:\"{}\"", &uuid(2017)[..35])],
        vec![
            "n > 190".into(),
            "--projection".into(),
            "uuid".into(),
            "parentUuid".into(),
        ],
    ] {
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let expected = search(&text, &arguments);
        assert!(!expected.is_empty(), "{arguments:?} matches the fixture");
        assert_eq!(expected, search(&binary, &arguments), "{arguments:?}");
    }
}

#[test]
fn field_logtype_dictionaries_read_back_exactly_and_answer_searches_the_same() {
    // Several text fields, some sharing a message, so their sections overlap and a field's search
    // must load only its own entries.
    let mut input = Vec::new();
    for index in 0..40 {
        let shared = format!("shared message {} for every field", index % 7);
        input.extend_from_slice(
            format!(
                "{{\"id\":{index},\"small\":\"small {} value\",\"note\":\"{shared}\",\
                 \"body\":\"long body text number {index} that only the body holds\",\
                 \"copy\":\"{shared}\"}}\n",
                index % 3
            )
            .as_bytes(),
        );
    }
    let temporary = TestDirectory::new("field-dictionaries");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, &input).expect("write field-dictionary fixture");
    let compress = |name: &str, extra: &[&str]| {
        let archives_path = temporary.path().join(name);
        let mut arguments: Vec<&OsStr> = vec!["c".as_ref(), "--single-file-archive".as_ref()];
        arguments.extend(extra.iter().map(|argument| OsStr::new(*argument)));
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        only_entry(&archives_path)
    };
    // Per-field dictionaries are the default; the plain archive opts out.
    let plain = compress("plain", &["--no-field-logtype-dictionaries"]);
    let fields = compress("fields", &[]);
    assert_ne!(
        fs::read(&plain).expect("read plain archive"),
        fs::read(&fields).expect("read per-field archive")
    );
    assert_eq!(
        extract_original(&plain, &temporary.path().join("plain-extracted")),
        extract_original(&fields, &temporary.path().join("fields-extracted"))
    );
    let search = |archive: &Path, query: &str, projection: &[&str]| {
        let mut arguments: Vec<&OsStr> = vec!["s".as_ref(), archive.as_os_str(), query.as_ref()];
        if !projection.is_empty() {
            arguments.push("--projection".as_ref());
            arguments.extend(projection.iter().map(|path| OsStr::new(*path)));
        }
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    };
    for (query, projection) in [
        ("small:\"small 1 value\"", &["id"][..]),
        ("note:\"*message 3*\"", &["copy"][..]),
        ("body:\"*number 2*\"", &["small", "note"][..]),
        ("copy:\"*message 5*\" AND id > 10", &[][..]),
        ("*", &["body"][..]),
    ] {
        let expected = search(&plain, query, projection);
        assert!(!expected.is_empty(), "{query} matches the fixture");
        assert_eq!(
            expected,
            search(&fields, query, projection),
            "{query} {projection:?}"
        );
    }
    // JSON integer columns require the version 0.10 mixed representation regardless
    // of the other selected compression options.
    // The header's version is bytes 4 to 8.
    let version = |archive: &Path| fs::read(archive).expect("read archive")[4..8].to_vec();
    assert_eq!(vec![0, 0, 15, 0], version(&fields));
    assert_eq!(
        vec![0, 0, 15, 0],
        version(&compress(
            "plain-strings",
            &["--no-adaptive-string-columns"]
        ))
    );
    assert_eq!(
        vec![0, 0, 15, 0],
        version(&compress("no-adaptive", &["--no-adaptive-numeric-columns"]))
    );
}

#[test]
fn arrays_are_structured_unless_disabled() {
    let temporary = TestDirectory::new("array-default");
    let input_path = temporary.path().join("input.jsonl");
    fs::write(&input_path, STRUCTURED_ARRAY_INPUT).expect("write structured-array fixture");
    let compress = |name: &str, array_flag: Option<&str>| {
        let archives_path = temporary.path().join(name);
        let mut arguments = vec![
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            "--disable-log-order".as_ref(),
        ];
        if let Some(flag) = array_flag {
            arguments.push(flag.as_ref());
        }
        arguments.extend([archives_path.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&arguments);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        fs::read(only_entry(&archives_path)).expect("read array-default SFA")
    };

    let default = compress("default", None);
    assert_eq!(
        compress("structured", Some("--structurize-arrays")),
        default,
        "arrays are structured by default, and the C++ flag still names that behaviour"
    );
    assert_ne!(
        compress("unstructured", Some("--no-structurize-arrays")),
        default
    );

    let conflicting_path = temporary.path().join("conflicting");
    let conflicting = clp_s(&[
        "c".as_ref(),
        "--structurize-arrays".as_ref(),
        "--no-structurize-arrays".as_ref(),
        conflicting_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(!conflicting.status.success());
}

#[test]
fn kv_ir_magic_legacy_and_truncation_routes_preserve_typed_errors_and_committed_prefixes() {
    let temporary = TestDirectory::new("kv-ir-errors");
    let fixture = decode_hex(FOUR_BYTE_KV_IR_HEX);

    let partial_magic_path = temporary.path().join("partial-magic.bin");
    let partial_magic_archives = temporary.path().join("partial-magic-archives");
    fs::write(&partial_magic_path, &fixture[..3]).expect("write partial KV-IR magic");
    let partial_magic = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        partial_magic_archives.as_os_str(),
        partial_magic_path.as_os_str(),
    ]);
    assert!(!partial_magic.status.success());
    let diagnostic = String::from_utf8_lossy(&partial_magic.stderr);
    assert!(diagnostic.contains("unsupported unknown binary data"));
    assert!(!diagnostic.contains("KV-IR stream"));

    let magic_only_path = temporary.path().join("magic-only.bin");
    let magic_only_archives = temporary.path().join("magic-only-archives");
    fs::write(&magic_only_path, &fixture[..4]).expect("write exact KV-IR magic");
    let magic_only = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        magic_only_archives.as_os_str(),
        magic_only_path.as_os_str(),
    ]);
    assert!(!magic_only.status.success());
    let diagnostic = String::from_utf8_lossy(&magic_only.stderr);
    assert!(diagnostic.contains("KV-IR stream 0 at input byte 4"));
    assert!(diagnostic.contains("truncated metadata header"));
    let stats: serde_json::Value =
        serde_json::from_slice(&magic_only.stdout).expect("magic-only partial stats remain JSON");
    assert_eq!(Some(0), stats["uncompressed_size"].as_u64());
    assert_eq!(Some(0), stats["range_index"].as_array().map(Vec::len));

    let mut legacy_bytes = fixture.clone();
    let version_start = legacy_bytes
        .windows(5)
        .position(|window| window == b"0.1.0")
        .expect("fixture contains current protocol version");
    legacy_bytes[version_start + 2] = b'0';
    legacy_bytes[version_start + 4] = b'2';
    let legacy_path = temporary.path().join("legacy.bin");
    let legacy_archives = temporary.path().join("legacy-archives");
    fs::write(&legacy_path, legacy_bytes).expect("write legacy KV-IR version fixture");
    let legacy = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        legacy_archives.as_os_str(),
        legacy_path.as_os_str(),
    ]);
    assert!(!legacy.status.success());
    let diagnostic = String::from_utf8_lossy(&legacy.stderr);
    assert!(diagnostic.contains("KV-IR stream 0"));
    assert!(diagnostic.contains("legacy unstructured IR version 0.0.2"));

    let truncated_path = temporary.path().join("truncated-event.bin");
    let truncated_archives = temporary.path().join("truncated-event-archives");
    let truncated = &fixture[..fixture.len() - 1];
    fs::write(&truncated_path, truncated).expect("write KV-IR stream without its end marker");
    let truncated_result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        truncated_archives.as_os_str(),
        truncated_path.as_os_str(),
    ]);
    assert!(!truncated_result.status.success());
    let diagnostic = String::from_utf8_lossy(&truncated_result.stderr);
    assert!(diagnostic.contains("KV-IR stream 0"));
    assert!(diagnostic.contains("truncated IR unit tag"));
    let stats: serde_json::Value = serde_json::from_slice(&truncated_result.stdout)
        .expect("truncated KV-IR partial stats remain JSON");
    assert_eq!(
        Some(u64::try_from(truncated.len()).unwrap()),
        stats["uncompressed_size"].as_u64()
    );
    assert_eq!(Some(0), stats["range_index"][0]["s"].as_u64());
    assert_eq!(Some(1), stats["range_index"][0]["e"].as_u64());
    assert_eq!(
        truncated_path.to_str(),
        stats["range_index"][0]["f"]["_filename"].as_str()
    );
    assert_eq!(
        EXPECTED_KV_IR_JSONL,
        extract_original(
            &only_entry(&truncated_archives),
            &temporary.path().join("truncated-extracted")
        )
        .as_slice()
    );
}

#[test]
fn incomplete_later_document_matches_cpp_warning_and_retains_the_committed_prefix() {
    let temporary = TestDirectory::new("malformed-prefix");
    let input_path = temporary.path().join("input.json");
    let archives_path = temporary.path().join("archives");
    fs::write(&input_path, b"{\"n\":0}\n{").expect("write malformed parse-many fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--print-archive-stats".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(result.status.success());
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(diagnostic.contains("ignored 1 truncated JSON bytes"));

    let archive_path = only_entry(&archives_path);
    let stats: serde_json::Value =
        serde_json::from_slice(&result.stdout).expect("partial archive stats remain valid JSON");
    assert_eq!(Some(9), stats["uncompressed_size"].as_u64());
    assert_eq!(Some(1), stats["range_index"][0]["e"].as_u64());
    assert_eq!(Some(0), stats["range_index"][0]["s"].as_u64());
    assert_eq!(
        input_path.to_str(),
        stats["range_index"][0]["f"]["_filename"].as_str()
    );

    let mut archive = SingleFileArchiveReader::open(
        File::open(archive_path).expect("open partial source-range archive"),
    )
    .expect("open partial SFA envelope");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read partial archive metadata");
    let range_index = metadata
        .range_index()
        .expect("partial archive retains committed source range");
    let [range] = range_index.entries() else {
        panic!("partial archive has one source range")
    };
    assert_eq!(0..1, range.range());
}

#[test]
fn zero_target_rotates_after_each_record_and_retains_trailing_bytes() {
    let temporary = TestDirectory::new("rotation");
    let input_path = temporary.path().join("input.json");
    let archives_path = temporary.path().join("archives");
    fs::write(&input_path, b"{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n")
        .expect("write rotation fixture");

    let result = clp_s(&[
        "c".as_ref(),
        "--disable-log-order".as_ref(),
        "--single-file-archive".as_ref(),
        "--target-encoded-size".as_ref(),
        "0".as_ref(),
        "--print-archive-stats".as_ref(),
        archives_path.as_os_str(),
        input_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).expect("statistics are UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(5, lines.len());
    for (line, uncompressed_size) in lines[..4].iter().zip([7, 8, 8, 8]) {
        assert!(line.contains("\"is_split\":true"));
        assert!(line.ends_with(&format!("\"uncompressed_size\":{uncompressed_size}}}")));
    }
    assert!(lines[4].contains("\"is_split\":false"));
    assert!(lines[4].ends_with("\"uncompressed_size\":1}"));
    assert_eq!(
        5,
        fs::read_dir(archives_path).expect("read archives").count()
    );
}

#[test]
fn invalid_preflight_does_not_create_the_output_root() {
    let temporary = TestDirectory::new("preflight");
    let archives_path = temporary.path().join("archives");
    let missing_path = temporary.path().join("missing.json");
    let result = clp_s(&[
        "c".as_ref(),
        archives_path.as_os_str(),
        missing_path.as_os_str(),
    ]);
    assert!(!result.status.success());
    assert!(!archives_path.exists());
    assert_eq!(b"", result.stdout.as_slice());
}


#[test]
fn frame_of_reference_filters_match_plain_archives_and_preserve_complete_records() {
    use clp_s::archive::{ArchiveCatalogLimits, ColumnLimits, PackedStreamLimits};
    let temporary = TestDirectory::new("for-predicate-bounds");
    let input_path = temporary.path().join("input.jsonl");
    let mut input = String::new();
    let mut state = 17_u64;
    for row in 0..4097 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let offset = (state >> 61) as i64;
        input.push_str(&format!(
            "{{\"id\":{},\"big\":{},\"neg\":{},\"top\":{},\"msg\":\"row {row}\"}}\n",
            22412 + offset, (1_i64 << 53) + offset, -5 + offset, i64::MAX - 7 + offset,
        ));
    }
    input.push_str("{\"id\":null,\"msg\":\"null\"}\n{\"msg\":\"missing\"}\n{\"id\":\"22419\",\"msg\":\"string\"}\n");
    fs::write(&input_path, input).unwrap();
    let compress = |name: &str, extra: &[&str]| {
        let output = temporary.path().join(name);
        let mut args: Vec<&OsStr> = vec!["c".as_ref(), "--single-file-archive".as_ref()];
        args.extend(extra.iter().map(OsStr::new));
        args.extend([output.as_os_str(), input_path.as_os_str()]);
        let result = clp_s(&args);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        only_entry(&output)
    };
    let adaptive = compress("adaptive", &[]);
    let plain = compress("plain", &["--no-adaptive-numeric-columns", "--separate-columns-min-size", "0"]);
    // Verify that the adaptive writer actually selected FOR in this fixture.
    let mut reader = SingleFileArchiveReader::open(File::open(&adaptive).unwrap()).unwrap();
    let catalog = reader.read_catalog(ArchiveCatalogLimits::default()).unwrap();
    let mut found = false;
    for stream_id in 0..catalog.table_metadata().packed_streams().len() {
        let stream = reader.read_packed_stream(catalog.metadata(), catalog.table_metadata(), stream_id,
            PackedStreamLimits::default()).unwrap();
        let stream = catalog.materialize_adaptive_numerics(stream_id as u64, stream, ColumnLimits::default()).unwrap();
        found |= stream.column_layout().unwrap_or(&[]).iter().any(|slot|
            slot.numeric_encoding == Some(clp_s::archive::NumericEncoding::FrameOfReference));
    }
    assert!(found, "fixture must exercise compact FOR");
    for query in ["*", "id:22419", "id:22420", "id > 22419", "id >= 22412", "id < 22412",
        "NOT id:22420", "id:22420 OR id:22412", "id:22420 AND id:22412",
        "id:22412 OR NOT id:22419", "id > 22412.5", "id < 22412.5", "id:22412.5",
        "big:9007199254740993", "big:9007199254740992.0", "big > 9007199254740992.0",
        "neg < -0.5", "top:9223372036854775807", "top < 9223372036854775808.0"] {
        let search = |path: &Path| {
            let output = clp_s(&["s".as_ref(), path.as_os_str(), query.as_ref()]);
            assert!(output.status.success(), "{query}: {}", String::from_utf8_lossy(&output.stderr));
            let mut lines: Vec<String> = String::from_utf8(output.stdout).unwrap().lines().map(str::to_owned).collect();
            lines.sort(); lines
        };
        assert_eq!(search(&plain), search(&adaptive), "{query}");
    }
}

#[test]
fn string_timestamp_units_work_for_both_layouts_and_kql_windows() {
    // String formats supply their own time; the declared unit quantizes them.
    for single_file in [false, true] {
        for (rounding, numeric, text, end_ms) in [
            ("truncate", "1679456094.576000000", "2023-03-21T23:34:54.576000000-04:00", "1679456094576"),
            ("nearest", "1679456094.577000000", "2023-03-21T23:34:54.577000000-04:00", "1679456094577"),
        ] {
            let temporary = TestDirectory::new("timestamp-storage");
            let input = temporary.path().join("input.json");
            let archives = temporary.path().join("archives");
            fs::write(&input, concat!(
                "{\"ts\":\"1679456094.576789012\",\"id\":1}\n",
                "{\"ts\":\"2023-03-21T23:34:54.576789012-04:00\",\"id\":2}\n"
            )).unwrap();
            let mut args = vec!["c".as_ref(), "--timestamp-key".as_ref(), "ts".as_ref(),
                "--timestamp-precision".as_ref(), "ms".as_ref(),
                "--timestamp-rounding".as_ref(), rounding.as_ref()];
            if single_file { args.push("--single-file-archive".as_ref()); }
            args.extend([archives.as_os_str(), input.as_os_str()]);
            let result = clp_s(&args);
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            let archive = only_entry(&archives);
            let expected = vec![serde_json::json!({"ts":numeric,"id":1}), serde_json::json!({"ts":text,"id":2})];
            let extracted = extract_original(&archive, &temporary.path().join("extracted"));
            assert_eq!(semantic_json_records(&extracted), expected);
            let result = clp_s(&["s".as_ref(), archive.as_os_str(), "*".as_ref(),
                "--tge".as_ref(), end_ms.as_ref(), "--tle".as_ref(), end_ms.as_ref()]);
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            assert_eq!(semantic_json_records(&result.stdout), expected);
        }
    }
}

#[test]
fn per_field_timestamp_units_precision_and_primary_order() {
    use clp_s::timestamp_precision::TimestampPrecision;
    for single_file in [true, false] {
        let temporary = TestDirectory::new("field-timestamp-options");
        let input = temporary.path().join("input.json");
        let archives = temporary.path().join("archives");
        // Different precisions, overlapping nested names and an escaped colon. Primary field
        // appears last in JSON to verify declaration order, not encounter order, chooses bounds.
        let source = concat!(
            "{\"id\":1,\"received\":1050123456,\"a\":{\"ts\":1051},\"b\":{\"ts\":1051},\"literal:key\":1050,\"event_time\":1050}\n",
            "{\"id\":2,\"received\":1150123456,\"a\":{\"ts\":1151},\"b\":{\"ts\":1151},\"literal:key\":1150,\"event_time\":1150}\n",
        );
        fs::write(&input, source).unwrap();
        let mut args = vec!["c".as_ref(),
            "--timestamp-key".as_ref(), "event_time:precision=ms".as_ref(),
            "--timestamp-key".as_ref(), "received:precision=us".as_ref(),
            "--timestamp-key".as_ref(), "a.ts:precision=s:rounding=nearest".as_ref(),
            "--timestamp-key".as_ref(), r"literal\u003akey:precision=ms".as_ref()];
        if single_file { args.push("--single-file-archive".as_ref()); }
        args.extend([archives.as_os_str(), input.as_os_str()]);
        let result = clp_s(&args);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let archive = only_entry(&archives);
        let expected = vec![
            serde_json::json!({"id":1,"received":1050123456_i64,"a":{"ts":1051},"b":{"ts":1051},"literal:key":1050,"event_time":1050}),
            serde_json::json!({"id":2,"received":1150123456_i64,"a":{"ts":1151},"b":{"ts":1151},"literal:key":1150,"event_time":1150})];
        let extracted = extract_original(&archive, &temporary.path().join("extracted"));
        let mut expected_sorted = expected.clone(); expected_sorted.sort_by_key(serde_json::Value::to_string);
        assert_eq!(semantic_json_records(&extracted), expected_sorted);
        let result = clp_s(&["s".as_ref(), archive.as_os_str(), "*".as_ref(), "--tge".as_ref(), "1050".as_ref(), "--tle".as_ref(), "1050".as_ref()]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(semantic_json_records(&result.stdout), vec![expected[0].clone()]);
        if single_file {
            let mut reader = SingleFileArchiveReader::open(File::open(&archive).unwrap()).unwrap();
            let metadata = reader.read_metadata(MetadataLimits::default()).unwrap();
            let ranges = metadata.timestamp_dictionary().ranges();
            assert_eq!(ranges[0].key(), "event_time");
            assert_eq!(ranges[0].storage_precision(), TimestampPrecision::Milliseconds);
            assert_eq!(ranges[1].storage_precision(), TimestampPrecision::Microseconds);
            assert_eq!(ranges[2].storage_precision(), TimestampPrecision::Seconds);
        }
    }
}

#[test]
fn duplicate_timestamp_paths_and_invalid_field_options_fail_before_output_creation() {
    for fields in [vec!["ts:precision=ms", "ts:precision=ns"],
        vec![r"a\.b:precision=ms", r"a\u002eb:precision=ns"],
        vec!["ts:precision=ms:increment=100ms"], vec!["ts:precison=ms"]] {
        let temporary = TestDirectory::new("invalid-timestamp-options");
        let output = temporary.path().join("archives");
        let input = temporary.path().join("input.json");
        fs::write(&input, "{\"ts\":1050}\n").unwrap();
        let mut args = vec!["c".as_ref()];
        for field in &fields { args.extend([OsStr::new("--timestamp-key"), OsStr::new(field)]); }
        args.extend([output.as_os_str(), input.as_os_str()]);
        let result = clp_s(&args);
        assert!(!result.status.success());
        assert!(!output.exists());
    }
}

#[test]
fn field_options_survive_archive_rotation_and_zeroed_fractional_seconds() {
    let temp = TestDirectory::new("timestamp-fields-rotation");
    let input = temp.path().join("input.json");
    let output = temp.path().join("archives");
    fs::write(&input, "{\"relative\":1051,\"seconds\":1.25}\n{\"relative\":1151,\"seconds\":2.25}\n").unwrap();
    let result = clp_s(&["c".as_ref(), "--single-file-archive".as_ref(),
        "--target-encoded-size".as_ref(), "1".as_ref(),
        "--timestamp-key".as_ref(), "seconds:precision=s".as_ref(),
        "--timestamp-key".as_ref(), "relative:precision=ms:rounding=nearest".as_ref(),
        output.as_os_str(), input.as_os_str()]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let mut seconds = Vec::new();
    for entry in fs::read_dir(&output).unwrap() {
        let path = entry.unwrap().path();
        let mut reader = SingleFileArchiveReader::open(File::open(&path).unwrap()).unwrap();
        let metadata = reader.read_metadata(MetadataLimits::default()).unwrap();
        // A source ending exactly at the rotation threshold can publish an empty final archive.
        if metadata.timestamp_dictionary().ranges().is_empty() {
            let result = clp_s(&["s".as_ref(), path.as_os_str(), "*".as_ref()]);
            assert!(result.status.success());
            assert!(result.stdout.is_empty());
            continue;
        }
        assert_eq!(metadata.timestamp_dictionary().ranges()[0].key(), "seconds");
        let result = clp_s(&["s".as_ref(), path.as_os_str(), "*".as_ref()]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let rows = semantic_json_records(&result.stdout);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0]["relative"].as_i64(), Some(1051 | 1151)));
        seconds.push(rows[0]["seconds"].as_f64().unwrap() as i64);
    }
    seconds.sort();
    assert_eq!(seconds, [1, 2]);
}

#[test]
fn removed_timestamp_storage_factor_flag_is_rejected_before_output_creation() {
    let temporary = TestDirectory::new("removed-timestamp-factor");
    let output = temporary.path().join("archives");
    let input = temporary.path().join("input.json");
    fs::write(&input, "{\"ts\":1050}\n").unwrap();
    let result = clp_s(&["c".as_ref(), "--timestamp-storage-factor".as_ref(),
        "100".as_ref(), output.as_os_str(), input.as_os_str()]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--timestamp-storage-factor"));
    assert!(!output.exists());
}

#[test]
fn native_tick_durations_roundtrip_in_both_archive_layouts() {
    for single_file in [false, true] {
        for (duration, scale) in [("us", 1_000), ("1us", 1_000), ("10us", 10_000),
            ("100us", 100_000), ("250us", 250_000), ("ms", 1_000_000), ("1ms", 1_000_000),
            ("10ms", 10_000_000), ("100ms", 100_000_000), ("s", 1_000_000_000),
            ("1s", 1_000_000_000), ("10s", 10_000_000_000)] {
            let temp = TestDirectory::new("native-ticks");
            let input = temp.path().join("input.json");
            let output = temp.path().join("archives");
            let source_ns = 123_456_i64;
            fs::write(&input, format!("{{\"ts\":{source_ns}}}\n")).unwrap();
            let field = format!("ts:precision={duration}");
            let mut args = vec!["c".as_ref(), "--timestamp-key".as_ref(), OsStr::new(&field)];
            if single_file { args.push("--single-file-archive".as_ref()); }
            args.extend([output.as_os_str(), input.as_os_str()]);
            let result = clp_s(&args);
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            let archive = only_entry(&output);
            let expected = source_ns;
            let extracted = extract_original(&archive, &temp.path().join("extracted"));
            assert_eq!(semantic_json_records(&extracted), vec![serde_json::json!({"ts":expected})]);
            if single_file {
                let mut reader = SingleFileArchiveReader::open(File::open(&archive).unwrap()).unwrap();
                let metadata = reader.read_metadata(MetadataLimits::default()).unwrap();
                assert_eq!(metadata.timestamp_dictionary().ranges()[0].storage_precision().nanoseconds_per_tick(), scale);
            }
        }
    }
}

#[test]
fn invalid_native_tick_durations_fail_before_creating_archives() {
    for duration in ["0us", "-10ms", "1.5ms", "10", "10ps", "9223372036854775808ns"] {
        let temp = TestDirectory::new("bad-native-ticks");
        let input = temp.path().join("input.json");
        fs::write(&input, "{\"ts\":1050}\n").unwrap();
        for global in [false, true] {
            let output = temp.path().join(if global { "global" } else { "field" });
            let field = format!("ts:precision={duration}");
            let args = if global {
                vec![OsStr::new("c"), OsStr::new("--timestamp-key"), OsStr::new("ts"),
                    OsStr::new("--timestamp-precision"), OsStr::new(duration), output.as_os_str(), input.as_os_str()]
            } else {
                vec![OsStr::new("c"), OsStr::new("--timestamp-key"), OsStr::new(&field), output.as_os_str(), input.as_os_str()]
            };
            let result = clp_s(&args);
            assert!(!result.status.success(), "{duration}");
            assert!(!output.exists());
        }
    }
}

#[test]
fn default_unit_is_nanoseconds_and_old_options_are_rejected() {
    let temp = TestDirectory::new("unified-timestamp-default");
    let input = temp.path().join("input.json");
    fs::write(&input, "{\"ts\":12345}\n").unwrap();
    let output = temp.path().join("archives");
    let result = clp_s(&["c".as_ref(), "--single-file-archive".as_ref(),
        "--timestamp-key".as_ref(), "ts".as_ref(), output.as_os_str(), input.as_os_str()]);
    assert!(result.status.success());
    let path = only_entry(&output);
    assert_timestamp_range(&path, "ts", 0, 1);
    for field in ["ts:input-unit=ms", "ts:unit=ms"] {
        let failed = temp.path().join("failed");
        let result = clp_s(&["c".as_ref(), "--timestamp-key".as_ref(), field.as_ref(),
            failed.as_os_str(), input.as_os_str()]);
        assert!(!result.status.success());
        assert!(!failed.exists());
    }
    let failed = temp.path().join("failed");
    let result = clp_s(&["c".as_ref(), "--timestamp-unit".as_ref(), "ms".as_ref(),
        failed.as_os_str(), input.as_os_str()]);
    assert!(!result.status.success());
    assert!(!failed.exists());
}

#[test]
fn auto_timestamp_precision_preserves_mixed_inputs_and_resolves_native_ticks() {
    let cases = [
        (vec!["1700000000", "1700000000123", "\"1700000000456\""], 1_000_000),
        (vec!["1700000000", "1700000000123456", "1700000000123", "\"1700000000456789\""], 1_000),
        (vec!["1700000000", "1700000000123", "1700000000123456789"], 1),
        (vec!["-1700000000", "-1700000000123"], 1_000_000),
        (vec!["\"2023-11-14T22:13:20Z\"", "\"2023-11-14T22:13:20.10Z\""], 1_000_000),
        (vec!["\"2023-11-14T22:13:20.12340-04:00\"", "\"2023-11-14 22:13:20.1200 EDT\""], 1_000),
        (vec!["1700000000.10", "\"1700000000.12345670\""], 1),
    ];
    for (values, scale) in cases {
        for single_file in [false, true] {
            for global in [false, true] {
                let tmp = TestDirectory::new("auto-precision");
                let input = tmp.path().join("input.ndjson");
                let output = tmp.path().join("archives");
                // Deliberately change encoded schema while promoting precision, then reuse it.
                let source = values.iter().enumerate().map(|(i, value)| format!(
                    "{{\"ts\":{value},\"id\":{i},\"explicit\":12345{} }}\n",
                    if i % 2 == 0 { ",\"extra\":true" } else { "" },
                )).collect::<String>();
                fs::write(&input, &source).unwrap();
                let mut args = vec!["c".as_ref(), "--timestamp-key".as_ref(),
                    if global { "ts".as_ref() } else { "ts:precision=auto".as_ref() },
                    "--timestamp-key".as_ref(), "explicit:precision=100us".as_ref()];
                if global { args.extend([OsStr::new("--timestamp-precision"), OsStr::new("auto")]); }
                if single_file { args.push("--single-file-archive".as_ref()); }
                args.extend([output.as_os_str(), input.as_os_str()]);
                let result = clp_s(&args);
                assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
                let archive = only_entry(&output);
                let extracted = extract_original(&archive, &tmp.path().join("extracted"));
                assert_eq!(semantic_json_records(&extracted), semantic_json_records(source.as_bytes()));
                if single_file {
                    use clp_s::archive::{ArchiveCatalogLimits, ColumnData, ColumnLimits, PackedStreamLimits};
                    let mut reader = SingleFileArchiveReader::open(File::open(&archive).unwrap()).unwrap();
                    let catalog = reader.read_catalog(ArchiveCatalogLimits::default()).unwrap();
                    let ranges = catalog.metadata().timestamp_dictionary().ranges();
                    let range = ranges.iter().find(|r| r.key() == "ts").unwrap();
                    assert_eq!(range.storage_precision().nanoseconds_per_tick(), scale);
                    let mut rows = 0;
                    for stream_id in 0..catalog.table_metadata().packed_streams().len() {
                        let stream = reader.read_packed_stream(catalog.metadata(), catalog.table_metadata(), stream_id, PackedStreamLimits::default()).unwrap();
                        let stream = catalog.materialize_adaptive_numerics(stream_id as u64, stream, ColumnLimits::default()).unwrap();
                        for table in catalog.schema_tables(stream_id as u64, &stream, ColumnLimits::default()).unwrap() {
                            for column in table.unwrap().table().columns() {
                                if let ColumnData::Timestamp(ts) = column.data() {
                                    for (tick, ns) in ts.epochs().ticks().zip(ts.epochs().values()) {
                                        assert_eq!(tick.checked_mul(ts.epochs().scale()), Some(ns));
                                        rows += 1;
                                    }
                                }
                            }
                        }
                    }
                    assert_eq!(rows, values.len() * 2);
                }
            }
        }
    }
}

#[test]
fn auto_precision_survives_archive_rotation_and_kv_ir_input() {
    for fixture in [FOUR_BYTE_KV_TIMESTAMP_HEX, EIGHT_BYTE_KV_TIMESTAMP_HEX] {
        let tmp = TestDirectory::new("auto-kv");
        let input = tmp.path().join("input.clp");
        fs::write(&input, decode_hex(fixture)).unwrap();
        // Untagged decode is the independent source-value reference.
        let mut reference = Vec::new();
        for (label, options) in [
            ("plain", vec![]),
            ("auto", vec!["--timestamp-key", "ts:precision=auto"]),
            ("rotated", vec!["--timestamp-key", "ts", "--timestamp-precision", "auto", "--target-encoded-size", "0"]),
        ] {
            let archives = tmp.path().join(label);
            let mut args = vec![OsStr::new("c"), OsStr::new("--single-file-archive")];
            args.extend(options.iter().map(OsStr::new));
            args.extend([archives.as_os_str(), input.as_os_str()]);
            let result = clp_s(&args);
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            let extracted = extract_original(&archives, &tmp.path().join(format!("{label}-extracted")));
            let mut rows = semantic_json_records(&extracted);
            // serde_json's default decimal parser can round a long epoch fraction by one
            // ULP; compare this binary64 source with Rust's correctly rounded float parser.
            for line in std::str::from_utf8(&extracted).unwrap().lines() {
                if line.contains("\"float\"") {
                    let lexeme = line.split("\"ts\":").nth(1).unwrap().split([',', '}']).next().unwrap();
                    let parsed = serde_json::json!(lexeme.parse::<f64>().unwrap());
                    rows.iter_mut().find(|row| row["kind"] == "float").unwrap()["ts"] = parsed;
                }
            }
            if label == "plain" { reference = rows; }
            else { assert_eq!(rows, reference); }
            if label == "rotated" {
                let mut scales = std::collections::BTreeSet::new();
                for entry in fs::read_dir(&archives).unwrap() {
                    let mut reader = SingleFileArchiveReader::open(File::open(entry.unwrap().path()).unwrap()).unwrap();
                    let metadata = reader.read_metadata(MetadataLimits::default()).unwrap();
                    for range in metadata.timestamp_dictionary().ranges() {
                        scales.insert(range.storage_precision().nanoseconds_per_tick());
                    }
                }
                assert!(scales.len() > 1, "each archive must resolve its own scale: {scales:?}");
            }
        }
    }
}

#[test]
fn explicit_precision_quantizes_every_fraction_width_without_losing_format() {
    let tmp = TestDirectory::new("fixed-fraction-rounding");
    let input = tmp.path().join("input.ndjson");
    let output = tmp.path().join("archives");
    let source = (1..=9).map(|width| format!(
        "{{\"ts\":\"2023-11-14T22:13:20.{}Z\",\"id\":{width}}}\n", "1".repeat(width),
    )).collect::<String>();
    let expected = (1..=9).map(|width| format!(
        "{{\"ts\":\"2023-11-14T22:13:20.{}Z\",\"id\":{width}}}\n", "0".repeat(width),
    )).collect::<String>();
    fs::write(&input, source).unwrap();
    let result = clp_s(&["c".as_ref(), "--single-file-archive".as_ref(), "--timestamp-key".as_ref(),
        "ts:precision=s".as_ref(), output.as_os_str(), input.as_os_str()]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let extracted = extract_original(&output, &tmp.path().join("extracted"));
    assert_eq!(semantic_json_records(&extracted), semantic_json_records(expected.as_bytes()));
}
