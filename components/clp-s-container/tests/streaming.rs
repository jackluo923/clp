use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_s_container::ContainerError;
use clp_s_container::ContainerLimits;
use clp_s_container::ContainerOptions;
use clp_s_container::EntryMetadata;
use clp_s_container::EntryReader;
use clp_s_container::EntryVisitor;
use clp_s_container::FormatPolicy;
use clp_s_container::InputFailureKind;
use clp_s_container::LimitResource;
use clp_s_container::VisitControl;
use clp_s_container::VisitOutcome;
use clp_s_container::visit_entries;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-container-{label}-{}-{counter}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Collector {
    entries: Vec<(EntryMetadata, Vec<u8>)>,
}

impl EntryVisitor for Collector {
    type Error = io::Error;

    fn visit(
        &mut self,
        metadata: &EntryMetadata,
        body: &mut EntryReader<'_>,
    ) -> Result<VisitControl, Self::Error> {
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)?;
        self.entries.push((metadata.clone(), bytes));
        Ok(VisitControl::Continue)
    }
}

fn assert_command(command: &mut Command) {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "command {command:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_common_sources(root: &Path) -> PathBuf {
    let source = root.join("source");
    fs::create_dir_all(source.join("nested")).expect("create source tree");
    fs::write(source.join("a.json"), b"{\"x\":1}\n{\"x\":2}\n").expect("write a");
    fs::write(source.join("nested/b.json"), b"{\"x\":3}\n").expect("write b");
    fs::write(source.join("empty.json"), []).expect("write empty");
    source
}

fn bsdtar(source: &Path, output: &Path, mode: &str, names: &[&str]) {
    let mut command = Command::new("bsdtar");
    command
        .current_dir(source)
        .arg(mode)
        .arg(output)
        .args(names);
    assert_command(&mut command);
}

fn compress(program: &str, input: &Path, output: &Path) {
    let target = File::create(output).expect("create compressed fixture");
    let mut command = Command::new(program);
    command.arg("-c").arg(input).stdout(Stdio::from(target));
    assert_command(&mut command);
}

fn collect(path: &Path, policy: FormatPolicy) -> (VisitOutcome, Collector) {
    let mut collector = Collector::default();
    let outcome = visit_entries(
        File::open(path).expect("open fixture"),
        b"raw-fallback",
        ContainerOptions::new(policy),
        &mut collector,
    )
    .expect("visit fixture");
    (outcome, collector)
}

#[test]
fn visits_tar_gzip_tar_xz_and_zip_in_physical_order() {
    let temp = TestDir::new("formats");
    let source = write_common_sources(temp.path());
    let tar_gzip = temp.path().join("regular.tar.gz");
    let tar_xz = temp.path().join("regular.tar.xz");
    let zip = temp.path().join("regular.zip");
    let names = ["a.json", "nested/b.json", "empty.json"];
    bsdtar(&source, &tar_gzip, "-czf", &names);
    bsdtar(&source, &tar_xz, "-cJf", &names);
    let mut zip_command = Command::new("bsdtar");
    zip_command
        .current_dir(&source)
        .arg("--format")
        .arg("zip")
        .arg("-cf")
        .arg(&zip)
        .args(names);
    assert_command(&mut zip_command);

    for fixture in [&tar_gzip, &tar_xz, &zip] {
        let (outcome, collector) = collect(fixture, FormatPolicy::Strict);
        assert!(matches!(outcome, VisitOutcome::Completed(_)));
        let paths: Vec<&[u8]> = collector
            .entries
            .iter()
            .map(|(metadata, _)| metadata.path())
            .collect();
        assert_eq!(
            paths,
            [
                b"a.json".as_slice(),
                b"nested/b.json".as_slice(),
                b"empty.json".as_slice()
            ]
        );
        assert_eq!(collector.entries[0].1, b"{\"x\":1}\n{\"x\":2}\n");
        assert_eq!(collector.entries[1].1, b"{\"x\":3}\n");
        assert_eq!(collector.entries[2].1, b"");
        let stats = outcome.stats();
        assert_eq!(stats.entries_seen(), 3);
        assert_eq!(stats.regular_entries_visited(), 3);
        assert_eq!(stats.decoded_bytes(), 24);
    }
}

#[test]
fn pins_empty_zero_header_and_unrecognized_input_classification() {
    for policy in [FormatPolicy::Strict, FormatPolicy::CppCompatible] {
        let mut empty_input = Collector::default();
        let outcome = visit_entries(
            Cursor::new([]),
            b"empty",
            ContainerOptions::new(policy),
            &mut empty_input,
        )
        .expect("libarchive recognizes its explicit empty format");
        assert!(matches!(outcome, VisitOutcome::Completed(_)));
        assert_eq!(outcome.stats().entries_seen(), 0);
        assert_eq!(empty_input.entries.len(), 0);

        // Two 512-byte end markers form a valid tar with no headers.
        let mut empty_tar = Collector::default();
        let outcome = visit_entries(
            Cursor::new([0_u8; 1024]),
            b"empty.tar",
            ContainerOptions::new(policy),
            &mut empty_tar,
        )
        .expect("a zero-header tar is still a recognized container");
        assert!(matches!(outcome, VisitOutcome::Completed(_)));
        assert_eq!(outcome.stats().entries_seen(), 0);
        assert_eq!(empty_tar.entries.len(), 0);
    }

    let mut unrecognized = Collector::default();
    let error = visit_entries(
        Cursor::new([0x13_u8, 0x37, 0xff, 0x00, 0xa5]),
        b"not-a-container",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut unrecognized,
    )
    .expect_err("strict mode must reject input for which no format bidder wins");
    assert!(matches!(error, ContainerError::NotContainer(_)));
    assert_eq!(unrecognized.entries.len(), 0);
}

#[test]
fn known_zero_size_entry_does_not_hide_a_later_truncated_member() {
    let temp = TestDir::new("zero-then-truncated");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("create source");
    fs::write(source.join("empty"), []).expect("write empty member");
    fs::write(source.join("later"), vec![b'x'; 2048]).expect("write later member");
    let archive = temp.path().join("two.tar");
    let mut command = Command::new("tar");
    command
        .current_dir(&source)
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&archive)
        .arg("empty")
        .arg("later");
    assert_command(&mut command);

    // Retain both headers but only a prefix of the second entry's declared body.
    let mut bytes = fs::read(&archive).expect("read tar");
    bytes.truncate(2 * 512 + 128);
    let mut collector = Collector::default();
    let error = visit_entries(
        Cursor::new(bytes),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut collector,
    )
    .expect_err("the later known-size body is truncated");
    assert!(matches!(error, ContainerError::Corrupt(_)));
    assert_eq!(collector.entries.len(), 1);
    assert_eq!(collector.entries[0].0.path(), b"empty");
    assert_eq!(collector.entries[0].0.declared_size(), Some(0));
    assert_eq!(collector.entries[0].1, b"");
}

#[cfg(unix)]
#[test]
fn skips_special_entries_and_hardlinks_but_visits_empty_regular_files() {
    let temp = TestDir::new("special");
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("directory")).expect("create directory");
    fs::write(source.join("first.json"), b"first").expect("write first");
    fs::write(source.join("directory/child.json"), b"child").expect("write child");
    fs::write(source.join("empty.json"), []).expect("write empty");
    fs::write(source.join("last.json"), b"last").expect("write last");
    fs::hard_link(source.join("first.json"), source.join("hardlink.json"))
        .expect("create hardlink");
    symlink("first.json", source.join("symlink.json")).expect("create symlink");
    let mut fifo = Command::new("mkfifo");
    fifo.arg(source.join("fifo"));
    assert_command(&mut fifo);

    let archive = temp.path().join("special.tar.gz");
    bsdtar(
        &source,
        &archive,
        "-czf",
        &[
            "first.json",
            "directory",
            "empty.json",
            "symlink.json",
            "fifo",
            "hardlink.json",
            "last.json",
        ],
    );
    let (outcome, collector) = collect(&archive, FormatPolicy::Strict);
    let paths: Vec<&[u8]> = collector
        .entries
        .iter()
        .map(|(metadata, _)| metadata.path())
        .collect();
    assert_eq!(
        paths,
        [
            b"first.json".as_slice(),
            b"directory/child.json".as_slice(),
            b"empty.json".as_slice(),
            b"last.json".as_slice()
        ]
    );
    assert_eq!(collector.entries[2].1, b"");
    assert_eq!(outcome.stats().regular_entries_visited(), 4);
    assert!(outcome.stats().special_entries_skipped() >= 4);
}

#[cfg(unix)]
#[test]
fn preserves_non_utf8_member_path_bytes() {
    let temp = TestDir::new("non-utf8");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("create source");
    let name = OsString::from_vec(b"bad-\xff.json".to_vec());
    fs::write(source.join(&name), b"record").expect("write byte-named file");
    let archive = temp.path().join("non-utf8.tar.gz");
    let mut command = Command::new("tar");
    command
        .current_dir(&source)
        .arg("--format=ustar")
        .arg("-czf")
        .arg(&archive)
        .arg("--")
        .arg(&name);
    assert_command(&mut command);

    let (_, collector) = collect(&archive, FormatPolicy::Strict);
    assert_eq!(collector.entries.len(), 1);
    assert_eq!(collector.entries[0].0.path(), b"bad-\xff.json");
    assert_eq!(collector.entries[0].1, b"record");
}

#[test]
fn raw_zstd_uses_opaque_fallback_and_strict_rejects_raw() {
    let temp = TestDir::new("raw-zstd");
    let raw = temp.path().join("raw.bin");
    let compressed = temp.path().join("raw.zst");
    let payload = b"\xff\x80\0opaque raw payload";
    fs::write(&raw, payload).expect("write raw payload");
    compress("zstd", &raw, &compressed);

    let mut collector = Collector::default();
    let outcome = visit_entries(
        File::open(&compressed).expect("open zstd"),
        b"outer/\xff.zst",
        ContainerOptions::new(FormatPolicy::CppCompatible),
        &mut collector,
    )
    .expect("raw zstd fallback");
    assert!(matches!(outcome, VisitOutcome::Completed(_)));
    assert_eq!(collector.entries.len(), 1);
    assert_eq!(collector.entries[0].0.path(), b"outer/\xff.zst");
    assert_eq!(collector.entries[0].1, payload);

    let mut strict = Collector::default();
    let error = visit_entries(
        File::open(&compressed).expect("reopen zstd"),
        b"ignored",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut strict,
    )
    .expect_err("strict must reject a raw stream");
    assert!(matches!(
        error,
        ContainerError::NotContainer(_) | ContainerError::Corrupt(_)
    ));
    assert_eq!(strict.entries.len(), 0);
}

#[test]
fn cpp_compatible_raw_gzip_json_retains_mtree_failure() {
    let temp = TestDir::new("raw-gzip-json");
    let json = temp.path().join("input.json");
    let gzip = temp.path().join("input.json.gz");
    fs::write(&json, b"{\"x\":1}\n").expect("write json");
    compress("gzip", &json, &gzip);

    let mut collector = Collector::default();
    let error = visit_entries(
        File::open(&gzip).expect("open gzip"),
        b"input.json.gz",
        ContainerOptions::new(FormatPolicy::CppCompatible),
        &mut collector,
    )
    .expect_err("mtree must win and then fail as in the pinned C++ binary");
    assert!(matches!(error, ContainerError::Corrupt(_)));
    assert_eq!(collector.entries.len(), 0);
}

#[test]
fn stacked_filters_work_and_obey_filter_layer_limit() {
    let temp = TestDir::new("filters");
    let source = write_common_sources(temp.path());
    let tar = temp.path().join("regular.tar");
    let zstd = temp.path().join("regular.tar.zst");
    let gzip = temp.path().join("regular.tar.zst.gz");
    bsdtar(&source, &tar, "-cf", &["a.json", "nested/b.json"]);
    compress("zstd", &tar, &zstd);
    compress("gzip", &zstd, &gzip);

    let (outcome, collector) = collect(&gzip, FormatPolicy::CppCompatible);
    assert_eq!(collector.entries.len(), 2);
    assert!(outcome.stats().filter_layers() >= 2);

    let limits = ContainerLimits::DEFAULT.with_max_filter_layers(1);
    let mut rejected = Collector::default();
    let error = visit_entries(
        File::open(&gzip).expect("open layered archive"),
        b"fallback",
        ContainerOptions::new(FormatPolicy::CppCompatible).with_limits(limits),
        &mut rejected,
    )
    .expect_err("filter limit must reject stacked filters");
    assert!(matches!(
        error,
        ContainerError::Limit(source) if LimitResource::FilterLayers == source.resource()
    ));
    assert_eq!(rejected.entries.len(), 0);
}

#[test]
fn does_not_recurse_into_nested_archive_members() {
    let temp = TestDir::new("no-recursion");
    let source = write_common_sources(temp.path());
    let inner = temp.path().join("inner.tar.gz");
    bsdtar(&source, &inner, "-czf", &["a.json"]);
    let outer_source = temp.path().join("outer-source");
    fs::create_dir(&outer_source).expect("create outer source");
    fs::copy(&inner, outer_source.join("inner.tar.gz")).expect("copy inner");
    let outer = temp.path().join("outer.tar.gz");
    bsdtar(&outer_source, &outer, "-czf", &["inner.tar.gz"]);

    let (_, collector) = collect(&outer, FormatPolicy::Strict);
    assert_eq!(collector.entries.len(), 1);
    assert_eq!(collector.entries[0].0.path(), b"inner.tar.gz");
    assert_eq!(collector.entries[0].1, fs::read(inner).expect("read inner"));
}

#[test]
fn continue_automatically_drains_before_the_next_callback() {
    struct PrefixVisitor {
        paths: Vec<Vec<u8>>,
        prefixes: Vec<u8>,
    }

    impl EntryVisitor for PrefixVisitor {
        type Error = io::Error;

        fn visit(
            &mut self,
            metadata: &EntryMetadata,
            body: &mut EntryReader<'_>,
        ) -> Result<VisitControl, Self::Error> {
            self.paths.push(metadata.path().to_vec());
            let mut byte = [0_u8; 1];
            if 0 < body.read(&mut byte)? {
                self.prefixes.push(byte[0]);
            }
            Ok(VisitControl::Continue)
        }
    }

    let temp = TestDir::new("auto-drain");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("create source");
    fs::write(source.join("first"), b"abcdef").expect("write first");
    fs::write(source.join("second"), b"ghijkl").expect("write second");
    let archive = temp.path().join("two.tar.gz");
    bsdtar(&source, &archive, "-czf", &["first", "second"]);

    let mut visitor = PrefixVisitor {
        paths: Vec::new(),
        prefixes: Vec::new(),
    };
    let outcome = visit_entries(
        File::open(&archive).expect("open archive"),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut visitor,
    )
    .expect("visit archive");
    assert_eq!(visitor.paths, [b"first".to_vec(), b"second".to_vec()]);
    assert_eq!(visitor.prefixes, b"ag");
    assert_eq!(outcome.stats().decoded_bytes(), 12);
}

#[test]
fn cancellation_stops_before_another_callback() {
    struct Canceller(u64);

    impl EntryVisitor for Canceller {
        type Error = Infallible;

        fn visit(
            &mut self,
            _metadata: &EntryMetadata,
            _body: &mut EntryReader<'_>,
        ) -> Result<VisitControl, Self::Error> {
            self.0 += 1;
            Ok(VisitControl::Cancel)
        }
    }

    let temp = TestDir::new("cancel");
    let source = write_common_sources(temp.path());
    let archive = temp.path().join("two.tar.gz");
    bsdtar(&source, &archive, "-czf", &["a.json", "nested/b.json"]);
    let mut visitor = Canceller(0);
    let outcome = visit_entries(
        File::open(archive).expect("open archive"),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut visitor,
    )
    .expect("cancel cleanly");
    assert!(matches!(outcome, VisitOutcome::Cancelled(_)));
    assert_eq!(visitor.0, 1);
    assert_eq!(outcome.stats().regular_entries_visited(), 1);
    assert_eq!(outcome.stats().decoded_bytes(), 0);
}

#[test]
fn visitor_error_is_distinct_and_does_not_visit_later_entries() {
    struct Reject(u64);

    impl EntryVisitor for Reject {
        type Error = &'static str;

        fn visit(
            &mut self,
            _metadata: &EntryMetadata,
            _body: &mut EntryReader<'_>,
        ) -> Result<VisitControl, Self::Error> {
            self.0 += 1;
            Err("rejected")
        }
    }

    let temp = TestDir::new("visitor-error");
    let source = write_common_sources(temp.path());
    let archive = temp.path().join("two.tar.gz");
    bsdtar(&source, &archive, "-czf", &["a.json", "nested/b.json"]);
    let mut visitor = Reject(0);
    let error = visit_entries(
        File::open(archive).expect("open archive"),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut visitor,
    )
    .expect_err("visitor rejects first entry");
    assert!(matches!(error, ContainerError::Visitor("rejected")));
    assert_eq!(visitor.0, 1);
}

#[test]
fn truncated_later_member_preserves_prior_callback_effects() {
    let temp = TestDir::new("prior-effects");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("create source");
    fs::write(source.join("first"), b"one").expect("write first");
    fs::write(source.join("second"), vec![b'x'; 2048]).expect("write second");
    let archive = temp.path().join("two.tar");
    let mut command = Command::new("tar");
    command
        .current_dir(&source)
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&archive)
        .arg("first")
        .arg("second");
    assert_command(&mut command);
    let mut bytes = fs::read(&archive).expect("read tar");
    bytes.truncate(1024 + 512 + 128);

    let mut collector = Collector::default();
    let error = visit_entries(
        Cursor::new(bytes),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut collector,
    )
    .expect_err("second member is truncated");
    assert!(matches!(error, ContainerError::Corrupt(_)));
    assert_eq!(collector.entries.len(), 1);
    assert_eq!(collector.entries[0].0.path(), b"first");
    assert_eq!(collector.entries[0].1, b"one");
}

struct ChunkFailReader {
    inner: Cursor<Vec<u8>>,
    delivered: usize,
    fail_after: usize,
    max_chunk: usize,
}

impl Read for ChunkFailReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.delivered >= self.fail_after {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected failure",
            ));
        }
        let allowed = output
            .len()
            .min(self.max_chunk)
            .min(self.fail_after - self.delivered);
        let read = self.inner.read(&mut output[..allowed])?;
        self.delivered += read;
        Ok(read)
    }
}

#[test]
fn caller_reader_io_failure_remains_distinct() {
    let temp = TestDir::new("reader-failure");
    let source = write_common_sources(temp.path());
    let archive = temp.path().join("regular.tar.gz");
    bsdtar(&source, &archive, "-czf", &["a.json"]);
    let reader = ChunkFailReader {
        inner: Cursor::new(fs::read(archive).expect("read fixture")),
        delivered: 0,
        fail_after: 32,
        max_chunk: 7,
    };
    let mut visitor = Collector::default();
    let error = visit_entries(
        reader,
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut visitor,
    )
    .expect_err("injected reader error");
    assert!(matches!(
        error,
        ContainerError::Input(ref source) if InputFailureKind::Io == source.kind()
    ));
}

struct PanicReader;

impl Read for PanicReader {
    fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
        panic!("injected Read panic")
    }
}

#[test]
fn caller_reader_panic_is_contained_across_c_callback() {
    let mut visitor = Collector::default();
    let error = visit_entries(
        PanicReader,
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut visitor,
    )
    .expect_err("reader panic must become a typed error");
    assert!(matches!(
        error,
        ContainerError::Input(ref source) if InputFailureKind::Panicked == source.kind()
    ));
}

#[test]
fn enforces_input_entry_total_count_and_path_limits() {
    let temp = TestDir::new("limits");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("create source");
    fs::write(source.join("first"), b"abc").expect("write first");
    fs::write(source.join("second"), b"def").expect("write second");
    let archive = temp.path().join("two.tar");
    bsdtar(&source, &archive, "-cf", &["first", "second"]);
    let bytes = fs::read(&archive).expect("read fixture");

    let cases = [
        (
            ContainerLimits::DEFAULT.with_max_input_bytes(8),
            LimitResource::InputBytes,
        ),
        (
            ContainerLimits::DEFAULT.with_max_entry_decoded_bytes(2),
            LimitResource::EntryDecodedBytes,
        ),
        (
            ContainerLimits::DEFAULT.with_max_total_decoded_bytes(5),
            LimitResource::TotalDecodedBytes,
        ),
        (
            ContainerLimits::DEFAULT.with_max_entries(1),
            LimitResource::Entries,
        ),
        (
            ContainerLimits::DEFAULT.with_max_path_bytes(2),
            LimitResource::PathBytes,
        ),
    ];
    for (limits, expected) in cases {
        let mut visitor = Collector::default();
        let error = visit_entries(
            Cursor::new(&bytes),
            b"fallback",
            ContainerOptions::new(FormatPolicy::Strict).with_limits(limits),
            &mut visitor,
        )
        .expect_err("limit must reject fixture");
        assert!(matches!(
            error,
            ContainerError::Limit(source) if expected == source.resource()
        ));
    }
}

#[test]
fn sparse_blocks_are_zero_filled_chunkwise_and_gap_limited() {
    struct TinyCollector(Vec<u8>);

    impl EntryVisitor for TinyCollector {
        type Error = io::Error;

        fn visit(
            &mut self,
            _metadata: &EntryMetadata,
            body: &mut EntryReader<'_>,
        ) -> Result<VisitControl, Self::Error> {
            let mut chunk = [0_u8; 3];
            loop {
                let read = body.read(&mut chunk)?;
                if 0 == read {
                    break;
                }
                self.0.extend_from_slice(&chunk[..read]);
            }
            Ok(VisitControl::Continue)
        }
    }

    let temp = TestDir::new("sparse");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("create source");
    let sparse_path = source.join("sparse.bin");
    let mut sparse = File::create(&sparse_path).expect("create sparse input");
    sparse.write_all(b"A").expect("write prefix");
    sparse.seek(SeekFrom::Start(1024 * 1024)).expect("seek gap");
    sparse.write_all(b"Z").expect("write suffix");
    drop(sparse);
    let archive = temp.path().join("sparse.tar.gz");
    let mut command = Command::new("tar");
    command
        .current_dir(&source)
        .arg("--sparse")
        .arg("--format=gnu")
        .arg("-czf")
        .arg(&archive)
        .arg("sparse.bin");
    assert_command(&mut command);

    let mut collector = TinyCollector(Vec::new());
    let outcome = visit_entries(
        File::open(&archive).expect("open sparse archive"),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict),
        &mut collector,
    )
    .expect("read sparse archive");
    assert_eq!(collector.0.len(), 1024 * 1024 + 1);
    assert_eq!(collector.0[0], b'A');
    assert!(collector.0[1..1024 * 1024].iter().all(|byte| 0 == *byte));
    assert_eq!(collector.0[1024 * 1024], b'Z');
    assert_eq!(outcome.stats().decoded_bytes(), 1024 * 1024 + 1);

    let limits = ContainerLimits::DEFAULT.with_max_sparse_gap_bytes(1024);
    let mut rejected = TinyCollector(Vec::new());
    let error = visit_entries(
        File::open(archive).expect("reopen sparse archive"),
        b"fallback",
        ContainerOptions::new(FormatPolicy::Strict).with_limits(limits),
        &mut rejected,
    )
    .expect_err("sparse gap limit must reject fixture");
    assert!(matches!(
        error,
        ContainerError::Limit(source) if LimitResource::SparseGapBytes == source.resource()
    ));
}
