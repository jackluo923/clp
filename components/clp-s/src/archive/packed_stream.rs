use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::BufReader;
use std::io::Read;
use std::io::Take;
use std::io::{self};
use std::ops::Range;

use super::table_metadata::PackedStreamMetadata;
use super::table_metadata::TableMetadata;

/// Resource limits applied while decompressing one packed stream from `/0`.
///
/// [`Default`] is intentionally safety-oriented for untrusted inputs. A valid archive created
/// with the C++ CLI's multi-gibibyte target size can contain a larger single-schema stream, so
/// compatibility-oriented applications should select explicit limits from their own memory and
/// archive-size policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackedStreamLimits {
    compressed: u64,
    decompressed: u64,
}

impl PackedStreamLimits {
    /// Creates explicit compressed and decompressed byte limits for one packed stream.
    #[must_use]
    pub const fn new(max_compressed_size: u64, max_decompressed_size: u64) -> Self {
        Self {
            compressed: max_compressed_size,
            decompressed: max_decompressed_size,
        }
    }

    /// Returns the maximum compressed bytes accepted for one packed stream.
    #[must_use]
    pub const fn max_compressed_size(self) -> u64 {
        self.compressed
    }

    /// Returns the maximum advertised decompressed bytes accepted for one packed stream.
    #[must_use]
    pub const fn max_decompressed_size(self) -> u64 {
        self.decompressed
    }
}

impl Default for PackedStreamLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(256 * MEBIBYTE, 1024 * MEBIBYTE)
    }
}

/// Exact decompressed contents of one validated packed stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedPackedStream {
    bytes: Vec<u8>,
    column_layout: Option<Vec<ColumnSlot>>,
}

/// One value column of a projected separate-column stream: its byte length within the table,
/// and whether its frame was inflated. A column that was not is a zero-filled gap of that length
/// which the table decoder steps over without reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnSlot {
    pub size: usize,
    pub loaded: bool,
}

impl DecodedPackedStream {
    /// Returns the decompressed stream bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the exact decompressed stream length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns whether this stream contains no decompressed bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Consumes the stream and returns its owned byte buffer.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl AsRef<[u8]> for DecodedPackedStream {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Decompresses and validates one packed zstd frame from the `/0` section.
///
/// `compressed` must already be positioned at [`PackedStreamMetadata::file_offset`] relative to
/// `/0` and bounded to [`PackedStreamMetadata::compressed_size`]. The decoder reads only through
/// that bound and allocates only the advertised decompressed size after enforcing `limits`.
/// Exactly one complete zstd frame must consume the nonempty bound and produce exactly the
/// advertised number of bytes. A `(0, 0)` stream is represented by no frame and returns an empty
/// buffer.
///
/// # Errors
///
/// Returns an error for mismatched bounds, resource-limit violations, inconsistent zero sizes,
/// allocation or size-conversion failures, input or zstd failures, truncated input, a mismatch in
/// decompressed length, or bytes following the first frame.
pub fn decode_packed_stream<R: Read>(
    compressed: Take<R>,
    metadata: &PackedStreamMetadata,
    limits: PackedStreamLimits,
) -> Result<DecodedPackedStream, PackedStreamError> {
    decode_frame(
        compressed,
        metadata.compressed_size(),
        metadata.uncompressed_size(),
        limits,
    )
}

/// Selects and bounds one packed stream relative to the physical `/0` member.
///
/// Both single-file and directory archives use the same table-metadata coordinates. Keeping the
/// validation here prevents either outer layout from seeking until the checked range is known to
/// fit its own `/0` bytes.
pub(super) fn packed_stream_range(
    table_metadata: &TableMetadata,
    stream_id: usize,
    tables_size: u64,
) -> Result<(&PackedStreamMetadata, Range<u64>), PackedStreamError> {
    let stream = table_metadata.packed_stream(stream_id).ok_or_else(|| {
        PackedStreamError::StreamIdOutOfBounds {
            stream_id,
            stream_count: table_metadata.packed_streams().len(),
        }
    })?;
    let start = stream.file_offset();
    let end = start
        .checked_add(stream.compressed_size())
        .ok_or(PackedStreamError::SizeOverflow)?;
    if end > tables_size {
        return Err(PackedStreamError::StreamRangeOutsideTablesSection {
            stream_id,
            start,
            end,
            tables_size,
        });
    }
    Ok((stream, start..end))
}

fn decode_frame<R: Read>(
    compressed: Take<R>,
    advertised_compressed_size: u64,
    advertised_decompressed_size: u64,
    limits: PackedStreamLimits,
) -> Result<DecodedPackedStream, PackedStreamError> {
    if validate_frame_sizes(
        compressed.limit(),
        advertised_compressed_size,
        advertised_decompressed_size,
        limits,
    )? {
        return Ok(DecodedPackedStream {
            bytes: Vec::new(),
            column_layout: None,
        });
    }

    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|source| classify_decoder_error(source, advertised_decompressed_size, 0))?
        .single_frame();
    let bytes = decode_exact_output(&mut decoder, advertised_decompressed_size)?;
    validate_compressed_tail(decoder.finish(), advertised_compressed_size)?;

    Ok(DecodedPackedStream {
        bytes,
        column_layout: None,
    })
}

impl DecodedPackedStream {
    /// Assembles a stream from per-column pieces: `None` for a column left unloaded.
    ///
    /// The result is laid out exactly as the whole-frame stream would be, with unloaded
    /// columns zero-filled, and carries the layout so the decoder can skip them.
    #[must_use]
    pub fn from_columns(columns: Vec<Option<Vec<u8>>>, sizes: &[usize]) -> Self {
        let total: usize = sizes.iter().sum();
        let mut bytes = Vec::with_capacity(total);
        let mut layout = Vec::with_capacity(sizes.len());
        for (column, &size) in columns.into_iter().zip(sizes) {
            match column {
                Some(data) => {
                    debug_assert_eq!(data.len(), size);
                    bytes.extend_from_slice(&data);
                    layout.push(ColumnSlot { size, loaded: true });
                }
                None => {
                    bytes.resize(bytes.len() + size, 0);
                    layout.push(ColumnSlot { size, loaded: false });
                }
            }
        }
        Self {
            bytes,
            column_layout: Some(layout),
        }
    }

    /// The per-column layout of a projected stream, or `None` for a whole stream.
    #[must_use]
    pub fn column_layout(&self) -> Option<&[ColumnSlot]> {
        self.column_layout.as_deref()
    }
}

const fn validate_frame_sizes(
    bounded_size: u64,
    advertised_compressed_size: u64,
    advertised_decompressed_size: u64,
    limits: PackedStreamLimits,
) -> Result<bool, PackedStreamError> {
    if advertised_compressed_size > limits.compressed {
        return Err(PackedStreamError::CompressedStreamTooLarge {
            actual: advertised_compressed_size,
            limit: limits.compressed,
        });
    }
    if advertised_decompressed_size > limits.decompressed {
        return Err(PackedStreamError::DecompressedStreamTooLarge {
            actual: advertised_decompressed_size,
            limit: limits.decompressed,
        });
    }

    if bounded_size != advertised_compressed_size {
        return Err(PackedStreamError::CompressedBoundMismatch {
            advertised: advertised_compressed_size,
            actual: bounded_size,
        });
    }

    match (advertised_compressed_size, advertised_decompressed_size) {
        (0, 0) => Ok(true),
        (compressed_size, 0) => {
            Err(PackedStreamError::EmptyStreamHasCompressedData { compressed_size })
        }
        (0, decompressed_size) => {
            Err(PackedStreamError::MissingCompressedFrame { decompressed_size })
        }
        _ => Ok(false),
    }
}

fn decode_exact_output<R: Read>(
    decoder: &mut R,
    advertised_size: u64,
) -> Result<Vec<u8>, PackedStreamError> {
    let allocation_size =
        usize::try_from(advertised_size).map_err(|_| PackedStreamError::SizeOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(allocation_size)
        .map_err(|_| PackedStreamError::AllocationFailed {
            requested: allocation_size,
        })?;
    let mut bounded = decoder.take(advertised_size);
    loop {
        match bounded.read_to_end(&mut bytes) {
            Ok(_) => break,
            Err(source) if io::ErrorKind::Interrupted == source.kind() => {}
            Err(source) => {
                return Err(classify_decoder_error(
                    source,
                    advertised_size,
                    usize_to_u64(bytes.len())?,
                ));
            }
        }
    }
    let decoder = bounded.into_inner();
    if bytes.len() != allocation_size {
        return Err(PackedStreamError::DecompressedSizeMismatch {
            advertised: advertised_size,
            actual: usize_to_u64(bytes.len())?,
        });
    }

    let mut extra = [0_u8; 1];
    loop {
        match decoder.read(&mut extra) {
            Ok(0) => break,
            Ok(read_size) => {
                let minimum_actual = advertised_size
                    .checked_add(usize_to_u64(read_size)?)
                    .ok_or(PackedStreamError::SizeOverflow)?;
                return Err(PackedStreamError::ExcessDecompressedData {
                    advertised: advertised_size,
                    minimum_actual,
                });
            }
            Err(source) if io::ErrorKind::Interrupted == source.kind() => {}
            Err(source) => {
                return Err(classify_decoder_error(
                    source,
                    advertised_size,
                    advertised_size,
                ));
            }
        }
    }

    Ok(bytes)
}

fn validate_compressed_tail<R: Read>(
    mut compressed: BufReader<Take<R>>,
    advertised_size: u64,
) -> Result<(), PackedStreamError> {
    let mut trailing_size = 0_u64;
    let mut scratch = [0_u8; 8192];
    loop {
        match compressed.read(&mut scratch) {
            Ok(0) => break,
            Ok(read_size) => {
                trailing_size = trailing_size
                    .checked_add(usize_to_u64(read_size)?)
                    .ok_or(PackedStreamError::SizeOverflow)?;
            }
            Err(source) if io::ErrorKind::Interrupted == source.kind() => {}
            Err(source) => return Err(PackedStreamError::Io(source)),
        }
    }

    let missing_compressed_size = compressed.get_ref().limit();
    if 0 != missing_compressed_size {
        let actual = advertised_size
            .checked_sub(missing_compressed_size)
            .ok_or(PackedStreamError::SizeOverflow)?;
        return Err(PackedStreamError::CompressedInputTruncated {
            advertised: advertised_size,
            actual,
        });
    }
    if 0 != trailing_size {
        return Err(PackedStreamError::TrailingCompressedData {
            remaining: trailing_size,
        });
    }

    Ok(())
}

fn classify_decoder_error(source: io::Error, advertised: u64, actual: u64) -> PackedStreamError {
    if io::ErrorKind::UnexpectedEof == source.kind() {
        PackedStreamError::TruncatedFrame {
            advertised,
            actual,
            source,
        }
    } else {
        PackedStreamError::Io(source)
    }
}

fn usize_to_u64(value: usize) -> Result<u64, PackedStreamError> {
    u64::try_from(value).map_err(|_| PackedStreamError::SizeOverflow)
}

/// Failure to decompress or validate one packed stream from `/0`.
#[derive(Debug)]
#[non_exhaustive]
pub enum PackedStreamError {
    /// The supplied archive metadata did not contain `/0`.
    MissingTablesSection,
    /// The supplied `/0` section was outside this reader's archive files region.
    TablesSectionOutsideArchive,
    /// The requested stream ID was absent from table metadata.
    StreamIdOutOfBounds {
        /// Requested zero-based packed-stream ID.
        stream_id: usize,
        /// Number of packed streams described by table metadata.
        stream_count: usize,
    },
    /// A packed stream's relative byte range did not fit in this archive's `/0` section.
    StreamRangeOutsideTablesSection {
        /// Zero-based packed-stream ID.
        stream_id: usize,
        /// Relative range start.
        start: u64,
        /// Relative range end.
        end: u64,
        /// Physical bytes in `/0`.
        tables_size: u64,
    },
    /// The advertised compressed stream exceeds the configured limit.
    CompressedStreamTooLarge {
        /// Advertised compressed bytes.
        actual: u64,
        /// Configured maximum compressed bytes.
        limit: u64,
    },
    /// The advertised decompressed stream exceeds the configured limit.
    DecompressedStreamTooLarge {
        /// Advertised decompressed bytes.
        actual: u64,
        /// Configured maximum decompressed bytes.
        limit: u64,
    },
    /// The supplied reader bound disagrees with the validated table metadata.
    CompressedBoundMismatch {
        /// Compressed bytes advertised by table metadata.
        advertised: u64,
        /// Compressed bytes exposed by the bounded reader.
        actual: u64,
    },
    /// A logical empty stream unexpectedly owns compressed bytes.
    EmptyStreamHasCompressedData {
        /// Unexpected compressed bytes.
        compressed_size: u64,
    },
    /// A logical nonempty stream owns no compressed frame.
    MissingCompressedFrame {
        /// Advertised decompressed bytes that cannot be produced.
        decompressed_size: u64,
    },
    /// A complete frame produced fewer bytes than advertised.
    DecompressedSizeMismatch {
        /// Advertised decompressed bytes.
        advertised: u64,
        /// Actual decompressed bytes.
        actual: u64,
    },
    /// A frame produced data after the advertised decompressed boundary.
    ExcessDecompressedData {
        /// Advertised decompressed bytes.
        advertised: u64,
        /// Minimum actual decompressed size observed before stopping.
        minimum_actual: u64,
    },
    /// The bounded source ended before its advertised compressed length.
    CompressedInputTruncated {
        /// Advertised compressed bytes.
        advertised: u64,
        /// Actual compressed bytes available in the source.
        actual: u64,
    },
    /// Zstd reported an incomplete frame.
    TruncatedFrame {
        /// Advertised decompressed bytes.
        advertised: u64,
        /// Bytes decompressed before truncation was detected.
        actual: u64,
        /// Underlying zstd or input error.
        source: io::Error,
    },
    /// Compressed bytes followed the one permitted zstd frame.
    TrailingCompressedData {
        /// Bytes following the first frame.
        remaining: u64,
    },
    /// Input or zstd decompression failed.
    Io(io::Error),
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// The exact bounded output allocation could not be reserved.
    AllocationFailed {
        /// Requested decompressed buffer length.
        requested: usize,
    },
}

impl Display for PackedStreamError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTablesSection => {
                formatter.write_str("archive metadata has no /0 tables section")
            }
            Self::TablesSectionOutsideArchive => {
                formatter.write_str("/0 tables section is outside the archive files region")
            }
            Self::StreamIdOutOfBounds {
                stream_id,
                stream_count,
            } => write!(
                formatter,
                "packed-stream ID {stream_id} is outside stream count {stream_count}"
            ),
            Self::StreamRangeOutsideTablesSection {
                stream_id,
                start,
                end,
                tables_size,
            } => write!(
                formatter,
                "packed-stream {stream_id} range {start}..{end} is outside /0 size {tables_size}"
            ),
            Self::CompressedStreamTooLarge { actual, limit } => write!(
                formatter,
                "compressed packed-stream size {actual} exceeds limit {limit}"
            ),
            Self::DecompressedStreamTooLarge { actual, limit } => write!(
                formatter,
                "decompressed packed-stream size {actual} exceeds limit {limit}"
            ),
            Self::CompressedBoundMismatch { advertised, actual } => write!(
                formatter,
                "packed-stream metadata advertises {advertised} compressed bytes but the reader \
                 is bounded to {actual}"
            ),
            Self::EmptyStreamHasCompressedData { compressed_size } => write!(
                formatter,
                "empty packed stream owns {compressed_size} compressed bytes"
            ),
            Self::MissingCompressedFrame { decompressed_size } => write!(
                formatter,
                "packed stream advertises {decompressed_size} decompressed bytes but has no \
                 compressed frame"
            ),
            Self::DecompressedSizeMismatch { advertised, actual } => write!(
                formatter,
                "packed stream advertises {advertised} decompressed bytes but produced {actual}"
            ),
            Self::ExcessDecompressedData {
                advertised,
                minimum_actual,
            } => write!(
                formatter,
                "packed stream advertises {advertised} decompressed bytes but produced at least \
                 {minimum_actual}"
            ),
            Self::CompressedInputTruncated { advertised, actual } => write!(
                formatter,
                "packed stream advertises {advertised} compressed bytes but only {actual} were \
                 available"
            ),
            Self::TruncatedFrame {
                advertised, actual, ..
            } => write!(
                formatter,
                "packed-stream zstd frame ended after {actual} of {advertised} advertised \
                 decompressed bytes"
            ),
            Self::TrailingCompressedData { remaining } => write!(
                formatter,
                "{remaining} compressed bytes follow the packed-stream zstd frame"
            ),
            Self::Io(error) => write!(formatter, "packed-stream I/O failed: {error}"),
            Self::SizeOverflow => formatter.write_str("packed-stream size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve bounded packed-stream allocation of {requested} bytes"
            ),
        }
    }
}

impl Error for PackedStreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TruncatedFrame { source, .. } | Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const TEST_LIMITS: PackedStreamLimits = PackedStreamLimits::new(u64::MAX, u64::MAX);

    fn compress(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 3).expect("compress packed-stream test data")
    }

    fn take(bytes: &[u8]) -> Take<Cursor<&[u8]>> {
        Cursor::new(bytes).take(usize_to_u64(bytes.len()).expect("test input length fits u64"))
    }

    fn decode(
        compressed: &[u8],
        advertised_decompressed_size: u64,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        decode_frame(
            take(compressed),
            usize_to_u64(compressed.len()).expect("test compressed length fits u64"),
            advertised_decompressed_size,
            TEST_LIMITS,
        )
    }

    #[test]
    fn decodes_one_exact_bounded_frame() {
        let expected = b"one packed stream split across decoder reads";
        let compressed = compress(expected);
        let chunked = ChunkedReader::new(Cursor::new(compressed.as_slice()), 2);
        let stream = decode_frame(
            chunked.take(usize_to_u64(compressed.len()).expect("test compressed length fits u64")),
            usize_to_u64(compressed.len()).expect("test compressed length fits u64"),
            usize_to_u64(expected.len()).expect("test output length fits u64"),
            TEST_LIMITS,
        )
        .expect("valid packed stream");

        assert_eq!(expected, stream.as_bytes());
        assert_eq!(expected.len(), stream.len());
        assert!(!stream.is_empty());
        assert_eq!(expected, stream.as_ref());
        assert_eq!(expected, stream.into_bytes().as_slice());
    }

    #[test]
    fn accepts_only_no_frame_for_a_zero_size_stream() {
        let stream = decode(&[], 0).expect("canonical empty stream");
        assert!(stream.is_empty());
        assert_eq!(b"", stream.as_bytes());

        let empty_frame = compress(&[]);
        assert!(matches!(
            decode(&empty_frame, 0),
            Err(PackedStreamError::EmptyStreamHasCompressedData { compressed_size })
                if compressed_size
                    == usize_to_u64(empty_frame.len()).expect("test frame length fits u64")
        ));
        assert!(matches!(
            decode_frame(Cursor::new(&[]).take(0), 0, 1, TEST_LIMITS),
            Err(PackedStreamError::MissingCompressedFrame {
                decompressed_size: 1
            })
        ));
    }

    #[test]
    fn rejects_a_reader_bound_that_disagrees_with_metadata() {
        let compressed = compress(b"value");
        let advertised = usize_to_u64(compressed.len()).expect("test frame length fits u64");
        assert!(matches!(
            decode_frame(
                Cursor::new(compressed.as_slice()).take(advertised - 1),
                advertised,
                5,
                TEST_LIMITS,
            ),
            Err(PackedStreamError::CompressedBoundMismatch {
                advertised: expected,
                actual,
            }) if expected == advertised && actual == advertised - 1
        ));
    }

    #[test]
    fn enforces_limits_before_decoding_or_allocating() {
        let compressed = compress(b"four");
        let compressed_size =
            usize_to_u64(compressed.len()).expect("test compressed length fits u64");
        assert!(matches!(
            decode_frame(
                take(&compressed),
                compressed_size,
                4,
                PackedStreamLimits::new(compressed_size - 1, u64::MAX),
            ),
            Err(PackedStreamError::CompressedStreamTooLarge { .. })
        ));
        assert!(matches!(
            decode_frame(
                take(&compressed),
                compressed_size,
                4,
                PackedStreamLimits::new(u64::MAX, 3),
            ),
            Err(PackedStreamError::DecompressedStreamTooLarge {
                actual: 4,
                limit: 3
            })
        ));

        let defaults = PackedStreamLimits::default();
        assert_eq!(256 * 1024 * 1024, defaults.max_compressed_size());
        assert_eq!(1024 * 1024 * 1024, defaults.max_decompressed_size());
    }

    #[test]
    fn rejects_short_and_excess_decompressed_output() {
        let short = compress(b"abc");
        assert!(matches!(
            decode(&short, 4),
            Err(PackedStreamError::DecompressedSizeMismatch {
                advertised: 4,
                actual: 3
            })
        ));

        let excess = compress(b"abcd");
        assert!(matches!(
            decode(&excess, 3),
            Err(PackedStreamError::ExcessDecompressedData {
                advertised: 3,
                minimum_actual: 4
            })
        ));
    }

    #[test]
    fn rejects_a_physically_short_compressed_bound() {
        let compressed = compress(b"complete frame");
        let actual = usize_to_u64(compressed.len()).expect("test frame length fits u64");
        let advertised = actual + 7;
        assert!(matches!(
            decode_frame(
                Cursor::new(compressed.as_slice()).take(advertised),
                advertised,
                14,
                TEST_LIMITS,
            ),
            Err(PackedStreamError::CompressedInputTruncated {
                advertised: expected,
                actual: observed,
            }) if expected == advertised && observed == actual
        ));
    }

    #[test]
    fn rejects_a_truncated_zstd_frame() {
        let expected = vec![0x5a; 128 * 1024];
        let mut compressed = compress(&expected);
        compressed.truncate(compressed.len() - 1);

        assert!(matches!(
            decode(
                &compressed,
                usize_to_u64(expected.len()).expect("test output length fits u64")
            ),
            Err(PackedStreamError::TruncatedFrame { .. })
        ));
    }

    #[test]
    fn rejects_trailing_bytes_and_multiple_frames() {
        let first = compress(b"first");
        let mut trailing = first.clone();
        trailing.extend_from_slice(b"junk");
        assert!(matches!(
            decode(&trailing, 5),
            Err(PackedStreamError::TrailingCompressedData { remaining: 4 })
        ));

        let second = compress(b"second");
        let mut concatenated = first;
        concatenated.extend_from_slice(&second);
        assert!(matches!(
            decode(&concatenated, 5),
            Err(PackedStreamError::TrailingCompressedData { remaining })
                if remaining
                    == usize_to_u64(second.len()).expect("test second frame length fits u64")
        ));
    }

    #[test]
    fn reports_invalid_frames_and_failed_bounded_allocations() {
        let invalid = b"not a zstd frame";
        assert!(matches!(decode(invalid, 1), Err(PackedStreamError::Io(_))));

        let frame = compress(&[]);
        let frame_size = usize_to_u64(frame.len()).expect("test frame length fits u64");
        assert!(matches!(
            decode_frame(
                take(&frame),
                frame_size,
                u64::MAX,
                PackedStreamLimits::new(u64::MAX, u64::MAX),
            ),
            Err(PackedStreamError::AllocationFailed { .. } | PackedStreamError::SizeOverflow)
        ));
    }

    struct ChunkedReader<R> {
        inner: R,
        max_read_size: usize,
    }

    impl<R> ChunkedReader<R> {
        const fn new(inner: R, max_read_size: usize) -> Self {
            Self {
                inner,
                max_read_size,
            }
        }
    }

    impl<R: Read> Read for ChunkedReader<R> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let read_size = output.len().min(self.max_read_size);
            self.inner.read(&mut output[..read_size])
        }
    }
}
