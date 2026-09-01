#![cfg(feature = "cli")]

use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;

const LOG_ORDER_ARCHIVE: &str = "sfa-v0.5.0-log-order-cpp.bin";
const LOG_ORDER_UNORDERED_JSONL: &str = "sfa-v0.5.0-log-order-cpp-unordered.jsonl";
const AGGREGATIONS_ARCHIVE: &str = "sfa-v0.5.0-aggregations-cpp.bin";
const MINIMAL_ARCHIVE: &str = "sfa-v0.5.0-minimal-cpp.bin";
const MINIMAL_JSONL: &str = "sfa-v0.5.0-minimal-cpp-input.jsonl";
const RETAINED_FLOATS_ARCHIVE: &str = "sfa-v0.5.0-retained-floats-cpp.bin";
const RETAINED_FLOATS_JSONL: &str = "sfa-v0.5.0-retained-floats-cpp-input.jsonl";
const STRINGS_ARCHIVE: &str = "sfa-v0.5.0-strings-cpp.bin";
const STRINGS_JSONL: &str = "sfa-v0.5.0-strings-cpp-input.jsonl";
const TIMESTAMPS_ARCHIVE: &str = "sfa-v0.5.0-timestamps-cpp.bin";
const TIMESTAMPS_JSONL: &str = "sfa-v0.5.0-timestamps-cpp-input.jsonl";
const ARRAYS_ARCHIVE: &str = "sfa-v0.5.0-unstructured-arrays-cpp.bin";
const ARRAYS_JSONL: &str = "sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl";
const LOG_ORDER_MSGPACK_HEX: &str = "search-file-v0.5.0-log-order-cpp.hex";
const MINIMAL_MSGPACK_HEX: &str = "search-file-v0.5.0-minimal-cpp.hex";
const PROJECTION_MSGPACK_HEX: &str = "search-file-v0.5.0-projection-cpp.hex";
const STRINGS_MSGPACK_HEX: &str = "search-file-v0.5.0-strings-cpp.hex";
const NETWORK_MSGPACK_HEX: &str = "search-network-v0.5.0-minimal-cpp.hex";
const REDUCER_COUNT_ONE_FRAME: &[u8] = b"\x82\xaagroup_tags\x90\xa7records\x91\x81\xa5count\x01";
const KV_IR_FOUR_BYTE_HEX: &str = "kv-ir-v0.1.0-four-byte-cpp.hex";
const KV_IR_NESTED_HEX: &str = "kv-ir-search-v0.1.0-nested-cpp.hex";
const KV_IR_CANONICAL_JSONL: &[u8] = concat!(
    "{\"auto_generated_kv_pairs\":{\"level\":\"info\",\"seq\":7},",
    "\"user_generated_kv_pairs\":{\"empty\":{},\"message\":\"task 42 done\",",
    "\"none\":null,\"ok\":true,\"ratio\":1.25}}\n"
)
.as_bytes();
const KV_IR_NESTED_FIRST_JSONL: &[u8] = concat!(
    "{\"auto_generated_kv_pairs\":{},\"user_generated_kv_pairs\":{\"a\":1,",
    "\"empty\":{},\"none\":null,\"obj\":{\"a\":true,\"b\":\"bee\"},\"z\":9}}\n"
)
.as_bytes();

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-rust-cli-search-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique CLI search test directory");
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

fn clp_s(arguments: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clp-s"))
        .args(arguments)
        .output()
        .expect("run Rust clp-s binary")
}

fn decode_hex_fixture(name: &str) -> Vec<u8> {
    let source = fs::read(fixture(name)).expect("read hex fixture");
    let digits: Vec<u8> = source
        .into_iter()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let (pairs, remainder) = digits.as_chunks::<2>();
    let decoded = pairs
        .iter()
        .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
        .collect();
    assert!(remainder.is_empty(), "hex fixture has even length");
    decoded
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("invalid hex fixture byte"),
    }
}

#[derive(Debug)]
struct ReducerCapture {
    job_id: i64,
    payload: Vec<u8>,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct ReducerRecordGroup {
    group_tags: Vec<String>,
    records: Vec<ReducerRecord>,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct ReducerRecord {
    count: i64,
}

fn spawn_reducer_capture(response: u8) -> (u16, thread::JoinHandle<io::Result<ReducerCapture>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback reducer listener");
    listener
        .set_nonblocking(true)
        .expect("make loopback reducer listener nonblocking");
    let port = listener
        .local_addr()
        .expect("read loopback reducer listener address")
        .port();
    let capture = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut connection, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if io::ErrorKind::WouldBlock == error.kind() => {
                    if deadline <= Instant::now() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Rust clp-s did not connect to the reducer",
                        ));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        };
        drop(listener);
        connection.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut encoded_job_id = [0_u8; size_of::<i64>()];
        connection.read_exact(&mut encoded_job_id)?;
        connection.write_all(&[response])?;
        connection.flush()?;
        let mut payload = Vec::new();
        connection.read_to_end(&mut payload)?;
        Ok(ReducerCapture {
            job_id: i64::from_ne_bytes(encoded_job_id),
            payload,
        })
    });
    (port, capture)
}

fn split_reducer_frames(mut payload: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    while !payload.is_empty() {
        assert!(
            size_of::<usize>() <= payload.len(),
            "truncated native reducer frame size"
        );
        let (encoded_size, remainder) = payload.split_at(size_of::<usize>());
        let frame_size = usize::from_ne_bytes(
            encoded_size
                .try_into()
                .expect("native reducer frame size has exact width"),
        );
        assert!(
            frame_size <= remainder.len(),
            "truncated reducer MessagePack frame"
        );
        let (frame, remainder) = remainder.split_at(frame_size);
        frames.push(frame);
        payload = remainder;
    }
    frames
}

fn decode_reducer_frame(frame: &[u8]) -> ReducerRecordGroup {
    rmp_serde::from_slice(frame).expect("decode C++ reducer MessagePack record group")
}

fn write_zstd(path: &Path, source: &[u8]) {
    let encoded = zstd::stream::encode_all(source, 3).expect("encode test KV-IR stream");
    fs::write(path, encoded).expect("write test KV-IR stream");
}

#[test]
fn direct_kv_ir_search_writes_exact_cpp_jsonl() {
    let temporary = TestDirectory::new("direct-kv-ir");
    let stream = temporary.path().join("events.clp.zst");
    write_zstd(&stream, &decode_hex_fixture(KV_IR_FOUR_BYTE_HEX));

    let result = clp_s(&["s".as_ref(), stream.as_os_str(), "*: *".as_ref()]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(KV_IR_CANONICAL_JSONL, result.stdout);
    assert_eq!(b"", result.stderr.as_slice());

    let ignore_case = clp_s(&[
        "s".as_ref(),
        stream.as_os_str(),
        "message:TASK*".as_ref(),
        "--ignore-case".as_ref(),
    ]);
    assert!(
        ignore_case.status.success(),
        "{}",
        String::from_utf8_lossy(&ignore_case.stderr)
    );
    assert_eq!(KV_IR_CANONICAL_JSONL, ignore_case.stdout);
}

#[test]
fn direct_kv_ir_ignores_timestamp_bounds_with_cpp_warning() {
    let temporary = TestDirectory::new("direct-kv-ir-timestamps");
    let stream = temporary.path().join("events.clp.zst");
    write_zstd(&stream, &decode_hex_fixture(KV_IR_FOUR_BYTE_HEX));

    let result = clp_s(&[
        "s".as_ref(),
        stream.as_os_str(),
        "*: *".as_ref(),
        "--tge".as_ref(),
        "9999999999999".as_ref(),
        "--tle".as_ref(),
        "-9999999999999".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(KV_IR_CANONICAL_JSONL, result.stdout);
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("Timestamp filters are currently not supported")
    );
}

#[test]
fn direct_kv_ir_preserves_cpp_tolerated_and_fatal_truncation_boundaries() {
    let temporary = TestDirectory::new("direct-kv-ir-truncation");
    let nested = decode_hex_fixture(KV_IR_NESTED_HEX);
    let tolerated_stream = temporary.path().join("tolerated.clp.zst");
    write_zstd(&tolerated_stream, &nested[..102]);

    let tolerated = clp_s(&["s".as_ref(), tolerated_stream.as_os_str(), "*: *".as_ref()]);
    assert!(
        tolerated.status.success(),
        "{}",
        String::from_utf8_lossy(&tolerated.stderr)
    );
    assert_eq!(KV_IR_NESTED_FIRST_JSONL, tolerated.stdout);
    assert!(String::from_utf8_lossy(&tolerated.stderr).contains("is truncated"));

    let canonical = decode_hex_fixture(KV_IR_FOUR_BYTE_HEX);
    let fatal_stream = temporary.path().join("fatal.clp.zst");
    write_zstd(&fatal_stream, &canonical[..canonical.len() - 1]);
    let fatal = clp_s(&["s".as_ref(), fatal_stream.as_os_str(), "*: *".as_ref()]);
    assert!(!fatal.status.success());
    assert_eq!(KV_IR_CANONICAL_JSONL, fatal.stdout);
    assert!(String::from_utf8_lossy(&fatal.stderr).contains("truncated"));
    assert!(!String::from_utf8_lossy(&fatal.stderr).contains("Falling back"));
}

#[test]
fn kv_ir_candidate_deserializer_failure_falls_back_to_archive_search() {
    let temporary = TestDirectory::new("direct-kv-ir-fallback");
    let extension_candidate = temporary.path().join("archive.clp.zst");
    fs::copy(fixture(MINIMAL_ARCHIVE), &extension_candidate).expect("copy extension candidate");

    let result = clp_s(&[
        "s".as_ref(),
        extension_candidate.as_os_str(),
        "*: *".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(fixture(MINIMAL_JSONL)).expect("read minimal JSONL oracle"),
        result.stdout
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("Falling back"));

    let parent = temporary.path().join("parent.clp.zst");
    fs::create_dir(&parent).expect("create candidate parent");
    let child = parent.join("archive.bin");
    fs::copy(fixture(MINIMAL_ARCHIVE), &child).expect("copy parent candidate");
    let child_result = clp_s(&["s".as_ref(), child.as_os_str(), "*: *".as_ref()]);
    assert!(
        child_result.status.success(),
        "{}",
        String::from_utf8_lossy(&child_result.stderr)
    );
    assert_eq!(
        fs::read(fixture(MINIMAL_JSONL)).expect("read minimal JSONL oracle"),
        child_result.stdout
    );

    let root = temporary.path().join("archive-root");
    fs::create_dir(&root).expect("create archive root");
    fs::copy(fixture(MINIMAL_ARCHIVE), root.join("selected.clp.zst"))
        .expect("copy archive-id candidate");
    let selected = clp_s(&[
        "s".as_ref(),
        root.as_os_str(),
        "*: *".as_ref(),
        "--archive-id".as_ref(),
        "selected.clp.zst".as_ref(),
    ]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert_eq!(
        fs::read(fixture(MINIMAL_JSONL)).expect("read minimal JSONL oracle"),
        selected.stdout
    );
}

#[test]
fn unsupported_direct_kv_ir_features_warn_then_search_the_archive() {
    let temporary = TestDirectory::new("direct-kv-ir-unsupported");
    let archive = temporary.path().join("archive.clp.zst");
    fs::copy(fixture(MINIMAL_ARCHIVE), &archive).expect("copy candidate archive");

    let projection = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "--projection".as_ref(),
        "ts".as_ref(),
    ]);
    assert!(
        projection.status.success(),
        "{}",
        String::from_utf8_lossy(&projection.stderr)
    );
    assert_eq!(b"{\"ts\":1700000000123}\n", projection.stdout.as_slice());
    assert!(String::from_utf8_lossy(&projection.stderr).contains("unsupported features"));

    let count = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "--count".as_ref(),
    ]);
    assert!(
        count.status.success(),
        "{}",
        String::from_utf8_lossy(&count.stderr)
    );
    assert_eq!(
        b"{\"archive_id\":\"archive.clp.zst\",\"count\":1}\n",
        count.stdout.as_slice()
    );
    assert!(String::from_utf8_lossy(&count.stderr).contains("unsupported features"));
}

#[test]
fn direct_kv_ir_raw_reader_failure_is_fatal_without_archive_fallback() {
    let temporary = TestDirectory::new("direct-kv-ir-open-failure");
    let missing = temporary.path().join("missing.clp.zst");
    let result = clp_s(&["s".as_ref(), missing.as_os_str(), "*: *".as_ref()]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("Falling back"));

    // Linux permits opening a directory as a `File`, so this reaches a physical read failure
    // after the raw reader has been created. It must not be confused with an invalid zstd stream.
    let candidate_container = temporary.path().join("container.clp.zst");
    let directory_member = candidate_container.join("directory-member");
    fs::create_dir_all(&directory_member).expect("create candidate directory member");
    let directory = clp_s(&[
        "s".as_ref(),
        candidate_container.as_os_str(),
        "*: *".as_ref(),
    ]);
    assert!(!directory.status.success());
    assert_eq!(b"", directory.stdout.as_slice());
    assert!(!String::from_utf8_lossy(&directory.stderr).contains("Falling back"));
}

#[test]
fn positional_search_writes_exact_cpp_records_in_physical_order() {
    let archive = fixture(LOG_ORDER_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "a:* OR b:* OR c:*".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(
        fs::read(fixture(LOG_ORDER_UNORDERED_JSONL)).expect("read C++ physical-order oracle"),
        result.stdout
    );
}

#[test]
fn network_output_stream_matches_cpp_and_keeps_stdout_clean() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback search listener");
    let port = listener
        .local_addr()
        .expect("read loopback listener address")
        .port()
        .to_string();
    let archive = fixture(MINIMAL_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "network".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let (mut connection, _) = listener.accept().expect("accept Rust clp-s connection");
    let mut received = Vec::new();
    connection
        .read_to_end(&mut received)
        .expect("read complete MessagePack stream");
    assert_eq!(decode_hex_fixture(NETWORK_MSGPACK_HEX), received);
}

#[test]
fn network_output_opens_one_connection_per_archive_like_cpp() {
    let temporary = TestDirectory::new("network-multiple-archives");
    fs::copy(fixture(MINIMAL_ARCHIVE), temporary.path().join("a.sfa")).expect("copy first SFA");
    fs::copy(fixture(MINIMAL_ARCHIVE), temporary.path().join("b.sfa")).expect("copy second SFA");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback search listener");
    let port = listener
        .local_addr()
        .expect("read loopback listener address")
        .port()
        .to_string();
    let result = clp_s(&[
        "s".as_ref(),
        temporary.path().as_os_str(),
        "*: *".as_ref(),
        "network".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let expected = decode_hex_fixture(NETWORK_MSGPACK_HEX);
    for _ in 0..2 {
        let (mut connection, _) = listener.accept().expect("accept per-archive connection");
        let mut received = Vec::new();
        connection
            .read_to_end(&mut received)
            .expect("read complete per-archive stream");
        assert_eq!(expected, received);
    }
}

#[test]
fn stdout_preserves_every_committed_cpp_value_representation() {
    let cases = [
        (STRINGS_ARCHIVE, "v:*", STRINGS_JSONL),
        (
            RETAINED_FLOATS_ARCHIVE,
            "formatted:*",
            RETAINED_FLOATS_JSONL,
        ),
        (TIMESTAMPS_ARCHIVE, "kind:*", TIMESTAMPS_JSONL),
        (ARRAYS_ARCHIVE, "kind:*", ARRAYS_JSONL),
    ];
    for (archive, query, expected) in cases {
        let archive = fixture(archive);
        let result = clp_s(&["s".as_ref(), archive.as_os_str(), query.as_ref()]);
        assert!(
            result.status.success(),
            "{}: {}",
            archive.display(),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::read(fixture(expected)).expect("read committed C++ JSONL oracle"),
            result.stdout,
            "{}",
            archive.display()
        );
    }
}

#[test]
fn query_option_ignore_case_and_projection_match_cpp_key_order() {
    let archive = fixture(MINIMAL_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "-q".as_ref(),
        "level:info".as_ref(),
        "-i".as_ref(),
        "stdout".as_ref(),
        "--projection".as_ref(),
        "message".as_ref(),
        "ts".as_ref(),
        "missing".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        b"{\"ts\":1700000000123,\"message\":\"oracle fixture\"}\n",
        result.stdout.as_slice()
    );

    let duplicate = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "--projection".as_ref(),
        "ts".as_ref(),
        "ts".as_ref(),
    ]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("duplicates"));
    assert_eq!(b"", duplicate.stdout.as_slice());
}

#[test]
fn file_handler_writes_exact_cpp_messagepack_in_physical_order() {
    let temporary = TestDirectory::new("file-oracles");
    for (archive_name, oracle_name) in [
        (MINIMAL_ARCHIVE, MINIMAL_MSGPACK_HEX),
        (LOG_ORDER_ARCHIVE, LOG_ORDER_MSGPACK_HEX),
        (STRINGS_ARCHIVE, STRINGS_MSGPACK_HEX),
    ] {
        let archive = fixture(archive_name);
        let output_path = temporary.path().join(format!("{archive_name}.msgpack"));
        fs::write(&output_path, b"stale result bytes").expect("write stale output");
        let result = clp_s(&[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "file".as_ref(),
            "--path".as_ref(),
            output_path.as_os_str(),
        ]);
        assert!(
            result.status.success(),
            "{archive_name}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(b"", result.stdout.as_slice(), "{archive_name}");
        assert_eq!(b"", result.stderr.as_slice(), "{archive_name}");
        assert_eq!(
            decode_hex_fixture(oracle_name),
            fs::read(output_path).expect("read MessagePack output"),
            "{archive_name}"
        );
    }
}

#[test]
fn file_handler_projection_matches_exact_cpp_tuple() {
    let temporary = TestDirectory::new("file-projection");
    let archive = fixture(MINIMAL_ARCHIVE);
    let output_path = temporary.path().join("projection.msgpack");
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        output_path.as_os_str(),
        "--projection".as_ref(),
        "message".as_ref(),
        "ts".as_ref(),
        "missing".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(
        decode_hex_fixture(PROJECTION_MSGPACK_HEX),
        fs::read(output_path).expect("read projected MessagePack output")
    );
}

#[cfg(unix)]
#[test]
fn file_handler_preserves_non_utf8_archive_identifier_bytes() {
    let temporary = TestDirectory::new("file-opaque-id");
    let archive_name = OsString::from_vec(b"opaque-\xff".to_vec());
    let archive = temporary.path().join(&archive_name);
    fs::copy(fixture(MINIMAL_ARCHIVE), &archive).expect("copy archive with opaque filename");
    let output_path = temporary.path().join("opaque.msgpack");
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let mut expected = decode_hex_fixture(MINIMAL_MSGPACK_HEX);
    let old_id = MINIMAL_ARCHIVE.as_bytes();
    let position = expected
        .windows(old_id.len())
        .position(|window| window == old_id)
        .expect("oracle contains archive ID");
    let new_id = archive_name.as_encoded_bytes();
    assert!(new_id.len() < 32);
    expected.splice(
        position - 1..position + old_id.len(),
        std::iter::once(0xa0 | u8::try_from(new_id.len()).expect("short ID"))
            .chain(new_id.iter().copied()),
    );
    assert_eq!(
        expected,
        fs::read(output_path).expect("read opaque-ID MessagePack output")
    );
}

#[test]
fn file_handler_recreates_the_destination_for_each_archive() {
    let temporary = TestDirectory::new("file-multiple");
    let archives = temporary.path().join("archives");
    fs::create_dir(&archives).expect("create archive container");
    let first = archives.join("a-minimal");
    let second = archives.join("b-log-order");
    fs::copy(fixture(MINIMAL_ARCHIVE), &first).expect("copy first archive");
    fs::copy(fixture(LOG_ORDER_ARCHIVE), &second).expect("copy second archive");

    let single_path = temporary.path().join("single.msgpack");
    let single = clp_s(&[
        "s".as_ref(),
        second.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        single_path.as_os_str(),
    ]);
    assert!(
        single.status.success(),
        "{}",
        String::from_utf8_lossy(&single.stderr)
    );

    let multiple_path = temporary.path().join("multiple.msgpack");
    let multiple = clp_s(&[
        "s".as_ref(),
        archives.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        multiple_path.as_os_str(),
    ]);
    assert!(
        multiple.status.success(),
        "{}",
        String::from_utf8_lossy(&multiple.stderr)
    );
    assert_eq!(b"", multiple.stdout.as_slice());
    assert_eq!(
        fs::read(single_path).expect("read single-archive output"),
        fs::read(multiple_path).expect("read multi-archive output")
    );
}

#[test]
fn file_handler_preserves_early_pruned_paths_and_truncates_runtime_zeroes() {
    let temporary = TestDirectory::new("file-zero");
    let archive = fixture(MINIMAL_ARCHIVE);
    let output_path = temporary.path().join("results.msgpack");
    let sentinel: &[u8] = b"existing output";

    for arguments in [
        vec!["definitely_missing:*".as_ref()],
        vec!["$_filename:NOPE".as_ref()],
        vec!["*: *".as_ref(), "--tge".as_ref(), "1700000000124".as_ref()],
    ] {
        fs::write(&output_path, sentinel).expect("write sentinel");
        let mut command = vec![
            "s".as_ref(),
            archive.as_os_str(),
            arguments[0],
            "file".as_ref(),
            "--path".as_ref(),
            output_path.as_os_str(),
        ];
        command.extend_from_slice(&arguments[1..]);
        let result = clp_s(&command);
        assert!(
            result.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(sentinel, fs::read(&output_path).expect("read sentinel"));
    }

    fs::remove_file(&output_path).expect("remove sentinel");
    let early_pruned = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "definitely_missing:*".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(early_pruned.status.success());
    assert!(!output_path.exists());

    fs::write(&output_path, sentinel).expect("rewrite sentinel");
    let runtime_zero = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:NOPE".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(
        runtime_zero.status.success(),
        "{}",
        String::from_utf8_lossy(&runtime_zero.stderr)
    );
    assert_eq!(0, fs::metadata(&output_path).expect("stat output").len());

    fs::remove_file(&output_path).expect("remove empty output");
    let runtime_zero = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:NOPE".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(runtime_zero.status.success());
    assert_eq!(
        0,
        fs::metadata(&output_path).expect("stat new output").len()
    );
}

#[test]
fn file_handler_validates_every_argument_before_touching_the_path() {
    let temporary = TestDirectory::new("file-validation");
    let archive = fixture(MINIMAL_ARCHIVE);
    let output_path = temporary.path().join("results.msgpack");
    let sentinel: &[u8] = b"existing output";

    let invalid: &[&[&OsStr]] = &[
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "file".as_ref(),
            "--path".as_ref(),
            output_path.as_os_str(),
            "--count".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "(".as_ref(),
            "file".as_ref(),
            "--path".as_ref(),
            output_path.as_os_str(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "stdout".as_ref(),
            "--path".as_ref(),
            output_path.as_os_str(),
        ],
    ];
    for arguments in invalid {
        fs::write(&output_path, sentinel).expect("write sentinel");
        let result = clp_s(arguments);
        assert!(!result.status.success(), "{arguments:?}");
        assert_eq!(b"", result.stdout.as_slice(), "{arguments:?}");
        assert_eq!(
            sentinel,
            fs::read(&output_path).expect("read unchanged sentinel"),
            "{arguments:?}"
        );
    }

    let without_path = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
    ]);
    assert!(!without_path.status.success());
    assert_eq!(b"", without_path.stdout.as_slice());
    assert!(String::from_utf8_lossy(&without_path.stderr).contains("requires --path"));

    let empty_path = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        "".as_ref(),
    ]);
    assert!(!empty_path.status.success());
    assert_eq!(b"", empty_path.stdout.as_slice());
    assert_ne!(b"", empty_path.stderr.as_slice());
}

#[test]
fn file_handler_reports_create_failures_without_stdout() {
    let temporary = TestDirectory::new("file-failure");
    let archive = fixture(MINIMAL_ARCHIVE);
    let output_path = temporary.path().join("existing-directory");
    fs::create_dir(&output_path).expect("create invalid output directory");
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "file".as_ref(),
        "--path".as_ref(),
        output_path.as_os_str(),
    ]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(output_path.is_dir());
}

#[test]
fn archive_containers_are_sorted_and_archive_id_selects_one() {
    let temporary = TestDirectory::new("archives");
    let archives = temporary.path().join("archives");
    fs::create_dir(&archives).expect("create archive container");
    fs::copy(fixture(MINIMAL_ARCHIVE), archives.join("a-minimal")).expect("copy first archive");
    fs::copy(fixture(LOG_ORDER_ARCHIVE), archives.join("b-log-order"))
        .expect("copy second archive");

    let result = clp_s(&[
        "s".as_ref(),
        archives.as_os_str(),
        "level:* OR a:*".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut expected = fs::read(fixture(MINIMAL_JSONL)).expect("read minimal oracle");
    expected.extend_from_slice(b"{\"a\":10}\n{\"a\":20}\n{\"a\":30}\n");
    assert_eq!(expected, result.stdout);

    let selected = clp_s(&[
        "s".as_ref(),
        archives.as_os_str(),
        "a:*".as_ref(),
        "--archive-id".as_ref(),
        "b-log-order".as_ref(),
    ]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert_eq!(
        b"{\"a\":10}\n{\"a\":20}\n{\"a\":30}\n",
        selected.stdout.as_slice()
    );
}

#[test]
fn count_matches_cpp_per_archive_output_order_and_omits_zeroes() {
    let temporary = TestDirectory::new("count");
    let archives = temporary.path().join("archives");
    fs::create_dir(&archives).expect("create count archive container");
    fs::copy(fixture(MINIMAL_ARCHIVE), archives.join("a-minimal")).expect("copy minimal archive");
    fs::copy(fixture(LOG_ORDER_ARCHIVE), archives.join("b-log-order"))
        .expect("copy log-order archive");

    let result = clp_s(&[
        "s".as_ref(),
        archives.as_os_str(),
        "*: *".as_ref(),
        "--count".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(
        concat!(
            "{\"archive_id\":\"a-minimal\",\"count\":1}\n",
            "{\"archive_id\":\"b-log-order\",\"count\":6}\n",
        )
        .as_bytes(),
        result.stdout
    );

    for query in ["definitely_missing:*", "level:NOPE OR a:999"] {
        let zero = clp_s(&[
            "s".as_ref(),
            archives.as_os_str(),
            query.as_ref(),
            "--count".as_ref(),
        ]);
        assert!(
            zero.status.success(),
            "{query}: {}",
            String::from_utf8_lossy(&zero.stderr)
        );
        assert_eq!(b"", zero.stdout.as_slice(), "{query}");
    }

    let selected = clp_s(&[
        "s".as_ref(),
        archives.as_os_str(),
        "*: *".as_ref(),
        "--count".as_ref(),
        "--archive-id".as_ref(),
        "b-log-order".as_ref(),
    ]);
    assert!(selected.status.success());
    assert_eq!(
        b"{\"archive_id\":\"b-log-order\",\"count\":6}\n",
        selected.stdout.as_slice()
    );
}

#[test]
fn count_escapes_the_archive_filename_as_json() {
    let temporary = TestDirectory::new("count-escape");
    let archives = temporary.path().join("archives");
    fs::create_dir(&archives).expect("create count archive container");
    fs::copy(fixture(MINIMAL_ARCHIVE), archives.join("quoted\"archive"))
        .expect("copy quoted archive");

    let result = clp_s(&[
        "s".as_ref(),
        archives.as_os_str(),
        "level:*".as_ref(),
        "--count".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        b"{\"archive_id\":\"quoted\\\"archive\",\"count\":1}\n",
        result.stdout.as_slice()
    );
}

#[test]
fn count_by_time_matches_cpp_negative_bucket_order_exactly() {
    let archive = fixture(AGGREGATIONS_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "--count-by-time".as_ref(),
        "1000".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(
        concat!(
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"count\":2,\"timestamp\":\
             -1700000001000}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"count\":3,\"timestamp\":\
             -1700000000000}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"count\":2,\"timestamp\":\
             -1699999999000}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"count\":3,\"timestamp\":\
             -1699999998000}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"count\":1,\"timestamp\":\
             -1699999997000}\n",
        )
        .as_bytes(),
        result.stdout
    );
}

#[test]
fn reducer_count_matches_the_exact_cpp_handshake_framing_and_msgpack_schema() {
    let (port, capture) = spawn_reducer_capture(b'y');
    let port = port.to_string();
    let archive = fixture(MINIMAL_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:*".as_ref(),
        "reducer".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
        "--job-id".as_ref(),
        "72623859790382856".as_ref(),
        "--count".as_ref(),
    ]);
    let capture = capture
        .join()
        .expect("join loopback reducer")
        .expect("capture reducer stream");

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(0x0102_0304_0506_0708_i64, capture.job_id);
    let mut expected = Vec::new();
    expected.extend_from_slice(&REDUCER_COUNT_ONE_FRAME.len().to_ne_bytes());
    expected.extend_from_slice(REDUCER_COUNT_ONE_FRAME);
    assert_eq!(expected, capture.payload);
    assert_eq!(
        ReducerRecordGroup {
            group_tags: Vec::new(),
            records: vec![ReducerRecord { count: 1 }],
        },
        decode_reducer_frame(REDUCER_COUNT_ONE_FRAME)
    );
}

#[test]
fn reducer_uses_one_connection_for_multiple_archives_and_keeps_archive_frames() {
    let temporary = TestDirectory::new("reducer-multiple-archives");
    fs::copy(fixture(MINIMAL_ARCHIVE), temporary.path().join("a.sfa")).expect("copy first SFA");
    fs::copy(fixture(MINIMAL_ARCHIVE), temporary.path().join("b.sfa")).expect("copy second SFA");
    let (port, capture) = spawn_reducer_capture(b'y');
    let port = port.to_string();
    let result = clp_s(&[
        "s".as_ref(),
        temporary.path().as_os_str(),
        "*: *".as_ref(),
        "reducer".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
        "--job-id".as_ref(),
        "19".as_ref(),
        "--count".as_ref(),
    ]);
    let capture = capture
        .join()
        .expect("join loopback reducer")
        .expect("capture reducer stream");

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(19, capture.job_id);
    let frames = split_reducer_frames(&capture.payload);
    assert_eq!(2, frames.len());
    for frame in frames {
        assert_eq!(REDUCER_COUNT_ONE_FRAME, frame);
        assert_eq!(
            ReducerRecordGroup {
                group_tags: Vec::new(),
                records: vec![ReducerRecord { count: 1 }],
            },
            decode_reducer_frame(frame)
        );
    }
}

#[test]
fn reducer_count_by_time_frames_follow_cpp_bucket_order() {
    let (port, capture) = spawn_reducer_capture(b'y');
    let port = port.to_string();
    let archive = fixture(AGGREGATIONS_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "reducer".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
        "--job-id".as_ref(),
        "23".as_ref(),
        "--count-by-time".as_ref(),
        "1000".as_ref(),
    ]);
    let capture = capture
        .join()
        .expect("join loopback reducer")
        .expect("capture reducer stream");

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(23, capture.job_id);
    let decoded: Vec<ReducerRecordGroup> = split_reducer_frames(&capture.payload)
        .into_iter()
        .map(decode_reducer_frame)
        .collect();
    assert_eq!(
        vec![
            ReducerRecordGroup {
                group_tags: vec!["-1700000001000".to_string()],
                records: vec![ReducerRecord { count: 2 }],
            },
            ReducerRecordGroup {
                group_tags: vec!["-1700000000000".to_string()],
                records: vec![ReducerRecord { count: 3 }],
            },
            ReducerRecordGroup {
                group_tags: vec!["-1699999999000".to_string()],
                records: vec![ReducerRecord { count: 2 }],
            },
            ReducerRecordGroup {
                group_tags: vec!["-1699999998000".to_string()],
                records: vec![ReducerRecord { count: 3 }],
            },
            ReducerRecordGroup {
                group_tags: vec!["-1699999997000".to_string()],
                records: vec![ReducerRecord { count: 1 }],
            },
        ],
        decoded
    );
}

#[test]
fn reducer_zero_matches_send_no_result_frames() {
    let (port, capture) = spawn_reducer_capture(b'y');
    let port = port.to_string();
    let archive = fixture(MINIMAL_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "definitely_missing:*".as_ref(),
        "reducer".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
        "--job-id".as_ref(),
        "29".as_ref(),
        "--count".as_ref(),
    ]);
    let capture = capture
        .join()
        .expect("join loopback reducer")
        .expect("capture reducer stream");

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(29, capture.job_id);
    assert_eq!(b"", capture.payload.as_slice());
}

#[test]
fn reducer_handshake_rejection_fails_without_stdout_or_result_frames() {
    let (port, capture) = spawn_reducer_capture(b'n');
    let port = port.to_string();
    let archive = fixture(MINIMAL_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "reducer".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        port.as_ref(),
        "--job-id".as_ref(),
        "31".as_ref(),
        "--count".as_ref(),
    ]);
    let capture = capture
        .join()
        .expect("join loopback reducer")
        .expect("capture rejected reducer stream");

    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(String::from_utf8_lossy(&result.stderr).contains("rejected"));
    assert_eq!(31, capture.job_id);
    assert_eq!(b"", capture.payload.as_slice());
}

#[test]
#[allow(clippy::too_many_lines)]
fn reducer_options_and_supported_aggregations_are_validated_before_connecting() {
    let archive = fixture(MINIMAL_ARCHIVE);
    let cases: Vec<(Vec<&OsStr>, &str)> = vec![
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--port".as_ref(),
                "1".as_ref(),
                "--job-id".as_ref(),
                "1".as_ref(),
                "--count".as_ref(),
            ],
            "host must be specified",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "".as_ref(),
                "--port".as_ref(),
                "1".as_ref(),
                "--job-id".as_ref(),
                "1".as_ref(),
                "--count".as_ref(),
            ],
            "host cannot be an empty string",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "127.0.0.1".as_ref(),
                "--job-id".as_ref(),
                "1".as_ref(),
                "--count".as_ref(),
            ],
            "port must be specified",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "127.0.0.1".as_ref(),
                "--port".as_ref(),
                "0".as_ref(),
                "--job-id".as_ref(),
                "1".as_ref(),
                "--count".as_ref(),
            ],
            "port must be greater than zero",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "127.0.0.1".as_ref(),
                "--port".as_ref(),
                "1".as_ref(),
                "--count".as_ref(),
            ],
            "job-id must be specified",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "127.0.0.1".as_ref(),
                "--port".as_ref(),
                "1".as_ref(),
                "--job-id".as_ref(),
                "-1".as_ref(),
                "--count".as_ref(),
            ],
            "job-id cannot be negative",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "127.0.0.1".as_ref(),
                "--port".as_ref(),
                "1".as_ref(),
                "--job-id".as_ref(),
                "1".as_ref(),
            ],
            "only supports count and count-by-time",
        ),
        (
            vec![
                "s".as_ref(),
                archive.as_os_str(),
                "*: *".as_ref(),
                "reducer".as_ref(),
                "--host".as_ref(),
                "127.0.0.1".as_ref(),
                "--port".as_ref(),
                "1".as_ref(),
                "--job-id".as_ref(),
                "1".as_ref(),
                "--min".as_ref(),
                "value".as_ref(),
            ],
            "only supports count and count-by-time",
        ),
    ];

    for (arguments, expected_error) in cases {
        let result = clp_s(&arguments);
        assert!(!result.status.success(), "{arguments:?}");
        assert_eq!(b"", result.stdout.as_slice(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected_error),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn min_and_max_match_cpp_numeric_types_and_mixed_precision() {
    let archive = fixture(AGGREGATIONS_ARCHIVE);
    let minimum = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "--min".as_ref(),
        "target".as_ref(),
    ]);
    assert!(
        minimum.status.success(),
        "{}",
        String::from_utf8_lossy(&minimum.stderr)
    );
    assert_eq!(
        b"{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"target\",\"min\":2.5}\n",
        minimum.stdout.as_slice()
    );

    let maximum = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "--max".as_ref(),
        "mixed".as_ref(),
    ]);
    assert!(
        maximum.status.success(),
        "{}",
        String::from_utf8_lossy(&maximum.stderr)
    );
    assert_eq!(
        concat!(
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",",
            "\"field\":\"mixed\",\"max\":9007199254740993}\n",
        )
        .as_bytes(),
        maximum.stdout.as_slice()
    );
}

#[test]
fn unique_matches_cpp_variant_type_and_value_order_exactly() {
    let archive = fixture(AGGREGATIONS_ARCHIVE);
    let result = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "*: *".as_ref(),
        "--unique".as_ref(),
        "unique".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(
        concat!(
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\
             -1}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\
             2}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\
             -0.0}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":1.\
             5}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\"\
             a\"}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\"\
             z\"}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\
             false}\n",
            "{\"archive_id\":\"sfa-v0.5.0-aggregations-cpp.bin\",\"field\":\"unique\",\"value\":\
             true}\n",
        )
        .as_bytes(),
        result.stdout
    );
}

#[test]
fn invalid_aggregation_configuration_fails_before_stdout() {
    let archive = fixture(AGGREGATIONS_ARCHIVE);
    let invalid: &[&[&OsStr]] = &[
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "--count-by-time".as_ref(),
            "0".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "--count-by-time".as_ref(),
            "-1000".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "--min".as_ref(),
            "wild.*".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "--unique".as_ref(),
            "".as_ref(),
        ],
    ];
    for arguments in invalid {
        let result = clp_s(arguments);
        assert!(!result.status.success(), "{arguments:?}");
        assert_eq!(b"", result.stdout.as_slice(), "{arguments:?}");
        assert!(!result.stderr.is_empty(), "{arguments:?}");
    }
}

#[test]
fn authoritative_timestamp_bounds_are_inclusive_and_compose_with_count() {
    let timestamp_archive = fixture(TIMESTAMPS_ARCHIVE);
    let exact = clp_s(&[
        "s".as_ref(),
        timestamp_archive.as_os_str(),
        "kind:*".as_ref(),
        "--tge".as_ref(),
        "1700000000123".as_ref(),
        "--tle".as_ref(),
        "1700000000123".as_ref(),
    ]);
    assert!(
        exact.status.success(),
        "{}",
        String::from_utf8_lossy(&exact.stderr)
    );
    assert_eq!(
        b"{\"ts\":1700000000123,\"kind\":2}\n",
        exact.stdout.as_slice()
    );

    let minimal_archive = fixture(MINIMAL_ARCHIVE);
    let counted = clp_s(&[
        "s".as_ref(),
        minimal_archive.as_os_str(),
        "level:*".as_ref(),
        "--tge".as_ref(),
        "1700000000123".as_ref(),
        "--tle".as_ref(),
        "1700000000123".as_ref(),
        "--count".as_ref(),
    ]);
    assert!(
        counted.status.success(),
        "{}",
        String::from_utf8_lossy(&counted.stderr)
    );
    assert_eq!(
        b"{\"archive_id\":\"sfa-v0.5.0-minimal-cpp.bin\",\"count\":1}\n",
        counted.stdout.as_slice()
    );

    let outside = clp_s(&[
        "s".as_ref(),
        minimal_archive.as_os_str(),
        "level:*".as_ref(),
        "--tge".as_ref(),
        "1700000000124".as_ref(),
        "--count".as_ref(),
    ]);
    assert!(outside.status.success());
    assert_eq!(b"", outside.stdout.as_slice());

    let reversed = clp_s(&[
        "s".as_ref(),
        minimal_archive.as_os_str(),
        "level:*".as_ref(),
        "--tge".as_ref(),
        "1".as_ref(),
        "--tle".as_ref(),
        "0".as_ref(),
    ]);
    assert!(!reversed.status.success());
    assert_eq!(b"", reversed.stdout.as_slice());
    assert!(String::from_utf8_lossy(&reversed.stderr).contains("after"));
}

#[test]
fn unsupported_cpp_surfaces_fail_explicitly_without_output() {
    let archive = fixture(MINIMAL_ARCHIVE);
    let cases: &[&[&OsStr]] = &[&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "--enable-telemetry".as_ref(),
    ]];
    for arguments in cases {
        let result = clp_s(arguments);
        assert!(!result.status.success(), "{arguments:?}");
        assert_eq!(b"", result.stdout.as_slice(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("not implemented yet"),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let local_s3_auth = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "--auth".as_ref(),
        "s3".as_ref(),
    ]);
    assert!(
        local_s3_auth.status.success(),
        "local input must ignore network authentication: {}",
        String::from_utf8_lossy(&local_s3_auth.stderr)
    );

    let missing_network_options = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "network".as_ref(),
    ]);
    assert!(!missing_network_options.status.success());
    assert_eq!(b"", missing_network_options.stdout.as_slice());
    assert!(String::from_utf8_lossy(&missing_network_options.stderr).contains("host must"));

    let network_aggregation = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "--count".as_ref(),
        "network".as_ref(),
        "--host".as_ref(),
        "127.0.0.1".as_ref(),
        "--port".as_ref(),
        "1".as_ref(),
    ]);
    assert!(!network_aggregation.status.success());
    assert_eq!(b"", network_aggregation.stdout.as_slice());
    assert!(
        String::from_utf8_lossy(&network_aggregation.stderr)
            .contains("network output handler does not support aggregations")
    );

    let conflicting = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "--count".as_ref(),
        "--unique".as_ref(),
        "level".as_ref(),
    ]);
    assert!(!conflicting.status.success());
    assert_eq!(b"", conflicting.stdout.as_slice());
    assert!(String::from_utf8_lossy(&conflicting.stderr).contains("mutually exclusive"));
}

#[test]
fn results_cache_validates_cpp_options_before_output_or_service_io() {
    let archive = fixture(MINIMAL_ARCHIVE);
    let invalid: &[&[&OsStr]] = &[
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "results-cache".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "results-cache".as_ref(),
            "--uri".as_ref(),
            "mongodb://localhost/database".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "results-cache".as_ref(),
            "--uri".as_ref(),
            "".as_ref(),
            "--collection".as_ref(),
            "results".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "results-cache".as_ref(),
            "--uri".as_ref(),
            "mongodb://localhost/database".as_ref(),
            "--collection".as_ref(),
            "".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "results-cache".as_ref(),
            "--uri".as_ref(),
            "mongodb://localhost/database".as_ref(),
            "--collection".as_ref(),
            "results".as_ref(),
            "--batch-size".as_ref(),
            "0".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "results-cache".as_ref(),
            "--uri".as_ref(),
            "mongodb://localhost/database".as_ref(),
            "--collection".as_ref(),
            "results".as_ref(),
            "--max-num-results".as_ref(),
            "0".as_ref(),
        ],
        &[
            "s".as_ref(),
            archive.as_os_str(),
            "*: *".as_ref(),
            "--uri".as_ref(),
            "mongodb://localhost/database".as_ref(),
        ],
    ];
    for arguments in invalid {
        let result = clp_s(arguments);
        assert!(!result.status.success(), "{arguments:?}");
        assert_eq!(b"", result.stdout.as_slice(), "{arguments:?}");
        assert!(!result.stderr.is_empty(), "{arguments:?}");
    }
}

#[test]
fn results_cache_connection_setup_is_lazy_and_never_writes_stdout() {
    let archive = fixture(MINIMAL_ARCHIVE);
    let schema_pruned = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "definitely_missing: value".as_ref(),
        "results-cache".as_ref(),
        "--uri".as_ref(),
        "invalid://not-contacted/database".as_ref(),
        "--collection".as_ref(),
        "results".as_ref(),
    ]);
    assert!(
        schema_pruned.status.success(),
        "{}",
        String::from_utf8_lossy(&schema_pruned.stderr)
    );
    assert_eq!(b"", schema_pruned.stdout.as_slice());

    let matching = clp_s(&[
        "s".as_ref(),
        archive.as_os_str(),
        "level:INFO".as_ref(),
        "results-cache".as_ref(),
        "--uri".as_ref(),
        "invalid://rejected/database".as_ref(),
        "--collection".as_ref(),
        "results".as_ref(),
    ]);
    assert!(!matching.status.success());
    assert_eq!(b"", matching.stdout.as_slice());
    assert_ne!(b"", matching.stderr.as_slice());
}
