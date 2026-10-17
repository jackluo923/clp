use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Read;
use std::io::Take;
use std::io::{self};
use std::ops::Range;

const ENTRY_LENGTH_SIZE: u64 = 8;
const DECODE_BUFFER_SIZE: usize = 128 * 1024;
const DECODE_GROWTH_SIZE: usize = 8 * 1024 * 1024;
const INTEGER_PLACEHOLDER: u8 = 0x11;
const DICTIONARY_PLACEHOLDER: u8 = 0x12;
const FLOAT_PLACEHOLDER: u8 = 0x13;
const ESCAPE_MARKER: u8 = b'\\';

/// The validated encoded-variable order for one escaped CLP logtype.
///
/// Keeping this compact sequence alongside the dictionary avoids rescanning every literal byte in
/// a logtype for every record that references it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum LogTypeVariableKind {
    /// A signed-integer placeholder.
    Integer,
    /// A variable-dictionary ID placeholder.
    Dictionary,
    /// A custom encoded-float placeholder.
    Float,
}

/// Resource limits applied while decoding a variable, logtype, or array dictionary section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DictionaryLimits {
    compressed: u64,
    decompressed: u64,
    entries: u64,
    entry_bytes: u64,
    total_value_bytes: u64,
}

impl DictionaryLimits {
    /// Creates explicit dictionary resource limits.
    ///
    /// The compressed limit includes the raw eight-byte entry-count prefix. The decompressed limit
    /// applies only to bytes inside the optional zstd frame, including each entry's eight-byte
    /// length field.
    #[must_use]
    pub const fn new(
        max_compressed_size: u64,
        max_decompressed_size: u64,
        max_entries: u64,
        max_entry_size: u64,
        max_total_value_bytes: u64,
    ) -> Self {
        Self {
            compressed: max_compressed_size,
            decompressed: max_decompressed_size,
            entries: max_entries,
            entry_bytes: max_entry_size,
            total_value_bytes: max_total_value_bytes,
        }
    }

    /// Maximum complete section bytes accepted, including the raw entry-count prefix.
    #[must_use]
    pub const fn max_compressed_size(self) -> u64 {
        self.compressed
    }

    /// Maximum bytes accepted after decompression, including entry-length fields.
    #[must_use]
    pub const fn max_decompressed_size(self) -> u64 {
        self.decompressed
    }

    /// Maximum number of dictionary entries accepted.
    #[must_use]
    pub const fn max_entries(self) -> u64 {
        self.entries
    }

    /// Maximum bytes accepted in one dictionary value.
    #[must_use]
    pub const fn max_entry_size(self) -> u64 {
        self.entry_bytes
    }

    /// Maximum cumulative value bytes accepted across the dictionary.
    #[must_use]
    pub const fn max_total_value_bytes(self) -> u64 {
        self.total_value_bytes
    }
}

impl Default for DictionaryLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(
            512 * MEBIBYTE,
            1024 * MEBIBYTE,
            16 * 1024 * 1024,
            64 * MEBIBYTE,
            1024 * MEBIBYTE,
        )
    }
}

/// One of the three independent CLP-S dictionary ID spaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DictionarySection {
    /// Arbitrary variable values in `/var.dict`.
    Variable,
    /// Escaped CLP logtypes in `/log.dict`.
    LogType,
    /// Escaped unstructured-array logtypes in `/array.dict`.
    Array,
}

impl DictionarySection {
    /// Returns the canonical archive section name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Variable => "/var.dict",
            Self::LogType => "/log.dict",
            Self::Array => "/array.dict",
        }
    }
}

impl Display for DictionarySection {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DictionaryStorage {
    bytes: Vec<u8>,
    ranges: Vec<Range<usize>>,
}

impl DictionaryStorage {
    const fn entry_count(&self) -> usize {
        self.ranges.len()
    }

    fn value(&self, id: u64) -> Option<&[u8]> {
        let index = usize::try_from(id).ok()?;
        let range = self.ranges.get(index)?;
        Some(&self.bytes[range.clone()])
    }
}

/// A decoded `/var.dict` whose implicit IDs are zero-based wire positions.
///
/// Values are retained as arbitrary bytes in one contiguous allocation to avoid one allocation
/// per dictionary entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VariableDictionary {
    storage: DictionaryStorage,
}

impl VariableDictionary {
    /// A dictionary with no entries, for a catalog whose dictionaries were not loaded.
    ///
    /// Every lookup misses. A caller that chose not to load the dictionaries has established that
    /// nothing it reads or filters resolves to a dictionary-backed column, so no lookup happens.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            storage: DictionaryStorage {
                bytes: Vec::new(),
                ranges: Vec::new(),
            },
        }
    }

    /// Returns the number of variable dictionary entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.storage.entry_count()
    }

    /// Returns whether the variable dictionary has no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        0 == self.len()
    }

    /// Returns an entry by its implicit unsigned 64-bit wire ID.
    #[must_use]
    pub fn entry(&self, id: u64) -> Option<VariableDictionaryEntry<'_>> {
        self.storage
            .value(id)
            .map(|value| VariableDictionaryEntry { id, value })
    }

    /// Iterates entries in implicit wire-ID order.
    pub fn entries(&self) -> impl Iterator<Item = VariableDictionaryEntry<'_>> {
        (0_u64..)
            .zip(&self.storage.ranges)
            .map(|(id, range)| VariableDictionaryEntry {
                id,
                value: &self.storage.bytes[range.clone()],
            })
    }
}

/// A borrowed variable-dictionary entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VariableDictionaryEntry<'a> {
    id: u64,
    value: &'a [u8],
}

impl<'a> VariableDictionaryEntry<'a> {
    /// Returns the implicit unsigned 64-bit wire ID.
    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }

    /// Returns the exact length-delimited value bytes.
    #[must_use]
    pub const fn value(self) -> &'a [u8] {
        self.value
    }
}

/// Counts of unescaped encoded-variable placeholders and escape sequences in one CLP logtype.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LogTypePlaceholderCounts {
    integers: u64,
    dictionaries: u64,
    floats: u64,
    escape_sequences: u64,
}

impl LogTypePlaceholderCounts {
    /// Returns the number of unescaped signed-integer placeholders (`0x11`).
    #[must_use]
    pub const fn integer(self) -> u64 {
        self.integers
    }

    /// Returns the number of unescaped variable-dictionary placeholders (`0x12`).
    #[must_use]
    pub const fn dictionary(self) -> u64 {
        self.dictionaries
    }

    /// Returns the number of unescaped custom-float placeholders (`0x13`).
    #[must_use]
    pub const fn float(self) -> u64 {
        self.floats
    }

    /// Returns the number of encoded variables consumed when reconstructing this logtype.
    #[must_use]
    pub const fn encoded_variables(self) -> u64 {
        self.integers + self.dictionaries + self.floats
    }

    /// Returns all marker positions, including escape markers and encoded variables.
    ///
    /// This matches the current C++ reader's `get_num_placeholders` definition. Use
    /// [`Self::encoded_variables`] for the number of column values consumed.
    #[must_use]
    pub const fn placeholders(self) -> u64 {
        self.encoded_variables() + self.escape_sequences
    }

    /// Returns the number of escape-marker and literal-byte pairs.
    #[must_use]
    pub const fn escape_sequences(self) -> u64 {
        self.escape_sequences
    }
}

/// A decoded `/log.dict`.
///
/// Decoding records only where each entry lies. An entry's escape structure and placeholders are
/// read when it is used, so opening an archive does not scan every logtype it holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogTypeDictionary {
    storage: DictionaryStorage,
}

impl LogTypeDictionary {
    /// A dictionary with no entries; see [`VariableDictionary::empty`].
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            storage: DictionaryStorage {
                bytes: Vec::new(),
                ranges: Vec::new(),
            },
        }
    }

    /// Returns the number of logtype dictionary entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.storage.entry_count()
    }

    /// Returns whether the logtype dictionary has no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        0 == self.len()
    }

    /// Returns an entry by its implicit zero-based wire ID.
    #[must_use]
    pub fn entry(&self, id: u64) -> Option<LogTypeDictionaryEntry<'_>> {
        logtype_entry(&self.storage, id)
    }

    /// Iterates entries in implicit wire-ID order.
    pub fn entries(&self) -> impl Iterator<Item = LogTypeDictionaryEntry<'_>> {
        logtype_entries(&self.storage)
    }
}

/// A decoded `/array.dict`, recorded the same way as [`LogTypeDictionary`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArrayDictionary {
    storage: DictionaryStorage,
}

impl ArrayDictionary {
    /// A dictionary with no entries; see [`VariableDictionary::empty`].
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            storage: DictionaryStorage {
                bytes: Vec::new(),
                ranges: Vec::new(),
            },
        }
    }

    /// Returns the number of unstructured-array logtype entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.storage.entry_count()
    }

    /// Returns whether the array dictionary has no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        0 == self.len()
    }

    /// Returns an entry by its implicit zero-based wire ID.
    #[must_use]
    pub fn entry(&self, id: u64) -> Option<LogTypeDictionaryEntry<'_>> {
        logtype_entry(&self.storage, id)
    }

    /// Iterates entries in implicit wire-ID order.
    pub fn entries(&self) -> impl Iterator<Item = LogTypeDictionaryEntry<'_>> {
        logtype_entries(&self.storage)
    }
}

/// A borrowed escaped CLP logtype entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogTypeDictionaryEntry<'a> {
    id: u64,
    escaped_value: &'a [u8],
}

impl<'a> LogTypeDictionaryEntry<'a> {
    /// Returns the implicit zero-based wire ID.
    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }

    /// Returns the exact escaped logtype bytes stored in the archive.
    #[must_use]
    pub const fn escaped_value(self) -> &'a [u8] {
        self.escaped_value
    }

    /// Counts the placeholders and escape sequences, scanning the entry on each call.
    ///
    /// A trailing escape marker with nothing after it is counted as an escape sequence; use
    /// [`Self::variable_kinds_into`] to reject it.
    #[must_use]
    pub fn placeholder_counts(self) -> LogTypePlaceholderCounts {
        let mut counts = LogTypePlaceholderCounts::default();
        let _ = scan_logtype(self.escaped_value, &mut counts, &mut |_| {});
        counts
    }

    /// Appends the encoded-variable kinds in logtype order to `kinds`, after checking that every
    /// escape marker escapes a byte.
    pub(super) fn variable_kinds_into(
        self,
        kinds: &mut Vec<LogTypeVariableKind>,
    ) -> Result<(), DictionaryError> {
        let mut counts = LogTypePlaceholderCounts::default();
        scan_logtype(self.escaped_value, &mut counts, &mut |kind| {
            kinds.push(kind);
        })
        .map_err(|offset| DictionaryError::DanglingEscape {
            entry_id: self.id,
            offset,
        })
    }
}

fn logtype_entry(storage: &DictionaryStorage, id: u64) -> Option<LogTypeDictionaryEntry<'_>> {
    Some(LogTypeDictionaryEntry {
        id,
        escaped_value: storage.value(id)?,
    })
}

fn logtype_entries(
    storage: &DictionaryStorage,
) -> impl Iterator<Item = LogTypeDictionaryEntry<'_>> {
    (0_u64..)
        .zip(storage.ranges.iter())
        .map(|(id, range)| LogTypeDictionaryEntry {
            id,
            escaped_value: &storage.bytes[range.clone()],
        })
}

pub(super) fn decode_variable_dictionary<R: Read>(
    section: Take<R>,
    limits: DictionaryLimits,
) -> Result<VariableDictionary, DictionaryError> {
    Ok(VariableDictionary {
        storage: decode_dictionary(section, limits)?,
    })
}

pub(super) fn decode_logtype_dictionary<R: Read>(
    section: Take<R>,
    limits: DictionaryLimits,
) -> Result<LogTypeDictionary, DictionaryError> {
    Ok(LogTypeDictionary {
        storage: decode_dictionary(section, limits)?,
    })
}

pub(super) fn decode_array_dictionary<R: Read>(
    section: Take<R>,
    limits: DictionaryLimits,
) -> Result<ArrayDictionary, DictionaryError> {
    Ok(ArrayDictionary {
        storage: decode_dictionary(section, limits)?,
    })
}

fn decode_dictionary<R: Read>(
    mut section: Take<R>,
    limits: DictionaryLimits,
) -> Result<DictionaryStorage, DictionaryError> {
    let compressed_size = section.limit();
    if compressed_size > limits.compressed {
        return Err(DictionaryError::CompressedSectionTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }

    let entry_count = read_u64(&mut section)?;
    if entry_count > limits.entries {
        return Err(DictionaryError::EntryCountTooLarge {
            actual: entry_count,
            limit: limits.entries,
        });
    }

    if 0 == entry_count {
        let remaining = section.limit();
        if 0 != remaining {
            return Err(DictionaryError::TrailingCompressedData { remaining });
        }
        return Ok(DictionaryStorage {
            bytes: Vec::new(),
            ranges: Vec::new(),
        });
    }

    let minimum_decompressed_size = entry_count
        .checked_mul(ENTRY_LENGTH_SIZE)
        .ok_or(DictionaryError::SizeOverflow)?;
    if minimum_decompressed_size > limits.decompressed {
        return Err(DictionaryError::DecompressedSectionTooLarge {
            actual: minimum_decompressed_size,
            limit: limits.decompressed,
        });
    }

    let entry_count = usize::try_from(entry_count).map_err(|_| DictionaryError::SizeOverflow)?;
    let mut dictionary = reserve_dictionary(entry_count)?;

    // The section is read whole and checked to hold exactly one frame. A frame that records its
    // decompressed size, as every dictionary compressed with a pledged size does, is then
    // decompressed in one call into a buffer of exactly that size; streaming it through a staging
    // buffer into a growing one copied every byte about three times.
    let mut compressed = Vec::new();
    let compressed_len =
        usize::try_from(section.limit()).map_err(|_| DictionaryError::SizeOverflow)?;
    compressed
        .try_reserve_exact(compressed_len)
        .map_err(|_| DictionaryError::AllocationFailed {
            requested: compressed_len,
        })?;
    section
        .read_to_end(&mut compressed)
        .map_err(DictionaryError::Io)?;
    let frame_len = zstd::zstd_safe::find_frame_compressed_size(&compressed).map_err(|_| {
        DictionaryError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary is not one complete zstd frame",
        ))
    })?;
    let remaining_compressed =
        u64::try_from(compressed.len() - frame_len).map_err(|_| DictionaryError::SizeOverflow)?;
    let frame = &compressed[..frame_len];
    let decompressed = match zstd::zstd_safe::get_frame_content_size(frame) {
        Ok(Some(size)) => decode_sized_frame(frame, size, limits.decompressed)?,
        _ => {
            let mut decoder = zstd::stream::read::Decoder::with_buffer(frame)
                .map_err(DictionaryError::Io)?
                .single_frame();
            decode_frame(&mut decoder, minimum_decompressed_size, limits.decompressed)?
        }
    };

    decode_entries(decompressed, entry_count, limits, &mut dictionary)?;
    if 0 != remaining_compressed {
        return Err(DictionaryError::TrailingCompressedData {
            remaining: remaining_compressed,
        });
    }

    Ok(dictionary)
}

/// Decompresses one frame whose header records its decompressed size, in one call, into a buffer
/// of exactly that size.
fn decode_sized_frame(
    frame: &[u8],
    size: u64,
    maximum_size: u64,
) -> Result<Vec<u8>, DictionaryError> {
    if size > maximum_size {
        return Err(DictionaryError::DecompressedSectionTooLarge {
            actual: size,
            limit: maximum_size,
        });
    }
    let size = usize::try_from(size).map_err(|_| DictionaryError::SizeOverflow)?;
    let mut decompressed = Vec::new();
    decompressed
        .try_reserve_exact(size)
        .map_err(|_| DictionaryError::AllocationFailed { requested: size })?;
    let written = zstd::bulk::Decompressor::new()
        .and_then(|mut decompressor| decompressor.decompress_to_buffer(frame, &mut decompressed))
        .map_err(DictionaryError::Io)?;
    if written != size {
        return Err(DictionaryError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary frame decompressed to a size other than its header records",
        )));
    }
    Ok(decompressed)
}

fn decode_frame<R: Read>(
    decoder: &mut R,
    minimum_size: u64,
    maximum_size: u64,
) -> Result<Vec<u8>, DictionaryError> {
    let minimum_size = usize::try_from(minimum_size).map_err(|_| DictionaryError::SizeOverflow)?;
    let mut decompressed = Vec::new();
    reserve_decompressed(&mut decompressed, minimum_size, maximum_size)?;

    let mut buffer = Vec::new();
    buffer.try_reserve_exact(DECODE_BUFFER_SIZE).map_err(|_| {
        DictionaryError::AllocationFailed {
            requested: DECODE_BUFFER_SIZE,
        }
    })?;
    buffer.resize(DECODE_BUFFER_SIZE, 0);

    loop {
        let read = decoder.read(&mut buffer).map_err(DictionaryError::Io)?;
        if 0 == read {
            return Ok(decompressed);
        }
        let resulting_size = decompressed
            .len()
            .checked_add(read)
            .ok_or(DictionaryError::SizeOverflow)?;
        let resulting_size_u64 =
            u64::try_from(resulting_size).map_err(|_| DictionaryError::SizeOverflow)?;
        if resulting_size_u64 > maximum_size {
            return Err(DictionaryError::DecompressedSectionTooLarge {
                actual: resulting_size_u64,
                limit: maximum_size,
            });
        }
        reserve_decompressed(&mut decompressed, resulting_size, maximum_size)?;
        decompressed.extend_from_slice(&buffer[..read]);
    }
}

fn reserve_decompressed(
    decompressed: &mut Vec<u8>,
    required_len: usize,
    maximum_size: u64,
) -> Result<(), DictionaryError> {
    if required_len <= decompressed.capacity() {
        return Ok(());
    }
    let rounded = required_len
        .checked_next_multiple_of(DECODE_GROWTH_SIZE)
        .unwrap_or(required_len);
    let maximum_size = usize::try_from(maximum_size).unwrap_or(usize::MAX);
    let target_capacity = rounded.min(maximum_size).max(required_len);
    let additional = target_capacity
        .checked_sub(decompressed.len())
        .ok_or(DictionaryError::SizeOverflow)?;
    decompressed
        .try_reserve_exact(additional)
        .map_err(|_| DictionaryError::AllocationFailed {
            requested: additional,
        })
}

fn reserve_dictionary(entry_count: usize) -> Result<DictionaryStorage, DictionaryError> {
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(entry_count)
        .map_err(|_| DictionaryError::AllocationFailed {
            requested: entry_count,
        })?;
    Ok(DictionaryStorage {
        bytes: Vec::new(),
        ranges,
    })
}

fn decode_entries(
    decompressed: Vec<u8>,
    entry_count: usize,
    limits: DictionaryLimits,
    decoded: &mut DictionaryStorage,
) -> Result<(), DictionaryError> {
    let mut total_value_bytes = 0_u64;
    let mut offset = 0_usize;

    for entry_id in 0..entry_count {
        let length_end = offset
            .checked_add(usize::try_from(ENTRY_LENGTH_SIZE).expect("entry length fits usize"))
            .ok_or(DictionaryError::SizeOverflow)?;
        let length_bytes = decompressed.get(offset..length_end).ok_or_else(|| {
            DictionaryError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "dictionary entry length is truncated",
            ))
        })?;
        let entry_size = u64::from_le_bytes(
            length_bytes
                .try_into()
                .expect("validated dictionary length slice has eight bytes"),
        );
        if entry_size > limits.entry_bytes {
            return Err(DictionaryError::EntryTooLarge {
                entry_id: wire_id(entry_id)?,
                actual: entry_size,
                limit: limits.entry_bytes,
            });
        }
        total_value_bytes = total_value_bytes
            .checked_add(entry_size)
            .ok_or(DictionaryError::SizeOverflow)?;
        if total_value_bytes > limits.total_value_bytes {
            return Err(DictionaryError::TotalValueBytesTooLarge {
                actual: total_value_bytes,
                limit: limits.total_value_bytes,
            });
        }

        let entry_size = usize::try_from(entry_size).map_err(|_| DictionaryError::SizeOverflow)?;
        let entry_start = length_end;
        let entry_end = entry_start
            .checked_add(entry_size)
            .ok_or(DictionaryError::SizeOverflow)?;
        if entry_end > decompressed.len() {
            return Err(DictionaryError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "dictionary entry is truncated",
            )));
        }
        decoded.ranges.push(entry_start..entry_end);
        offset = entry_end;
    }
    if offset != decompressed.len() {
        return Err(DictionaryError::TrailingDecompressedData);
    }
    decoded.bytes = decompressed;
    Ok(())
}

/// Walks one escaped logtype, counting into `counts` and reporting each encoded variable's kind in
/// order. Returns the offset of a trailing escape marker that escapes nothing, after counting it as
/// an escape sequence.
fn scan_logtype(
    escaped_value: &[u8],
    counts: &mut LogTypePlaceholderCounts,
    on_variable: &mut dyn FnMut(LogTypeVariableKind),
) -> Result<(), u64> {
    let mut offset = 0_usize;
    while offset < escaped_value.len() {
        match escaped_value[offset] {
            ESCAPE_MARKER => {
                counts.escape_sequences += 1;
                if offset + 1 == escaped_value.len() {
                    return Err(u64::try_from(offset).unwrap_or(u64::MAX));
                }
                offset += 2;
            }
            INTEGER_PLACEHOLDER => {
                counts.integers += 1;
                on_variable(LogTypeVariableKind::Integer);
                offset += 1;
            }
            DICTIONARY_PLACEHOLDER => {
                counts.dictionaries += 1;
                on_variable(LogTypeVariableKind::Dictionary);
                offset += 1;
            }
            FLOAT_PLACEHOLDER => {
                counts.floats += 1;
                on_variable(LogTypeVariableKind::Float);
                offset += 1;
            }
            _ => offset += 1,
        }
    }
    Ok(())
}

fn wire_id(value: usize) -> Result<u64, DictionaryError> {
    u64::try_from(value).map_err(|_| DictionaryError::SizeOverflow)
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64, DictionaryError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(DictionaryError::Io)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Failure to decode or structurally validate a CLP-S dictionary section.
#[derive(Debug)]
#[non_exhaustive]
pub enum DictionaryError {
    /// The complete compressed section exceeds the configured limit.
    CompressedSectionTooLarge {
        /// Actual complete section bytes, including the raw entry count.
        actual: u64,
        /// Configured maximum bytes.
        limit: u64,
    },
    /// The decompressed entry sequence exceeds the configured limit.
    DecompressedSectionTooLarge {
        /// Minimum or fully declared decompressed bytes.
        actual: u64,
        /// Configured maximum bytes.
        limit: u64,
    },
    /// The entry count exceeds the configured limit.
    EntryCountTooLarge {
        /// Declared entry count.
        actual: u64,
        /// Configured maximum count.
        limit: u64,
    },
    /// One entry exceeds the configured per-entry byte limit.
    EntryTooLarge {
        /// Implicit zero-based wire ID.
        entry_id: u64,
        /// Declared entry bytes.
        actual: u64,
        /// Configured maximum bytes.
        limit: u64,
    },
    /// Cumulative value bytes exceed the configured limit.
    TotalValueBytesTooLarge {
        /// Cumulative declared value bytes.
        actual: u64,
        /// Configured maximum bytes.
        limit: u64,
    },
    /// A logtype ends with an escape marker that has no following literal byte.
    DanglingEscape {
        /// Implicit zero-based wire ID.
        entry_id: u64,
        /// Byte offset of the dangling escape marker.
        offset: u64,
    },
    /// Input or zstd decompression failed.
    Io(io::Error),
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded allocation could not be reserved.
    AllocationFailed {
        /// Elements or bytes requested by the failed reservation.
        requested: usize,
    },
    /// Decompressed bytes followed the declared entry sequence.
    TrailingDecompressedData,
    /// Compressed bytes followed the permitted frame, or followed an empty dictionary prefix.
    TrailingCompressedData {
        /// Bytes remaining inside the bounded section.
        remaining: u64,
    },
    /// The supplied metadata did not contain the requested dictionary section.
    MissingSection {
        /// Requested dictionary ID space.
        section: DictionarySection,
    },
    /// The requested dictionary range was outside this archive's files region.
    SectionOutsideArchive {
        /// Requested dictionary ID space.
        section: DictionarySection,
    },
}

impl Display for DictionaryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedSectionTooLarge { actual, limit } => write!(
                formatter,
                "compressed dictionary section size {actual} exceeds limit {limit}"
            ),
            Self::DecompressedSectionTooLarge { actual, limit } => write!(
                formatter,
                "decompressed dictionary section size {actual} exceeds limit {limit}"
            ),
            Self::EntryCountTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "dictionary entry count {actual} exceeds limit {limit}"
                )
            }
            Self::EntryTooLarge {
                entry_id,
                actual,
                limit,
            } => write!(
                formatter,
                "dictionary entry {entry_id} size {actual} exceeds limit {limit}"
            ),
            Self::TotalValueBytesTooLarge { actual, limit } => write!(
                formatter,
                "dictionary value bytes {actual} exceed limit {limit}"
            ),
            Self::DanglingEscape { entry_id, offset } => write!(
                formatter,
                "logtype dictionary entry {entry_id} has a dangling escape at byte {offset}"
            ),
            Self::Io(error) => write!(formatter, "dictionary I/O failed: {error}"),
            Self::SizeOverflow => formatter.write_str("dictionary size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve bounded dictionary allocation of {requested} elements or bytes"
            ),
            Self::TrailingDecompressedData => {
                formatter.write_str("data follows the declared dictionary entries")
            }
            Self::TrailingCompressedData { remaining } => write!(
                formatter,
                "{remaining} compressed bytes follow the permitted dictionary content"
            ),
            Self::MissingSection { section } => {
                write!(formatter, "archive metadata has no {section} section")
            }
            Self::SectionOutsideArchive { section } => {
                write!(formatter, "{section} is outside the archive files region")
            }
        }
    }
}

impl Error for DictionaryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn section(entries: &[&[u8]]) -> Vec<u8> {
        let mut encoded = u64::try_from(entries.len())
            .expect("test entry count fits u64")
            .to_le_bytes()
            .to_vec();
        if entries.is_empty() {
            return encoded;
        }

        let mut decompressed = Vec::new();
        for entry in entries {
            decompressed.extend_from_slice(
                &u64::try_from(entry.len())
                    .expect("test entry size fits u64")
                    .to_le_bytes(),
            );
            decompressed.extend_from_slice(entry);
        }
        encoded.extend_from_slice(
            &zstd::stream::encode_all(decompressed.as_slice(), 3)
                .expect("compress test dictionary"),
        );
        encoded
    }

    fn take(bytes: &[u8]) -> Take<Cursor<&[u8]>> {
        Cursor::new(bytes).take(u64::try_from(bytes.len()).expect("test section size fits u64"))
    }

    #[test]
    fn decodes_arbitrary_variable_bytes_with_implicit_ids() {
        let bytes = section(&[b"alpha", b"\xff\0beta"]);
        let dictionary = decode_variable_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("valid variable dictionary");

        assert_eq!(2, dictionary.len());
        assert!(!dictionary.is_empty());
        assert_eq!(b"alpha", dictionary.entry(0).unwrap().value());
        assert_eq!(b"\xff\0beta", dictionary.entry(1).unwrap().value());
        assert_eq!(None, dictionary.entry(2));
        assert_eq!(
            vec![(0, b"alpha".as_slice()), (1, b"\xff\0beta".as_slice())],
            dictionary
                .entries()
                .map(|entry| (entry.id(), entry.value()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn decodes_entries_spanning_bulk_decode_buffers() {
        let first = vec![b'a'; DECODE_BUFFER_SIZE - 3];
        let second = vec![0x5a; DECODE_BUFFER_SIZE + 17];
        let bytes = section(&[first.as_slice(), second.as_slice(), b"tail"]);
        let dictionary = decode_variable_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("dictionary entries may span decoder buffer boundaries");

        assert_eq!(first.as_slice(), dictionary.entry(0).unwrap().value());
        assert_eq!(second.as_slice(), dictionary.entry(1).unwrap().value());
        assert_eq!(b"tail", dictionary.entry(2).unwrap().value());
    }

    #[test]
    fn accepts_only_the_canonical_eight_byte_empty_section() {
        let bytes = section(&[]);
        let dictionary = decode_array_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("canonical empty dictionary");
        assert!(dictionary.is_empty());
        assert_eq!(0, dictionary.entries().count());

        let mut noncanonical = bytes;
        noncanonical.extend_from_slice(
            &zstd::stream::encode_all(&[][..], 3).expect("compress empty frame"),
        );
        assert!(matches!(
            decode_array_dictionary(take(&noncanonical), DictionaryLimits::default()),
            Err(DictionaryError::TrailingCompressedData { .. })
        ));
    }

    #[test]
    fn validates_logtype_escapes_and_counts_unescaped_placeholders() {
        let escaped = b"a\x11b\\\x12c\x13\\\\";
        let bytes = section(&[escaped]);
        let dictionary = decode_logtype_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("valid escaped logtype");
        let entry = dictionary.entry(0).expect("entry zero");

        assert_eq!(escaped, entry.escaped_value());
        assert_eq!(0, entry.id());
        let counts = entry.placeholder_counts();
        assert_eq!(1, counts.integer());
        assert_eq!(0, counts.dictionary());
        assert_eq!(1, counts.float());
        assert_eq!(2, counts.encoded_variables());
        assert_eq!(2, counts.escape_sequences());
        assert_eq!(4, counts.placeholders());
        let mut kinds = Vec::new();
        entry
            .variable_kinds_into(&mut kinds)
            .expect("escapes are complete");
        assert_eq!(
            vec![LogTypeVariableKind::Integer, LogTypeVariableKind::Float],
            kinds
        );
        assert_eq!(1, dictionary.entries().count());
    }

    #[test]
    fn reports_a_dangling_logtype_escape_on_use_not_on_decode() {
        // Opening an archive does not scan its logtypes; the dangling escape surfaces only when the
        // entry's variables are asked for.
        let bytes = section(&[b"constant\\"]);
        let dictionary = decode_logtype_dictionary(take(&bytes), DictionaryLimits::default())
            .expect("decoding records ranges only");
        let entry = dictionary.entry(0).expect("entry zero");
        assert!(matches!(
            entry.variable_kinds_into(&mut Vec::new()),
            Err(DictionaryError::DanglingEscape {
                entry_id: 0,
                offset: 8
            })
        ));
    }

    #[test]
    fn enforces_count_and_value_limits_before_allocation() {
        let bytes = section(&[b"abc", b"def"]);
        let count_limit = DictionaryLimits::new(u64::MAX, u64::MAX, 1, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_variable_dictionary(take(&bytes), count_limit),
            Err(DictionaryError::EntryCountTooLarge {
                actual: 2,
                limit: 1
            })
        ));

        let entry_limit = DictionaryLimits::new(u64::MAX, u64::MAX, 2, 2, u64::MAX);
        assert!(matches!(
            decode_variable_dictionary(take(&bytes), entry_limit),
            Err(DictionaryError::EntryTooLarge {
                entry_id: 0,
                actual: 3,
                limit: 2
            })
        ));

        let total_limit = DictionaryLimits::new(u64::MAX, u64::MAX, 2, u64::MAX, 5);
        assert!(matches!(
            decode_variable_dictionary(take(&bytes), total_limit),
            Err(DictionaryError::TotalValueBytesTooLarge {
                actual: 6,
                limit: 5
            })
        ));
    }

    #[test]
    fn enforces_compressed_and_decompressed_limits() {
        let bytes = section(&[b"abc"]);
        let compressed_limit = DictionaryLimits::new(7, u64::MAX, 1, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_variable_dictionary(take(&bytes), compressed_limit),
            Err(DictionaryError::CompressedSectionTooLarge { .. })
        ));

        let header_limit = DictionaryLimits::new(u64::MAX, 7, 1, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_variable_dictionary(take(&bytes), header_limit),
            Err(DictionaryError::DecompressedSectionTooLarge {
                actual: 8,
                limit: 7
            })
        ));

        let value_limit = DictionaryLimits::new(u64::MAX, 10, 1, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_variable_dictionary(take(&bytes), value_limit),
            Err(DictionaryError::DecompressedSectionTooLarge {
                actual: 11,
                limit: 10
            })
        ));

        let large_entry = vec![b'x'; 2 * DECODE_BUFFER_SIZE];
        let large_bytes = section(&[large_entry.as_slice()]);
        let bulk_limit = DictionaryLimits::new(
            u64::MAX,
            u64::try_from(DECODE_BUFFER_SIZE).expect("buffer size fits u64"),
            1,
            u64::MAX,
            u64::MAX,
        );
        assert!(matches!(
            decode_variable_dictionary(take(&large_bytes), bulk_limit),
            Err(DictionaryError::DecompressedSectionTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_truncated_and_extra_dictionary_content() {
        let short_prefix = [0_u8; 7];
        assert!(matches!(
            decode_variable_dictionary(take(&short_prefix), DictionaryLimits::default()),
            Err(DictionaryError::Io(_))
        ));

        let missing_frame = 1_u64.to_le_bytes();
        assert!(matches!(
            decode_variable_dictionary(take(&missing_frame), DictionaryLimits::default()),
            Err(DictionaryError::Io(_))
        ));

        let mut missing_entry = section(&[b"only entry"]);
        missing_entry[..8].copy_from_slice(&2_u64.to_le_bytes());
        assert!(matches!(
            decode_variable_dictionary(take(&missing_entry), DictionaryLimits::default()),
            Err(DictionaryError::Io(_))
        ));

        let mut decompressed = 0_u64.to_le_bytes().to_vec();
        decompressed.push(1);
        let mut trailing_decompressed = 1_u64.to_le_bytes().to_vec();
        trailing_decompressed.extend_from_slice(
            &zstd::stream::encode_all(decompressed.as_slice(), 3)
                .expect("compress trailing decompressed byte"),
        );
        assert!(matches!(
            decode_variable_dictionary(take(&trailing_decompressed), DictionaryLimits::default()),
            Err(DictionaryError::TrailingDecompressedData)
        ));

        let mut trailing_compressed = section(&[b"ok"]);
        trailing_compressed.extend_from_slice(
            &zstd::stream::encode_all(&[][..], 3).expect("compress second zstd frame"),
        );
        assert!(matches!(
            decode_variable_dictionary(take(&trailing_compressed), DictionaryLimits::default()),
            Err(DictionaryError::TrailingCompressedData { .. })
        ));
    }
}
