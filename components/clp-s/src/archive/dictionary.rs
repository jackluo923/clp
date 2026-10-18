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
    bytes: StorageBytes,
    ranges: Vec<Range<usize>>,
}

/// The bytes a dictionary's entries lie in: its own, or a decompressed frame its field shares with
/// the other fields loaded from that frame.
#[derive(Clone, Debug, Eq, PartialEq)]
enum StorageBytes {
    Owned(Vec<u8>),
    Shared(std::sync::Arc<Vec<u8>>),
}

impl StorageBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

impl DictionaryStorage {
    const fn entry_count(&self) -> usize {
        self.ranges.len()
    }

    fn value(&self, id: u64) -> Option<&[u8]> {
        let index = usize::try_from(id).ok()?;
        let range = self.ranges.get(index)?;
        Some(&self.bytes.as_slice()[range.clone()])
    }
}

/// A decoded `/var.dict` whose implicit IDs are zero-based wire positions.
///
/// Values are retained as arbitrary bytes in one contiguous allocation to avoid one allocation
/// per dictionary entry.
#[derive(Clone, Debug)]
pub struct VariableDictionary {
    storage: DictionaryStorage,
    /// Bytes the dictionary frame decoded to. An entry stored as a binary UUID has a range of 36
    /// bytes at or past this, which addresses its text in `rendered` rather than `storage`.
    frame_len: usize,
    /// The binary-UUID entries in ID order, laid out as 16 byte columns (see [`uuid_columns`]),
    /// as the archive stores them; a UUID's bytes are gathered only when one is read.
    uuids: Vec<u8>,
    /// The text of every binary-UUID entry, built the first time one is read.
    rendered: std::sync::OnceLock<Vec<u8>>,
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
                bytes: StorageBytes::Owned(Vec::new()),
                ranges: Vec::new(),
            },
            frame_len: 0,
            uuids: Vec::new(),
            rendered: std::sync::OnceLock::new(),
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
        let range = self.storage.ranges.get(usize::try_from(id).ok()?)?;
        Some(VariableDictionaryEntry {
            id,
            value: self.value_in(range),
        })
    }

    /// Iterates entries in implicit wire-ID order.
    pub fn entries(&self) -> impl Iterator<Item = VariableDictionaryEntry<'_>> {
        (0_u64..)
            .zip(&self.storage.ranges)
            .map(|(id, range)| VariableDictionaryEntry {
                id,
                value: self.value_in(range),
            })
    }

    /// The 16 bytes of an entry stored as a binary UUID, or `None` for any other entry. Reading
    /// them needs no text, so a search can compare a UUID literal without rendering the UUIDs.
    #[must_use]
    pub fn binary_uuid(&self, id: u64) -> Option<[u8; 16]> {
        let range = self.storage.ranges.get(usize::try_from(id).ok()?)?;
        let index = self.uuid_index(range)?;
        Some(self.gather_uuid(index))
    }

    /// The 16 bytes of the `index`th binary UUID, one from each byte column.
    fn gather_uuid(&self, index: usize) -> [u8; 16] {
        let count = self.uuids.len() / 16;
        std::array::from_fn(|column| self.uuids[column * count + index])
    }

    /// Whether some entries are stored as binary UUIDs.
    #[must_use]
    pub const fn has_binary_uuids(&self) -> bool {
        !self.uuids.is_empty()
    }

    fn uuid_index(&self, range: &Range<usize>) -> Option<usize> {
        (!self.uuids.is_empty() && 36 == range.len() && range.start >= self.frame_len)
            .then(|| (range.start - self.frame_len) / 36)
    }

    fn value_in(&self, range: &Range<usize>) -> &[u8] {
        if self.uuid_index(range).is_none() {
            return &self.storage.bytes.as_slice()[range.clone()];
        }
        let rendered = self.rendered.get_or_init(|| {
            let count = self.uuids.len() / 16;
            let mut rendered = Vec::with_capacity(count * 36);
            for index in 0..count {
                render_uuid(&self.gather_uuid(index), &mut rendered);
            }
            rendered
        });
        &rendered[range.start - self.frame_len..range.end - self.frame_len]
    }
}

// Two dictionaries are equal when their entries are, whether or not either has rendered its UUIDs.
impl PartialEq for VariableDictionary {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .entries()
                .zip(other.entries())
                .all(|(left, right)| left.value() == right.value())
    }
}

impl Eq for VariableDictionary {}

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
///
/// An archive may store one dictionary per CLP-string field instead, each with its own IDs; then
/// this holds the fields' dictionaries, of which a query may have loaded only some, and a column
/// resolves its logtypes through [`Self::for_field`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogTypeDictionary {
    storage: DictionaryStorage,
    /// The loaded fields' dictionaries, by schema node, when stored per field.
    fields: Vec<(u32, Self)>,
    per_field: bool,
    complete: bool,
}

/// What a field whose dictionary was not loaded resolves to: no entries.
static NO_LOG_TYPES: LogTypeDictionary = LogTypeDictionary::empty();

impl LogTypeDictionary {
    /// A dictionary with no entries; see [`VariableDictionary::empty`].
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            storage: DictionaryStorage {
                bytes: StorageBytes::Owned(Vec::new()),
                ranges: Vec::new(),
            },
            fields: Vec::new(),
            per_field: false,
            complete: true,
        }
    }

    /// The dictionary the column of `node` reads its logtypes through: this one, or the field's
    /// own when stored per field. A field that was not loaded has no entries.
    #[must_use]
    pub fn for_field(&self, node: u32) -> &Self {
        if !self.per_field {
            return self;
        }
        self.fields
            .binary_search_by_key(&node, |(field, _)| *field)
            .map_or(&NO_LOG_TYPES, |index| &self.fields[index].1)
    }

    /// Whether the dictionary is stored per field.
    #[must_use]
    pub const fn is_per_field(&self) -> bool {
        self.per_field
    }

    /// Returns the number of logtype dictionary entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.storage.entry_count()
    }

    /// Whether every entry is loaded, as it is unless a per-field dictionary was loaded for only
    /// some fields.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
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
                bytes: StorageBytes::Owned(Vec::new()),
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
            escaped_value: &storage.bytes.as_slice()[range.clone()],
        })
}

pub(super) fn decode_variable_dictionary<R: Read>(
    mut section: Take<R>,
    limits: DictionaryLimits,
) -> Result<VariableDictionary, DictionaryError> {
    let compressed_size = section.limit();
    if compressed_size > limits.compressed {
        return Err(DictionaryError::CompressedSectionTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }
    let first = read_u64(&mut section)?;
    if BINARY_UUIDS_MARKER != first {
        let storage = decode_entry_sequence(section, limits, first, None)?;
        return Ok(VariableDictionary {
            frame_len: storage.bytes.as_slice().len(),
            storage,
            uuids: Vec::new(),
            rendered: std::sync::OnceLock::new(),
        });
    }
    // Entries that are canonical UUIDs are stored as their 16 bytes, in ID order and laid out as
    // byte columns (see `uuid_columns`), in a zstd frame of their own ahead of the frame holding
    // the rest; that frame marks each one's place with a length of `BINARY_UUID_LENGTH` and no
    // bytes.
    let entry_count = read_u64(&mut section)?;
    let uuid_count = read_u64(&mut section)?;
    if uuid_count > entry_count || entry_count > limits.entries {
        return Err(DictionaryError::EntryCountTooLarge {
            actual: uuid_count.max(entry_count),
            limit: limits.entries.min(entry_count),
        });
    }
    let uuid_bytes = uuid_count
        .checked_mul(UUID_SIZE)
        .filter(|bytes| *bytes <= limits.decompressed)
        .ok_or(DictionaryError::SizeOverflow)?;
    let frame_len = read_u64(&mut section)?;
    if frame_len > section.limit() {
        return Err(DictionaryError::SizeOverflow);
    }
    let frame_len = usize::try_from(frame_len).map_err(|_| DictionaryError::SizeOverflow)?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(frame_len)
        .map_err(|_| DictionaryError::AllocationFailed {
            requested: frame_len,
        })?;
    frame.resize(frame_len, 0);
    section
        .read_exact(&mut frame)
        .map_err(DictionaryError::Io)?;
    // Sixteen frames, one per byte column, each holding one byte of every UUID.
    let column_bytes = uuid_bytes / UUID_SIZE;
    let uuid_bytes = usize::try_from(uuid_bytes).map_err(|_| DictionaryError::SizeOverflow)?;
    let mut columns = Vec::new();
    columns
        .try_reserve_exact(uuid_bytes)
        .map_err(|_| DictionaryError::AllocationFailed {
            requested: uuid_bytes,
        })?;
    columns.resize(uuid_bytes, 0);
    let mut rest = frame.as_slice();
    let column_bytes = usize::try_from(column_bytes).map_err(|_| DictionaryError::SizeOverflow)?;
    if 0 != column_bytes {
        // One context decompresses every column straight into its place.
        let invalid = |message: &'static str| {
            DictionaryError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
        };
        let mut context = zstd::zstd_safe::DCtx::create();
        for column in columns.chunks_exact_mut(column_bytes) {
            let frame_len = zstd::zstd_safe::find_frame_compressed_size(rest)
                .map_err(|_| invalid("binary UUIDs are not sixteen complete zstd frames"))?;
            let written = context
                .decompress(column, &rest[..frame_len])
                .map_err(|_| invalid("a binary-UUID column does not decompress"))?;
            if written != column_bytes {
                return Err(invalid(
                    "a binary-UUID column holds the wrong number of bytes",
                ));
            }
            rest = &rest[frame_len..];
        }
    }
    if !rest.is_empty() {
        return Err(DictionaryError::TrailingCompressedData {
            remaining: u64::try_from(rest.len()).map_err(|_| DictionaryError::SizeOverflow)?,
        });
    }
    let uuids = columns;
    let storage = decode_entry_sequence(section, limits, entry_count, Some(&uuids))?;
    Ok(VariableDictionary {
        frame_len: storage.bytes.as_slice().len(),
        storage,
        uuids,
        rendered: std::sync::OnceLock::new(),
    })
}

/// Marks a `/var.dict` whose canonical-UUID entries are stored as binary. It is read where a plain
/// dictionary stores its entry count, and exceeds any count a reader accepts, so a reader that does
/// not know the layout refuses the dictionary rather than misreading it.
pub const BINARY_UUIDS_MARKER: u64 = 0xC1F5_B1A5_0D1D_0008;

/// The length a binary-UUID entry carries in place of its own inside the dictionary frame.
pub const BINARY_UUID_LENGTH: u64 = u64::MAX;

const UUID_SIZE: u64 = 16;

/// Whether `value` is a UUID in its canonical text form, lowercase hex digits grouped 8-4-4-4-12
/// by hyphens, which [`render_uuid`] rebuilds exactly from its 16 bytes.
#[must_use]
pub fn canonical_uuid(value: &[u8]) -> Option<[u8; 16]> {
    if 36 != value.len() {
        return None;
    }
    let mut bytes = [0_u8; 16];
    let mut nibble = 0;
    for (index, &character) in value.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if b'-' != character {
                return None;
            }
            continue;
        }
        let digit = match character {
            b'0'..=b'9' => character - b'0',
            b'a'..=b'f' => character - b'a' + 10,
            _ => return None,
        };
        bytes[nibble / 2] |= if 0 == nibble % 2 { digit << 4 } else { digit };
        nibble += 1;
    }
    Some(bytes)
}

/// Lays UUIDs out as 16 byte columns, the first byte of every UUID, then the second, and so on, as
/// a dictionary stores them. A column of a byte the UUID version fixes, or of a slowly changing
/// time-based prefix, then compresses on its own, where interleaved it would not.
#[must_use]
pub fn uuid_columns(rows: &[u8]) -> Vec<u8> {
    let count = rows.len() / 16;
    let mut columns = vec![0_u8; count * 16];
    for (row, uuid) in rows.as_chunks::<16>().0.iter().enumerate() {
        for (column, byte) in uuid.iter().enumerate() {
            columns[column * count + row] = *byte;
        }
    }
    columns
}

/// Appends the canonical text of a UUID's 16 bytes: lowercase hex grouped 8-4-4-4-12 by hyphens.
pub fn render_uuid(bytes: &[u8], output: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            output.push(b'-');
        }
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0F)]);
    }
}

pub(super) fn decode_logtype_dictionary<R: Read>(
    section: Take<R>,
    limits: DictionaryLimits,
) -> Result<LogTypeDictionary, DictionaryError> {
    decode_logtype_dictionary_fields(section, limits, None)
}

/// Decodes `/log.dict`, loading only the dictionaries of `fields` when it is stored per field.
/// `None`, or a dictionary stored as one sequence, loads everything.
pub(super) fn decode_logtype_dictionary_fields<R: Read>(
    mut section: Take<R>,
    limits: DictionaryLimits,
    fields: Option<&[u32]>,
) -> Result<LogTypeDictionary, DictionaryError> {
    let compressed_size = section.limit();
    if compressed_size > limits.compressed {
        return Err(DictionaryError::CompressedSectionTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }
    let first = read_u64(&mut section)?;
    if FIELD_DICTIONARIES_MARKER == first {
        return decode_field_dictionaries(section, limits, fields);
    }
    Ok(LogTypeDictionary {
        storage: decode_entry_sequence(section, limits, first, None)?,
        fields: Vec::new(),
        per_field: false,
        complete: true,
    })
}

/// Marks a `/log.dict` stored as one dictionary per field. It is read where a plain dictionary
/// stores its entry count, and exceeds any count a reader accepts, so a reader that does not know
/// the layout refuses the dictionary rather than misreading it.
pub const FIELD_DICTIONARIES_MARKER: u64 = 0xC1F5_F1E1_DD1C_7002;

/// One field's dictionary as the index describes it: its schema node, entry count, the frame
/// holding it, and where it lies in that frame's decompressed bytes.
struct FieldDictionaryIndex {
    node: u64,
    entries: u64,
    frame: usize,
    start: u64,
    end: u64,
}

/// Decodes a `/log.dict` stored as one dictionary per CLP-string field.
///
/// After the marker, an uncompressed index: the dictionary count and frame count, then for each
/// dictionary its field (a schema node ID), entry count, frame, and start and end within the
/// frame's decompressed bytes, then each frame's compressed and decompressed size. Then the frames.
/// A dictionary is its entries' 4-byte lengths followed by their bytes, and an entry's ID is its
/// position in its field's dictionary. Dictionaries sharing a frame lie back to back, so reading
/// one decompresses its frame only up to its end, and a reader skips a frame it needs nothing from.
fn decode_field_dictionaries<R: Read>(
    mut section: Take<R>,
    limits: DictionaryLimits,
    fields: Option<&[u32]>,
) -> Result<LogTypeDictionary, DictionaryError> {
    let count_limit = limits.entries.saturating_add(1);
    let dictionary_count = read_u64(&mut section)?;
    let frame_count = read_u64(&mut section)?;
    for count in [dictionary_count, frame_count] {
        if count > count_limit {
            return Err(DictionaryError::EntryCountTooLarge {
                actual: count,
                limit: limits.entries,
            });
        }
    }
    let index = read_field_index(&mut section, dictionary_count, frame_count, limits)?;
    let frames = read_frame_index(&mut section, frame_count, limits)?;
    // The dictionaries of each frame must tile it from the start, in order.
    for (frame, (_, decompressed)) in frames.iter().enumerate() {
        let mut expected = 0_u64;
        for entry in index.iter().filter(|entry| entry.frame == frame) {
            if entry.start != expected {
                return Err(DictionaryError::TrailingDecompressedData);
            }
            expected = entry.end;
        }
        if expected != *decompressed {
            return Err(DictionaryError::TrailingDecompressedData);
        }
    }

    let mut compressed = Vec::new();
    section
        .read_to_end(&mut compressed)
        .map_err(DictionaryError::Io)?;
    let mut frame_ranges = Vec::with_capacity(frames.len());
    let mut offset = 0_usize;
    for (size, _) in &frames {
        let end = usize::try_from(*size)
            .ok()
            .and_then(|size| offset.checked_add(size))
            .ok_or(DictionaryError::SizeOverflow)?;
        frame_ranges.push(offset..end);
        offset = end;
    }
    if offset != compressed.len() {
        return Err(DictionaryError::TrailingCompressedData {
            remaining: u64::try_from(compressed.len().abs_diff(offset)).unwrap_or(u64::MAX),
        });
    }

    let wanted = |entry: &FieldDictionaryIndex| {
        fields.is_none_or(|fields| {
            u32::try_from(entry.node).is_ok_and(|node| fields.binary_search(&node).is_ok())
        })
    };
    let mut loaded = Vec::new();
    let mut total_value_bytes = 0_u64;
    for (frame, range) in frame_ranges.into_iter().enumerate() {
        let in_frame: Vec<&FieldDictionaryIndex> =
            index.iter().filter(|entry| entry.frame == frame).collect();
        let Some(last) = in_frame.iter().rposition(|entry| wanted(entry)) else {
            continue;
        };
        // The frame is decompressed once, up to the end of the last dictionary wanted from it,
        // into one buffer the wanted fields share.
        let prefix =
            usize::try_from(in_frame[last].end).map_err(|_| DictionaryError::SizeOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(prefix)
            .map_err(|_| DictionaryError::AllocationFailed { requested: prefix })?;
        decompress_prefix(&compressed[range], &mut bytes, prefix)?;
        let bytes = std::sync::Arc::new(bytes);
        for entry in in_frame[..=last].iter().filter(|entry| wanted(entry)) {
            let ranges = field_ranges(&bytes, entry, limits, &mut total_value_bytes)?;
            let node = u32::try_from(entry.node).map_err(|_| DictionaryError::SizeOverflow)?;
            loaded.push((
                node,
                LogTypeDictionary {
                    storage: DictionaryStorage {
                        bytes: StorageBytes::Shared(std::sync::Arc::clone(&bytes)),
                        ranges,
                    },
                    fields: Vec::new(),
                    per_field: false,
                    complete: true,
                },
            ));
        }
    }
    loaded.sort_unstable_by_key(|(node, _)| *node);
    let complete = loaded.len() == index.len();
    Ok(LogTypeDictionary {
        storage: DictionaryStorage {
            bytes: StorageBytes::Owned(Vec::new()),
            ranges: Vec::new(),
        },
        fields: loaded,
        per_field: true,
        complete,
    })
}

/// Reads the per-dictionary part of the index.
fn read_field_index<R: Read>(
    section: &mut Take<R>,
    dictionary_count: u64,
    frame_count: u64,
    limits: DictionaryLimits,
) -> Result<Vec<FieldDictionaryIndex>, DictionaryError> {
    let mut index = Vec::new();
    let mut total_entries = 0_u64;
    for _ in 0..dictionary_count {
        let node = read_u64(section)?;
        let entries = read_u64(section)?;
        let frame = read_u64(section)?;
        if frame >= frame_count {
            return Err(DictionaryError::SizeOverflow);
        }
        let frame = usize::try_from(frame).map_err(|_| DictionaryError::SizeOverflow)?;
        let start = read_u64(section)?;
        let end = read_u64(section)?;
        let lengths = entries
            .checked_mul(4)
            .ok_or(DictionaryError::SizeOverflow)?;
        if end < start || end - start < lengths {
            return Err(DictionaryError::SizeOverflow);
        }
        total_entries = total_entries
            .checked_add(entries)
            .ok_or(DictionaryError::SizeOverflow)?;
        index.push(FieldDictionaryIndex {
            node,
            entries,
            frame,
            start,
            end,
        });
    }
    if total_entries > limits.entries {
        return Err(DictionaryError::EntryCountTooLarge {
            actual: total_entries,
            limit: limits.entries,
        });
    }
    Ok(index)
}

/// Reads each frame's compressed and decompressed size.
fn read_frame_index<R: Read>(
    section: &mut Take<R>,
    frame_count: u64,
    limits: DictionaryLimits,
) -> Result<Vec<(u64, u64)>, DictionaryError> {
    let mut frames = Vec::new();
    let mut decompressed_total = 0_u64;
    for _ in 0..frame_count {
        let compressed = read_u64(section)?;
        let decompressed = read_u64(section)?;
        decompressed_total = decompressed_total
            .checked_add(decompressed)
            .ok_or(DictionaryError::SizeOverflow)?;
        frames.push((compressed, decompressed));
    }
    if decompressed_total > limits.decompressed {
        return Err(DictionaryError::DecompressedSectionTooLarge {
            actual: decompressed_total,
            limit: limits.decompressed,
        });
    }
    Ok(frames)
}

/// Decompresses one frame into `output` until it holds at least `prefix` bytes. Writing into the
/// vector's spare capacity needs no zero-filling; it may run past `prefix` to the capacity, which
/// is harmless.
fn decompress_prefix(
    frame: &[u8],
    output: &mut Vec<u8>,
    prefix: usize,
) -> Result<(), DictionaryError> {
    let mut context = zstd::zstd_safe::DCtx::create();
    let mut input = zstd::zstd_safe::InBuffer::around(frame);
    let mut out = zstd::zstd_safe::OutBuffer::around(output);
    while out.pos() < prefix {
        let before = (input.pos(), out.pos());
        context
            .decompress_stream(&mut out, &mut input)
            .map_err(|code| {
                DictionaryError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    zstd::zstd_safe::get_error_name(code),
                ))
            })?;
        if (input.pos(), out.pos()) == before {
            return Err(DictionaryError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "dictionary frame ends before its dictionaries do",
            )));
        }
    }
    Ok(())
}

/// Locates the entries of one field's dictionary in its decompressed frame: `entries` 4-byte
/// lengths, then the bytes, between the index's start and end.
fn field_ranges(
    frame: &[u8],
    entry: &FieldDictionaryIndex,
    limits: DictionaryLimits,
    total_value_bytes: &mut u64,
) -> Result<Vec<Range<usize>>, DictionaryError> {
    let start = usize::try_from(entry.start).map_err(|_| DictionaryError::SizeOverflow)?;
    let end = usize::try_from(entry.end).map_err(|_| DictionaryError::SizeOverflow)?;
    let entries = usize::try_from(entry.entries).map_err(|_| DictionaryError::SizeOverflow)?;
    let lengths_end = entries
        .checked_mul(4)
        .and_then(|lengths| start.checked_add(lengths))
        .filter(|lengths_end| *lengths_end <= end && end <= frame.len())
        .ok_or(DictionaryError::SizeOverflow)?;
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(entries)
        .map_err(|_| DictionaryError::AllocationFailed { requested: entries })?;
    let mut offset = lengths_end;
    for (entry_id, length) in frame[start..lengths_end]
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
    {
        let length = u32::from_le_bytes(*length);
        if u64::from(length) > limits.entry_bytes {
            return Err(DictionaryError::EntryTooLarge {
                entry_id: wire_id(entry_id)?,
                actual: u64::from(length),
                limit: limits.entry_bytes,
            });
        }
        *total_value_bytes = total_value_bytes
            .checked_add(u64::from(length))
            .ok_or(DictionaryError::SizeOverflow)?;
        if *total_value_bytes > limits.total_value_bytes {
            return Err(DictionaryError::TotalValueBytesTooLarge {
                actual: *total_value_bytes,
                limit: limits.total_value_bytes,
            });
        }
        let value_end = usize::try_from(length)
            .ok()
            .and_then(|length| offset.checked_add(length))
            .filter(|value_end| *value_end <= end)
            .ok_or(DictionaryError::TrailingDecompressedData)?;
        ranges.push(offset..value_end);
        offset = value_end;
    }
    if offset != end {
        return Err(DictionaryError::TrailingDecompressedData);
    }
    Ok(ranges)
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
    decode_entry_sequence(section, limits, entry_count, None)
}

/// Decodes a dictionary stored as one sequence of entries, once its entry count has been read.
/// `uuids` holds the 16 bytes of each entry stored as a binary UUID, in ID order.
fn decode_entry_sequence<R: Read>(
    mut section: Take<R>,
    limits: DictionaryLimits,
    entry_count: u64,
    uuids: Option<&[u8]>,
) -> Result<DictionaryStorage, DictionaryError> {
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
            bytes: StorageBytes::Owned(Vec::new()),
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
    compressed.try_reserve_exact(compressed_len).map_err(|_| {
        DictionaryError::AllocationFailed {
            requested: compressed_len,
        }
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
    let decompressed = if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(frame) {
        decode_sized_frame(frame, size, limits.decompressed)?
    } else {
        {
            let mut decoder = zstd::stream::read::Decoder::with_buffer(frame)
                .map_err(DictionaryError::Io)?
                .single_frame();
            decode_frame(&mut decoder, minimum_decompressed_size, limits.decompressed)?
        }
    };

    decode_entries(decompressed, entry_count, limits, uuids, &mut dictionary)?;
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
        bytes: StorageBytes::Owned(Vec::new()),
        ranges,
    })
}

fn decode_entries(
    decompressed: Vec<u8>,
    entry_count: usize,
    limits: DictionaryLimits,
    uuids: Option<&[u8]>,
    decoded: &mut DictionaryStorage,
) -> Result<(), DictionaryError> {
    let mut total_value_bytes = 0_u64;
    let mut offset = 0_usize;
    // A binary UUID's range lies past the frame's bytes, where its text is rendered on demand.
    let frame_end = decompressed.len();
    let mut rendered = 0_usize;
    let mut uuids = uuids.map(|bytes| bytes.as_chunks::<16>().0.iter());

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
        if BINARY_UUID_LENGTH == entry_size
            && let Some(uuids) = uuids.as_mut()
        {
            uuids.next().ok_or_else(|| {
                DictionaryError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "dictionary marks more binary UUIDs than it stores",
                ))
            })?;
            let start = frame_end
                .checked_add(rendered)
                .ok_or(DictionaryError::SizeOverflow)?;
            rendered += 36;
            total_value_bytes = total_value_bytes
                .checked_add(36)
                .ok_or(DictionaryError::SizeOverflow)?;
            if total_value_bytes > limits.total_value_bytes {
                return Err(DictionaryError::TotalValueBytesTooLarge {
                    actual: total_value_bytes,
                    limit: limits.total_value_bytes,
                });
            }
            decoded.ranges.push(start..start + 36);
            offset = length_end;
            continue;
        }
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
    if uuids.is_some_and(|mut uuids| uuids.next().is_some()) {
        return Err(DictionaryError::TrailingDecompressedData);
    }
    decoded.bytes = StorageBytes::Owned(decompressed);
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

    #[test]
    fn canonical_uuids_round_trip_through_their_bytes() {
        let text = b"34c75dc7-30ee-42e1-9f1e-58cf1667f88e";
        let bytes = canonical_uuid(text).expect("canonical UUID");
        let mut rendered = Vec::new();
        render_uuid(&bytes, &mut rendered);
        assert_eq!(text.as_slice(), rendered.as_slice());
        // Only the exact lowercase 8-4-4-4-12 form is stored as bytes, so text always round-trips.
        for other in [
            b"34C75DC7-30EE-42E1-9F1E-58CF1667F88E".as_slice(),
            b"34c75dc730ee42e19f1e58cf1667f88e",
            b"34c75dc7-30ee-42e1-9f1e-58cf1667f88",
            b"34c75dc7-30ee-42e1-9f1e_58cf1667f88e",
            b"g4c75dc7-30ee-42e1-9f1e-58cf1667f88e",
        ] {
            assert_eq!(
                None,
                canonical_uuid(other),
                "{}",
                String::from_utf8_lossy(other)
            );
        }
    }

    #[test]
    fn binary_uuid_entries_read_back_as_their_text() {
        let uuid = b"34c75dc7-30ee-42e1-9f1e-58cf1667f88e";
        let other = b"0f2b6a1e-9c4d-4e8b-a3f5-7d6c5b4a3921";
        let entries: [&[u8]; 4] = [b"plain", uuid, b"", other];
        let mut frame = Vec::new();
        let mut uuids = Vec::new();
        for entry in entries {
            if let Some(bytes) = canonical_uuid(entry) {
                frame.extend_from_slice(&BINARY_UUID_LENGTH.to_le_bytes());
                uuids.extend_from_slice(&bytes);
            } else {
                frame.extend_from_slice(&(entry.len() as u64).to_le_bytes());
                frame.extend_from_slice(entry);
            }
        }
        let mut encoded = BINARY_UUIDS_MARKER.to_le_bytes().to_vec();
        encoded.extend_from_slice(&4_u64.to_le_bytes());
        encoded.extend_from_slice(&2_u64.to_le_bytes());
        let columns = uuid_columns(&uuids);
        let uuid_frame: Vec<u8> = columns
            .chunks_exact(uuids.len() / 16)
            .flat_map(|column| zstd::bulk::compress(column, 3).expect("compress UUID column"))
            .collect();
        encoded.extend_from_slice(&(uuid_frame.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&uuid_frame);
        encoded.extend_from_slice(&zstd::bulk::compress(&frame, 3).expect("compress frame"));
        let dictionary = decode_variable_dictionary(take(&encoded), DictionaryLimits::default())
            .expect("valid binary-UUID dictionary");
        assert!(dictionary.has_binary_uuids());
        assert_eq!(None, dictionary.binary_uuid(0));
        assert_eq!(None, dictionary.binary_uuid(2));
        assert_eq!(
            Some(<[u8; 16]>::try_from(&uuids[..16]).expect("16 bytes")),
            dictionary.binary_uuid(1)
        );
        let values: Vec<&[u8]> = dictionary
            .entries()
            .map(VariableDictionaryEntry::value)
            .collect();
        assert_eq!(entries.to_vec(), values);
        let plain = section(&entries);
        assert_eq!(
            decode_variable_dictionary(take(&plain), DictionaryLimits::default())
                .expect("valid plain dictionary"),
            dictionary
        );

        // A frame marking more UUIDs than the section stores is refused.
        let mut short = BINARY_UUIDS_MARKER.to_le_bytes().to_vec();
        short.extend_from_slice(&4_u64.to_le_bytes());
        short.extend_from_slice(&1_u64.to_le_bytes());
        let short_frame: Vec<u8> = uuids[..16]
            .iter()
            .flat_map(|byte| {
                zstd::bulk::compress(std::slice::from_ref(byte), 3).expect("compress UUID column")
            })
            .collect();
        short.extend_from_slice(&(short_frame.len() as u64).to_le_bytes());
        short.extend_from_slice(&short_frame);
        short.extend_from_slice(&zstd::bulk::compress(&frame, 3).expect("compress frame"));
        assert!(decode_variable_dictionary(take(&short), DictionaryLimits::default()).is_err());
    }

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

    /// Each frame's dictionaries, as (field, entries).
    type Frames<'a> = &'a [&'a [(u64, &'a [&'a [u8]])]];

    /// A per-field `/log.dict` holding `frames`.
    fn field_dictionaries(frames: Frames<'_>) -> Vec<u8> {
        let mut index = Vec::new();
        let mut frame_index = Vec::new();
        let mut payloads = Vec::new();
        for (frame, dictionaries) in frames.iter().enumerate() {
            let mut payload = Vec::new();
            for (field, entries) in *dictionaries {
                let start = payload.len() as u64;
                for entry in *entries {
                    let length = u32::try_from(entry.len()).expect("test entry fits u32");
                    payload.extend_from_slice(&length.to_le_bytes());
                }
                for entry in *entries {
                    payload.extend_from_slice(entry);
                }
                index.push([
                    *field,
                    entries.len() as u64,
                    frame as u64,
                    start,
                    payload.len() as u64,
                ]);
            }
            let compressed = zstd::stream::encode_all(payload.as_slice(), 3).expect("zstd");
            frame_index.push([compressed.len() as u64, payload.len() as u64]);
            payloads.extend_from_slice(&compressed);
        }
        let mut encoded = Vec::new();
        for word in [
            FIELD_DICTIONARIES_MARKER,
            index.len() as u64,
            frame_index.len() as u64,
        ] {
            encoded.extend_from_slice(&word.to_le_bytes());
        }
        for word in index.iter().flatten().chain(frame_index.iter().flatten()) {
            encoded.extend_from_slice(&word.to_le_bytes());
        }
        encoded.extend_from_slice(&payloads);
        encoded
    }

    #[test]
    fn per_field_dictionary_loads_only_the_requested_fields() {
        fn value(dictionary: &LogTypeDictionary, field: u32, id: u64) -> Option<&[u8]> {
            dictionary
                .for_field(field)
                .entry(id)
                .map(LogTypeDictionaryEntry::escaped_value)
        }

        // Fields 5 and 9 share a frame, 12 has its own; IDs restart in every field.
        let bytes = field_dictionaries(&[
            &[(5, &[b"shared"]), (9, &[b"shared", b"", b"wide only"])],
            &[(12, &[b"big field"])],
        ]);
        let load = |fields: Option<&[u32]>| {
            decode_logtype_dictionary_fields(take(&bytes), DictionaryLimits::default(), fields)
                .expect("per-field dictionary")
        };

        let narrow = load(Some(&[5]));
        assert!(narrow.is_per_field());
        assert!(!narrow.is_complete());
        assert_eq!(Some(&b"shared"[..]), value(&narrow, 5, 0));
        assert_eq!(None, value(&narrow, 5, 1));
        assert_eq!(
            None,
            value(&narrow, 9, 0),
            "a field not asked for resolves to nothing"
        );

        let wide = load(Some(&[9, 12]));
        assert_eq!(
            Some(&b""[..]),
            value(&wide, 9, 1),
            "an empty entry is loaded, not absent"
        );
        assert_eq!(Some(&b"wide only"[..]), value(&wide, 9, 2));
        assert_eq!(Some(&b"big field"[..]), value(&wide, 12, 0));

        let whole = load(None);
        assert!(whole.is_complete());
        assert_eq!(Some(&b"shared"[..]), value(&whole, 5, 0));
        assert_eq!(3, whole.for_field(9).entries().count());
        assert_eq!(0, whole.for_field(42).entries().count());
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
