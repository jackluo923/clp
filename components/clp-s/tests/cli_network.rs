#![cfg(all(feature = "cli", feature = "network"))]

use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use clp_s::archive::MetadataLimits;
use clp_s::archive::RangeIndexValue;
use clp_s::archive::SingleFileArchiveReader;
use flate2::Compression;
use flate2::write::GzEncoder;

const JSON_INPUT: &[u8] = include_bytes!("fixtures/compression-cli-v1-input.json");
const CANONICAL_JSONL: &[u8] =
    b"{\"id\":0,\"kind\":\"a\"}\n{\"id\":1,\"kind\":\"b\"}\n{\"id\":2,\"kind\":\"c\"}\n";
const MINIMAL_SFA: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp.bin");
const MINIMAL_JSONL: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl");
const LOG_ORDER_SFA: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-log-order-cpp.bin");
const NO_LOG_ORDER_SFA: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-strings-cpp.bin");
const NO_LOG_ORDER_JSONL: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-strings-cpp-input.jsonl");
const KV_IR_FOUR_BYTE_HEX: &str = include_str!("fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
const KV_IR_CANONICAL_JSONL: &[u8] = concat!(
    "{\"auto_generated_kv_pairs\":{\"level\":\"info\",\"seq\":7},",
    "\"user_generated_kv_pairs\":{\"empty\":{},\"message\":\"task 42 done\",",
    "\"none\":null,\"ok\":true,\"ratio\":1.25}}\n"
)
.as_bytes();

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-rust-cli-network-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique CLI network test directory");
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

type Responder = dyn Fn(&str) -> Vec<u8> + Send + Sync;

struct MockServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    fn start(responder: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback HTTP server");
        listener
            .set_nonblocking(true)
            .expect("make loopback listener nonblocking");
        let address = listener.local_addr().expect("read loopback address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let responder: Arc<Responder> = Arc::new(responder);
        let thread = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        requests_for_thread
                            .lock()
                            .expect("lock loopback requests")
                            .push(request.clone());
                        let response = responder(&request);
                        stream
                            .write_all(&response)
                            .expect("write loopback HTTP response");
                    }
                    Err(error) if io::ErrorKind::WouldBlock == error.kind() => {
                        thread::yield_now();
                    }
                    Err(error) => panic!("loopback HTTP accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("lock loopback requests")
            .clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join loopback HTTP server");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set loopback request timeout");
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| b"\r\n\r\n" == window) {
        let read = stream.read(&mut buffer).expect("read loopback request");
        assert!(0 < read, "loopback request headers ended early");
        request.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8(request).expect("loopback request is ASCII")
}

fn content_length_response(status: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn chunked_response(chunks: &[&[u8]]) -> Vec<u8> {
    let mut response =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in chunks {
        write!(response, "{:x}\r\n", chunk.len()).expect("write chunk length");
        response.extend_from_slice(chunk);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"0\r\n\r\n");
    response
}

fn decode_hex(source: &str) -> Vec<u8> {
    let digits: Vec<u8> = source
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let (pairs, remainder) = digits.as_chunks::<2>();
    assert!(remainder.is_empty(), "hex fixture has even length");
    pairs
        .iter()
        .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
        .collect()
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("invalid hex fixture byte"),
    }
}

fn tar_gzip(path: &[u8], body: &[u8]) -> Vec<u8> {
    assert!(path.len() <= 100, "test USTAR member name fits header");
    let mut archive = Vec::new();
    let mut header = [0_u8; 512];
    header[..path.len()].copy_from_slice(path);
    write_ustar_octal(&mut header[100..108], 0o644);
    write_ustar_octal(&mut header[108..116], 0);
    write_ustar_octal(&mut header[116..124], 0);
    write_ustar_octal(
        &mut header[124..136],
        u64::try_from(body.len()).expect("test member length fits u64"),
    );
    write_ustar_octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    write_ustar_checksum(&mut header[148..156], checksum);
    archive.extend_from_slice(&header);
    archive.extend_from_slice(body);
    archive.resize(archive.len().next_multiple_of(512), 0);
    archive.resize(archive.len() + 1024, 0);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&archive).expect("gzip test USTAR bytes");
    encoder.finish().expect("finish gzip test USTAR bytes")
}

fn write_ustar_octal(field: &mut [u8], value: u64) {
    field.fill(b'0');
    let digits = format!("{value:o}");
    let digit_end = field.len() - 1;
    let digit_begin = digit_end - digits.len();
    field[digit_begin..digit_end].copy_from_slice(digits.as_bytes());
    field[digit_end] = 0;
}

fn write_ustar_checksum(field: &mut [u8], value: u64) {
    field.fill(b'0');
    let digits = format!("{value:06o}");
    field[..6].copy_from_slice(digits.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}

fn clp_s(arguments: &[&OsStr]) -> Output {
    clp_s_with_environment(arguments, &[])
}

fn clp_s_with_environment(arguments: &[&OsStr], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clp-s"));
    command.args(arguments);
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("run Rust clp-s binary")
}

fn clp_s_with_removed_environment(arguments: &[&OsStr], removed_environment: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clp-s"));
    command.args(arguments);
    for key in removed_environment {
        command.env_remove(key);
    }
    command.output().expect("run Rust clp-s binary")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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

fn single_source_metadata(archives_path: &Path) -> (std::ops::Range<u64>, String) {
    let archive_path = only_entry(archives_path);
    let mut archive = SingleFileArchiveReader::open(
        File::open(archive_path).expect("open generated network-input archive"),
    )
    .expect("open generated network-input SFA");
    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("read generated network-input metadata");
    let [range] = metadata
        .range_index()
        .expect("network input records source metadata")
        .entries()
    else {
        panic!("one network input produces one source range")
    };
    let filename = range
        .field("_filename")
        .and_then(RangeIndexValue::as_str)
        .expect("network source range has a filename")
        .to_owned();
    (range.range(), filename)
}

fn single_source_filename(archives_path: &Path) -> String {
    single_source_metadata(archives_path).1
}

fn extract_local_archive(archive_path: &Path, output_path: &Path) -> Vec<u8> {
    let extraction = clp_s(&[
        "x".as_ref(),
        archive_path.as_os_str(),
        output_path.as_os_str(),
    ]);
    assert_success(&extraction);
    assert_eq!(b"", extraction.stdout.as_slice());
    assert_eq!(b"", extraction.stderr.as_slice());
    fs::read(output_path.join("original")).expect("read extracted network input")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| needle == candidate)
}

#[test]
fn compresses_chunked_remote_json_without_auth_and_retains_the_exact_source_url() {
    let split = JSON_INPUT.len() / 2;
    let server =
        MockServer::start(move |_| chunked_response(&[&JSON_INPUT[..split], &JSON_INPUT[split..]]));
    let source = format!(
        "{}?download=raw%20json#caller-fragment",
        server.url("/logs/input.json")
    );
    let temporary = TestDirectory::new("compress-none");
    let archives_path = temporary.path().join("archives");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        "--auth".as_ref(),
        "none".as_ref(),
        archives_path.as_os_str(),
        OsStr::new(&source),
    ]);
    assert_success(&result);
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let requests = server.requests();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("GET /logs/input.json?download=raw%20json HTTP/1.1\r\n"));
    assert!(!requests[0].contains("caller-fragment"));
    assert_eq!(source, single_source_filename(&archives_path));
    assert_eq!(
        CANONICAL_JSONL,
        extract_local_archive(
            &only_entry(&archives_path),
            &temporary.path().join("extracted")
        )
        .as_slice()
    );
}

#[test]
fn uppercase_plain_http_ignores_an_unrelated_invalid_ca_bundle() {
    let server = MockServer::start(|_| content_length_response("200 OK", JSON_INPUT, ""));
    let source = server.url("/uppercase.json").replacen("http", "HTTP", 1);
    let temporary = TestDirectory::new("uppercase-http");
    let archives_path = temporary.path().join("archives");
    let missing_ca = temporary.path().join("does-not-exist.pem");

    let result = clp_s_with_environment(
        &[
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            archives_path.as_os_str(),
            OsStr::new(&source),
        ],
        &[(
            "CURL_CA_BUNDLE",
            missing_ca.to_str().expect("temporary CA path is UTF-8"),
        )],
    );
    assert_success(&result);
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(1, server.requests().len());
    assert_eq!(source, single_source_filename(&archives_path));
}

#[test]
fn an_existing_http_spelled_path_takes_filesystem_precedence() {
    let temporary = TestDirectory::new("http-filesystem-precedence");
    let input_parent = temporary.path().join("http:");
    fs::create_dir(&input_parent).expect("create HTTP-spelled local parent");
    fs::write(input_parent.join("input.json"), JSON_INPUT).expect("write HTTP-spelled local input");
    let archives_path = temporary.path().join("archives");

    let mut command = Command::new(env!("CARGO_BIN_EXE_clp-s"));
    let result = command
        .current_dir(temporary.path())
        .args([
            OsStr::new("c"),
            OsStr::new("--single-file-archive"),
            archives_path.as_os_str(),
            OsStr::new("http://input.json"),
        ])
        .output()
        .expect("run Rust clp-s binary");
    assert_success(&result);
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!("http://input.json", single_source_filename(&archives_path));
}

#[test]
fn mixed_compression_defers_s3_credentials_until_the_remote_input() {
    let server = MockServer::start(|_| content_length_response("200 OK", JSON_INPUT, ""));
    let remote_source = server.url("/requires-s3-auth.json");
    let temporary = TestDirectory::new("mixed-lazy-s3");
    let local_source = temporary.path().join("local.json");
    fs::write(&local_source, JSON_INPUT).expect("write local input");
    let archives_path = temporary.path().join("archives");

    let result = clp_s_with_removed_environment(
        &[
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            "--auth".as_ref(),
            "s3".as_ref(),
            archives_path.as_os_str(),
            local_source.as_os_str(),
            OsStr::new(&remote_source),
        ],
        &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ],
    );
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(String::from_utf8_lossy(&result.stderr).contains("AWS_ACCESS_KEY_ID"));
    assert_eq!(0, server.requests().len());
    assert_eq!(
        (0..3, local_source.to_string_lossy().into_owned()),
        single_source_metadata(&archives_path),
        "the preceding local source must be committed before remote authentication fails"
    );
}

#[test]
fn compresses_a_remote_container_in_one_streaming_request() {
    let container = tar_gzip(b"nested/events.json", JSON_INPUT);
    let server = MockServer::start(move |_| content_length_response("200 OK", &container, ""));
    let source = server.url("/logs/bundle.tar.gz?version=one");
    let temporary = TestDirectory::new("compress-container");
    let archives_path = temporary.path().join("archives");

    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        OsStr::new(&source),
    ]);
    assert_success(&result);
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(1, server.requests().len());
    assert_eq!(
        (0..3, "nested/events.json".to_owned()),
        single_source_metadata(&archives_path)
    );
    assert_eq!(
        CANONICAL_JSONL,
        extract_local_archive(
            &only_entry(&archives_path),
            &temporary.path().join("extracted")
        )
        .as_slice()
    );
}

#[test]
fn extracts_a_remote_single_file_archive() {
    let server = MockServer::start(|_| content_length_response("200 OK", MINIMAL_SFA, ""));
    let source = format!(
        "{}?object=minimal#local-fragment",
        server.url("/minimal.sfa")
    );
    let temporary = TestDirectory::new("extract-sfa");
    let output_path = temporary.path().join("output");

    let result = clp_s(&[
        "x".as_ref(),
        "--auth".as_ref(),
        "none".as_ref(),
        OsStr::new(&source),
        output_path.as_os_str(),
    ]);
    assert_success(&result);
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());
    assert_eq!(
        MINIMAL_JSONL,
        fs::read(output_path.join("original"))
            .expect("read remotely extracted JSONL")
            .as_slice()
    );

    let requests = server.requests();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("GET /minimal.sfa?object=minimal HTTP/1.1\r\n"));
    assert!(!requests[0].contains("local-fragment"));
}

#[test]
fn searches_a_remote_single_file_archive() {
    let split = MINIMAL_SFA.len() / 2;
    let server = MockServer::start(move |_| {
        chunked_response(&[&MINIMAL_SFA[..split], &MINIMAL_SFA[split..]])
    });
    let source = server.url("/searchable.sfa?object=minimal");

    let result = clp_s(&["s".as_ref(), OsStr::new(&source), "*: *".as_ref()]);
    assert_success(&result);
    assert_eq!(MINIMAL_JSONL, result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let requests = server.requests();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("GET /searchable.sfa?object=minimal HTTP/1.1\r\n"));
}

#[test]
fn searches_remote_kv_ir_directly_and_reopens_only_for_archive_fallback() {
    let encoded = zstd::stream::encode_all(decode_hex(KV_IR_FOUR_BYTE_HEX).as_slice(), 3)
        .expect("encode remote KV-IR fixture");
    let direct_server = MockServer::start(move |_| content_length_response("200 OK", &encoded, ""));
    let direct_source = direct_server.url("/events.clp.zst?object=stream");
    let direct = clp_s(&["s".as_ref(), OsStr::new(&direct_source), "*: *".as_ref()]);
    assert_success(&direct);
    assert_eq!(KV_IR_CANONICAL_JSONL, direct.stdout.as_slice());
    assert_eq!(b"", direct.stderr.as_slice());
    assert_eq!(1, direct_server.requests().len());

    let fallback_server = MockServer::start(|_| content_length_response("200 OK", MINIMAL_SFA, ""));
    let fallback_source = fallback_server.url("/archive.clp.zst?object=sfa");
    let fallback = clp_s(&["s".as_ref(), OsStr::new(&fallback_source), "*: *".as_ref()]);
    assert_success(&fallback);
    assert_eq!(MINIMAL_JSONL, fallback.stdout.as_slice());
    let fallback_stderr = String::from_utf8_lossy(&fallback.stderr);
    assert!(fallback_stderr.contains("Falling back to archive search"));
    assert!(!fallback_stderr.contains("object=sfa"));
    assert_eq!(
        2,
        fallback_server.requests().len(),
        "archive fallback must reopen a one-pass remote source"
    );
}

#[test]
fn remote_archive_ids_exclude_queries_and_ordered_fallback_reopens() {
    let aggregation_server =
        MockServer::start(|_| content_length_response("200 OK", MINIMAL_SFA, ""));
    let aggregation_source = aggregation_server.url("/named.sfa?private=query");
    let aggregation = clp_s(&[
        "s".as_ref(),
        OsStr::new(&aggregation_source),
        "*: *".as_ref(),
        "--count".as_ref(),
    ]);
    assert_success(&aggregation);
    assert_eq!(
        b"{\"archive_id\":\"named.sfa\",\"count\":1}\n",
        aggregation.stdout.as_slice()
    );
    assert_eq!(1, aggregation_server.requests().len());

    let ordered_server =
        MockServer::start(|_| content_length_response("200 OK", LOG_ORDER_SFA, ""));
    let ordered_source = ordered_server.url("/ordered.sfa?private=query");
    let ordered_temporary = TestDirectory::new("ordered-id");
    let ordered_output = ordered_temporary.path().join("output");
    let ordered = clp_s(&[
        "x".as_ref(),
        "--ordered".as_ref(),
        OsStr::new(&ordered_source),
        ordered_output.as_os_str(),
    ]);
    assert_success(&ordered);
    assert_eq!(1, ordered_server.requests().len());
    let output_names: Vec<_> = fs::read_dir(&ordered_output)
        .expect("read ordered remote output")
        .map(|entry| entry.expect("read ordered output entry").file_name())
        .collect();
    assert_eq!(
        vec![std::ffi::OsString::from("ordered.sfa_0_6.jsonl")],
        output_names
    );

    let fallback_server =
        MockServer::start(|_| content_length_response("200 OK", NO_LOG_ORDER_SFA, ""));
    let fallback_source = fallback_server.url("/unordered.sfa?private=query");
    let fallback_temporary = TestDirectory::new("ordered-fallback");
    let fallback_output = fallback_temporary.path().join("output");
    let fallback = clp_s(&[
        "x".as_ref(),
        "--ordered".as_ref(),
        OsStr::new(&fallback_source),
        fallback_output.as_os_str(),
    ]);
    assert_success(&fallback);
    let fallback_stderr = String::from_utf8_lossy(&fallback.stderr);
    assert!(fallback_stderr.contains("falling back to physical order"));
    assert!(!fallback_stderr.contains("private=query"));
    assert_eq!(
        NO_LOG_ORDER_JSONL,
        fs::read(fallback_output.join("original")).expect("read fallback remote output")
    );
    assert_eq!(
        2,
        fallback_server.requests().len(),
        "physical-order fallback must reopen a one-pass remote archive"
    );
}

#[test]
fn remote_archive_ids_match_cpp_decoding_but_ordered_paths_remain_contained() {
    for (path, expected_id) in [
        ("/a%20b.sfa", "a b.sfa"),
        ("/a%2Fb.sfa", "a/b.sfa"),
        ("/named.sfa/", ""),
    ] {
        let server = MockServer::start(|_| content_length_response("200 OK", MINIMAL_SFA, ""));
        let source = server.url(path);
        let aggregation = clp_s(&[
            "s".as_ref(),
            OsStr::new(&source),
            "*: *".as_ref(),
            "--count".as_ref(),
        ]);
        assert_success(&aggregation);
        assert_eq!(
            format!("{{\"archive_id\":\"{expected_id}\",\"count\":1}}\n"),
            String::from_utf8(aggregation.stdout).expect("aggregation output is UTF-8")
        );
        assert_eq!(1, server.requests().len());
    }

    let server = MockServer::start(|_| content_length_response("200 OK", LOG_ORDER_SFA, ""));
    let source = server.url("/a%2Fb.sfa");
    let temporary = TestDirectory::new("escaped-ordered-id");
    let output = temporary.path().join("output");
    let extraction = clp_s(&[
        "x".as_ref(),
        "--ordered".as_ref(),
        OsStr::new(&source),
        output.as_os_str(),
    ]);
    assert_success(&extraction);
    assert_eq!(1, server.requests().len());
    let output_names: Vec<_> = fs::read_dir(&output)
        .expect("read escaped ordered output")
        .map(|entry| entry.expect("read escaped output entry").file_name())
        .collect();
    assert_eq!(
        vec![std::ffi::OsString::from("a%2Fb.sfa_0_6.jsonl")],
        output_names,
        "a decoded path separator must not escape the output directory"
    );
    assert!(!output.join("a").exists());

    let rejected_server = MockServer::start(|_| content_length_response("200 OK", MINIMAL_SFA, ""));
    for (path, expected_error) in [
        ("/bad%FF.sfa", "not valid UTF-8"),
        ("/parent/%2e%2e", "path-normalizing dot segment"),
        ("/", "has no archive ID"),
    ] {
        let source = rejected_server.url(path);
        let result = clp_s(&[
            "s".as_ref(),
            OsStr::new(&source),
            "*: *".as_ref(),
            "--count".as_ref(),
        ]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected_error));
    }
    assert_eq!(0, rejected_server.requests().len());
}

#[test]
fn remote_archive_id_filter_is_rejected_before_any_request() {
    let server = MockServer::start(|_| content_length_response("200 OK", MINIMAL_SFA, ""));
    let source = server.url("/archive-root");
    let temporary = TestDirectory::new("archive-id-filter");
    let output = temporary.path().join("output");
    let result = clp_s(&[
        "x".as_ref(),
        OsStr::new(&source),
        output.as_os_str(),
        "--archive-id".as_ref(),
        "child".as_ref(),
    ]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(String::from_utf8_lossy(&result.stderr).contains("requested archive does not exist"));
    assert_eq!(Vec::<String>::new(), server.requests());
    assert!(!output.exists());
}

#[test]
fn http_404_is_fatal_and_302_is_an_empty_non_followed_input() {
    let not_found =
        MockServer::start(|_| content_length_response("404 Not Found", b"missing object", ""));
    let missing_source = not_found.url("/missing.json");
    let missing_temporary = TestDirectory::new("http-404");
    let missing_output = missing_temporary.path().join("output");
    let missing = clp_s(&[
        "x".as_ref(),
        OsStr::new(&missing_source),
        missing_output.as_os_str(),
    ]);
    assert!(!missing.status.success());
    assert_eq!(b"", missing.stdout.as_slice());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("404"));
    let requests = not_found.requests();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("GET /missing.json HTTP/1.1\r\n"));

    let redirect = MockServer::start(|request| {
        if request.starts_with("GET /redirect.json?caller-query=retained ") {
            content_length_response("302 Found", b"", "Location: /target.json\r\n")
        } else {
            content_length_response("200 OK", JSON_INPUT, "")
        }
    });
    let redirect_source = format!(
        "{}?caller-query=retained#caller-fragment",
        redirect.url("/redirect.json")
    );
    let redirect_temporary = TestDirectory::new("http-302");
    let redirect_archives = redirect_temporary.path().join("archives");
    let redirected = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        redirect_archives.as_os_str(),
        OsStr::new(&redirect_source),
    ]);
    assert_success(&redirected);
    assert_eq!(b"", redirected.stdout.as_slice());
    assert_eq!(b"", redirected.stderr.as_slice());
    assert_eq!(
        (0..0, redirect_source),
        single_source_metadata(&redirect_archives)
    );
    let requests = redirect.requests();
    assert_eq!(
        1,
        requests.len(),
        "the CLI must not request the redirect target"
    );
    assert!(requests[0].starts_with("GET /redirect.json?caller-query=retained HTTP/1.1\r\n"));
    assert!(!requests[0].contains("caller-fragment"));
}

#[test]
fn compression_matches_the_pinned_single_attempt_http_status_behavior() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_server = Arc::clone(&attempts);
    let server = MockServer::start(move |_| {
        if 0 == attempts_for_server.fetch_add(1, Ordering::Relaxed) {
            content_length_response("503 Service Unavailable", b"retry", "")
        } else {
            content_length_response("200 OK", JSON_INPUT, "")
        }
    });
    let source = server.url("/eventually-available.json");
    let temporary = TestDirectory::new("compression-retry");
    let archives_path = temporary.path().join("archives");
    let result = clp_s(&[
        "c".as_ref(),
        "--single-file-archive".as_ref(),
        archives_path.as_os_str(),
        OsStr::new(&source),
    ]);
    assert!(!result.status.success());
    assert_eq!(b"", result.stdout.as_slice());
    assert!(String::from_utf8_lossy(&result.stderr).contains("503"));
    assert_eq!(1, attempts.load(Ordering::Relaxed));
    assert_eq!(1, server.requests().len());
}

#[test]
fn s3_auth_signs_only_the_request_and_does_not_publish_credentials() {
    const ACCESS_KEY: &str = "cli-network-access-key";
    const SECRET_KEY: &str = "cli-network-secret-key";
    const SESSION_TOKEN: &str = "cli-network-session+/=";

    let server = MockServer::start(|_| chunked_response(&[JSON_INPUT]));
    let source = server.url("/bucket/input.json?caller-query=retained");
    let temporary = TestDirectory::new("compress-s3");
    let archives_path = temporary.path().join("archives");
    let environment = [
        ("AWS_ACCESS_KEY_ID", ACCESS_KEY),
        ("AWS_SECRET_ACCESS_KEY", SECRET_KEY),
        ("AWS_SESSION_TOKEN", SESSION_TOKEN),
    ];
    let result = clp_s_with_environment(
        &[
            "c".as_ref(),
            "--single-file-archive".as_ref(),
            "--auth".as_ref(),
            "s3".as_ref(),
            archives_path.as_os_str(),
            OsStr::new(&source),
        ],
        &environment,
    );
    assert_success(&result);
    assert_eq!(b"", result.stdout.as_slice());
    assert_eq!(b"", result.stderr.as_slice());

    let requests = server.requests();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("GET /bucket/input.json?X-Amz-Algorithm="));
    assert!(requests[0].contains("X-Amz-Credential=cli-network-access-key%2F"));
    assert!(requests[0].contains("X-Amz-Security-Token=cli-network-session%2B%2F%3D"));
    assert!(requests[0].contains("X-Amz-Signature="));
    assert!(!requests[0].contains("caller-query=retained"));

    assert_eq!(source, single_source_filename(&archives_path));
    let archive_bytes = fs::read(only_entry(&archives_path)).expect("read S3-input archive");
    for private_value in [
        ACCESS_KEY.as_bytes(),
        SECRET_KEY.as_bytes(),
        SESSION_TOKEN.as_bytes(),
        b"X-Amz-Signature=".as_slice(),
    ] {
        assert!(
            !contains_bytes(&archive_bytes, private_value),
            "request credentials and generated query must not enter archive metadata"
        );
    }

    let not_found =
        MockServer::start(|_| content_length_response("404 Not Found", b"missing archive", ""));
    let missing_source = not_found.url("/bucket/missing.sfa?discarded=yes");
    let missing_output = temporary.path().join("missing-output");
    let failure = clp_s_with_environment(
        &[
            "x".as_ref(),
            "--auth".as_ref(),
            "s3".as_ref(),
            OsStr::new(&missing_source),
            missing_output.as_os_str(),
        ],
        &environment,
    );
    assert!(!failure.status.success());
    assert_eq!(b"", failure.stdout.as_slice());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("404"));
    for private_value in [ACCESS_KEY, SECRET_KEY, SESSION_TOKEN, "X-Amz-"] {
        assert!(
            !String::from_utf8_lossy(&failure.stderr).contains(private_value),
            "generated request credentials must be redacted from errors"
        );
    }
    let requests = not_found.requests();
    assert_eq!(1, requests.len());
    assert!(requests[0].starts_with("GET /bucket/missing.sfa?X-Amz-Algorithm="));
    assert!(!requests[0].contains("discarded=yes"));
}
