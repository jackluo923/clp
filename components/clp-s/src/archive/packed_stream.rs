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
/// [`Default`] bounds a single stream but stays above what this crate's own CLI produces: it
/// rotates an archive at 8 GiB of encoded output by default, so a table in a legitimate archive
/// can decompress past a gibibyte. Applications that read untrusted input, or that have a tighter
/// memory budget, should select explicit limits from their own policy.
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
        const GIBIBYTE: u64 = 1024 * 1024 * 1024;
        Self::new(GIBIBYTE, 8 * GIBIBYTE)
    }
}

/// Exact decompressed contents of one validated packed stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedPackedStream {
    bytes: Vec<u8>,
    column_layout: Option<Vec<ColumnSlot>>,
}

/// One value column of a projected separate-column stream.
///
/// `size` is the column's length in a whole table; `stored` is how much of it the stream actually
/// holds, which is all of it, none of it for a column the projection skipped, or a leading part
/// for a column read only as far as the rows a query can match. The buffer holds exactly the
/// stored bytes of each column in order. A skipped column occupies nothing. A compact numeric
/// slot stores its encoded payload for all rows while `size` remains the logical rows * 8 size;
/// `numeric_encoding` tells the table decoder how to interpret those bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnSlot {
    /// Reader-only adaptive CLP view; absent on raw frame reads.
    pub clp_layout: Option<std::sync::Arc<super::ClpColumnLayout>>,
    /// Reader-only compact numeric representation; absent on raw frame reads.
    pub numeric_encoding: Option<crate::encoded_numeric::NumericEncoding>,
    pub size: usize,
    pub stored: usize,
    pub loaded: bool,
}

impl ColumnSlot {
    /// Returns how many of `rows` this column holds, given a whole column covers all of them.
    #[must_use]
    pub const fn stored_rows(&self, rows: usize) -> usize {
        if self.clp_layout.is_some() || self.numeric_encoding.is_some() || self.stored >= self.size {
            return rows;
        }
        if 0 == self.size || 0 == rows {
            return 0;
        }
        let width = self.size / rows;
        if 0 == width {
            return 0;
        }
        // `stored` is a whole number of rows by construction, so this divides exactly.
        self.stored / width
    }
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

/// How far into a stream's columns a scan can reach.
///
/// A query whose range-index selector accepts one source file needs only the rows up to that
/// file's last, so every column after it is decompressed for nothing. Columns named truncatable
/// are read that far and zero-filled beyond, which the table decoder lays out exactly as a whole
/// read would. Only fixed-width columns free of referential validation qualify, since a row's byte
/// offset must be known without reading and an unread tail must stay structurally valid.
#[derive(Clone, Copy, Debug)]
pub struct ColumnTruncation<'a> {
    /// The log-order column, read first and in full so the stopping row can be found.
    pub log_order_column: usize,
    /// The first log event no accepted source file reaches.
    pub limit: u64,
    /// Rows the table holds.
    pub total_rows: usize,
    /// Whether each column may stop early, indexed as the stream's columns are.
    pub truncatable: &'a [bool],
}

impl ColumnTruncation<'_> {
    /// Returns the byte prefix of column `index` a scan needs, or `None` for all of it.
    #[must_use]
    pub(super) fn prefix(&self, index: usize, size: usize, rows: usize) -> Option<usize> {
        if !self.truncatable.get(index).copied().unwrap_or(false) {
            return None;
        }
        if 0 == self.total_rows || rows >= self.total_rows {
            return None;
        }
        // A column whose bytes do not divide evenly across rows is not the fixed-width layout this
        // can address, whatever its type says.
        if 0 != size % self.total_rows {
            return None;
        }
        Some(size / self.total_rows * rows)
    }
}

/// Rows decoded per block while looking for where a log-order column crosses a limit.
///
/// Whether a column frame is stored raw rather than as zstd: in an archive that stores a column raw
/// when zstd would not shrink it ([`super::ArchiveVersion::supports_raw_column_frames`]), a frame
/// whose compressed and uncompressed sizes are equal, since the writer stores raw whenever zstd
/// comes out no smaller.
pub(super) const fn is_raw_frame(raw_frames: bool, metadata: &PackedStreamMetadata) -> bool {
    raw_frames
        && 0 != metadata.uncompressed_size()
        && metadata.compressed_size() == metadata.uncompressed_size()
}

/// Reads the first `length` bytes of a column frame stored raw.
///
/// # Errors
///
/// Returns a framing, resource, or I/O error.
pub(super) fn read_raw_frame<R: Read>(
    mut compressed: Take<R>,
    metadata: &PackedStreamMetadata,
    length: u64,
    limits: PackedStreamLimits,
) -> Result<Vec<u8>, PackedStreamError> {
    validate_frame_sizes(
        compressed.limit(),
        metadata.compressed_size(),
        metadata.uncompressed_size(),
        limits,
    )?;
    let length = usize::try_from(length.min(metadata.uncompressed_size()))
        .map_err(|_| PackedStreamError::SizeOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| PackedStreamError::AllocationFailed { requested: length })?;
    bytes.resize(length, 0);
    compressed
        .read_exact(&mut bytes)
        .map_err(PackedStreamError::Io)?;
    Ok(bytes)
}

/// A whole number of rows so a block boundary is always a row boundary.
const LOG_ORDER_BLOCK_ROWS: usize = 8192;

/// Bytes per row in a log-order column: one little-endian `i64` delta.
const LOG_ORDER_ROW_BYTES: usize = 8;

/// Inflates a log-order column only as far as the rows a query can reach.
///
/// This is the column read to decide where the others stop, so it cannot be given a byte count
/// in advance. It is pulled a block at a time and abandoned once its running index reaches
/// `limit`, which is the first log event no accepted source file covers.
///
/// A record's log event index comes from a counter the writer only ever increments, and rows
/// enter a schema's table in arrival order, so the index rises down a table and a row past the
/// crossing cannot belong to an accepted file.
///
/// Returns the bytes read, always a whole number of rows, and the row the crossing happened at.
///
/// # Errors
///
/// As for a whole-frame read, minus what only the end of a frame can raise.
pub(super) fn decode_log_order_prefix<R: Read>(
    compressed: Take<R>,
    metadata: &PackedStreamMetadata,
    limit: u64,
    limits: PackedStreamLimits,
    raw_frames: bool,
) -> Result<(Vec<u8>, usize), PackedStreamError> {
    if is_raw_frame(raw_frames, metadata) {
        let mut bytes = read_raw_frame(compressed, metadata, metadata.uncompressed_size(), limits)?;
        let mut current = 0_i64;
        for (row, delta) in bytes
            .as_chunks::<LOG_ORDER_ROW_BYTES>()
            .0
            .iter()
            .enumerate()
        {
            current = current.wrapping_add(i64::from_le_bytes(*delta));
            if current >= 0 && current.cast_unsigned() >= limit {
                bytes.truncate(row * LOG_ORDER_ROW_BYTES);
                return Ok((bytes, row));
            }
        }
        let rows = bytes.len() / LOG_ORDER_ROW_BYTES;
        return Ok((bytes, rows));
    }
    if validate_frame_sizes(
        compressed.limit(),
        metadata.compressed_size(),
        metadata.uncompressed_size(),
        limits,
    )? {
        return Ok((Vec::new(), 0));
    }
    let total = usize::try_from(metadata.uncompressed_size())
        .map_err(|_| PackedStreamError::SizeOverflow)?;
    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|source| classify_decoder_error(source, metadata.uncompressed_size(), 0))?
        .single_frame();
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| PackedStreamError::AllocationFailed { requested: total })?;
    let block = LOG_ORDER_BLOCK_ROWS * LOG_ORDER_ROW_BYTES;
    let mut current = 0_i64;
    let mut scanned = 0_usize;
    while bytes.len() < total {
        let want = block.min(total - bytes.len());
        let start = bytes.len();
        let mut chunk = decode_prefix_output(&mut decoder, usize_to_u64(want)?)?;
        bytes.append(&mut chunk);
        for delta in bytes[start..].as_chunks::<LOG_ORDER_ROW_BYTES>().0 {
            current = current.wrapping_add(i64::from_le_bytes(*delta));
            scanned += 1;
            if current >= 0 && current.cast_unsigned() >= limit {
                // Drop the rows past the crossing so this column reports the same count as the
                // ones truncated to match it, rather than the whole block it was read in.
                let rows = scanned - 1;
                bytes.truncate(rows * LOG_ORDER_ROW_BYTES);
                return Ok((bytes, rows));
            }
        }
    }
    Ok((bytes, total / LOG_ORDER_ROW_BYTES))
}

/// Inflates only the first `prefix` bytes of one frame.
///
/// zstd decodes forward, so a reader that needs rows up to some point still pays for everything
/// before them but nothing after. The frame is left unfinished on purpose, so the trailing-byte
/// check a whole-frame read performs does not apply and the caller is responsible for treating the
/// missing tail as unread rather than as data.
///
/// # Errors
///
/// Returns the same framing, resource, and I/O errors a whole-frame read does, except those that
/// can only be raised at the end of a frame.
pub(super) fn decode_packed_stream_prefix<R: Read>(
    compressed: Take<R>,
    metadata: &PackedStreamMetadata,
    prefix: u64,
    limits: PackedStreamLimits,
) -> Result<Vec<u8>, PackedStreamError> {
    if prefix >= metadata.uncompressed_size() {
        return Ok(decode_frame(
            compressed,
            metadata.compressed_size(),
            metadata.uncompressed_size(),
            limits,
        )?
        .bytes);
    }
    if validate_frame_sizes(
        compressed.limit(),
        metadata.compressed_size(),
        metadata.uncompressed_size(),
        limits,
    )? {
        return Ok(Vec::new());
    }
    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|source| classify_decoder_error(source, prefix, 0))?
        .single_frame();
    decode_prefix_output(&mut decoder, prefix)
}

/// Reads exactly `prefix` decompressed bytes, leaving whatever follows unread.
fn decode_prefix_output<R: Read>(
    decoder: &mut R,
    prefix: u64,
) -> Result<Vec<u8>, PackedStreamError> {
    let allocation_size = usize::try_from(prefix).map_err(|_| PackedStreamError::SizeOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(allocation_size)
        .map_err(|_| PackedStreamError::AllocationFailed {
            requested: allocation_size,
        })?;
    let mut bounded = decoder.take(prefix);
    loop {
        match bounded.read_to_end(&mut bytes) {
            Ok(_) => break,
            Err(source) if io::ErrorKind::Interrupted == source.kind() => {}
            Err(source) => {
                return Err(classify_decoder_error(
                    source,
                    prefix,
                    usize_to_u64(bytes.len())?,
                ));
            }
        }
    }
    if bytes.len() != allocation_size {
        return Err(PackedStreamError::DecompressedSizeMismatch {
            advertised: prefix,
            actual: usize_to_u64(bytes.len())?,
        });
    }
    Ok(bytes)
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

/// Decompresses the wanted column frames of a separate-column stream into one buffer.
///
/// `offsets` holds each frame's compressed byte range and `sizes` its decompressed size;
/// `read_run` fills a buffer with the compressed bytes of a range. Consecutive wanted frames are
/// read with one call, and each frame decompresses straight into its place in the output through
/// one reused context, so a stream of many small columns costs a few reads rather than a reader,
/// a decoder and a copy per column. A column `wanted` excludes is left unloaded.
pub(super) fn decode_column_frames(
    offsets: &[(u64, u64)],
    sizes: &[usize],
    wanted: Option<&[bool]>,
    limits: PackedStreamLimits,
    raw_frames: bool,
    mut read_run: impl FnMut(Range<u64>, &mut Vec<u8>) -> io::Result<()>,
) -> Result<DecodedPackedStream, PackedStreamError> {
    let loaded: Vec<bool> = (0..sizes.len())
        .map(|index| wanted.is_none_or(|mask| mask.get(index).copied().unwrap_or(true)))
        .collect();
    let mut total = 0_usize;
    let mut layout = Vec::with_capacity(sizes.len());
    for (&size, &loaded) in sizes.iter().zip(&loaded) {
        let stored = if loaded { size } else { 0 };
        total = total
            .checked_add(stored)
            .ok_or(PackedStreamError::SizeOverflow)?;
        layout.push(ColumnSlot {
            clp_layout: None, numeric_encoding: None,
            size,
            stored,
            loaded,
        });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| PackedStreamError::AllocationFailed { requested: total })?;

    let mut context = zstd::zstd_safe::DCtx::create();
    let mut compressed = Vec::new();
    let mut index = 0;
    while index < sizes.len() {
        if !loaded[index] {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < sizes.len() && loaded[end] {
            end += 1;
        }
        let run_start = offsets[index].0;
        read_run(run_start..offsets[end - 1].1, &mut compressed).map_err(PackedStreamError::Io)?;
        for frame in index..end {
            let (start, stop) = offsets[frame];
            let input = usize::try_from(start - run_start)
                .ok()
                .zip(usize::try_from(stop - run_start).ok())
                .and_then(|(start, stop)| compressed.get(start..stop))
                .ok_or(PackedStreamError::SizeOverflow)?;
            decode_frame_into(
                &mut context, input, &mut bytes, sizes[frame], limits, raw_frames,
            )?;
        }
        index = end;
    }
    Ok(DecodedPackedStream {
        bytes,
        column_layout: Some(layout),
    })
}

/// Frames' compressed byte ranges, and separately their decompressed sizes.
pub(super) type FrameRanges = (Vec<(u64, u64)>, Vec<usize>);

/// Each frame's compressed byte range and decompressed size in a grouped stream that starts at
/// `start`, its frames stored back to back.
pub(super) fn grouped_frame_ranges(
    grouped: &super::GroupedStream,
    start: u64,
) -> Result<FrameRanges, PackedStreamError> {
    let mut offsets = Vec::with_capacity(grouped.frames().len());
    let mut sizes = Vec::with_capacity(grouped.frames().len());
    let mut cursor = start;
    for frame in grouped.frames() {
        let end = cursor
            .checked_add(frame.compressed_size())
            .ok_or(PackedStreamError::SizeOverflow)?;
        offsets.push((cursor, end));
        sizes.push(
            usize::try_from(frame.uncompressed_size())
                .map_err(|_| PackedStreamError::SizeOverflow)?,
        );
        cursor = end;
    }
    Ok((offsets, sizes))
}

/// Appends a whole frame into reserved capacity. Zstd's safe Cursor<Vec> WriteBuf
/// initializes and exposes only the bytes actually written, avoiding a zero-fill pass.
/// The allocation bounds the entire selected stream; each frame's result must match its size.
fn decode_frame_into(
    context: &mut zstd::zstd_safe::DCtx<'_>,
    compressed: &[u8],
    output: &mut Vec<u8>,
    expected_size: usize,
    limits: PackedStreamLimits,
    raw_frames: bool,
) -> Result<(), PackedStreamError> {
    let compressed_size = usize_to_u64(compressed.len())?;
    let advertised = usize_to_u64(expected_size)?;
    if validate_frame_sizes(compressed_size, compressed_size, advertised, limits)? {
        return Ok(());
    }
    if raw_frames && compressed.len() == expected_size {
        output.extend_from_slice(compressed);
        return Ok(());
    }
    let zstd_error = |code| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            zstd::zstd_safe::get_error_name(code),
        )
    };
    let frame_size = zstd::zstd_safe::find_frame_compressed_size(compressed).map_err(|code| {
        PackedStreamError::TruncatedFrame {
            advertised,
            actual: 0,
            source: io::Error::new(
                io::ErrorKind::UnexpectedEof,
                zstd::zstd_safe::get_error_name(code),
            ),
        }
    })?;
    if frame_size < compressed.len() {
        return Err(PackedStreamError::TrailingCompressedData {
            remaining: usize_to_u64(compressed.len() - frame_size)?,
        });
    }
    let start = output.len();
    let mut cursor = io::Cursor::new(output);
    cursor.set_position(usize_to_u64(start)?);
    let written = context
        .decompress(&mut cursor, compressed)
        .map_err(|code| PackedStreamError::Io(zstd_error(code)))?;
    if written != expected_size {
        return Err(PackedStreamError::DecompressedSizeMismatch {
            advertised,
            actual: usize_to_u64(written)?,
        });
    }
    Ok(())
}

impl DecodedPackedStream {
    /// Assembles a stream from per-column pieces: `None` for a column left unloaded.
    ///
    /// Only the bytes actually read are stored, so a skipped or early-stopped column costs
    /// nothing. The layout records what each column would have held, so the decoder can tell how
    /// many rows it got.
    #[must_use]
    pub fn from_columns(columns: Vec<Option<Vec<u8>>>, sizes: &[usize]) -> Self {
        let mut bytes = Vec::new();
        let mut layout = Vec::with_capacity(sizes.len());
        for (column, &size) in columns.into_iter().zip(sizes) {
            match column {
                Some(data) => {
                    debug_assert!(data.len() <= size);
                    let stored = data.len().min(size);
                    bytes.extend_from_slice(&data[..stored]);
                    layout.push(ColumnSlot {
            clp_layout: None, numeric_encoding: None,
                        size,
                        stored,
                        loaded: true,
                    });
                }
                None => {
                    layout.push(ColumnSlot {
            clp_layout: None, numeric_encoding: None,
                        size,
                        stored: 0,
                        loaded: false,
                    });
                }
            }
        }
        Self {
            bytes,
            column_layout: Some(layout),
        }
    }

    /// Combines disjoint projections of the same separate-column stream.
    ///
    /// The caller must supply projections from the same physical stream, at the same
    /// decoding stage. Loaded slots retain their representation and logical size.
    /// Returns `None` for missing/inconsistent layouts or overlapping loaded columns.
    #[must_use]
    pub fn merge_disjoint_columns(self, other: Self) -> Option<Self> {
        let left = self.column_layout.as_ref()?;
        let right = other.column_layout.as_ref()?;
        if left.len() != right.len() { return None; }
        for (a, b) in left.iter().zip(right) {
            if (a.loaded && b.loaded) || (!a.loaded && a.stored != 0)
                || (!b.loaded && b.stored != 0) { return None; }
        }
        let total = self.bytes.len().checked_add(other.bytes.len())?;
        let mut bytes = Vec::with_capacity(total);
        let mut layout = Vec::with_capacity(left.len());
        let (mut left_offset, mut right_offset) = (0usize, 0usize);
        for (a, b) in left.iter().zip(right) {
            let left_end = left_offset.checked_add(a.stored)?;
            let right_end = right_offset.checked_add(b.stored)?;
            let left_bytes = self.bytes.get(left_offset..left_end)?;
            let right_bytes = other.bytes.get(right_offset..right_end)?;
            let (slot, data) = if a.loaded { (a.clone(), left_bytes) } else { (b.clone(), right_bytes) };
            bytes.extend_from_slice(data);
            layout.push(slot);
            left_offset = left_end;
            right_offset = right_end;
        }
        if left_offset != self.bytes.len() || right_offset != other.bytes.len() { return None; }
        Some(Self::from_column_buffer(bytes, layout))
    }

    /// Takes already assembled canonical columns without concatenating them again.
    pub(crate) fn from_column_buffer(bytes: Vec<u8>, layout: Vec<ColumnSlot>) -> Self {
        debug_assert_eq!(bytes.len(), layout.iter().map(|slot| slot.stored).sum::<usize>());
        Self { bytes, column_layout: Some(layout) }
    }

    /// A whole stream of tables laid out back to back, as a shared frame decodes to.
    #[must_use]
    pub(crate) const fn from_tables(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            column_layout: None,
        }
    }

    pub(crate) fn column_layout_mut(&mut self) -> Option<&mut [ColumnSlot]> {
        self.column_layout.as_deref_mut()
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
    fn column_frames_append_into_spare_capacity_and_reject_wrong_sizes() {
        let a = compress(b"first column");
        let b = compress(b"second column");
        let mut context = zstd::zstd_safe::DCtx::create();
        let mut output = Vec::with_capacity(100);
        decode_frame_into(&mut context, &a, &mut output, 12, TEST_LIMITS, false).unwrap();
        decode_frame_into(&mut context, &b, &mut output, 13, TEST_LIMITS, false).unwrap();
        decode_frame_into(&mut context, b"raw", &mut output, 3, TEST_LIMITS, true).unwrap();
        assert_eq!(output, b"first columnsecond columnraw");
        // Even spare capacity beyond this frame cannot hide an advertised-size mismatch.
        assert!(decode_frame_into(&mut context, &a, &mut output, 11, TEST_LIMITS, false).is_err());
        let mut truncated = Vec::with_capacity(100);
        assert!(decode_frame_into(&mut context, &a[..a.len()-1], &mut truncated, 12, TEST_LIMITS, false).is_err());
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
        assert_eq!(1024 * 1024 * 1024, defaults.max_compressed_size());
        assert_eq!(8 * 1024 * 1024 * 1024, defaults.max_decompressed_size());
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

#[cfg(test)]
mod projected_merge_tests {
    use super::*;
    use crate::archive::NumericEncoding;

    #[test]
    fn merge_keeps_compact_layouts_and_interleaved_columns() {
        let compact = ColumnSlot { clp_layout: None, size: 800, stored: 8, loaded: true,
            numeric_encoding: Some(NumericEncoding::Constant) };
        let skipped = ColumnSlot { clp_layout: None, size: 9, stored: 0, loaded: false, numeric_encoding: None };
        let left = DecodedPackedStream::from_column_buffer(42i64.to_le_bytes().to_vec(),
            vec![compact.clone(), skipped.clone(), skipped]);
        let right = DecodedPackedStream::from_columns(vec![None, Some(vec![1, 2, 3]), None], &[9, 3, 9]);
        let merged = left.merge_disjoint_columns(right).unwrap();
        assert_eq!(&merged.as_bytes()[..8], &42i64.to_le_bytes());
        assert_eq!(&merged.as_bytes()[8..], &[1, 2, 3]);
        let layout = merged.column_layout().unwrap();
        assert_eq!(layout[0], compact);
        assert_eq!(layout[0].stored_rows(100), 100);
        assert_eq!((layout[1].stored, layout[1].loaded), (3, true));
        assert!(!layout[2].loaded);
    }

    #[test]
    fn merge_rejects_overlap_and_invalid_layouts() {
        let loaded = || DecodedPackedStream::from_columns(vec![Some(vec![1])], &[1]);
        let skipped = || DecodedPackedStream::from_columns(vec![None], &[1]);
        assert!(loaded().merge_disjoint_columns(loaded()).is_none());
        assert!(loaded().merge_disjoint_columns(DecodedPackedStream::from_tables(vec![])).is_none());
        assert!(loaded().merge_disjoint_columns(DecodedPackedStream::from_columns(vec![], &[])).is_none());
        let mut corrupt = skipped();
        corrupt.column_layout.as_mut().unwrap()[0].stored = usize::MAX;
        assert!(loaded().merge_disjoint_columns(corrupt).is_none());
        let merged = skipped().merge_disjoint_columns(loaded()).unwrap();
        assert_eq!(merged.as_bytes(), &[1]);
    }
}
