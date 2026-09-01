//! Bounded streaming input wrapper detection and decompression.
//!
//! [`DecodedInput`] detects gzip and standard zstd frames from their exact magic bytes, never from
//! a filename extension. It replays the probe bytes, unwraps recursively, accepts concatenated
//! gzip members and zstd frames, and keeps the caller-provided source name unchanged. The final
//! reader therefore composes directly with [`super::ParseManyReader`] and [`super::KvIrReader`]
//! without buffering a complete input file.
//!
//! The pinned C++ binary has two unsafe compatibility quirks that this adapter deliberately does
//! not reproduce: its current libarchive path rejects nonempty raw gzip-wrapped JSON, while its
//! zstd reader can accept an incomplete frame if the decoded prefix happens to be valid input.
//! This implementation supports raw gzip directly and requires every gzip member or zstd frame to
//! terminate cleanly. Callers must drive a nonempty read through EOF to validate checksums, frame
//! termination, and trailing bytes; empty reads or dropping a streaming decoder early necessarily
//! skip that validation.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use flate2::read::MultiGzDecoder;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const PROBE_BYTES: usize = ZSTD_MAGIC.len();
const MEBIBYTE: u64 = 1024 * 1024;
const GIBIBYTE: u64 = 1024 * MEBIBYTE;

type BoxedReader<'input> = Box<dyn Read + 'input>;

/// Compression wrappers that [`DecodedInput`] may unwrap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum InputCompressionPolicy {
    /// Detect gzip and zstd wrappers recursively.
    #[default]
    GzipAndZstd,
    /// Detect only zstd wrappers, matching the pinned C++ structured-input probe.
    ZstdOnly,
}

/// Compression format discovered at one input layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InputCompression {
    /// An RFC 1952 gzip stream, including concatenated members.
    Gzip,
    /// One or more concatenated standard zstd frames.
    Zstd,
}

impl Display for InputCompression {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        })
    }
}

/// Independent hard limits for one physical input and all recursively decoded layers.
///
/// `max_decompressed_bytes` is enforced independently on the output of every compression layer
/// and on the final plaintext stream. This prevents an outer layer from expanding into an
/// over-limit inner compressed stream even when the final plaintext would be small.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputLimits {
    compressed_bytes: u64,
    decompressed_bytes: u64,
    layers: u64,
}

impl InputLimits {
    /// Safety-oriented defaults suitable for large local log files.
    pub const DEFAULT: Self = Self::new(64 * GIBIBYTE, 256 * GIBIBYTE, 8);

    /// Creates explicit physical, per-decoded-layer, and nesting limits.
    #[must_use]
    pub const fn new(
        max_compressed_bytes: u64,
        max_decompressed_bytes: u64,
        max_layers: u64,
    ) -> Self {
        Self {
            compressed_bytes: max_compressed_bytes,
            decompressed_bytes: max_decompressed_bytes,
            layers: max_layers,
        }
    }

    /// Maximum bytes that may be read from the caller-owned physical source.
    #[must_use]
    pub const fn max_compressed_bytes(self) -> u64 {
        self.compressed_bytes
    }

    /// Maximum bytes emitted by each compression layer and by the final plaintext stream.
    #[must_use]
    pub const fn max_decompressed_bytes(self) -> u64 {
        self.decompressed_bytes
    }

    /// Maximum recursively nested gzip and zstd layers.
    #[must_use]
    pub const fn max_layers(self) -> u64 {
        self.layers
    }
}

impl Default for InputLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Resource guarded by an [`InputLimits`] boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InputLimitResource {
    /// Bytes pulled from the physical caller-owned source.
    CompressedBytes,
    /// Bytes emitted by one compression layer or by the final raw stream.
    DecompressedBytes,
    /// Recursively nested compression wrappers.
    CompressionLayers,
}

impl Display for InputLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CompressedBytes => "physical input bytes",
            Self::DecompressedBytes => "decompressed input bytes",
            Self::CompressionLayers => "input compression layers",
        })
    }
}

/// Exact measurement for one rejected input limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputLimitViolation {
    resource: InputLimitResource,
    actual: u64,
    limit: u64,
    layer: Option<u64>,
}

impl InputLimitViolation {
    const fn new(
        resource: InputLimitResource,
        actual: u64,
        limit: u64,
        layer: Option<u64>,
    ) -> Self {
        Self {
            resource,
            actual,
            limit,
            layer,
        }
    }

    /// Returns the limited resource.
    #[must_use]
    pub const fn resource(self) -> InputLimitResource {
        self.resource
    }

    /// Returns the minimum observed size or count.
    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    /// Returns the configured maximum.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }

    /// Returns the one-based compression layer for a per-layer output violation.
    ///
    /// Physical bytes, the final plaintext boundary, and the nesting count have no layer.
    #[must_use]
    pub const fn layer(self) -> Option<u64> {
        self.layer
    }
}

impl Display for InputLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(layer) = self.layer {
            write!(
                formatter,
                "{} {} at compression layer {layer} exceeds limit {}",
                self.actual, self.resource, self.limit
            )
        } else {
            write!(
                formatter,
                "{} {} exceeds limit {}",
                self.actual, self.resource, self.limit
            )
        }
    }
}

impl Error for InputLimitViolation {}

/// Stable classification of a compression decoder failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InputDecodeErrorKind {
    /// The physical source ended before the current member or frame terminated.
    Truncated,
    /// A header, payload, checksum, trailing byte sequence, or subsequent member was invalid.
    InvalidData,
}

impl Display for InputDecodeErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "truncated compressed data",
            Self::InvalidData => "invalid compressed data",
        })
    }
}

/// Compression format and nesting context for one decoder failure.
#[derive(Debug)]
pub struct InputDecodeError {
    layer: u64,
    compression: InputCompression,
    kind: InputDecodeErrorKind,
    source: io::Error,
}

impl InputDecodeError {
    const fn new(
        layer: u64,
        compression: InputCompression,
        kind: InputDecodeErrorKind,
        source: io::Error,
    ) -> Self {
        Self {
            layer,
            compression,
            kind,
            source,
        }
    }

    /// Returns the one-based compression layer, with one denoting the outermost wrapper.
    #[must_use]
    pub const fn layer(&self) -> u64 {
        self.layer
    }

    /// Returns the decoder format.
    #[must_use]
    pub const fn compression(&self) -> InputCompression {
        self.compression
    }

    /// Returns the stable decoder-failure classification.
    #[must_use]
    pub const fn kind(&self) -> InputDecodeErrorKind {
        self.kind
    }

    /// Returns the underlying codec error.
    #[must_use]
    pub const fn io_error(&self) -> &io::Error {
        &self.source
    }
}

impl Display for InputDecodeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} decoder at compression layer {} rejected {}: {}",
            self.compression, self.layer, self.kind, self.source
        )
    }
}

impl Error for InputDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Fatal source, decompression, resource-limit, or accounting error.
#[derive(Debug)]
#[non_exhaustive]
pub enum InputError {
    /// Reading the caller-owned physical source failed.
    Source(io::Error),
    /// One compression member or frame was truncated or invalid.
    Decode(InputDecodeError),
    /// A configured physical, decompressed, or nesting boundary was exceeded.
    Limit(InputLimitViolation),
    /// A bounded input buffer could not reserve the requested additional bytes.
    AllocationFailed {
        /// Requested additional elements or bytes.
        requested_additional: usize,
    },
    /// A byte or layer counter could not be represented as `u64`.
    SizeOverflow,
    /// A previous source, decoder, limit, or accounting failure permanently stopped this reader.
    Stopped,
}

impl Display for InputError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => write!(formatter, "failed to read physical input: {source}"),
            Self::Decode(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional input bytes"
            ),
            Self::SizeOverflow => formatter.write_str("input byte or layer counter overflow"),
            Self::Stopped => formatter.write_str("input decoder stopped after an earlier error"),
        }
    }
}

impl Error for InputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::Limit(source) => Some(source),
            Self::AllocationFailed { .. } | Self::SizeOverflow | Self::Stopped => None,
        }
    }
}

/// Streaming input counters accumulated so far.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputStats {
    physical_bytes: u64,
    decoded_bytes: u64,
    compression_layers: u64,
}

impl InputStats {
    /// Returns bytes actually pulled from the physical source, including decoder read-ahead.
    #[must_use]
    pub const fn physical_bytes(self) -> u64 {
        self.physical_bytes
    }

    /// Returns final plaintext bytes delivered to the caller.
    #[must_use]
    pub const fn decoded_bytes(self) -> u64 {
        self.decoded_bytes
    }

    /// Returns the number of detected compression layers.
    #[must_use]
    pub const fn compression_layers(self) -> u64 {
        self.compression_layers
    }
}

/// Auto-detected, recursively decompressed input stream.
///
/// Detection performs only a four-byte streaming probe at each layer; decoder buffering remains
/// bounded and subject to the configured physical limit.
pub struct DecodedInput<'input> {
    reader: BoxedReader<'input>,
    limits: InputLimits,
    compression_layers: Vec<InputCompression>,
    source_name: Option<PathBuf>,
    physical_bytes: Arc<AtomicU64>,
    decoded_bytes: u64,
    reached_eof: bool,
    stopped: bool,
}

impl<'input> DecodedInput<'input> {
    /// Detects and unwraps an unnamed caller-owned input stream.
    ///
    /// # Errors
    ///
    /// Returns an error if probing the physical source or a decoded layer fails, a compression
    /// layer cannot initialize, or a configured limit is exceeded during detection.
    pub fn new<R>(input: R, limits: InputLimits) -> Result<Self, InputError>
    where
        R: Read + 'input, {
        Self::open(input, None, limits, InputCompressionPolicy::GzipAndZstd)
    }

    /// Detects and unwraps an unnamed stream using an explicit compression policy.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::new`].
    pub fn with_compression_policy<R>(
        input: R,
        limits: InputLimits,
        policy: InputCompressionPolicy,
    ) -> Result<Self, InputError>
    where
        R: Read + 'input, {
        Self::open(input, None, limits, policy)
    }

    /// Detects and unwraps an input while preserving its caller-selected source name verbatim.
    ///
    /// The path is metadata only. Its extension is never consulted, and wrapper suffixes are not
    /// removed.
    ///
    /// # Errors
    ///
    /// Returns an error if probing the physical source or a decoded layer fails, a compression
    /// layer cannot initialize, or a configured limit is exceeded during detection.
    pub fn with_source_name<R, P>(
        input: R,
        source_name: P,
        limits: InputLimits,
    ) -> Result<Self, InputError>
    where
        R: Read + 'input,
        P: Into<PathBuf>, {
        Self::open(
            input,
            Some(source_name.into()),
            limits,
            InputCompressionPolicy::GzipAndZstd,
        )
    }

    /// Detects and unwraps a named stream using an explicit compression policy.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::with_source_name`].
    pub fn with_source_name_and_compression_policy<R, P>(
        input: R,
        source_name: P,
        limits: InputLimits,
        policy: InputCompressionPolicy,
    ) -> Result<Self, InputError>
    where
        R: Read + 'input,
        P: Into<PathBuf>, {
        Self::open(input, Some(source_name.into()), limits, policy)
    }

    fn open<R>(
        input: R,
        source_name: Option<PathBuf>,
        limits: InputLimits,
        policy: InputCompressionPolicy,
    ) -> Result<Self, InputError>
    where
        R: Read + 'input, {
        let physical_bytes = Arc::new(AtomicU64::new(0));
        let physical_reader = PhysicalReader::new(
            input,
            limits.max_compressed_bytes(),
            Arc::clone(&physical_bytes),
        );
        let mut reader: BoxedReader<'input> = Box::new(physical_reader);
        let mut compression_layers = Vec::new();

        loop {
            let probe = read_probe(&mut reader)?;
            let Some(compression) = detect_compression(probe.bytes(), policy) else {
                reader = Box::new(PrefixReader::new(probe, reader));
                break;
            };
            let layer = u64::try_from(compression_layers.len())
                .map_err(|_| InputError::SizeOverflow)?
                .checked_add(1)
                .ok_or(InputError::SizeOverflow)?;
            if layer > limits.max_layers() {
                return Err(InputError::Limit(InputLimitViolation::new(
                    InputLimitResource::CompressionLayers,
                    layer,
                    limits.max_layers(),
                    None,
                )));
            }
            compression_layers.push(compression);

            let prefixed = PrefixReader::new(probe, reader);
            let decoded: BoxedReader<'input> = match compression {
                InputCompression::Gzip => Box::new(DecoderReader::new(
                    MultiGzDecoder::new(prefixed),
                    compression,
                    layer,
                )),
                InputCompression::Zstd => {
                    let decoder = zstd::stream::read::Decoder::new(prefixed).map_err(|source| {
                        InputError::Decode(InputDecodeError::new(
                            layer,
                            compression,
                            classify_decode_error(&source),
                            source,
                        ))
                    })?;
                    Box::new(DecoderReader::new(decoder, compression, layer))
                }
            };
            reader = Box::new(DecompressedLimitReader::new(
                decoded,
                limits.max_decompressed_bytes(),
                layer,
            ));
        }

        Ok(Self {
            reader,
            limits,
            compression_layers,
            source_name,
            physical_bytes,
            decoded_bytes: 0,
            reached_eof: false,
            stopped: false,
        })
    }

    /// Returns the immutable limit configuration.
    #[must_use]
    pub const fn limits(&self) -> InputLimits {
        self.limits
    }

    /// Returns detected compression layers ordered from outermost to innermost.
    #[must_use]
    pub fn compression_layers(&self) -> &[InputCompression] {
        &self.compression_layers
    }

    /// Returns the untouched caller-provided source name, if any.
    #[must_use]
    pub fn source_name(&self) -> Option<&Path> {
        self.source_name.as_deref()
    }

    /// Returns streaming counters accumulated so far.
    #[must_use]
    pub fn stats(&self) -> InputStats {
        InputStats {
            physical_bytes: self.physical_bytes.load(Ordering::Relaxed),
            decoded_bytes: self.decoded_bytes,
            compression_layers: u64::try_from(self.compression_layers.len()).unwrap_or(u64::MAX),
        }
    }

    /// Reads final plaintext while preserving typed source, decoder, and limit failures.
    ///
    /// Reading through `Ok(0)` with a nonempty output buffer is required to validate the final
    /// compressed member or frame and reject trailing data. As required by [`Read`], an empty
    /// output buffer returns `Ok(0)` immediately without driving the decoder or validating EOF.
    ///
    /// # Errors
    ///
    /// Returns a typed source, decoder, limit, or accounting error. Any such error permanently
    /// stops the reader.
    pub fn read_typed(&mut self, output: &mut [u8]) -> Result<usize, InputError> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.stopped {
            return Err(InputError::Stopped);
        }
        if self.reached_eof {
            return Ok(0);
        }

        let remaining = self
            .limits
            .max_decompressed_bytes()
            .checked_sub(self.decoded_bytes)
            .ok_or(InputError::SizeOverflow)?;
        if 0 == remaining {
            let mut extra = [0_u8; 1];
            return match read_internal(&mut self.reader, &mut extra) {
                Ok(0) => {
                    self.reached_eof = true;
                    Ok(0)
                }
                Ok(_) => self.fail(InputError::Limit(InputLimitViolation::new(
                    InputLimitResource::DecompressedBytes,
                    self.decoded_bytes
                        .checked_add(1)
                        .ok_or(InputError::SizeOverflow)?,
                    self.limits.max_decompressed_bytes(),
                    None,
                ))),
                Err(source) => self.fail(source),
            };
        }

        let bounded_len = output
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        match read_internal(&mut self.reader, &mut output[..bounded_len]) {
            Ok(0) => {
                self.reached_eof = true;
                Ok(0)
            }
            Ok(read) => {
                let read = u64::try_from(read).map_err(|_| InputError::SizeOverflow)?;
                self.decoded_bytes = self
                    .decoded_bytes
                    .checked_add(read)
                    .ok_or(InputError::SizeOverflow)?;
                usize::try_from(read).map_err(|_| InputError::SizeOverflow)
            }
            Err(source) => self.fail(source),
        }
    }

    const fn fail<T>(&mut self, source: InputError) -> Result<T, InputError> {
        self.stopped = true;
        Err(source)
    }
}

impl Read for DecodedInput<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_typed(output).map_err(input_error_to_io)
    }
}

struct Probe {
    bytes: [u8; PROBE_BYTES],
    len: usize,
}

impl Probe {
    const fn new() -> Self {
        Self {
            bytes: [0; PROBE_BYTES],
            len: 0,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

fn read_probe(reader: &mut BoxedReader<'_>) -> Result<Probe, InputError> {
    let mut probe = Probe::new();
    while probe.len < probe.bytes.len() {
        let read = read_internal(reader, &mut probe.bytes[probe.len..])?;
        if 0 == read {
            break;
        }
        probe.len = probe
            .len
            .checked_add(read)
            .ok_or(InputError::SizeOverflow)?;
    }
    Ok(probe)
}

fn detect_compression(probe: &[u8], policy: InputCompressionPolicy) -> Option<InputCompression> {
    if InputCompressionPolicy::GzipAndZstd == policy && probe.starts_with(&GZIP_MAGIC) {
        Some(InputCompression::Gzip)
    } else if probe.starts_with(&ZSTD_MAGIC) {
        Some(InputCompression::Zstd)
    } else {
        None
    }
}

fn read_internal(reader: &mut dyn Read, output: &mut [u8]) -> Result<usize, InputError> {
    loop {
        match reader.read(output) {
            Ok(read) => return Ok(read),
            Err(source) if io::ErrorKind::Interrupted == source.kind() => {}
            Err(source) => return Err(input_error_from_io(source)),
        }
    }
}

fn input_error_from_io(source: io::Error) -> InputError {
    if source
        .get_ref()
        .is_some_and(<dyn Error + Send + Sync + 'static>::is::<InputError>)
    {
        let inner = source
            .into_inner()
            .expect("an io::Error with an InputError reference owns that error");
        return *inner
            .downcast::<InputError>()
            .expect("the inspected io::Error payload is an InputError");
    }
    InputError::Source(source)
}

fn input_error_to_io(source: InputError) -> io::Error {
    let kind = match &source {
        InputError::Source(source) => source.kind(),
        InputError::Decode(source) => match source.kind() {
            InputDecodeErrorKind::Truncated => io::ErrorKind::UnexpectedEof,
            InputDecodeErrorKind::InvalidData => io::ErrorKind::InvalidData,
        },
        InputError::Limit(_) => io::ErrorKind::InvalidData,
        InputError::AllocationFailed { .. } | InputError::SizeOverflow | InputError::Stopped => {
            io::ErrorKind::Other
        }
    };
    io::Error::new(kind, source)
}

fn classify_decode_error(source: &io::Error) -> InputDecodeErrorKind {
    if io::ErrorKind::UnexpectedEof == source.kind() {
        InputDecodeErrorKind::Truncated
    } else {
        InputDecodeErrorKind::InvalidData
    }
}

fn is_input_error(source: &io::Error) -> bool {
    source
        .get_ref()
        .is_some_and(<dyn Error + Send + Sync + 'static>::is::<InputError>)
}

struct PrefixReader<R> {
    probe: Probe,
    position: usize,
    inner: R,
}

impl<R> PrefixReader<R> {
    const fn new(probe: Probe, inner: R) -> Self {
        Self {
            probe,
            position: 0,
            inner,
        }
    }
}

impl<R: Read> Read for PrefixReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let prefix = &self.probe.bytes()[self.position..];
        if prefix.is_empty() {
            return self.inner.read(output);
        }
        let read = prefix.len().min(output.len());
        output[..read].copy_from_slice(&prefix[..read]);
        self.position += read;
        Ok(read)
    }
}

struct PhysicalReader<R> {
    inner: R,
    limit: u64,
    bytes: Arc<AtomicU64>,
    reached_eof: bool,
}

impl<R> PhysicalReader<R> {
    const fn new(inner: R, limit: u64, bytes: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            limit,
            bytes,
            reached_eof: false,
        }
    }
}

impl<R: Read> Read for PhysicalReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.reached_eof {
            return Ok(0);
        }
        let consumed = self.bytes.load(Ordering::Relaxed);
        let remaining = self
            .limit
            .checked_sub(consumed)
            .ok_or_else(|| input_error_to_io(InputError::SizeOverflow))?;
        if 0 == remaining {
            let mut extra = [0_u8; 1];
            return match self.inner.read(&mut extra) {
                Ok(0) => {
                    self.reached_eof = true;
                    Ok(0)
                }
                Ok(_) => {
                    let actual = consumed
                        .checked_add(1)
                        .ok_or_else(|| input_error_to_io(InputError::SizeOverflow))?;
                    self.bytes.store(actual, Ordering::Relaxed);
                    Err(input_error_to_io(InputError::Limit(
                        InputLimitViolation::new(
                            InputLimitResource::CompressedBytes,
                            actual,
                            self.limit,
                            None,
                        ),
                    )))
                }
                Err(source) if io::ErrorKind::Interrupted == source.kind() => Err(source),
                Err(source) => Err(input_error_to_io(InputError::Source(source))),
            };
        }

        let bounded_len = output
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        match self.inner.read(&mut output[..bounded_len]) {
            Ok(0) => {
                self.reached_eof = true;
                Ok(0)
            }
            Ok(read) => {
                let read =
                    u64::try_from(read).map_err(|_| input_error_to_io(InputError::SizeOverflow))?;
                let total = consumed
                    .checked_add(read)
                    .ok_or_else(|| input_error_to_io(InputError::SizeOverflow))?;
                self.bytes.store(total, Ordering::Relaxed);
                usize::try_from(read).map_err(|_| input_error_to_io(InputError::SizeOverflow))
            }
            Err(source) if io::ErrorKind::Interrupted == source.kind() => Err(source),
            Err(source) => Err(input_error_to_io(InputError::Source(source))),
        }
    }
}

struct DecoderReader<R> {
    inner: R,
    compression: InputCompression,
    layer: u64,
}

impl<R> DecoderReader<R> {
    const fn new(inner: R, compression: InputCompression, layer: u64) -> Self {
        Self {
            inner,
            compression,
            layer,
        }
    }
}

impl<R: Read> Read for DecoderReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(output) {
            Err(source)
                if io::ErrorKind::Interrupted == source.kind() || is_input_error(&source) =>
            {
                Err(source)
            }
            Err(source) => {
                let kind = classify_decode_error(&source);
                Err(input_error_to_io(InputError::Decode(
                    InputDecodeError::new(self.layer, self.compression, kind, source),
                )))
            }
            result => result,
        }
    }
}

struct DecompressedLimitReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
    layer: u64,
    reached_eof: bool,
}

impl<R> DecompressedLimitReader<R> {
    const fn new(inner: R, limit: u64, layer: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
            layer,
            reached_eof: false,
        }
    }
}

impl<R: Read> Read for DecompressedLimitReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.reached_eof {
            return Ok(0);
        }
        let remaining = self
            .limit
            .checked_sub(self.consumed)
            .ok_or_else(|| input_error_to_io(InputError::SizeOverflow))?;
        if 0 == remaining {
            let mut extra = [0_u8; 1];
            return match self.inner.read(&mut extra) {
                Ok(0) => {
                    self.reached_eof = true;
                    Ok(0)
                }
                Ok(_) => Err(input_error_to_io(InputError::Limit(
                    InputLimitViolation::new(
                        InputLimitResource::DecompressedBytes,
                        self.consumed
                            .checked_add(1)
                            .ok_or_else(|| input_error_to_io(InputError::SizeOverflow))?,
                        self.limit,
                        Some(self.layer),
                    ),
                ))),
                Err(source) => Err(source),
            };
        }

        let bounded_len = output
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        match self.inner.read(&mut output[..bounded_len]) {
            Ok(0) => {
                self.reached_eof = true;
                Ok(0)
            }
            Ok(read) => {
                let read =
                    u64::try_from(read).map_err(|_| input_error_to_io(InputError::SizeOverflow))?;
                self.consumed = self
                    .consumed
                    .checked_add(read)
                    .ok_or_else(|| input_error_to_io(InputError::SizeOverflow))?;
                usize::try_from(read).map_err(|_| input_error_to_io(InputError::SizeOverflow))
            }
            Err(source) => Err(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    const UNBOUNDED: InputLimits = InputLimits::new(u64::MAX, u64::MAX, u64::MAX);
    const PLAINTEXT: &[u8] = b"{\"part\":1}\n{\"part\":2}\n";

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).expect("write gzip test input");
        encoder.finish().expect("finish gzip test input")
    }

    fn zstd(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 1).expect("encode zstd test input")
    }

    fn read_all_typed(input: &mut DecodedInput<'_>) -> Result<Vec<u8>, InputError> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 7];
        loop {
            let read = input.read_typed(&mut buffer)?;
            if 0 == read {
                return Ok(bytes);
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
    }

    fn decode(bytes: Vec<u8>) -> Result<(Vec<u8>, Vec<InputCompression>), InputError> {
        let mut input = DecodedInput::new(Cursor::new(bytes), UNBOUNDED)?;
        let layers = input.compression_layers().to_vec();
        let decoded = read_all_typed(&mut input)?;
        Ok((decoded, layers))
    }

    fn expect_limit(error: &InputError) -> InputLimitViolation {
        let InputError::Limit(source) = error else {
            panic!("expected input limit error, received {error}");
        };
        *source
    }

    fn expect_decode(error: InputError) -> InputDecodeError {
        let InputError::Decode(source) = error else {
            panic!("expected input decode error, received {error}");
        };
        source
    }

    #[test]
    fn raw_input_and_source_name_are_preserved_without_extension_inference() {
        let source_name = PathBuf::from("logs/raw-input.json.gz");
        let mut input = DecodedInput::with_source_name(
            Cursor::new(PLAINTEXT.to_vec()),
            source_name.clone(),
            UNBOUNDED,
        )
        .expect("open misleadingly named raw input");

        assert_eq!(&[] as &[InputCompression], input.compression_layers());
        assert_eq!(Some(source_name.as_path()), input.source_name());
        assert_eq!(PLAINTEXT, read_all_typed(&mut input).unwrap());
        assert_eq!(
            InputStats {
                physical_bytes: u64::try_from(PLAINTEXT.len()).unwrap(),
                decoded_bytes: u64::try_from(PLAINTEXT.len()).unwrap(),
                compression_layers: 0,
            },
            input.stats()
        );
    }

    #[test]
    fn gzip_and_zstd_are_detected_by_magic_with_an_untouched_outer_name() {
        for (encoded, expected) in [
            (gzip(PLAINTEXT), InputCompression::Gzip),
            (zstd(PLAINTEXT), InputCompression::Zstd),
        ] {
            let outer_name = PathBuf::from("outer/no-wrapper-suffix.data");
            let mut input =
                DecodedInput::with_source_name(Cursor::new(encoded), outer_name.clone(), UNBOUNDED)
                    .expect("detect compressed input");
            assert_eq!(&[expected], input.compression_layers());
            assert_eq!(Some(outer_name.as_path()), input.source_name());
            assert_eq!(PLAINTEXT, read_all_typed(&mut input).unwrap());
        }
    }

    struct ChunkedReader<'input> {
        remaining: &'input [u8],
        chunk_bytes: usize,
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let read = self.remaining.len().min(output.len()).min(self.chunk_bytes);
            output[..read].copy_from_slice(&self.remaining[..read]);
            self.remaining = &self.remaining[read..];
            Ok(read)
        }
    }

    #[test]
    fn borrowed_short_read_sources_stream_through_nested_decoders() {
        let encoded = gzip(&zstd(PLAINTEXT));
        let source = ChunkedReader {
            remaining: &encoded,
            chunk_bytes: 1,
        };
        let mut input = DecodedInput::new(source, UNBOUNDED).expect("probe one byte at a time");

        assert_eq!(
            &[InputCompression::Gzip, InputCompression::Zstd],
            input.compression_layers()
        );
        assert_eq!(PLAINTEXT, read_all_typed(&mut input).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_source_names_are_metadata_and_remain_byte_exact() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let source_name = PathBuf::from(OsString::from_vec(b"outer-\xff.json.gz".to_vec()));
        let mut input = DecodedInput::with_source_name(
            Cursor::new(gzip(PLAINTEXT)),
            source_name.clone(),
            UNBOUNDED,
        )
        .expect("open named gzip input");

        assert_eq!(Some(source_name.as_path()), input.source_name());
        assert_eq!(PLAINTEXT, read_all_typed(&mut input).unwrap());
    }

    #[test]
    fn arbitrary_gzip_zstd_nesting_is_streamed_outermost_first() {
        let gzip_inner = gzip(PLAINTEXT);
        let zstd_middle = zstd(&gzip_inner);
        let gzip_outer = gzip(&zstd_middle);
        let (decoded, layers) = decode(gzip_outer).expect("decode nested wrappers");

        assert_eq!(PLAINTEXT, decoded);
        assert_eq!(
            [
                InputCompression::Gzip,
                InputCompression::Zstd,
                InputCompression::Gzip,
            ],
            layers.as_slice()
        );
    }

    #[test]
    fn concatenated_gzip_members_and_zstd_frames_are_complete_streams() {
        let first = b"{\"part\":1}\n";
        let second = b"{\"part\":2}\n";
        let expected = [first.as_slice(), second.as_slice()].concat();

        for (mut first_member, second_member, compression) in [
            (gzip(first), gzip(second), InputCompression::Gzip),
            (zstd(first), zstd(second), InputCompression::Zstd),
        ] {
            first_member.extend_from_slice(&second_member);
            let (decoded, layers) = decode(first_member).expect("decode concatenated stream");
            assert_eq!(expected, decoded);
            assert_eq!(&[compression], layers.as_slice());
        }
    }

    #[test]
    fn decompressed_limit_cannot_be_bypassed_by_a_second_member_or_frame() {
        let first = b"12345";
        let second = b"67890";
        for (mut encoded, tail) in [(gzip(first), gzip(second)), (zstd(first), zstd(second))] {
            encoded.extend_from_slice(&tail);
            let limits = InputLimits::new(u64::MAX, 7, 1);
            let mut input = DecodedInput::new(Cursor::new(encoded), limits)
                .expect("first probe is within the decoded limit");
            let source = expect_limit(&read_all_typed(&mut input).unwrap_err());
            assert_eq!(InputLimitResource::DecompressedBytes, source.resource());
            assert_eq!(8, source.actual());
            assert_eq!(7, source.limit());
            assert_eq!(Some(1), source.layer());
            assert!(matches!(
                input.read_typed(&mut [0; 1]),
                Err(InputError::Stopped)
            ));
        }
    }

    #[test]
    fn physical_limit_cannot_be_bypassed_by_a_second_frame() {
        let first = zstd(b"first");
        let second = zstd(b"second");
        let physical_limit = u64::try_from(first.len()).unwrap();
        let mut encoded = first;
        encoded.extend_from_slice(&second);
        let limits = InputLimits::new(physical_limit, u64::MAX, 1);

        let mut input = DecodedInput::new(Cursor::new(encoded), limits)
            .expect("the first frame can be probed within the physical limit");
        let error = read_all_typed(&mut input)
            .expect_err("reading through the second frame crosses the physical limit");
        let source = expect_limit(&error);
        assert_eq!(InputLimitResource::CompressedBytes, source.resource());
        assert_eq!(physical_limit + 1, source.actual());
        assert_eq!(physical_limit, source.limit());
        assert_eq!(physical_limit + 1, input.stats().physical_bytes());
    }

    #[test]
    fn raw_and_intermediate_decompressed_bytes_have_independent_limits() {
        let raw_limits = InputLimits::new(u64::MAX, 3, 0);
        let mut raw = DecodedInput::new(Cursor::new(b"four".to_vec()), raw_limits)
            .expect("raw probe fits physical bound");
        let source = expect_limit(&read_all_typed(&mut raw).unwrap_err());
        assert_eq!(InputLimitResource::DecompressedBytes, source.resource());
        assert_eq!(None, source.layer());

        let inner = zstd(b"x");
        let outer = gzip(&inner);
        let intermediate_limit = u64::try_from(inner.len() - 1).unwrap();
        let error = DecodedInput::new(
            Cursor::new(outer),
            InputLimits::new(u64::MAX, intermediate_limit, 2),
        )
        .err()
        .expect("outer layer exceeds its decoded byte bound while probing inner magic");
        let source = expect_limit(&error);
        assert_eq!(InputLimitResource::DecompressedBytes, source.resource());
        assert_eq!(Some(1), source.layer());
        assert_eq!(intermediate_limit + 1, source.actual());
    }

    #[test]
    fn nesting_limit_rejects_the_next_recognized_wrapper() {
        let nested = gzip(&zstd(PLAINTEXT));
        let error = DecodedInput::new(Cursor::new(nested), InputLimits::new(u64::MAX, u64::MAX, 1))
            .err()
            .expect("second wrapper exceeds nesting bound");
        let source = expect_limit(&error);
        assert_eq!(InputLimitResource::CompressionLayers, source.resource());
        assert_eq!(2, source.actual());
        assert_eq!(1, source.limit());
        assert_eq!(None, source.layer());
    }

    #[test]
    fn valid_empty_wrappers_are_not_confused_with_raw_empty_input() {
        for (encoded, compression) in [
            (gzip(&[]), InputCompression::Gzip),
            (zstd(&[]), InputCompression::Zstd),
        ] {
            let (decoded, layers) = decode(encoded).expect("decode valid empty wrapper");
            assert_eq!(Vec::<u8>::new(), decoded);
            assert_eq!(&[compression], layers.as_slice());
        }
        let (decoded, layers) = decode(Vec::new()).expect("read raw empty input");
        assert_eq!(Vec::<u8>::new(), decoded);
        assert_eq!(Vec::<InputCompression>::new(), layers);
    }

    #[test]
    fn truncated_gzip_and_zstd_are_typed_failures_unlike_the_cpp_zstd_quirk() {
        for (mut encoded, compression) in [
            (gzip(PLAINTEXT), InputCompression::Gzip),
            (zstd(PLAINTEXT), InputCompression::Zstd),
        ] {
            encoded.pop();
            let mut input = DecodedInput::new(Cursor::new(encoded), UNBOUNDED)
                .expect("compressed prefix is sufficient for detection");
            let source = expect_decode(read_all_typed(&mut input).unwrap_err());
            assert_eq!(compression, source.compression());
            assert_eq!(1, source.layer());
            assert_eq!(InputDecodeErrorKind::Truncated, source.kind());
        }
    }

    #[test]
    fn truncation_in_a_second_concatenated_member_or_frame_is_not_hidden() {
        for (mut encoded, mut tail, compression) in [
            (gzip(b"first"), gzip(b"second"), InputCompression::Gzip),
            (zstd(b"first"), zstd(b"second"), InputCompression::Zstd),
        ] {
            tail.pop();
            encoded.extend_from_slice(&tail);
            let mut input = DecodedInput::new(Cursor::new(encoded), UNBOUNDED)
                .expect("first member permits compressed-stream detection");
            let source = expect_decode(read_all_typed(&mut input).unwrap_err());
            assert_eq!(compression, source.compression());
            assert_eq!(1, source.layer());
            assert_eq!(InputDecodeErrorKind::Truncated, source.kind());
        }
    }

    #[test]
    fn decoder_errors_report_the_failing_nested_layer() {
        let mut inner = zstd(PLAINTEXT);
        inner.pop();
        let outer = gzip(&inner);
        let mut input = DecodedInput::new(Cursor::new(outer), UNBOUNDED)
            .expect("inner frame emits enough data for recursive detection");
        let source = expect_decode(read_all_typed(&mut input).unwrap_err());

        assert_eq!(InputCompression::Zstd, source.compression());
        assert_eq!(2, source.layer());
        assert_eq!(InputDecodeErrorKind::Truncated, source.kind());
    }

    #[test]
    fn trailing_bytes_after_gzip_or_zstd_are_rejected() {
        for (mut encoded, compression) in [
            (gzip(PLAINTEXT), InputCompression::Gzip),
            (zstd(PLAINTEXT), InputCompression::Zstd),
        ] {
            encoded.extend_from_slice(b"not-a-compressed-member");
            let mut input = DecodedInput::new(Cursor::new(encoded), UNBOUNDED)
                .expect("detect compressed stream before trailing bytes");
            let source = expect_decode(read_all_typed(&mut input).unwrap_err());
            assert_eq!(compression, source.compression());
            assert_eq!(InputDecodeErrorKind::InvalidData, source.kind());
        }
    }

    #[test]
    fn empty_reads_do_not_mistake_read_contract_zero_for_validated_eof() {
        let mut encoded = zstd(PLAINTEXT);
        encoded.extend_from_slice(b"not-a-compressed-member");
        let mut input = DecodedInput::new(Cursor::new(encoded), UNBOUNDED)
            .expect("detect zstd before trailing data");

        assert_eq!(0, input.read_typed(&mut []).unwrap());
        let source = expect_decode(read_all_typed(&mut input).unwrap_err());
        assert_eq!(InputCompression::Zstd, source.compression());
        assert_eq!(InputDecodeErrorKind::InvalidData, source.kind());
    }

    #[derive(Debug)]
    struct FailingReader {
        bytes: Cursor<Vec<u8>>,
        fail_after: u64,
    }

    impl Read for FailingReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.bytes.position() >= self.fail_after {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "test failure",
                ));
            }
            let remaining = usize::try_from(self.fail_after - self.bytes.position()).unwrap();
            let bounded_len = output.len().min(remaining);
            self.bytes.read(&mut output[..bounded_len])
        }
    }

    #[test]
    fn physical_source_failures_survive_nested_decoder_layers() {
        let encoded = gzip(&zstd(PLAINTEXT));
        let fail_after = u64::try_from(encoded.len() - 1).unwrap();
        let reader = FailingReader {
            bytes: Cursor::new(encoded),
            fail_after,
        };
        let mut input = DecodedInput::new(reader, UNBOUNDED)
            .expect("detection completes before the late source error");
        let InputError::Source(source) = read_all_typed(&mut input).unwrap_err() else {
            panic!("nested source failure lost its type");
        };
        assert_eq!(io::ErrorKind::PermissionDenied, source.kind());
    }

    #[test]
    fn read_trait_embeds_typed_errors_for_generic_stream_consumers() {
        let mut encoded = zstd(PLAINTEXT);
        encoded.pop();
        let mut input = DecodedInput::new(Cursor::new(encoded), UNBOUNDED)
            .expect("detect truncated zstd stream");
        let mut output = Vec::new();
        let source = input.read_to_end(&mut output).unwrap_err();
        assert_eq!(io::ErrorKind::UnexpectedEof, source.kind());
        assert!(
            source
                .get_ref()
                .is_some_and(<dyn Error + Send + Sync + 'static>::is::<InputError>)
        );
    }
}
