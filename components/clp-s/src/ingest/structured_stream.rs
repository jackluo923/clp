//! Streaming classification and archive-set ingestion for one structured input.
//!
//! Classification mirrors the pinned C++ probe order after wrapper decoding: KV-IR, JSON,
//! potentially truncated UTF-8 log text, then unknown. The probe retains at most 64 KiB and
//! replays every byte to the selected parser.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;

use super::DecodedInput;
use super::InputCompression;
use super::InputCompressionPolicy;
use super::InputError;
use super::InputLimits;
use super::JsonArchiveOptions;
use super::JsonArchiveSetAppendError;
use super::JsonArchiveSetError;
use super::JsonArchiveSetSink;
use super::JsonTimestampResolver;
use super::KvIrArchiveError;
use super::KvIrArchiveOptions;
use super::KvIrArchiveSetSink;
use super::KvIrEncoding;
use super::KvIrOptions;
use super::KvIrReadError;
use super::KvIrReader;
use super::KvIrTimestampResolver;
use super::ParseManyOptions;
use super::ParseManyReadError;
use super::ParseManyReader;
use crate::writer::ArchiveSetStatsCallback;
use crate::writer::ArchiveSetWriter;
use crate::writer::ArchiveSourceContext;
use crate::writer::FinalizedArchiveSink;

const CPP_PROBE_CAPACITY: usize = 64 * 1024;

/// Content classification produced by [`probe_structured_input`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StructuredInputKind {
    /// A stream whose first non-whitespace probe byte is an opening object brace.
    Json,
    /// A current four- or eight-byte KV-IR stream.
    KvIr(KvIrEncoding),
    /// A stream that reached EOF without a plaintext byte.
    Empty,
    /// Valid UTF-8, permitting one incomplete final codepoint in the bounded probe.
    LogText,
    /// Binary or otherwise unrecognized input.
    Unknown,
}

impl Display for StructuredInputKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Json => "JSON",
            Self::KvIr(_) => "KV-IR",
            Self::Empty => "empty",
            Self::LogText => "unstructured log text",
            Self::Unknown => "unknown binary data",
        })
    }
}

/// A classified, recursively decoded input that replays the complete classification probe.
pub struct ProbedStructuredInput<'input> {
    kind: StructuredInputKind,
    prefix: Vec<u8>,
    prefix_offset: usize,
    decoded: DecodedInput<'input>,
}

impl ProbedStructuredInput<'_> {
    /// Returns the detected plaintext content kind.
    #[must_use]
    pub const fn kind(&self) -> StructuredInputKind {
        self.kind
    }

    /// Returns wrapper formats ordered from outermost to innermost.
    #[must_use]
    pub fn compression_layers(&self) -> &[InputCompression] {
        self.decoded.compression_layers()
    }
}

impl Read for ProbedStructuredInput<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.prefix_offset < self.prefix.len() {
            let remaining = &self.prefix[self.prefix_offset..];
            let copied = remaining.len().min(output.len());
            output[..copied].copy_from_slice(&remaining[..copied]);
            self.prefix_offset += copied;
            return Ok(copied);
        }
        self.decoded.read(output)
    }
}

/// Recursively decodes and classifies a caller-owned stream without losing probe bytes.
///
/// The compression policy is explicit so a caller can select the Rust gzip/zstd superset or the
/// pinned C++ zstd-only wrapper behavior. At most 64 KiB of plaintext is retained.
///
/// # Errors
///
/// Returns a typed source, decoder, limit, or accounting failure from [`DecodedInput`].
pub fn probe_structured_input<'input, R>(
    input: R,
    limits: InputLimits,
    compression_policy: InputCompressionPolicy,
) -> Result<ProbedStructuredInput<'input>, InputError>
where
    R: Read + 'input, {
    let mut decoded = DecodedInput::with_compression_policy(input, limits, compression_policy)?;
    let mut prefix = Vec::new();
    prefix
        .try_reserve_exact(CPP_PROBE_CAPACITY)
        .map_err(|_| InputError::AllocationFailed {
            requested_additional: CPP_PROBE_CAPACITY,
        })?;
    let mut buffer = [0_u8; 16 * 1024];
    while prefix.len() < CPP_PROBE_CAPACITY {
        let capacity = (CPP_PROBE_CAPACITY - prefix.len()).min(buffer.len());
        let read = decoded.read_typed(&mut buffer[..capacity])?;
        if 0 == read {
            break;
        }
        prefix.extend_from_slice(&buffer[..read]);
    }
    let kind = classify_prefix(&prefix);
    Ok(ProbedStructuredInput {
        kind,
        prefix,
        prefix_offset: 0,
        decoded,
    })
}

fn classify_prefix(prefix: &[u8]) -> StructuredInputKind {
    if prefix.is_empty() {
        return StructuredInputKind::Empty;
    }
    if let Some(encoding) = KvIrEncoding::from_magic_number(prefix.get(..4).unwrap_or(prefix)) {
        return StructuredInputKind::KvIr(encoding);
    }
    if prefix
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'{')
    {
        return StructuredInputKind::Json;
    }
    if could_be_truncated_utf8(prefix) {
        StructuredInputKind::LogText
    } else {
        StructuredInputKind::Unknown
    }
}

const fn could_be_truncated_utf8(prefix: &[u8]) -> bool {
    match std::str::from_utf8(prefix) {
        Ok(_) => true,
        Err(source) => source.error_len().is_none() && 0 < source.valid_up_to(),
    }
}

/// Parser and archive-adapter options shared by direct and container-member ingestion.
#[derive(Clone, Copy)]
pub struct StructuredStreamOptions<'resolver> {
    parse_many: ParseManyOptions,
    json_archive: JsonArchiveOptions,
    kv_ir_reader: KvIrOptions,
    kv_ir_archive: KvIrArchiveOptions,
    json_timestamp: Option<&'resolver JsonTimestampResolver>,
    kv_ir_timestamp: Option<&'resolver KvIrTimestampResolver>,
}

impl StructuredStreamOptions<'_> {
    /// Creates safe, bounded parser and adapter defaults without timestamp resolution.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            parse_many: ParseManyOptions::new(),
            json_archive: JsonArchiveOptions::new(),
            kv_ir_reader: KvIrOptions::new(),
            kv_ir_archive: KvIrArchiveOptions::new(),
            json_timestamp: None,
            kv_ir_timestamp: None,
        }
    }

    /// Replaces JSON framing limits and behavior.
    #[must_use]
    pub const fn with_parse_many(mut self, options: ParseManyOptions) -> Self {
        self.parse_many = options;
        self
    }

    /// Replaces JSON-to-archive conversion behavior.
    #[must_use]
    pub const fn with_json_archive(mut self, options: JsonArchiveOptions) -> Self {
        self.json_archive = options;
        self
    }

    /// Replaces KV-IR reader behavior.
    #[must_use]
    pub const fn with_kv_ir_reader(mut self, options: KvIrOptions) -> Self {
        self.kv_ir_reader = options;
        self
    }

    /// Replaces KV-IR-to-archive conversion behavior.
    #[must_use]
    pub const fn with_kv_ir_archive(mut self, options: KvIrArchiveOptions) -> Self {
        self.kv_ir_archive = options;
        self
    }

    /// Selects optional authoritative timestamp resolvers for both input encodings.
    #[must_use]
    pub const fn with_timestamp_resolvers<'new_resolver>(
        self,
        json: Option<&'new_resolver JsonTimestampResolver>,
        kv_ir: Option<&'new_resolver KvIrTimestampResolver>,
    ) -> StructuredStreamOptions<'new_resolver> {
        StructuredStreamOptions {
            parse_many: self.parse_many,
            json_archive: self.json_archive,
            kv_ir_reader: self.kv_ir_reader,
            kv_ir_archive: self.kv_ir_archive,
            json_timestamp: json,
            kv_ir_timestamp: kv_ir,
        }
    }
}

impl Default for StructuredStreamOptions<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Successfully consumed plaintext counters for one structured stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StructuredStreamStats {
    input_bytes: u64,
    records: u64,
    truncated_json_bytes: u64,
}

impl StructuredStreamStats {
    const fn new(input_bytes: u64, records: u64, truncated_json_bytes: u64) -> Self {
        Self {
            input_bytes,
            records,
            truncated_json_bytes,
        }
    }

    /// Returns final plaintext bytes charged to the archive source.
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input_bytes
    }

    /// Returns complete JSON objects or KV-IR log events committed to the archive set.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }

    /// Returns bytes in an ignored incomplete final JSON object.
    #[must_use]
    pub const fn truncated_json_bytes(self) -> u64 {
        self.truncated_json_bytes
    }
}

/// Failure while adapting one classified structured stream into an archive set.
#[non_exhaustive]
pub enum StructuredStreamError<S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback, {
    /// The supplied kind cannot be ingested by the structured writer.
    Unsupported(StructuredInputKind),
    /// Opening the JSON source context failed.
    JsonBegin(JsonArchiveSetError<S, C>),
    /// JSON framing, conversion, or archive append failed.
    JsonRead(ParseManyReadError<JsonArchiveSetAppendError<S, C>>),
    /// Charging trailing JSON bytes or closing its source context failed.
    JsonFinish(JsonArchiveSetError<S, C>),
    /// KV-IR decoding, conversion, or archive append failed.
    KvIr(KvIrReadError<KvIrArchiveError<S::Error, C::Error>>),
}

impl<S, C> fmt::Debug for StructuredStreamError<S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: Display,
    C::Error: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl<S, C> Display for StructuredStreamError<S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: Display,
    C::Error: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(kind) => write!(formatter, "cannot ingest {kind} as structured data"),
            Self::JsonBegin(source) => write!(formatter, "failed to begin JSON source: {source}"),
            Self::JsonRead(source) => Display::fmt(source, formatter),
            Self::JsonFinish(source) => write!(formatter, "failed to finish JSON source: {source}"),
            Self::KvIr(source) => Display::fmt(source, formatter),
        }
    }
}

impl<S, C> Error for StructuredStreamError<S, C>
where
    S: FinalizedArchiveSink + 'static,
    C: ArchiveSetStatsCallback + 'static,
    S::Error: Error + 'static,
    C::Error: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unsupported(_) => None,
            Self::JsonBegin(source) | Self::JsonFinish(source) => Some(source),
            Self::JsonRead(source) => Some(source),
            Self::KvIr(source) => Some(source),
        }
    }
}

/// Streams one classified JSON, KV-IR, or empty input into an archive-set session.
///
/// The function owns source-range bracketing and charges only final plaintext bytes. Container
/// headers, filters, skipped entries, and padding therefore never affect archive uncompressed-size
/// accounting.
///
/// # Errors
///
/// Returns a structured parser, conversion, source-lifecycle, or archive-set failure. Log text and
/// unknown binary data are rejected without opening a source context.
pub fn ingest_structured_stream<R, S, C>(
    input: R,
    kind: StructuredInputKind,
    source: ArchiveSourceContext,
    archive_set: &mut ArchiveSetWriter<S, C>,
    options: StructuredStreamOptions<'_>,
) -> Result<StructuredStreamStats, StructuredStreamError<S, C>>
where
    R: Read,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback, {
    match kind {
        StructuredInputKind::Json | StructuredInputKind::Empty => {
            ingest_json(input, source, archive_set, options)
        }
        StructuredInputKind::KvIr(_) => ingest_kv_ir(input, source, archive_set, options),
        StructuredInputKind::LogText | StructuredInputKind::Unknown => {
            Err(StructuredStreamError::Unsupported(kind))
        }
    }
}

fn ingest_json<R, S, C>(
    input: R,
    source: ArchiveSourceContext,
    archive_set: &mut ArchiveSetWriter<S, C>,
    options: StructuredStreamOptions<'_>,
) -> Result<StructuredStreamStats, StructuredStreamError<S, C>>
where
    R: Read,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback, {
    let mut reader = ParseManyReader::new(input, options.parse_many);
    let sink = JsonArchiveSetSink::for_source(archive_set, options.json_archive, source)
        .map_err(StructuredStreamError::JsonBegin)?;
    let stats = if let Some(resolver) = options.json_timestamp {
        let mut sink = sink.with_timestamp_resolver(resolver);
        let stats = reader
            .read_to_end(&mut sink)
            .map_err(StructuredStreamError::JsonRead)?;
        sink.finish_source(stats.input_bytes())
            .map_err(StructuredStreamError::JsonFinish)?;
        stats
    } else {
        let mut sink = sink;
        let stats = reader
            .read_to_end(&mut sink)
            .map_err(StructuredStreamError::JsonRead)?;
        sink.finish_source(stats.input_bytes())
            .map_err(StructuredStreamError::JsonFinish)?;
        stats
    };
    Ok(StructuredStreamStats::new(
        stats.input_bytes(),
        stats.documents(),
        stats.truncated_bytes(),
    ))
}

fn ingest_kv_ir<R, S, C>(
    input: R,
    source: ArchiveSourceContext,
    archive_set: &mut ArchiveSetWriter<S, C>,
    options: StructuredStreamOptions<'_>,
) -> Result<StructuredStreamStats, StructuredStreamError<S, C>>
where
    R: Read,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback, {
    let sink = KvIrArchiveSetSink::for_source(archive_set, options.kv_ir_archive, source);
    let mut reader = KvIrReader::new(input, options.kv_ir_reader);
    let stats = if let Some(resolver) = options.kv_ir_timestamp {
        let mut sink = sink.with_timestamp_resolver(resolver);
        reader
            .read_to_end(&mut sink)
            .map_err(StructuredStreamError::KvIr)?
    } else {
        let mut sink = sink;
        reader
            .read_to_end(&mut sink)
            .map_err(StructuredStreamError::KvIr)?
    };
    Ok(StructuredStreamStats::new(
        stats.input_bytes(),
        stats.log_events(),
        0,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const LIMITS: InputLimits = InputLimits::new(u64::MAX, u64::MAX, 4);

    fn kind(bytes: &[u8]) -> StructuredInputKind {
        probe_structured_input(Cursor::new(bytes), LIMITS, InputCompressionPolicy::ZstdOnly)
            .expect("probe input")
            .kind()
    }

    #[test]
    fn classifier_matches_cpp_precedence_and_truncated_utf8() {
        assert_eq!(StructuredInputKind::Empty, kind(b""));
        assert_eq!(StructuredInputKind::Json, kind(b" \n{\"x\":1}"));
        assert_eq!(StructuredInputKind::LogText, kind(b"plain log\n"));
        assert_eq!(StructuredInputKind::LogText, kind(b"ascii\xe2\x82"));
        assert_eq!(StructuredInputKind::Unknown, kind(b"\xe2\x82"));
        assert_eq!(StructuredInputKind::Unknown, kind(b"ok\xff"));
    }

    #[test]
    fn probe_replays_every_plaintext_byte() {
        let source = vec![b'x'; CPP_PROBE_CAPACITY + 17];
        let mut input = probe_structured_input(
            Cursor::new(source.clone()),
            LIMITS,
            InputCompressionPolicy::ZstdOnly,
        )
        .expect("probe input");
        let mut replayed = Vec::new();
        input.read_to_end(&mut replayed).expect("replay input");
        assert_eq!(source, replayed);
    }
}
