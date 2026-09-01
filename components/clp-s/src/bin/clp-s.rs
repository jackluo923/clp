//! Thin command-line adapter for the `clp-s` library.

#![forbid(unsafe_code)]

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use clp_s::ArchiveReader;
use clp_s::ExtractionError;
use clp_s::ExtractionMode;
use clp_s::ExtractionOptions;
use clp_s::JsonlRecord;
use clp_s::JsonlRecordSink;
use clp_s::archive::DirectoryArchiveMember;
use clp_s::archive::DirectoryArchiveReader;
use clp_s::archive::FsDirectoryArchiveSource;
use clp_s::archive::MetadataLimits;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::extract_jsonl_records;
use clp_s::ingest::ContainerArchiveOptions;
use clp_s::ingest::ContainerLimits;
use clp_s::ingest::ContainerOptions;
use clp_s::ingest::FormatPolicy;
use clp_s::ingest::IncompleteDocumentPolicy;
use clp_s::ingest::InputCompressionPolicy;
use clp_s::ingest::InputLimits;
use clp_s::ingest::JsonArchiveOptions;
use clp_s::ingest::JsonStructuredArrayLimits;
use clp_s::ingest::JsonTimestampResolver;
use clp_s::ingest::KvIrNamespace;
use clp_s::ingest::KvIrOptions;
use clp_s::ingest::KvIrReadError;
use clp_s::ingest::KvIrReader;
use clp_s::ingest::KvIrTimestampResolver;
use clp_s::ingest::ParseManyLimits;
use clp_s::ingest::ParseManyOptions;
use clp_s::ingest::ProbedStructuredInput;
use clp_s::ingest::SourcePathTransform;
use clp_s::ingest::StructuredInputKind;
use clp_s::ingest::StructuredStreamOptions;
use clp_s::ingest::ingest_container_archive_set;
use clp_s::ingest::ingest_structured_stream;
use clp_s::ingest::probe_structured_input;
use clp_s::json::JsonBytePolicy;
use clp_s::json::JsonEscapeLimits;
use clp_s::json::append_json_string;
use clp_s::network::ForwardSeekReader;
use clp_s::network::HttpClient;
use clp_s::network::HttpClientOptions;
use clp_s::network::NetworkAuth;
use clp_s::network::S3CredentialsRef;
use clp_s::search::AggregationPlan;
use clp_s::search::AggregationResultRef;
use clp_s::search::AggregationResultsCacheAdapter;
use clp_s::search::AggregationResultsCacheBatchSink;
use clp_s::search::AggregationValueRef;
use clp_s::search::ArchiveMatchSink;
use clp_s::search::ArchiveSearchOptions;
use clp_s::search::ArchiveTableMatches;
use clp_s::search::AuthoritativeTimestampRange;
use clp_s::search::KqlLimits;
use clp_s::search::KvIrJsonlMatchSink;
use clp_s::search::KvIrJsonlOptions;
use clp_s::search::KvIrSearchLimits;
use clp_s::search::KvIrSearchOptions;
use clp_s::search::KvIrSearchSink;
use clp_s::search::Projection;
use clp_s::search::ProjectionLimits;
use clp_s::search::ReducerProtocol;
use clp_s::search::ResultsCacheSearchResult;
use clp_s::search::SearchJsonlAdapter;
use clp_s::search::SearchJsonlOptions;
use clp_s::search::SearchLimits;
use clp_s::search::SearchMsgpackAdapter;
use clp_s::search::SearchMsgpackOptions;
use clp_s::search::SearchOptions;
use clp_s::search::SearchResultsCacheAdapter;
use clp_s::search::SearchResultsCacheBatchSink;
use clp_s::search::SearchResultsCacheOptions;
use clp_s::search::is_cpp_tolerated_kv_ir_truncation;
use clp_s::search::is_kv_ir_search_candidate;
use clp_s::search::parse_kql;
use clp_s::search::search_archive;
use clp_s::search::search_first_kv_ir_stream;
use clp_s::writer::ArchiveSetArchive;
use clp_s::writer::ArchiveSetOptions;
use clp_s::writer::ArchiveSetStats;
use clp_s::writer::ArchiveSetStatsCallback;
use clp_s::writer::ArchiveSetWriter;
use clp_s::writer::ArchiveSourceContext;
use clp_s::writer::FinalizedArchiveSink;
use clp_s::writer::FsDirectoryArchiveSink;
use clp_s::writer::WriterLimits;
use clp_s::writer::WriterOptions;
use mongodb::bson::Bson;
use mongodb::bson::Document;
use mongodb::sync::Client as MongoClient;
use mongodb::sync::Collection as MongoCollection;
use reqwest::Url;
use uuid::Uuid;

const OUTPUT_BUFFER_CAPACITY: usize = 1024 * 1024;
/// The pinned C++ CLI has no aggregate input-byte ceiling and detects at most four wrappers.
const LOCAL_INPUT_LIMITS: InputLimits = InputLimits::new(u64::MAX, u64::MAX, 4);

type CliResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Parser)]
#[command(name = "clp-s", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compress input into CLP structured archives.
    #[command(name = "c")]
    Compress(CompressArgs),
    /// Extract one archive or a directory containing archives.
    #[command(name = "x")]
    Extract(ExtractArgs),
    /// Search CLP structured archives.
    #[command(name = "s")]
    Search(Box<SearchArgs>),
}

#[derive(Debug, Args)]
struct CompressArgs {
    /// Directory in which generated archives are published.
    archives_dir: PathBuf,
    /// Local files or directories to ingest, in command-line order.
    #[arg(required_unless_present = "files_from")]
    input_paths: Vec<PathBuf>,
    /// Zstd compression level.
    #[arg(long, default_value_t = 3, allow_hyphen_values = true)]
    compression_level: i32,
    /// Rotate after the dictionaries and encoded messages reach this many bytes.
    #[arg(long, default_value_t = 8_u64 * 1024 * 1024 * 1024)]
    target_encoded_size: u64,
    /// Minimum uncompressed packed-table bytes before starting a new zstd frame.
    #[arg(long, default_value_t = 1024_u64 * 1024)]
    min_table_size: u64,
    /// Maximum bytes accepted for one JSON document.
    #[arg(long, default_value_t = 512_u64 * 1024 * 1024)]
    max_document_size: u64,
    /// Path of the authoritative timestamp field.
    #[arg(long)]
    timestamp_key: Option<String>,
    /// Read additional input paths from this file.
    #[arg(long, short = 'f')]
    files_from: Option<PathBuf>,
    #[command(flatten)]
    output: CompressionOutputArgs,
    #[command(flatten)]
    representation: CompressionRepresentationArgs,
    #[command(flatten)]
    path_transform: CompressionPathArgs,
    /// Authentication method for network inputs.
    #[arg(long, value_enum, default_value = "none")]
    auth: AuthMethod,
}

#[derive(Debug, Args)]
struct CompressionOutputArgs {
    /// Print one JSON statistics object after each archive is published.
    #[arg(long)]
    print_archive_stats: bool,
    /// Publish each archive as one file instead of a directory.
    #[arg(long)]
    single_file_archive: bool,
}

#[derive(Debug, Args)]
struct CompressionRepresentationArgs {
    /// Decode floating-point values without retaining their exact source spelling.
    #[arg(long)]
    no_retain_float_format: bool,
    /// Encode arrays as structured schema nodes.
    #[arg(long)]
    structurize_arrays: bool,
    /// Omit archive-global log-order and range-index metadata.
    #[arg(long)]
    disable_log_order: bool,
}

#[derive(Debug, Args)]
struct CompressionPathArgs {
    /// Make source filenames absolute before recording them.
    #[arg(long)]
    normalize_paths: bool,
    /// Remove this prefix from recorded source filenames.
    #[arg(long)]
    remove_path_prefix: Option<OsString>,
    /// Remove a leading slash from recorded source filenames.
    #[arg(long)]
    remove_leading_slash: bool,
}

#[derive(Debug, Args)]
struct ExtractArgs {
    /// Path to one archive or a directory containing archives.
    archive_path: PathBuf,
    /// Directory for extracted JSONL files.
    output_dir: PathBuf,
    /// Reconstruct records in canonical archive log order.
    #[arg(long)]
    ordered: bool,
    /// Approximate JSONL bytes per ordered output chunk; zero disables rotation.
    #[arg(long, default_value_t = 0)]
    target_ordered_chunk_size: u64,
    /// Print one JSON object after each ordered chunk is finalized.
    #[arg(long)]
    print_ordered_chunk_stats: bool,
    /// Extract only this archive beneath `archive_path`.
    #[arg(long)]
    archive_id: Option<OsString>,
    /// Authentication method for network archives.
    #[arg(long, value_enum, default_value = "none")]
    auth: AuthMethod,
    /// MongoDB URI for ordered-decompression metadata.
    #[arg(long)]
    mongodb_uri: Option<String>,
    /// MongoDB collection for ordered-decompression metadata.
    #[arg(long)]
    mongodb_collection: Option<String>,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Path to one local archive or a directory containing archives.
    archive_path: PathBuf,
    /// Positional KQL query followed by an optional C++ output-handler name.
    positional: Vec<OsString>,
    /// KQL query; may be used instead of the positional query.
    #[arg(long, short = 'q', allow_hyphen_values = true)]
    query: Option<String>,
    /// Ignore ASCII case distinctions in string comparisons.
    #[arg(long, short = 'i')]
    ignore_case: bool,
    /// Search only this archive beneath `archive_path`.
    #[arg(long)]
    archive_id: Option<OsString>,
    /// Project only these exact escaped column paths.
    #[arg(long, num_args = 1..)]
    projection: Vec<String>,
    /// Authentication method for network archives.
    #[arg(long, value_enum, default_value = "none")]
    auth: AuthMethod,
    /// Inclusive lower authoritative timestamp bound.
    #[arg(long, allow_hyphen_values = true)]
    tge: Option<i64>,
    /// Inclusive upper authoritative timestamp bound.
    #[arg(long, allow_hyphen_values = true)]
    tle: Option<i64>,
    /// Publish search telemetry (recognized, not implemented).
    #[arg(long)]
    enable_telemetry: bool,
    /// Count matching records.
    #[arg(long)]
    count: bool,
    /// Count matches in fixed epoch-millisecond timestamp buckets.
    #[arg(long, allow_hyphen_values = true)]
    count_by_time: Option<i64>,
    /// Find the minimum numeric value of this field.
    #[arg(long)]
    min: Option<String>,
    /// Find the maximum numeric value of this field.
    #[arg(long)]
    max: Option<String>,
    /// Find distinct scalar values of this field.
    #[arg(long)]
    unique: Option<String>,
    /// File output destination for metadata-bearing `MessagePack` tuples.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Network/reducer host.
    #[arg(long)]
    host: Option<String>,
    /// Network/reducer port.
    #[arg(long)]
    port: Option<u16>,
    /// Reducer job ID.
    #[arg(long, allow_hyphen_values = true)]
    job_id: Option<i64>,
    /// Results-cache MongoDB URI.
    #[arg(long)]
    uri: Option<String>,
    /// Results-cache collection.
    #[arg(long)]
    collection: Option<String>,
    /// Results-cache batch size.
    #[arg(long)]
    batch_size: Option<u64>,
    /// Results-cache maximum result count per archive.
    #[arg(long)]
    max_num_results: Option<u64>,
    /// Results-cache dataset name.
    #[arg(long)]
    dataset: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum AuthMethod {
    None,
    S3,
}

struct CliNetworkContext {
    auth: AuthMethod,
    plain_client: RefCell<Option<HttpClient>>,
    tls_client: RefCell<Option<HttpClient>>,
}

struct CliS3Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl CliNetworkContext {
    const fn new(auth: AuthMethod) -> Self {
        Self {
            auth,
            plain_client: RefCell::new(None),
            tls_client: RefCell::new(None),
        }
    }

    fn open(&self, source_url: &str) -> CliResult<clp_s::network::HttpReader> {
        // Match the C++ adapter's per-open environment lookup. In particular, a local input that
        // precedes a remote S3 input is ingested before missing credentials can fail the latter.
        let credentials = match self.auth {
            AuthMethod::None => None,
            AuthMethod::S3 => Some(CliS3Credentials {
                access_key_id: required_utf8_environment("AWS_ACCESS_KEY_ID")?,
                secret_access_key: required_utf8_environment("AWS_SECRET_ACCESS_KEY")?,
                session_token: optional_utf8_environment("AWS_SESSION_TOKEN")?,
            }),
        };
        let auth = credentials
            .as_ref()
            .map_or(NetworkAuth::None, |credentials| {
                NetworkAuth::S3(S3CredentialsRef::new(
                    &credentials.access_key_id,
                    &credentials.secret_access_key,
                    credentials.session_token.as_deref(),
                ))
            });
        let client = if has_url_scheme(source_url, "https") {
            &self.tls_client
        } else {
            &self.plain_client
        };
        let mut client = client
            .try_borrow_mut()
            .map_err(|_| io::Error::other("HTTP client is already borrowed"))?;
        if client.is_none() {
            let options = HttpClientOptions::compatibility_unbounded();
            *client = Some(if has_url_scheme(source_url, "https") {
                match env::var_os("CURL_CA_BUNDLE") {
                    Some(path) => {
                        let pem = fs::read(&path).map_err(|source| {
                            io::Error::new(
                                source.kind(),
                                format!("failed to read CURL_CA_BUNDLE: {source}"),
                            )
                        })?;
                        HttpClient::new_with_ca_bundle(options, &pem)?
                    }
                    None => HttpClient::new(options)?,
                }
            } else {
                // libcurl does not consult a CA bundle for a plaintext request. Deferring the
                // bundle preserves that behavior and avoids rejecting HTTP for an unrelated path.
                HttpClient::new(options)?
            });
        }
        Ok(client
            .as_ref()
            .ok_or_else(|| io::Error::other("HTTP client initialization produced no client"))?
            .open(source_url, auth)?)
    }
}

fn required_utf8_environment(name: &'static str) -> CliResult<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Err(invalid_input(format!(
            "{name} environment variable is required for S3 authentication"
        ))),
        Err(env::VarError::NotUnicode(_)) => Err(invalid_input(format!(
            "{name} environment variable must be valid UTF-8"
        ))),
    }
}

fn optional_utf8_environment(name: &'static str) -> CliResult<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(invalid_input(format!(
            "{name} environment variable must be valid UTF-8"
        ))),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("clp-s: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> CliResult<()> {
    match cli.command {
        Command::Extract(arguments) => run_extract(&arguments),
        Command::Compress(arguments) => run_compress(&arguments),
        Command::Search(arguments) => run_search(&arguments),
    }
}

fn run_compress(arguments: &CompressArgs) -> CliResult<()> {
    let timestamp_resolvers =
        CompressionTimestampResolvers::parse(arguments.timestamp_key.as_deref())?;
    let archive_creator_id = Uuid::new_v4().to_string();
    let inputs = prepare_compression_inputs(arguments, &archive_creator_id)?;
    create_output_directory(&arguments.archives_dir)?;
    let network = inputs
        .iter()
        .any(PreparedCompressionInput::is_network)
        .then(|| CliNetworkContext::new(arguments.auth));

    let published_ids = Rc::new(PublishedArchiveIds::default());
    let publisher = CompressionArchivePublisher::new(
        &arguments.archives_dir,
        arguments.output.single_file_archive,
        Rc::clone(&published_ids),
    );
    let stats = CompressionArchiveStats::new(arguments.output.print_archive_stats, published_ids);
    let writer_options = WriterOptions::new(arguments.compression_level)
        .with_limits(WriterLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX))
        .with_minimum_packed_stream_size(arguments.min_table_size)
        .with_log_order(!arguments.representation.disable_log_order);
    let mut archive_set = ArchiveSetWriter::new(
        publisher,
        stats,
        ArchiveSetOptions::new(writer_options, arguments.target_encoded_size),
    );
    let parse_limits = ParseManyLimits::new(
        arguments.max_document_size,
        arguments.max_document_size,
        arguments.max_document_size,
        arguments.max_document_size,
    );
    let json_options = JsonArchiveOptions::new()
        .with_retain_float_format(!arguments.representation.no_retain_float_format)
        .with_structurize_arrays(arguments.representation.structurize_arrays)
        .with_structured_array_limits(JsonStructuredArrayLimits::new(u64::MAX, u64::MAX));
    let stream_options = StructuredStreamOptions::new()
        .with_parse_many(
            ParseManyOptions::new()
                .with_limits(parse_limits)
                .with_incomplete_document_policy(IncompleteDocumentPolicy::Ignore),
        )
        .with_json_archive(json_options)
        .with_timestamp_resolvers(
            timestamp_resolvers.json.as_ref(),
            timestamp_resolvers.kv_ir.as_ref(),
        );
    let mut ingestion_error = None;
    for input in inputs {
        let PreparedCompressionInput {
            physical_path,
            source_context,
        } = input;
        if let Err(error) = ingest_compression_input(
            &physical_path,
            network.as_ref(),
            source_context,
            &mut archive_set,
            stream_options,
        ) {
            ingestion_error = Some(error);
            break;
        }
    }

    archive_set.finish()?;
    if let Some(error) = ingestion_error {
        return Err(error);
    }
    Ok(())
}

#[derive(Debug, Default)]
struct CompressionTimestampResolvers {
    json: Option<JsonTimestampResolver>,
    kv_ir: Option<KvIrTimestampResolver>,
}

impl CompressionTimestampResolvers {
    fn parse(descriptor: Option<&str>) -> CliResult<Self> {
        let Some(descriptor) = descriptor.filter(|value| !value.is_empty()) else {
            return Ok(Self::default());
        };
        let kv_ir = KvIrTimestampResolver::parse(descriptor)?;
        let json = if Some(KvIrNamespace::UserGenerated) == kv_ir.namespace() {
            Some(JsonTimestampResolver::parse(descriptor)?)
        } else {
            None
        };
        Ok(Self {
            json,
            kv_ir: Some(kv_ir),
        })
    }
}

struct PreparedCompressionInput {
    physical_path: PathBuf,
    source_context: ArchiveSourceContext,
}

impl PreparedCompressionInput {
    fn is_network(&self) -> bool {
        looks_like_network_path(&self.physical_path)
    }
}

fn prepare_compression_inputs(
    arguments: &CompressArgs,
    archive_creator_id: &str,
) -> CliResult<Vec<PreparedCompressionInput>> {
    let mut requested = arguments.input_paths.clone();
    if let Some(path) = &arguments.files_from {
        let contents = fs::read_to_string(path)?;
        requested.extend(
            contents
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from),
        );
    }
    if requested.is_empty() {
        return Err(invalid_input("no input paths specified"));
    }
    let prefix = prepare_compression_prefix(arguments)?;
    if arguments.path_transform.normalize_paths {
        for path in &mut requested {
            if looks_like_network_path(path) {
                continue;
            }
            *path = fs::canonicalize(&*path).map_err(|source| {
                io::Error::new(
                    source.kind(),
                    format!(
                        "failed to normalize input path '{}': {source}",
                        path.display()
                    ),
                )
            })?;
        }
    }

    let mut files = Vec::new();
    let mut active_directories = HashSet::new();
    for path in requested {
        if looks_like_network_path(&path) {
            files.push(path);
        } else {
            collect_compression_inputs(&path, &mut active_directories, &mut files)?;
        }
    }
    if files.is_empty() {
        return Err(invalid_input("no input paths specified"));
    }

    let mut transform = SourcePathTransform::new()
        .with_remove_leading_slash(arguments.path_transform.remove_leading_slash);
    if let Some(prefix) = prefix {
        transform = transform.with_prefix_to_remove(prefix);
    }
    files
        .into_iter()
        .map(|physical_path| {
            let source_context = if looks_like_network_path(&physical_path) {
                let source_url = physical_path
                    .to_str()
                    .ok_or_else(|| invalid_input("network input URL must be valid UTF-8"))?;
                ArchiveSourceContext::new(source_url, archive_creator_id)
            } else {
                transform.source_context(&physical_path, archive_creator_id)?
            };
            Ok(PreparedCompressionInput {
                physical_path,
                source_context,
            })
        })
        .collect()
}

fn prepare_compression_prefix(arguments: &CompressArgs) -> CliResult<Option<PathBuf>> {
    let Some(prefix) = arguments
        .path_transform
        .remove_path_prefix
        .as_deref()
        .filter(|prefix| !prefix.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(None);
    };
    let metadata = match fs::metadata(&prefix) {
        Ok(metadata) => metadata,
        Err(source) if io::ErrorKind::NotFound == source.kind() => {
            return Err(invalid_input("specified prefix to remove does not exist"));
        }
        Err(source) => return Err(Box::new(source)),
    };
    if !metadata.is_dir() {
        return Err(invalid_input(
            "specified prefix to remove is not a directory",
        ));
    }
    if arguments.path_transform.normalize_paths {
        return fs::canonicalize(&prefix).map(Some).map_err(|source| {
            Box::new(io::Error::new(
                source.kind(),
                format!(
                    "failed to normalize prefix '{}': {source}",
                    prefix.display()
                ),
            )) as Box<dyn Error>
        });
    }
    Ok(Some(prefix))
}

fn collect_compression_inputs(
    path: &Path,
    active_directories: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
) -> CliResult<()> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "input path '{}' is not a regular file or directory",
                path.display()
            ),
        )));
    }

    let canonical = fs::canonicalize(path)?;
    if !active_directories.insert(canonical.clone()) {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("recursive input directory cycle at '{}'", path.display()),
        )));
    }
    for entry in fs::read_dir(path)? {
        collect_compression_inputs(&entry?.path(), active_directories, files)?;
    }
    active_directories.remove(&canonical);
    Ok(())
}

fn ingest_compression_input<S, C>(
    path: &Path,
    network: Option<&CliNetworkContext>,
    source_context: ArchiveSourceContext,
    archive_set: &mut ArchiveSetWriter<S, C>,
    stream_options: StructuredStreamOptions<'_>,
) -> CliResult<()>
where
    S: FinalizedArchiveSink + 'static,
    C: ArchiveSetStatsCallback + 'static,
    S::Error: Error + 'static,
    C::Error: Error + 'static, {
    let probed = if looks_like_network_path(path) {
        let source_url = path
            .to_str()
            .ok_or_else(|| invalid_input("network input URL must be valid UTF-8"))?;
        probe_network_input(
            network.ok_or_else(|| io::Error::other("network input has no HTTP client"))?,
            source_url,
        )?
    } else {
        probe_structured_input(
            File::open(path)?,
            LOCAL_INPUT_LIMITS,
            InputCompressionPolicy::GzipAndZstd,
        )?
    };
    ingest_probed_compression_input(probed, path, source_context, archive_set, stream_options)
}

fn probe_network_input(
    network: &CliNetworkContext,
    source_url: &str,
) -> CliResult<ProbedStructuredInput<'static>> {
    let source = network.open(source_url)?;
    Ok(probe_structured_input(
        source,
        LOCAL_INPUT_LIMITS,
        InputCompressionPolicy::GzipAndZstd,
    )?)
}

fn ingest_probed_compression_input<S, C>(
    mut input: ProbedStructuredInput<'_>,
    source_path: &Path,
    source_context: ArchiveSourceContext,
    archive_set: &mut ArchiveSetWriter<S, C>,
    stream_options: StructuredStreamOptions<'_>,
) -> CliResult<()>
where
    S: FinalizedArchiveSink + 'static,
    C: ArchiveSetStatsCallback + 'static,
    S::Error: Error + 'static,
    C::Error: Error + 'static, {
    let kind = input.kind();
    if matches!(
        kind,
        StructuredInputKind::Json | StructuredInputKind::KvIr(_) | StructuredInputKind::Empty
    ) {
        let stats = ingest_structured_stream(
            &mut input,
            kind,
            source_context,
            archive_set,
            stream_options,
        )?;
        warn_truncated_json(source_path, stats.truncated_json_bytes());
        return Ok(());
    }

    let raw_fallback_name = source_context.canonical_filename().as_bytes().to_vec();
    let archive_creator_id = source_context.archive_creator_id().to_owned();
    let container_limits = ContainerLimits::default()
        .with_max_input_bytes(u64::MAX)
        .with_max_entry_decoded_bytes(u64::MAX)
        .with_max_total_decoded_bytes(u64::MAX)
        .with_max_entries(u64::MAX)
        .with_max_path_bytes(u64::MAX)
        .with_max_sparse_gap_bytes(u64::MAX)
        .with_max_filter_layers(u64::MAX);
    let options = ContainerArchiveOptions::new(
        ContainerOptions::new(FormatPolicy::CppCompatible).with_limits(container_limits),
    )
    .with_member_input_limits(LOCAL_INPUT_LIMITS)
    .with_stream_options(stream_options);
    let outcome = ingest_container_archive_set(
        input,
        &raw_fallback_name,
        archive_set,
        options,
        |metadata| -> io::Result<ArchiveSourceContext> {
            let filename = std::str::from_utf8(metadata.path()).map_err(|source| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("container member path is not valid UTF-8: {source}"),
                )
            })?;
            Ok(ArchiveSourceContext::new(
                filename,
                archive_creator_id.clone(),
            ))
        },
    )?;
    warn_truncated_json(source_path, outcome.truncated_json_bytes());
    Ok(())
}

fn warn_truncated_json(path: &Path, bytes: u64) {
    if 0 < bytes {
        eprintln!(
            "clp-s: warning: ignored {bytes} truncated JSON bytes at end of input '{}'",
            diagnostic_source_name(path)
        );
    }
}

#[derive(Debug, Default)]
struct PublishedArchiveIds {
    ids: RefCell<VecDeque<(u64, String)>>,
}

#[derive(Debug)]
struct CompressionArchivePublisher {
    archives_dir: PathBuf,
    single_file_archive: bool,
    published_ids: Rc<PublishedArchiveIds>,
}

impl CompressionArchivePublisher {
    fn new(
        archives_dir: &Path,
        single_file_archive: bool,
        published_ids: Rc<PublishedArchiveIds>,
    ) -> Self {
        Self {
            archives_dir: archives_dir.to_path_buf(),
            single_file_archive,
            published_ids,
        }
    }

    fn publish_directory(&self, archive: &ArchiveSetArchive, archive_id: &str) -> io::Result<()> {
        let target = self.archives_dir.join(archive_id);
        let staging = self.archives_dir.join(format!("{archive_id}.tmp"));
        archive
            .encoded()
            .write_to(FsDirectoryArchiveSink::new(target, staging))
            .map(|_| ())
            .map_err(io::Error::other)
    }

    fn publish_sfa(&self, archive: &ArchiveSetArchive, archive_id: &str) -> io::Result<()> {
        let target = self.archives_dir.join(archive_id);
        let staging = self.archives_dir.join(format!("{archive_id}.tmp"));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        archive.write_sfa(&mut output)?;
        drop(output);
        fs::rename(staging, target)
    }
}

impl FinalizedArchiveSink for CompressionArchivePublisher {
    type Error = io::Error;

    fn publish(&mut self, archive: &ArchiveSetArchive) -> Result<(), Self::Error> {
        let archive_id = Uuid::new_v4().to_string();
        if self.single_file_archive {
            self.publish_sfa(archive, &archive_id)?;
        } else {
            self.publish_directory(archive, &archive_id)?;
        }
        self.published_ids
            .ids
            .try_borrow_mut()
            .map_err(|_| io::Error::other("archive identity queue is already borrowed"))?
            .push_back((archive.stats().archive_index(), archive_id));
        Ok(())
    }
}

#[derive(Debug)]
struct CompressionArchiveStats {
    print: bool,
    published_ids: Rc<PublishedArchiveIds>,
}

impl CompressionArchiveStats {
    const fn new(print: bool, published_ids: Rc<PublishedArchiveIds>) -> Self {
        Self {
            print,
            published_ids,
        }
    }
}

impl ArchiveSetStatsCallback for CompressionArchiveStats {
    type Error = io::Error;

    fn on_archive(&mut self, stats: ArchiveSetStats) -> Result<(), Self::Error> {
        let (archive_index, archive_id) = self
            .published_ids
            .ids
            .try_borrow_mut()
            .map_err(|_| io::Error::other("archive identity queue is already borrowed"))?
            .pop_front()
            .ok_or_else(|| io::Error::other("published archive has no generated identity"))?;
        if archive_index != stats.archive_index() {
            return Err(io::Error::other(
                "published archive identity is out of order",
            ));
        }
        if self.print {
            print_archive_stats(&archive_id, &stats)?;
        }
        Ok(())
    }
}

fn print_archive_stats(archive_id: &str, stats: &ArchiveSetStats) -> io::Result<()> {
    let range_index = serde_json::to_string(stats.range_index()).map_err(io::Error::other)?;
    let message = format!(
        "{{\"begin_timestamp\":{},\"end_timestamp\":{},\"id\":\"{archive_id}\",\"is_split\":{},\"\
         range_index\":{range_index},\"size\":{},\"uncompressed_size\":{}}}\n",
        stats.begin_timestamp(),
        stats.end_timestamp(),
        stats.is_split(),
        stats.compressed_size(),
        stats.uncompressed_size(),
    );
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()
}

fn run_search(arguments: &SearchArgs) -> CliResult<()> {
    validate_search_arguments(arguments)?;
    let (query, output_handler) = resolve_search_query_and_output(arguments)?;
    let aggregation = search_aggregation_plan(arguments)?;
    validate_aggregation_output(aggregation.as_ref(), output_handler)?;
    let query = parse_kql(query, KqlLimits::default())?;
    let search_options = SearchOptions::new(arguments.ignore_case, SearchLimits::default())
        .with_authoritative_timestamp_range(AuthoritativeTimestampRange::new(
            arguments.tge,
            arguments.tle,
        ));
    let archive_options = ArchiveSearchOptions::default().with_search(search_options);
    let mut archive_paths =
        discover_archive_paths(&arguments.archive_path, arguments.archive_id.as_deref())?;
    archive_paths.sort();
    let source_network = archive_paths
        .iter()
        .any(|path| looks_like_network_path(path))
        .then(|| CliNetworkContext::new(arguments.auth));

    if let Some(plan) = aggregation.as_ref() {
        return run_configured_aggregation_search(
            arguments,
            output_handler,
            &archive_paths,
            source_network.as_ref(),
            &query,
            &archive_options,
            plan,
        );
    }

    let projection = if arguments.projection.is_empty() {
        Projection::all()
    } else {
        Projection::selected(&arguments.projection, ProjectionLimits::default())?
    };
    match output_handler {
        SearchOutput::File => {
            warn_for_unsupported_direct_kv_ir_search(&archive_paths);
            let path = arguments
                .path
                .as_deref()
                .ok_or_else(|| invalid_input("The file output handler requires --path."))?;
            let options = SearchMsgpackOptions::new(projection)
                .with_byte_policy(JsonBytePolicy::PreserveInvalidUtf8);
            return run_file_search(
                &archive_paths,
                source_network.as_ref(),
                path,
                &query,
                &archive_options,
                &options,
            );
        }
        SearchOutput::Network => {
            warn_for_unsupported_direct_kv_ir_search(&archive_paths);
            let host = arguments
                .host
                .as_deref()
                .ok_or_else(|| invalid_input("host must be specified."))?;
            let port = arguments
                .port
                .ok_or_else(|| invalid_input("port must be specified."))?;
            let options = SearchMsgpackOptions::new(projection)
                .with_byte_policy(JsonBytePolicy::PreserveInvalidUtf8)
                .with_result_metadata(false);
            return run_network_search(
                &archive_paths,
                source_network.as_ref(),
                host,
                port,
                &query,
                &archive_options,
                &options,
            );
        }
        SearchOutput::Reducer => {
            return Err(invalid_input(
                "The reducer output handler currently only supports count and count-by-time \
                 aggregations.",
            ));
        }
        SearchOutput::ResultsCache => {
            return run_configured_results_cache_search(
                arguments,
                &archive_paths,
                source_network.as_ref(),
                &query,
                &archive_options,
                projection,
            );
        }
        SearchOutput::Stdout => {}
    }

    run_configured_stdout_search(
        arguments,
        &archive_paths,
        source_network.as_ref(),
        &query,
        &archive_options,
        projection,
    )
}

fn run_configured_stdout_search(
    arguments: &SearchArgs,
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    projection: Projection,
) -> CliResult<()> {
    if !arguments.projection.is_empty() {
        warn_for_unsupported_direct_kv_ir_search(archive_paths);
    }
    run_stdout_search(
        archive_paths,
        source_network,
        query,
        CliStdoutSearchOptions {
            archive: archive_options,
            projection,
            ignore_case: arguments.ignore_case,
            has_timestamp_filter: arguments.tge.is_some() || arguments.tle.is_some(),
            direct_search_supported: arguments.projection.is_empty(),
        },
    )
}

fn run_configured_aggregation_search(
    arguments: &SearchArgs,
    output: SearchOutput,
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    plan: &AggregationPlan,
) -> CliResult<()> {
    warn_for_unsupported_direct_kv_ir_search(archive_paths);
    match output {
        SearchOutput::Stdout => {
            run_aggregation_search(archive_paths, source_network, query, archive_options, plan)
        }
        SearchOutput::Reducer => {
            let host = arguments
                .host
                .as_deref()
                .ok_or_else(|| invalid_input("host must be specified."))?;
            let port = arguments
                .port
                .ok_or_else(|| invalid_input("port must be specified."))?;
            let job_id = arguments
                .job_id
                .ok_or_else(|| invalid_input("job-id must be specified."))?;
            run_reducer_search(
                archive_paths,
                source_network,
                host,
                port,
                job_id,
                query,
                archive_options,
                plan,
            )
        }
        SearchOutput::ResultsCache => {
            let results_cache = results_cache_cli_options(arguments)?;
            run_results_cache_aggregation_search(
                archive_paths,
                source_network,
                query,
                archive_options,
                plan,
                results_cache,
            )
        }
        SearchOutput::File | SearchOutput::Network => Err(invalid_input(
            "the selected output handler does not support aggregations",
        )),
    }
}

fn validate_aggregation_output(
    aggregation: Option<&AggregationPlan>,
    output: SearchOutput,
) -> CliResult<()> {
    match (aggregation, output) {
        (Some(_), SearchOutput::File) => Err(invalid_input(
            "The file output handler does not support aggregations.",
        )),
        (Some(_), SearchOutput::Network) => Err(invalid_input(
            "The network output handler does not support aggregations.",
        )),
        (Some(plan), SearchOutput::Reducer)
            if matches!(
                plan.kind(),
                clp_s::search::AggregationKind::Count | clp_s::search::AggregationKind::CountByTime
            ) =>
        {
            Ok(())
        }
        (None | Some(_), SearchOutput::Reducer) => Err(invalid_input(
            "The reducer output handler currently only supports count and count-by-time \
             aggregations.",
        )),
        (None | Some(_), SearchOutput::Stdout | SearchOutput::ResultsCache)
        | (None, SearchOutput::File | SearchOutput::Network) => Ok(()),
    }
}

struct CliStdoutSearchOptions<'options> {
    archive: &'options ArchiveSearchOptions,
    projection: Projection,
    ignore_case: bool,
    has_timestamp_filter: bool,
    direct_search_supported: bool,
}

fn run_stdout_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    options: CliStdoutSearchOptions<'_>,
) -> CliResult<()> {
    let jsonl_options = SearchJsonlOptions::new(options.projection)
        .with_byte_policy(JsonBytePolicy::PreserveInvalidUtf8);

    let stdout = io::stdout();
    let mut output = WriteJsonlSink(BufWriter::with_capacity(
        OUTPUT_BUFFER_CAPACITY,
        stdout.lock(),
    ));
    for archive_path in archive_paths {
        if options.direct_search_supported && is_kv_ir_search_candidate(archive_path) {
            match run_direct_kv_ir_search(
                archive_path,
                source_network,
                query,
                options.ignore_case,
                options.has_timestamp_filter,
                &mut output.0,
            )? {
                DirectKvIrSearchOutcome::Searched => continue,
                DirectKvIrSearchOutcome::FallBackToArchive => {}
            }
        }
        let result = with_archive_reader(archive_path, source_network, |archive| {
            let mut adapter = SearchJsonlAdapter::new(&mut output, &jsonl_options);
            search_archive(archive, query, &mut adapter, options.archive)
        })?;
        result?;
    }
    output.flush()?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectKvIrSearchOutcome {
    Searched,
    FallBackToArchive,
}

struct DirectKvIrInputReader {
    input: Box<dyn Read>,
    read_failed: Rc<Cell<bool>>,
}

impl Read for DirectKvIrInputReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.input.read(buffer) {
            Ok(read) => Ok(read),
            Err(error) => {
                self.read_failed.set(true);
                Err(error)
            }
        }
    }
}

fn warn_for_unsupported_direct_kv_ir_search(archive_paths: &[PathBuf]) {
    for archive_path in archive_paths {
        if is_kv_ir_search_candidate(archive_path) {
            eprintln!(
                "Attempted to search an IR stream using unsupported features. Falling back to \
                 searching the input as an archive."
            );
        }
    }
}

fn run_direct_kv_ir_search<W: Write>(
    stream_path: &Path,
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    ignore_case: bool,
    has_timestamp_filter: bool,
    output: &mut W,
) -> CliResult<DirectKvIrSearchOutcome> {
    // C++ treats raw-reader failures as fatal. Only failures while constructing the logical IR
    // deserializer may retry the same path as an archive.
    let physical_read_failed = Rc::new(Cell::new(false));
    let input: Box<dyn Read> = if looks_like_network_path(stream_path) {
        let source_url = stream_path
            .to_str()
            .ok_or_else(|| invalid_input("network input URL must be valid UTF-8"))?;
        Box::new(
            source_network
                .ok_or_else(|| io::Error::other("network input has no HTTP client"))?
                .open(source_url)?,
        )
    } else {
        Box::new(File::open(stream_path)?)
    };
    let input = DirectKvIrInputReader {
        input,
        read_failed: Rc::clone(&physical_read_failed),
    };
    if has_timestamp_filter {
        eprintln!(
            "kv-ir search: Timestamp filters are currently not supported. Values will be ignored."
        );
    }

    // This corresponds to C++ `Decompressor::open`; construction failure is not the later
    // `make_deserializer` fallback case.
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let match_sink = KvIrJsonlMatchSink::new(output, KvIrJsonlOptions::default());
    let mut searcher = KvIrSearchSink::new(
        query,
        match_sink,
        KvIrSearchOptions::new(ignore_case, KvIrSearchLimits::default()),
    )?;
    let mut reader = KvIrReader::new(decoder, KvIrOptions::default());
    let result = search_first_kv_ir_stream(&mut reader, &mut searcher);
    let parsed_streams = reader.stats().streams();
    let physical_read_failed = physical_read_failed.get();

    match result {
        Ok(_) => Ok(DirectKvIrSearchOutcome::Searched),
        Err(error) if is_cpp_tolerated_kv_ir_truncation(&error) => {
            eprintln!(
                "IR stream `{}` is truncated",
                diagnostic_source_name(stream_path)
            );
            Ok(DirectKvIrSearchOutcome::Searched)
        }
        Err(error) if physical_read_failed => Err(Box::new(error)),
        Err(error) if 0 == parsed_streams && matches!(error, KvIrReadError::Reader(_)) => {
            eprintln!(
                "Failed to create a KV-IR deserializer for '{}': {error}. Falling back to archive \
                 search.",
                diagnostic_source_name(stream_path)
            );
            Ok(DirectKvIrSearchOutcome::FallBackToArchive)
        }
        Err(error) => Err(Box::new(error)),
    }
}

fn run_file_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    output_path: &Path,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    msgpack_options: &SearchMsgpackOptions,
) -> CliResult<()> {
    for archive_path in archive_paths {
        let archive_id = archive_id_for_source(archive_path)?;
        let mut output = LazySearchFile::new(output_path);
        let mut sink = FileSearchSink {
            adapter: SearchMsgpackAdapter::new(
                &mut output,
                archive_id.as_encoded_bytes(),
                msgpack_options,
            ),
        };
        with_archive_reader(archive_path, source_network, |archive| {
            search_archive(archive, query, &mut sink, archive_options)
        })??;
        drop(sink);
        output.finish()?;
    }
    Ok(())
}

fn run_network_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    host: &str,
    port: u16,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    msgpack_options: &SearchMsgpackOptions,
) -> CliResult<()> {
    for archive_path in archive_paths {
        let result =
            with_archive_reader(archive_path, source_network, |archive| -> CliResult<()> {
                // The C++ command constructs and destroys its network handler once per archive.
                let mut output = TcpStream::connect((host, port))?;
                let mut adapter = SearchMsgpackAdapter::new(&mut output, b"", msgpack_options);
                search_archive(archive, query, &mut adapter, archive_options)?;
                output.flush()?;
                Ok(())
            })?;
        result?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ResultsCacheCliOptions<'arguments> {
    uri: &'arguments str,
    collection: &'arguments str,
    dataset: &'arguments str,
    batch_size: usize,
    max_num_results: usize,
}

fn results_cache_cli_options(arguments: &SearchArgs) -> CliResult<ResultsCacheCliOptions<'_>> {
    let uri = arguments
        .uri
        .as_deref()
        .ok_or_else(|| invalid_input("uri must be specified."))?;
    if uri.is_empty() {
        return Err(invalid_input("uri cannot be an empty string."));
    }
    let collection = arguments
        .collection
        .as_deref()
        .ok_or_else(|| invalid_input("collection must be specified."))?;
    if collection.is_empty() {
        return Err(invalid_input("collection cannot be an empty string."));
    }
    let batch_size = arguments.batch_size.unwrap_or(1000);
    if 0 == batch_size {
        return Err(invalid_input("batch-size cannot be 0."));
    }
    let max_num_results = arguments.max_num_results.unwrap_or(1000);
    if 0 == max_num_results {
        return Err(invalid_input("max-num-results cannot be 0."));
    }
    Ok(ResultsCacheCliOptions {
        uri,
        collection,
        dataset: arguments.dataset.as_deref().unwrap_or(""),
        batch_size: usize::try_from(batch_size)
            .map_err(|_| invalid_input("batch-size exceeds this platform's size limit."))?,
        max_num_results: usize::try_from(max_num_results)
            .map_err(|_| invalid_input("max-num-results exceeds this platform's size limit."))?,
    })
}

fn run_configured_results_cache_search(
    arguments: &SearchArgs,
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    projection: Projection,
) -> CliResult<()> {
    warn_for_unsupported_direct_kv_ir_search(archive_paths);
    let cli_options = results_cache_cli_options(arguments)?;
    let options = SearchResultsCacheOptions::new(
        projection,
        cli_options.batch_size,
        cli_options.max_num_results,
    )?;
    run_results_cache_search(
        archive_paths,
        source_network,
        query,
        archive_options,
        cli_options,
        &options,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_results_cache_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    cli_options: ResultsCacheCliOptions<'_>,
    options: &SearchResultsCacheOptions,
) -> CliResult<()> {
    for archive_path in archive_paths {
        let archive_id = archive_id_for_source(archive_path)?;
        let archive_id = archive_id
            .to_str()
            .ok_or_else(|| invalid_input("search archive filename must be valid UTF-8"))?;
        let result =
            with_archive_reader(archive_path, source_network, |archive| -> CliResult<()> {
                // C++ constructs one MongoDB handler per archive. Initialization remains lazy until
                // search preflight reaches ArchiveMatchSink::begin_archive.
                let mut mongo =
                    CliMongoResultsCacheSink::new(cli_options.uri, cli_options.collection);
                let mut adapter = SearchResultsCacheAdapter::new(
                    &mut mongo,
                    archive_id,
                    cli_options.dataset,
                    options,
                );
                search_archive(archive, query, &mut adapter, archive_options)?;
                adapter.finish()?;
                Ok(())
            })?;
        result?;
    }
    Ok(())
}

fn run_results_cache_aggregation_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    archive_options: &ArchiveSearchOptions,
    plan: &AggregationPlan,
    cli_options: ResultsCacheCliOptions<'_>,
) -> CliResult<()> {
    for archive_path in archive_paths {
        let archive_id = archive_id_for_source(archive_path)?;
        let archive_id = archive_id
            .to_str()
            .ok_or_else(|| invalid_input("search archive filename must be valid UTF-8"))?;
        let result =
            with_archive_reader(archive_path, source_network, |archive| -> CliResult<()> {
                let mut mongo =
                    CliMongoResultsCacheSink::new(cli_options.uri, cli_options.collection);
                let mut adapter = AggregationResultsCacheAdapter::new(
                    &mut mongo,
                    archive_id,
                    plan,
                    cli_options.batch_size,
                )?;
                search_archive(archive, query, &mut adapter, archive_options)?;
                adapter.finish()?;
                Ok(())
            })?;
        result?;
    }
    Ok(())
}

struct CliMongoResultsCacheSink<'options> {
    uri: &'options str,
    collection_name: &'options str,
    collection: Option<MongoCollection<Document>>,
}

impl<'options> CliMongoResultsCacheSink<'options> {
    const fn new(uri: &'options str, collection_name: &'options str) -> Self {
        Self {
            uri,
            collection_name,
            collection: None,
        }
    }

    fn begin_archive(&mut self) -> io::Result<()> {
        if self.collection.is_some() {
            return Ok(());
        }
        let client = MongoClient::with_uri_str(self.uri).map_err(io::Error::other)?;
        let database = client.default_database().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "results-cache MongoDB URI must include a database name",
            )
        })?;
        self.collection = Some(database.collection(self.collection_name));
        Ok(())
    }

    fn insert_documents(&self, documents: Vec<Document>) -> io::Result<()> {
        let collection = self
            .collection
            .as_ref()
            .ok_or_else(|| io::Error::other("results-cache MongoDB collection is not open"))?;
        collection
            .insert_many(documents)
            .run()
            .map(|_| ())
            .map_err(io::Error::other)
    }
}

impl SearchResultsCacheBatchSink for CliMongoResultsCacheSink<'_> {
    fn begin_archive(&mut self) -> io::Result<()> {
        Self::begin_archive(self)
    }

    fn insert_search_batch(
        &mut self,
        archive_id: &str,
        dataset: &str,
        results: Vec<ResultsCacheSearchResult>,
    ) -> io::Result<()> {
        let documents = results
            .into_iter()
            .map(|result| search_result_bson(archive_id, dataset, result))
            .collect();
        self.insert_documents(documents)
    }
}

impl AggregationResultsCacheBatchSink for CliMongoResultsCacheSink<'_> {
    fn begin_archive(&mut self) -> io::Result<()> {
        Self::begin_archive(self)
    }

    fn insert_aggregation_batch(
        &mut self,
        archive_id: &str,
        results: &[AggregationResultRef<'_>],
    ) -> io::Result<()> {
        let documents = results
            .iter()
            .copied()
            .map(|result| aggregation_result_bson(archive_id, result))
            .collect::<io::Result<Vec<_>>>()?;
        self.insert_documents(documents)
    }
}

fn search_result_bson(
    archive_id: &str,
    dataset: &str,
    result: ResultsCacheSearchResult,
) -> Document {
    let (message, timestamp, log_event_index) = result.into_parts();
    let mut document = Document::new();
    document.insert("orig_file_path", Bson::String(String::new()));
    document.insert("message", Bson::String(message));
    document.insert("timestamp", Bson::Int64(timestamp));
    document.insert("archive_id", Bson::String(archive_id.to_owned()));
    document.insert("log_event_ix", Bson::Int64(log_event_index));
    document.insert("dataset", Bson::String(dataset.to_owned()));
    document
}

fn aggregation_result_bson(
    archive_id: &str,
    result: AggregationResultRef<'_>,
) -> io::Result<Document> {
    let mut document = Document::new();
    document.insert("archive_id", Bson::String(archive_id.to_owned()));
    match result {
        AggregationResultRef::Count { count } => {
            document.insert("count", Bson::Int64(count));
        }
        AggregationResultRef::CountByTime { timestamp, count } => {
            document.insert("timestamp", Bson::Int64(timestamp));
            document.insert("count", Bson::Int64(count));
        }
        AggregationResultRef::Minimum { field, value } => {
            document.insert("field", Bson::String(field.to_owned()));
            document.insert("min", aggregation_number_bson(value)?);
        }
        AggregationResultRef::Maximum { field, value } => {
            document.insert("field", Bson::String(field.to_owned()));
            document.insert("max", aggregation_number_bson(value)?);
        }
        AggregationResultRef::Unique { field, value } => {
            document.insert("field", Bson::String(field.to_owned()));
            document.insert("value", aggregation_value_bson(value)?);
        }
        _ => return Err(unsupported_bson_aggregation_variant()),
    }
    Ok(document)
}

fn aggregation_number_bson(value: clp_s::search::AggregationNumber) -> io::Result<Bson> {
    Ok(match value {
        clp_s::search::AggregationNumber::Integer(value) => Bson::Int64(value),
        clp_s::search::AggregationNumber::Float(value) => Bson::Double(value),
        _ => return Err(unsupported_bson_aggregation_variant()),
    })
}

fn aggregation_value_bson(value: AggregationValueRef<'_>) -> io::Result<Bson> {
    Ok(match value {
        AggregationValueRef::Integer(value) => Bson::Int64(value),
        AggregationValueRef::Float(value) => Bson::Double(value),
        AggregationValueRef::String(value) => Bson::String(value.to_owned()),
        AggregationValueRef::Boolean(value) => Bson::Boolean(value),
        _ => return Err(unsupported_bson_aggregation_variant()),
    })
}

fn unsupported_bson_aggregation_variant() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "aggregation result type is not supported by the MongoDB adapter",
    )
}

fn search_aggregation_plan(arguments: &SearchArgs) -> CliResult<Option<AggregationPlan>> {
    if arguments.count {
        return Ok(Some(AggregationPlan::count()));
    }
    if let Some(bucket_size) = arguments.count_by_time {
        return Ok(Some(AggregationPlan::count_by_time(bucket_size)?));
    }
    if let Some(field) = arguments.min.as_deref() {
        return Ok(Some(AggregationPlan::minimum(field)?));
    }
    if let Some(field) = arguments.max.as_deref() {
        return Ok(Some(AggregationPlan::maximum(field)?));
    }
    if let Some(field) = arguments.unique.as_deref() {
        return Ok(Some(AggregationPlan::unique(field)?));
    }
    Ok(None)
}

fn run_aggregation_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    query: &clp_s::search::ParsedQuery,
    options: &ArchiveSearchOptions,
    plan: &AggregationPlan,
) -> CliResult<()> {
    let stdout = io::stdout();
    let mut output = BufWriter::with_capacity(OUTPUT_BUFFER_CAPACITY, stdout.lock());
    let mut document = String::new();
    for archive_path in archive_paths {
        let archive_id = archive_id_for_source(archive_path)?;
        let archive_id = archive_id
            .to_str()
            .ok_or_else(|| invalid_input("search archive filename must be valid UTF-8"))?;
        let mut sink = plan.start();
        with_archive_reader(archive_path, source_network, |archive| {
            search_archive(archive, query, &mut sink, options)
        })??;
        for result in sink.results() {
            document.clear();
            result
                .with_archive_id(archive_id)
                .append_compact_json(&mut document)?;
            output.write_all(document.as_bytes())?;
            output.write_all(b"\n")?;
        }
    }
    output.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_reducer_search(
    archive_paths: &[PathBuf],
    source_network: Option<&CliNetworkContext>,
    host: &str,
    port: u16,
    job_id: i64,
    query: &clp_s::search::ParsedQuery,
    options: &ArchiveSearchOptions,
    plan: &AggregationPlan,
) -> CliResult<()> {
    // The C++ command negotiates one reducer connection before searching any archive and shares
    // that connection across every per-archive aggregation handler.
    let stream = TcpStream::connect((host, port))?;
    let mut reducer = ReducerProtocol::handshake(stream, job_id)?;
    for archive_path in archive_paths {
        let mut sink = plan.start();
        with_archive_reader(archive_path, source_network, |archive| {
            search_archive(archive, query, &mut sink, options)
        })??;
        reducer.send_archive_results(sink.results())?;
    }
    Ok(())
}

fn validate_search_arguments(arguments: &SearchArgs) -> CliResult<()> {
    if arguments.enable_telemetry {
        return Err(unsupported_search_feature("search telemetry"));
    }
    let aggregation_count = [
        arguments.count,
        arguments.count_by_time.is_some(),
        arguments.min.is_some(),
        arguments.max.is_some(),
        arguments.unique.is_some(),
    ]
    .into_iter()
    .filter(|requested| *requested)
    .count();
    if 1 < aggregation_count {
        return Err(invalid_input(
            "--count, --count-by-time, --min, --max, and --unique are mutually exclusive",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchOutput {
    Stdout,
    File,
    Network,
    ResultsCache,
    Reducer,
}

fn resolve_search_query_and_output(arguments: &SearchArgs) -> CliResult<(&str, SearchOutput)> {
    let mut positional = arguments.positional.iter();
    let query = match arguments.query.as_deref() {
        Some(query) => query,
        None => positional
            .next()
            .ok_or_else(|| invalid_input("missing required KQL query"))?
            .to_str()
            .ok_or_else(|| invalid_input("KQL query must be valid UTF-8"))?,
    };
    if query.is_empty() {
        return Err(invalid_input("KQL query must not be empty"));
    }

    let output = if let Some(handler) = positional.next() {
        let handler = handler
            .to_str()
            .ok_or_else(|| invalid_input("output handler must be valid UTF-8"))?;
        match handler {
            "stdout" => SearchOutput::Stdout,
            "file" => SearchOutput::File,
            "network" => SearchOutput::Network,
            "results-cache" => SearchOutput::ResultsCache,
            "reducer" => SearchOutput::Reducer,
            _ => {
                return Err(invalid_input(format!(
                    "unknown search output handler '{handler}'"
                )));
            }
        }
    } else {
        SearchOutput::Stdout
    };
    if positional.next().is_some() {
        return Err(invalid_input(
            "multiple search output handlers are not supported",
        ));
    }
    match (output, arguments.path.is_some()) {
        (SearchOutput::File, false) => {
            return Err(invalid_input("The file output handler requires --path."));
        }
        (
            SearchOutput::Stdout
            | SearchOutput::Network
            | SearchOutput::ResultsCache
            | SearchOutput::Reducer,
            true,
        ) => {
            return Err(invalid_input("--path requires the file output handler."));
        }
        (
            SearchOutput::Stdout
            | SearchOutput::File
            | SearchOutput::Network
            | SearchOutput::ResultsCache
            | SearchOutput::Reducer,
            _,
        ) => {}
    }
    if arguments
        .path
        .as_deref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(invalid_input("path cannot be an empty string."));
    }
    validate_network_handler_options(arguments, output)?;
    validate_results_cache_handler_options(arguments, output)?;
    Ok((query, output))
}

fn validate_network_handler_options(arguments: &SearchArgs, output: SearchOutput) -> CliResult<()> {
    match output {
        SearchOutput::Network | SearchOutput::Reducer => {
            let host = arguments
                .host
                .as_deref()
                .ok_or_else(|| invalid_input("host must be specified."))?;
            if host.is_empty() {
                return Err(invalid_input("host cannot be an empty string."));
            }
            let port = arguments
                .port
                .ok_or_else(|| invalid_input("port must be specified."))?;
            if 0 == port {
                return Err(invalid_input("port must be greater than zero."));
            }
            if SearchOutput::Reducer == output {
                let job_id = arguments
                    .job_id
                    .ok_or_else(|| invalid_input("job-id must be specified."))?;
                if job_id < 0 {
                    return Err(invalid_input("job-id cannot be negative."));
                }
            } else if arguments.job_id.is_some() {
                return Err(invalid_input(
                    "--job-id requires the reducer output handler.",
                ));
            }
        }
        SearchOutput::Stdout | SearchOutput::File | SearchOutput::ResultsCache => {
            if arguments.host.is_some() || arguments.port.is_some() || arguments.job_id.is_some() {
                return Err(invalid_input(
                    "--host, --port, and --job-id require a network or reducer output handler.",
                ));
            }
        }
    }
    Ok(())
}

fn validate_results_cache_handler_options(
    arguments: &SearchArgs,
    output: SearchOutput,
) -> CliResult<()> {
    let has_handler_option = arguments.uri.is_some()
        || arguments.collection.is_some()
        || arguments.batch_size.is_some()
        || arguments.max_num_results.is_some()
        || arguments.dataset.is_some();
    if SearchOutput::ResultsCache == output {
        let _ = results_cache_cli_options(arguments)?;
    } else if has_handler_option {
        return Err(invalid_input(
            "--uri, --collection, --batch-size, --max-num-results, and --dataset require the \
             results-cache output handler.",
        ));
    }
    Ok(())
}

fn unsupported_search_feature(feature: &str) -> Box<dyn Error> {
    Box::new(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("Rust clp-s search {feature} is not implemented yet"),
    ))
}

struct LazySearchFile<'path> {
    path: &'path Path,
    output: Option<BufWriter<File>>,
}

impl<'path> LazySearchFile<'path> {
    const fn new(path: &'path Path) -> Self {
        Self { path, output: None }
    }

    fn begin_archive(&mut self) -> io::Result<()> {
        if self.output.is_none() {
            self.output = Some(BufWriter::with_capacity(
                OUTPUT_BUFFER_CAPACITY,
                File::create(self.path)?,
            ));
        }
        Ok(())
    }

    fn output(&mut self) -> io::Result<&mut BufWriter<File>> {
        self.begin_archive()?;
        self.output
            .as_mut()
            .ok_or_else(|| io::Error::other("search output file is not open"))
    }

    fn finish(mut self) -> io::Result<()> {
        if let Some(output) = &mut self.output {
            output.flush()?;
        }
        Ok(())
    }
}

impl Write for LazySearchFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output()?.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.as_mut().map_or(Ok(()), Write::flush)
    }
}

struct FileSearchSink<'sink, 'archive, 'options, 'path> {
    adapter: SearchMsgpackAdapter<'sink, 'archive, 'options, LazySearchFile<'path>>,
}

impl ArchiveMatchSink for FileSearchSink<'_, '_, '_, '_> {
    fn begin_archive(&mut self) -> io::Result<()> {
        self.adapter.sink_mut().begin_archive()
    }

    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
        self.adapter.write_matches(matches)
    }
}

fn run_extract(arguments: &ExtractArgs) -> CliResult<()> {
    validate_extract_arguments(arguments)?;
    let archive_paths =
        discover_archive_paths(&arguments.archive_path, arguments.archive_id.as_deref())?;
    let source_network = archive_paths
        .iter()
        .any(|path| looks_like_network_path(path))
        .then(|| CliNetworkContext::new(arguments.auth));
    create_output_directory(&arguments.output_dir)?;

    for archive_path in archive_paths {
        if arguments.ordered {
            extract_ordered_archive(&archive_path, source_network.as_ref(), arguments)?;
        } else {
            extract_unordered_archive(
                &archive_path,
                source_network.as_ref(),
                &arguments.output_dir,
            )?;
        }
    }
    Ok(())
}

fn validate_extract_arguments(arguments: &ExtractArgs) -> CliResult<()> {
    if !arguments.ordered && 0 != arguments.target_ordered_chunk_size {
        return Err(invalid_input(
            "target-ordered-chunk-size must be used with ordered argument",
        ));
    }
    if !arguments.ordered && arguments.print_ordered_chunk_stats {
        return Err(invalid_input(
            "print-ordered-chunk-stats must be used with ordered argument",
        ));
    }

    let mongodb_uri = arguments
        .mongodb_uri
        .as_deref()
        .filter(|value| !value.is_empty());
    let mongodb_collection = arguments
        .mongodb_collection
        .as_deref()
        .filter(|value| !value.is_empty());
    if mongodb_uri.is_some() != mongodb_collection.is_some() {
        return Err(invalid_input(
            "mongodb-uri and mongodb-collection must both be non-empty",
        ));
    }
    if !arguments.ordered && mongodb_uri.is_some() {
        return Err(invalid_input(
            "recording decompression metadata is only supported for ordered decompression",
        ));
    }
    if mongodb_uri.is_some() {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::Unsupported,
            "MongoDB decompression metadata is not implemented yet",
        )));
    }
    Ok(())
}

fn invalid_input(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

fn looks_like_network_path(path: &Path) -> bool {
    if path.try_exists().unwrap_or(false) {
        return false;
    }
    path.to_str().is_some_and(|value| {
        has_url_scheme(value, "http")
            || has_url_scheme(value, "https")
            || has_url_scheme(value, "s3")
    })
}

fn has_url_scheme(value: &str, expected: &str) -> bool {
    value.split_once(':').is_some_and(|(scheme, remainder)| {
        scheme.eq_ignore_ascii_case(expected) && remainder.starts_with("//")
    })
}

fn diagnostic_source_name(path: &Path) -> String {
    if !looks_like_network_path(path) {
        return path.to_string_lossy().into_owned();
    }
    let Some(source_url) = path.to_str() else {
        return "[invalid network URL]".to_owned();
    };
    let Ok(mut url) = Url::parse(source_url) else {
        return "[invalid network URL]".to_owned();
    };
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

fn discover_archive_paths(
    archive_path: &Path,
    archive_id: Option<&OsStr>,
) -> CliResult<Vec<PathBuf>> {
    if looks_like_network_path(archive_path) {
        if archive_id.is_some_and(|value| !value.is_empty()) {
            return Err(invalid_input("requested archive does not exist"));
        }
        return Ok(vec![archive_path.to_path_buf()]);
    }
    if let Some(archive_id) = archive_id.filter(|value| !value.is_empty()) {
        let requested_path = archive_path.join(archive_id);
        if !requested_path.try_exists()? {
            return Err(invalid_input("requested archive does not exist"));
        }
        return Ok(vec![requested_path]);
    }

    let metadata = match fs::metadata(archive_path) {
        Ok(metadata) => metadata,
        Err(error) if io::ErrorKind::NotFound == error.kind() => {
            return Ok(vec![archive_path.to_path_buf()]);
        }
        Err(error) => return Err(Box::new(error)),
    };
    if !metadata.is_dir() {
        return Ok(vec![archive_path.to_path_buf()]);
    }
    if is_directory_archive(archive_path)? {
        return Ok(vec![archive_path.to_path_buf()]);
    }

    let mut archive_paths = Vec::new();
    for entry in fs::read_dir(archive_path)? {
        archive_paths.push(entry?.path());
    }
    if archive_paths.is_empty() {
        return Err(invalid_input("no archive paths specified"));
    }
    Ok(archive_paths)
}

fn archive_id_for_source(archive_path: &Path) -> CliResult<OsString> {
    if !looks_like_network_path(archive_path) {
        return archive_path
            .file_name()
            .filter(|name| !name.is_empty())
            .map(OsStr::to_os_string)
            .ok_or_else(|| invalid_input("archive path has no archive ID"));
    }

    Ok(remote_archive_id_parts(archive_path)?.decoded)
}

fn archive_file_id_for_source(archive_path: &Path) -> CliResult<OsString> {
    if !looks_like_network_path(archive_path) {
        return archive_id_for_source(archive_path);
    }
    let RemoteArchiveId { encoded, decoded } = remote_archive_id_parts(archive_path)?;
    if is_safe_file_component(&decoded) {
        return Ok(decoded);
    }
    let encoded = OsString::from(encoded);
    if is_safe_file_component(&encoded) {
        return Ok(encoded);
    }
    Ok(OsString::from("remote-archive"))
}

struct RemoteArchiveId {
    encoded: String,
    decoded: OsString,
}

fn remote_archive_id_parts(archive_path: &Path) -> CliResult<RemoteArchiveId> {
    let source_url = archive_path
        .to_str()
        .ok_or_else(|| invalid_input("network archive URL must be valid UTF-8"))?;
    Url::parse(source_url).map_err(|_| invalid_input("invalid network archive URL"))?;

    let authority_start = source_url
        .find("//")
        .map(|index| index + 2)
        .ok_or_else(|| invalid_input("invalid network archive URL"))?;
    let suffix = &source_url[authority_start..];
    let authority_and_path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    let authority_and_path = &suffix[..authority_and_path_end];
    let Some(path_start) = authority_and_path.find('/') else {
        return Err(invalid_input("network archive URL has no archive ID"));
    };
    let encoded_path = &authority_and_path[path_start + 1..];
    if encoded_path.is_empty() {
        return Err(invalid_input("network archive URL has no archive ID"));
    }
    for segment in encoded_path.split('/') {
        let decoded = percent_decode_uri_segment(segment)?;
        if "." == decoded || ".." == decoded {
            return Err(invalid_input(
                "network archive URL contains a path-normalizing dot segment",
            ));
        }
    }
    let encoded = encoded_path
        .rsplit('/')
        .next()
        .ok_or_else(|| invalid_input("network archive URL has no archive ID"))?;
    let decoded = percent_decode_uri_segment(encoded)?;
    Ok(RemoteArchiveId {
        encoded: encoded.to_owned(),
        decoded: OsString::from(decoded),
    })
}

fn percent_decode_uri_segment(encoded: &str) -> CliResult<String> {
    let source = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if b'%' != source[index] {
            decoded.push(source[index]);
            index += 1;
            continue;
        }
        let Some(encoded_byte) = source.get(index + 1..index + 3) else {
            return Err(invalid_input(
                "network archive URL has invalid percent encoding",
            ));
        };
        let high = hex_digit(encoded_byte[0])
            .ok_or_else(|| invalid_input("network archive URL has invalid percent encoding"))?;
        let low = hex_digit(encoded_byte[1])
            .ok_or_else(|| invalid_input("network archive URL has invalid percent encoding"))?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| invalid_input("network archive ID is not valid UTF-8"))
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_safe_file_component(value: &OsStr) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn is_directory_archive(path: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.metadata()?.is_dir() || !is_directory_archive_member_name(&entry.file_name()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_directory_archive_member_name(name: &OsStr) -> bool {
    if DirectoryArchiveMember::ALL
        .iter()
        .any(|member| OsStr::new(member.file_name()) == name)
    {
        return true;
    }

    let Some(name) = name.to_str() else {
        return false;
    };
    !name.is_empty()
        && name.bytes().all(|byte| byte.is_ascii_digit())
        && name.parse::<u64>().is_ok()
}

fn create_output_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if io::ErrorKind::AlreadyExists == error.kind() && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

fn extract_unordered_archive(
    archive_path: &Path,
    source_network: Option<&CliNetworkContext>,
    output_dir: &Path,
) -> CliResult<()> {
    let mut sink = UnorderedFileSink::new(output_dir.join("original"));
    let extraction = with_archive_reader(archive_path, source_network, |archive| {
        extract_jsonl_records(
            archive,
            &mut sink,
            extraction_options(ExtractionMode::Unordered),
        )
    });
    match extraction {
        Ok(Ok(_)) => sink.finish().map_err(Into::into),
        Ok(Err(error)) => Err(Box::new(error)),
        Err(error) => Err(error),
    }
}

fn extract_ordered_archive(
    archive_path: &Path,
    source_network: Option<&CliNetworkContext>,
    arguments: &ExtractArgs,
) -> CliResult<()> {
    let archive_id = archive_file_id_for_source(archive_path)?;
    let mut sink = OrderedFileSink::new(
        &arguments.output_dir,
        &archive_id,
        arguments.target_ordered_chunk_size,
        arguments.print_ordered_chunk_stats,
    );
    let extraction = with_archive_reader(archive_path, source_network, |archive| {
        extract_jsonl_records(
            archive,
            &mut sink,
            extraction_options(ExtractionMode::LogOrder),
        )
    });

    match extraction {
        Ok(Ok(_)) => sink.finish().map_err(Into::into),
        Ok(Err(ExtractionError::MissingLogOrderColumn)) => {
            sink.abort()?;
            eprintln!(
                "clp-s: archive '{}' has no log-order metadata; falling back to physical order",
                diagnostic_source_name(archive_path)
            );
            extract_unordered_archive(archive_path, source_network, &arguments.output_dir)
        }
        Ok(Err(error)) => {
            drop(sink.abort());
            Err(Box::new(error))
        }
        Err(error) => {
            drop(sink.abort());
            Err(error)
        }
    }
}

fn extraction_options(mode: ExtractionMode) -> ExtractionOptions {
    ExtractionOptions::new(mode).with_byte_policy(JsonBytePolicy::PreserveInvalidUtf8)
}

fn with_archive_reader<T>(
    archive_path: &Path,
    source_network: Option<&CliNetworkContext>,
    operation: impl FnOnce(&mut dyn ArchiveReader) -> T,
) -> CliResult<T> {
    if looks_like_network_path(archive_path) {
        let source_url = archive_path
            .to_str()
            .ok_or_else(|| invalid_input("network archive URL must be valid UTF-8"))?;
        let source = source_network
            .ok_or_else(|| io::Error::other("network archive has no HTTP client"))?
            .open(source_url)?;
        let content_length = source.content_length();
        let source = ForwardSeekReader::new(source);
        let mut archive = SingleFileArchiveReader::open_streaming(source, content_length)?;
        Ok(operation(&mut archive))
    } else if archive_path.is_dir() {
        let source = FsDirectoryArchiveSource::new(archive_path);
        let mut archive = DirectoryArchiveReader::open(source, MetadataLimits::default())?;
        Ok(operation(&mut archive))
    } else {
        let source = File::open(archive_path)?;
        let mut archive = SingleFileArchiveReader::open(source)?;
        Ok(operation(&mut archive))
    }
}

struct WriteJsonlSink<W>(W);

impl<W: Write> WriteJsonlSink<W> {
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<W: Write> JsonlRecordSink for WriteJsonlSink<W> {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.0.write_all(record.jsonl_bytes())
    }
}

struct UnorderedFileSink {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
}

impl UnorderedFileSink {
    const fn new(path: PathBuf) -> Self {
        Self { path, writer: None }
    }

    fn writer(&mut self) -> io::Result<&mut BufWriter<File>> {
        if self.writer.is_none() {
            let output = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            self.writer = Some(BufWriter::with_capacity(OUTPUT_BUFFER_CAPACITY, output));
        }
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("unordered output file is not open"))
    }

    fn finish(mut self) -> io::Result<()> {
        self.writer()?.flush()
    }
}

impl JsonlRecordSink for UnorderedFileSink {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.writer()?.write_all(record.jsonl_bytes())
    }
}

struct OrderedFileSink {
    output_dir: PathBuf,
    archive_id: OsString,
    temporary_path: PathBuf,
    target_chunk_size: u64,
    print_chunk_stats: bool,
    active: Option<ActiveChunk>,
}

struct ActiveChunk {
    writer: BufWriter<File>,
    first_index: Option<u64>,
    last_index_exclusive: u64,
    bytes: u64,
}

impl OrderedFileSink {
    fn new(
        output_dir: &Path,
        archive_id: &OsStr,
        target_chunk_size: u64,
        print_chunk_stats: bool,
    ) -> Self {
        let temporary_path = output_dir.join(archive_id);
        Self {
            output_dir: output_dir.to_path_buf(),
            archive_id: archive_id.to_os_string(),
            temporary_path,
            target_chunk_size,
            print_chunk_stats,
            active: None,
        }
    }

    fn open_chunk(path: &Path) -> io::Result<ActiveChunk> {
        Ok(ActiveChunk {
            writer: BufWriter::with_capacity(OUTPUT_BUFFER_CAPACITY, File::create(path)?),
            first_index: None,
            last_index_exclusive: 0,
            bytes: 0,
        })
    }

    fn finish(mut self) -> io::Result<()> {
        if self.active.is_some() {
            self.finalize_active()?;
        }
        Ok(())
    }

    fn abort(mut self) -> io::Result<()> {
        if self.active.is_none() {
            return Ok(());
        }
        self.active.take();
        match fs::remove_file(&self.temporary_path) {
            Ok(()) => Ok(()),
            Err(error) if io::ErrorKind::NotFound == error.kind() => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn finalize_active(&mut self) -> io::Result<()> {
        let mut active = self
            .active
            .take()
            .ok_or_else(|| io::Error::other("ordered output chunk is not open"))?;
        let first_index = active.first_index.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "ordered output chunk is empty")
        })?;
        active.writer.flush()?;
        let last_index_exclusive = active.last_index_exclusive;
        drop(active.writer);

        let final_path = self.final_path(first_index, last_index_exclusive);
        fs::rename(&self.temporary_path, &final_path)?;
        if self.print_chunk_stats {
            print_chunk_stat(&final_path)?;
        }
        Ok(())
    }

    fn final_path(&self, first_index: u64, last_index_exclusive: u64) -> PathBuf {
        let mut file_name = self.archive_id.clone();
        file_name.push(format!("_{first_index}_{last_index_exclusive}.jsonl"));
        self.output_dir.join(file_name)
    }
}

impl JsonlRecordSink for OrderedFileSink {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        let index = record.log_event_idx().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ordered extraction record has no log-event index",
            )
        })?;
        let last_index_exclusive = index.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "log-event index overflow")
        })?;
        let record_bytes = u64::try_from(record.jsonl_bytes().len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record size overflow"))?;

        if self.active.is_none() {
            self.active = Some(Self::open_chunk(&self.temporary_path)?);
        }
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| io::Error::other("ordered output chunk is not open"))?;
        if let Some(previous_end) = active.first_index.map(|_| active.last_index_exclusive) {
            if index != previous_end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("ordered record index {index} follows {previous_end}"),
                ));
            }
        } else {
            active.first_index = Some(index);
        }
        let new_size = active
            .bytes
            .checked_add(record_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "chunk size overflow"))?;
        active.writer.write_all(record.jsonl_bytes())?;
        active.last_index_exclusive = last_index_exclusive;
        active.bytes = new_size;

        if 0 != self.target_chunk_size && new_size >= self.target_chunk_size {
            self.finalize_active()?;
        }
        Ok(())
    }
}

fn print_chunk_stat(path: &Path) -> io::Result<()> {
    let mut message = String::from("{\"path\":");
    append_json_string(
        path.to_string_lossy().as_ref(),
        &mut message,
        JsonEscapeLimits::default(),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    message.push_str("}\n");

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use clp_s::search::AggregationNumber;

    use super::*;

    fn keys(document: &Document) -> Vec<&str> {
        document.iter().map(|(key, _)| key.as_str()).collect()
    }

    #[test]
    fn ordinary_results_cache_bson_matches_cpp_field_order_and_types() {
        let result = ResultsCacheSearchResult::new("{\"message\":\"ok\"}\n".to_owned(), -7, 42);
        let document = search_result_bson("archive", "dataset", result.clone());
        assert_eq!(
            vec![
                "orig_file_path",
                "message",
                "timestamp",
                "archive_id",
                "log_event_ix",
                "dataset",
            ],
            keys(&document)
        );
        assert_eq!(Some(""), document.get_str("orig_file_path").ok());
        assert_eq!(Some(result.message()), document.get_str("message").ok());
        assert_eq!(Some(-7), document.get_i64("timestamp").ok());
        assert_eq!(Some("archive"), document.get_str("archive_id").ok());
        assert_eq!(Some(42), document.get_i64("log_event_ix").ok());
        assert_eq!(Some("dataset"), document.get_str("dataset").ok());
    }

    #[test]
    fn aggregation_results_cache_bson_matches_cpp_order_and_scalar_types() {
        let count = aggregation_result_bson("archive", AggregationResultRef::Count { count: 5 })
            .expect("supported count result");
        assert_eq!(vec!["archive_id", "count"], keys(&count));
        assert_eq!(Some(5), count.get_i64("count").ok());

        let by_time = aggregation_result_bson(
            "archive",
            AggregationResultRef::CountByTime {
                timestamp: -1000,
                count: 2,
            },
        )
        .expect("supported count-by-time result");
        assert_eq!(vec!["archive_id", "timestamp", "count"], keys(&by_time));

        let minimum = aggregation_result_bson(
            "archive",
            AggregationResultRef::Minimum {
                field: "value",
                value: AggregationNumber::Float(-0.0),
            },
        )
        .expect("supported minimum result");
        assert_eq!(vec!["archive_id", "field", "min"], keys(&minimum));
        assert_eq!(Some(-0.0), minimum.get_f64("min").ok());

        let maximum = aggregation_result_bson(
            "archive",
            AggregationResultRef::Maximum {
                field: "value",
                value: AggregationNumber::Integer(i64::MAX),
            },
        )
        .expect("supported maximum result");
        assert_eq!(vec!["archive_id", "field", "max"], keys(&maximum));
        assert_eq!(Some(i64::MAX), maximum.get_i64("max").ok());

        let unique = aggregation_result_bson(
            "archive",
            AggregationResultRef::Unique {
                field: "value",
                value: AggregationValueRef::String("text"),
            },
        )
        .expect("supported unique result");
        assert_eq!(vec!["archive_id", "field", "value"], keys(&unique));
        assert_eq!(Some("text"), unique.get_str("value").ok());
    }
}
