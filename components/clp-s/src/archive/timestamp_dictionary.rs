use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str::Utf8Error;

/// Resource limits applied while decoding a timestamp-dictionary metadata packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampDictionaryLimits {
    range_entries: u64,
    key_bytes: u64,
    column_ids_per_range: u64,
    patterns: u64,
    pattern_bytes: u64,
}

impl TimestampDictionaryLimits {
    /// Default limits used by archive metadata decoding.
    pub const DEFAULT: Self = Self::new(65_536, 1024 * 1024, 1_048_576, 1_048_576, 1024 * 1024);

    /// Creates explicit timestamp-dictionary resource limits.
    #[must_use]
    pub const fn new(
        max_range_entries: u64,
        max_key_bytes: u64,
        max_column_ids_per_range: u64,
        max_patterns: u64,
        max_pattern_bytes: u64,
    ) -> Self {
        Self {
            range_entries: max_range_entries,
            key_bytes: max_key_bytes,
            column_ids_per_range: max_column_ids_per_range,
            patterns: max_patterns,
            pattern_bytes: max_pattern_bytes,
        }
    }

    /// Maximum number of timestamp range entries.
    #[must_use]
    pub const fn max_range_entries(self) -> u64 {
        self.range_entries
    }

    /// Maximum UTF-8 bytes in one range key.
    #[must_use]
    pub const fn max_key_bytes(self) -> u64 {
        self.key_bytes
    }

    /// Maximum number of column IDs in one range entry.
    #[must_use]
    pub const fn max_column_ids_per_range(self) -> u64 {
        self.column_ids_per_range
    }

    /// Maximum number of timestamp patterns.
    #[must_use]
    pub const fn max_patterns(self) -> u64 {
        self.patterns
    }

    /// Maximum UTF-8 bytes in one raw timestamp pattern.
    #[must_use]
    pub const fn max_pattern_bytes(self) -> u64 {
        self.pattern_bytes
    }
}

impl Default for TimestampDictionaryLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A timestamp range's wire encoding and archive-level bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum TimestampBounds {
    /// No timestamp representation or bounds were recorded.
    Unknown,
    /// Signed integer epoch bounds.
    ///
    /// Current v0.5 writers store outward-rounded milliseconds here.
    Epoch {
        /// Inclusive lower bound.
        start: i64,
        /// Inclusive upper bound.
        end: i64,
    },
    /// Binary64 epoch bounds retained for compatibility with older producers.
    DoubleEpoch {
        /// Inclusive lower bound.
        start: f64,
        /// Inclusive upper bound.
        end: f64,
    },
}

impl TimestampBounds {
    /// Returns the numeric wire encoding used by C++ CLP-S.
    #[must_use]
    pub const fn wire_encoding(self) -> u64 {
        match self {
            Self::Unknown => 0,
            Self::Epoch { .. } => 1,
            Self::DoubleEpoch { .. } => 2,
        }
    }
}

/// One descriptor-to-column-set timestamp range.
#[derive(Clone, Debug, PartialEq)]
pub struct TimestampRangeEntry {
    key: String,
    column_ids: Vec<i32>,
    bounds: TimestampBounds,
}

impl TimestampRangeEntry {
    /// Returns the authoritative column descriptor encoded by this entry.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the schema-tree column IDs associated with the descriptor.
    #[must_use]
    pub fn column_ids(&self) -> &[i32] {
        &self.column_ids
    }

    /// Returns the entry's timestamp encoding and bounds.
    #[must_use]
    pub const fn bounds(&self) -> TimestampBounds {
        self.bounds
    }
}

/// One explicitly identified raw timestamp pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimestampPatternEntry {
    id: u64,
    raw: String,
}

impl TimestampPatternEntry {
    /// Returns the explicit pattern ID stored in the archive.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns the raw CLP-S timestamp-parser pattern.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

/// A structurally validated v0.5 timestamp-dictionary packet.
///
/// Pattern syntax is retained verbatim. Interpreting patterns and formatting timestamp values are
/// separate layers because pre-v0.5 archives use a different pattern dialect.
#[derive(Clone, Debug, PartialEq)]
pub struct TimestampDictionary {
    encoded: Vec<u8>,
    ranges: Vec<TimestampRangeEntry>,
    patterns: Vec<TimestampPatternEntry>,
    pattern_indexes: HashMap<u64, usize>,
}

impl TimestampDictionary {
    /// Decodes an owned timestamp-dictionary payload without duplicating its raw bytes.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampDictionaryError`] when the payload is truncated, exceeds a configured
    /// limit, contains an unknown or invalid entry, repeats a set/map identifier, or has trailing
    /// bytes.
    pub fn decode(
        encoded: Vec<u8>,
        limits: TimestampDictionaryLimits,
    ) -> Result<Self, TimestampDictionaryError> {
        let (ranges, patterns, pattern_indexes) = decode_entries(&encoded, limits)?;
        Ok(Self {
            encoded,
            ranges,
            patterns,
            pattern_indexes,
        })
    }

    /// Returns the exact decompressed packet bytes.
    #[must_use]
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.encoded
    }

    /// Returns range entries in their serialized order.
    #[must_use]
    pub fn ranges(&self) -> &[TimestampRangeEntry] {
        &self.ranges
    }

    /// Returns the first range, which the current C++ reader treats as authoritative.
    #[must_use]
    pub fn authoritative_range(&self) -> Option<&TimestampRangeEntry> {
        self.ranges.first()
    }

    /// Returns timestamp patterns in their serialized order.
    #[must_use]
    pub fn patterns(&self) -> &[TimestampPatternEntry] {
        &self.patterns
    }

    /// Looks up a timestamp pattern by its explicit wire ID.
    #[must_use]
    pub fn pattern(&self, id: u64) -> Option<&TimestampPatternEntry> {
        self.pattern_indexes
            .get(&id)
            .map(|index| &self.patterns[*index])
    }
}

type DecodedEntries = (
    Vec<TimestampRangeEntry>,
    Vec<TimestampPatternEntry>,
    HashMap<u64, usize>,
);

fn decode_entries(
    encoded: &[u8],
    limits: TimestampDictionaryLimits,
) -> Result<DecodedEntries, TimestampDictionaryError> {
    let mut reader = PayloadReader::new(encoded);
    let range_count = reader.read_u64("range entry count")?;
    check_limit(
        TimestampDictionaryResource::RangeEntries,
        range_count,
        limits.range_entries,
    )?;

    let mut ranges = Vec::new();
    for range_index in 0..range_count {
        ranges.push(decode_range(&mut reader, range_index, limits)?);
    }

    let pattern_count = reader.read_u64("pattern count")?;
    check_limit(
        TimestampDictionaryResource::Patterns,
        pattern_count,
        limits.patterns,
    )?;
    let mut patterns = Vec::new();
    let mut pattern_indexes = HashMap::new();
    for pattern_index in 0..pattern_count {
        let id = reader.read_u64("pattern ID")?;
        let raw = reader.read_string(
            "timestamp pattern",
            pattern_index,
            TimestampDictionaryResource::PatternBytes,
            limits.pattern_bytes,
        )?;
        if pattern_indexes.contains_key(&id) {
            return Err(TimestampDictionaryError::DuplicatePatternId { id });
        }
        pattern_indexes.insert(id, patterns.len());
        patterns.push(TimestampPatternEntry { id, raw });
    }

    if 0 != reader.remaining() {
        return Err(TimestampDictionaryError::TrailingData {
            remaining: reader.remaining(),
        });
    }
    Ok((ranges, patterns, pattern_indexes))
}

fn decode_range(
    reader: &mut PayloadReader<'_>,
    range_index: u64,
    limits: TimestampDictionaryLimits,
) -> Result<TimestampRangeEntry, TimestampDictionaryError> {
    let key = reader.read_string(
        "timestamp range key",
        range_index,
        TimestampDictionaryResource::KeyBytes,
        limits.key_bytes,
    )?;
    let column_id_count = reader.read_u64("column ID count")?;
    check_limit(
        TimestampDictionaryResource::ColumnIdsPerRange,
        column_id_count,
        limits.column_ids_per_range,
    )?;
    if 0 == column_id_count {
        return Err(TimestampDictionaryError::EmptyColumnIds { range_index });
    }

    let minimum_remaining = column_id_count
        .checked_mul(4)
        .and_then(|size| size.checked_add(8))
        .ok_or(TimestampDictionaryError::SizeOverflow {
            field: "column IDs",
        })?;
    if minimum_remaining > reader.remaining_u64() {
        return Err(TimestampDictionaryError::LengthOutOfBounds {
            field: "column IDs",
            declared: column_id_count,
            remaining: reader.remaining(),
        });
    }

    let column_id_capacity =
        usize::try_from(column_id_count).map_err(|_| TimestampDictionaryError::SizeOverflow {
            field: "column ID count",
        })?;
    let mut column_ids = Vec::with_capacity(column_id_capacity);
    let mut unique_column_ids = HashSet::with_capacity(column_id_capacity);
    for _ in 0..column_id_count {
        let column_id = reader.read_i32("column ID")?;
        if !unique_column_ids.insert(column_id) {
            return Err(TimestampDictionaryError::DuplicateColumnId {
                range_index,
                column_id,
            });
        }
        column_ids.push(column_id);
    }

    let encoding = reader.read_u64("timestamp encoding")?;
    let bounds = match encoding {
        0 => TimestampBounds::Unknown,
        1 => {
            let start = reader.read_i64("integer epoch start")?;
            let end = reader.read_i64("integer epoch end")?;
            if start > end {
                return Err(TimestampDictionaryError::InvalidIntegerBounds {
                    range_index,
                    start,
                    end,
                });
            }
            TimestampBounds::Epoch { start, end }
        }
        2 => {
            let start = reader.read_f64("double epoch start")?;
            let end = reader.read_f64("double epoch end")?;
            if !start.is_finite() || !end.is_finite() || start > end {
                return Err(TimestampDictionaryError::InvalidDoubleBounds {
                    range_index,
                    start,
                    end,
                });
            }
            TimestampBounds::DoubleEpoch { start, end }
        }
        value => {
            return Err(TimestampDictionaryError::UnknownEncoding { range_index, value });
        }
    };

    Ok(TimestampRangeEntry {
        key,
        column_ids,
        bounds,
    })
}

const fn check_limit(
    resource: TimestampDictionaryResource,
    actual: u64,
    limit: u64,
) -> Result<(), TimestampDictionaryError> {
    if actual > limit {
        Err(TimestampDictionaryError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

struct PayloadReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PayloadReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn remaining_u64(&self) -> u64 {
        u64::try_from(self.remaining()).unwrap_or(u64::MAX)
    }

    fn take(
        &mut self,
        size: usize,
        field: &'static str,
    ) -> Result<&'a [u8], TimestampDictionaryError> {
        let end = self
            .offset
            .checked_add(size)
            .ok_or(TimestampDictionaryError::SizeOverflow { field })?;
        let remaining = self.remaining();
        let bytes =
            self.bytes
                .get(self.offset..end)
                .ok_or(TimestampDictionaryError::Truncated {
                    field,
                    offset: self.offset,
                    needed: size,
                    remaining,
                })?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_u64(&mut self, field: &'static str) -> Result<u64, TimestampDictionaryError> {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(self.take(8, field)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_i64(&mut self, field: &'static str) -> Result<i64, TimestampDictionaryError> {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(self.take(8, field)?);
        Ok(i64::from_le_bytes(bytes))
    }

    fn read_i32(&mut self, field: &'static str) -> Result<i32, TimestampDictionaryError> {
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(self.take(4, field)?);
        Ok(i32::from_le_bytes(bytes))
    }

    fn read_f64(&mut self, field: &'static str) -> Result<f64, TimestampDictionaryError> {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(self.take(8, field)?);
        Ok(f64::from_le_bytes(bytes))
    }

    fn read_string(
        &mut self,
        field: &'static str,
        entry_index: u64,
        resource: TimestampDictionaryResource,
        limit: u64,
    ) -> Result<String, TimestampDictionaryError> {
        let length = self.read_u64(field)?;
        check_limit(resource, length, limit)?;
        if length > self.remaining_u64() {
            return Err(TimestampDictionaryError::LengthOutOfBounds {
                field,
                declared: length,
                remaining: self.remaining(),
            });
        }
        let length = usize::try_from(length)
            .map_err(|_| TimestampDictionaryError::SizeOverflow { field })?;
        let bytes = self.take(length, field)?;
        let text =
            std::str::from_utf8(bytes).map_err(|source| TimestampDictionaryError::InvalidUtf8 {
                field,
                entry_index,
                source,
            })?;
        Ok(text.to_owned())
    }
}

/// A timestamp-dictionary resource controlled by [`TimestampDictionaryLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampDictionaryResource {
    /// Number of range entries.
    RangeEntries,
    /// Bytes in one range key.
    KeyBytes,
    /// Number of column IDs in one range entry.
    ColumnIdsPerRange,
    /// Number of timestamp patterns.
    Patterns,
    /// Bytes in one timestamp pattern.
    PatternBytes,
}

impl Display for TimestampDictionaryResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RangeEntries => "range entries",
            Self::KeyBytes => "range-key bytes",
            Self::ColumnIdsPerRange => "column IDs per range",
            Self::Patterns => "patterns",
            Self::PatternBytes => "pattern bytes",
        })
    }
}

/// Failure to decode or structurally validate a v0.5 timestamp-dictionary packet.
#[derive(Debug)]
#[non_exhaustive]
pub enum TimestampDictionaryError {
    /// An encoded field ended before its fixed or declared size.
    Truncated {
        /// Field being decoded.
        field: &'static str,
        /// Byte offset at which the field began.
        offset: usize,
        /// Bytes required for the field.
        needed: usize,
        /// Bytes available at the offset.
        remaining: usize,
    },
    /// A declared length could not fit in the remaining payload.
    LengthOutOfBounds {
        /// Length-delimited field being decoded.
        field: &'static str,
        /// Declared length or element count.
        declared: u64,
        /// Bytes remaining after the declaration.
        remaining: usize,
    },
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow {
        /// Field whose size overflowed.
        field: &'static str,
    },
    /// A configured resource limit was exceeded.
    LimitExceeded {
        /// Limited resource.
        resource: TimestampDictionaryResource,
        /// Value declared by the packet.
        actual: u64,
        /// Configured maximum value.
        limit: u64,
    },
    /// A range key or timestamp pattern was not UTF-8.
    InvalidUtf8 {
        /// Text field being decoded.
        field: &'static str,
        /// Zero-based range or pattern index.
        entry_index: u64,
        /// UTF-8 validation error.
        source: Utf8Error,
    },
    /// A range entry did not identify any schema-tree columns.
    EmptyColumnIds {
        /// Zero-based range index.
        range_index: u64,
    },
    /// A column ID appeared more than once in one range entry.
    DuplicateColumnId {
        /// Zero-based range index.
        range_index: u64,
        /// Repeated schema-tree column ID.
        column_id: i32,
    },
    /// A range used an unknown numeric timestamp encoding.
    UnknownEncoding {
        /// Zero-based range index.
        range_index: u64,
        /// Unknown wire value.
        value: u64,
    },
    /// Integer epoch bounds were reversed.
    InvalidIntegerBounds {
        /// Zero-based range index.
        range_index: u64,
        /// Encoded lower bound.
        start: i64,
        /// Encoded upper bound.
        end: i64,
    },
    /// Double epoch bounds were non-finite or reversed.
    InvalidDoubleBounds {
        /// Zero-based range index.
        range_index: u64,
        /// Encoded lower bound.
        start: f64,
        /// Encoded upper bound.
        end: f64,
    },
    /// A timestamp pattern ID appeared more than once.
    DuplicatePatternId {
        /// Repeated explicit wire ID.
        id: u64,
    },
    /// Bytes followed the declared range and pattern sequences.
    TrailingData {
        /// Unconsumed payload bytes.
        remaining: usize,
    },
}

impl Display for TimestampDictionaryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                field,
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "timestamp dictionary {field} at offset {offset} needs {needed} bytes; \
                 {remaining} remain"
            ),
            Self::LengthOutOfBounds {
                field,
                declared,
                remaining,
            } => write!(
                formatter,
                "timestamp dictionary {field} declares {declared}; only {remaining} bytes remain"
            ),
            Self::SizeOverflow { field } => {
                write!(formatter, "timestamp dictionary {field} size overflow")
            }
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "timestamp dictionary {resource} value {actual} exceeds limit {limit}"
            ),
            Self::InvalidUtf8 {
                field,
                entry_index,
                source,
            } => write!(
                formatter,
                "timestamp dictionary {field} {entry_index} is not UTF-8: {source}"
            ),
            Self::EmptyColumnIds { range_index } => write!(
                formatter,
                "timestamp dictionary range {range_index} has no column IDs"
            ),
            Self::DuplicateColumnId {
                range_index,
                column_id,
            } => write!(
                formatter,
                "timestamp dictionary range {range_index} repeats column ID {column_id}"
            ),
            Self::UnknownEncoding { range_index, value } => write!(
                formatter,
                "timestamp dictionary range {range_index} uses unknown encoding {value}"
            ),
            Self::InvalidIntegerBounds {
                range_index,
                start,
                end,
            } => write!(
                formatter,
                "timestamp dictionary range {range_index} has reversed integer bounds \
                 {start}..{end}"
            ),
            Self::InvalidDoubleBounds {
                range_index,
                start,
                end,
            } => write!(
                formatter,
                "timestamp dictionary range {range_index} has invalid double bounds {start}..{end}"
            ),
            Self::DuplicatePatternId { id } => {
                write!(formatter, "timestamp dictionary repeats pattern ID {id}")
            }
            Self::TrailingData { remaining } => write!(
                formatter,
                "timestamp dictionary has {remaining} trailing bytes"
            ),
        }
    }
}

impl Error for TimestampDictionaryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8 { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_u64(value: u64, output: &mut Vec<u8>) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn append_i64(value: i64, output: &mut Vec<u8>) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn append_i32(value: i32, output: &mut Vec<u8>) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn append_f64(value: f64, output: &mut Vec<u8>) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn append_string(value: &[u8], output: &mut Vec<u8>) {
        append_u64(
            u64::try_from(value.len()).expect("test string length fits u64"),
            output,
        );
        output.extend_from_slice(value);
    }

    fn append_range_prefix(key: &[u8], column_ids: &[i32], output: &mut Vec<u8>) {
        append_string(key, output);
        append_u64(
            u64::try_from(column_ids.len()).expect("test column count fits u64"),
            output,
        );
        for column_id in column_ids {
            append_i32(*column_id, output);
        }
    }

    fn representative_payload() -> Vec<u8> {
        let mut payload = Vec::new();
        append_u64(3, &mut payload);

        append_range_prefix(b"unknown", &[3], &mut payload);
        append_u64(0, &mut payload);

        append_range_prefix(b"ts", &[7, 9], &mut payload);
        append_u64(1, &mut payload);
        append_i64(-1_001, &mut payload);
        append_i64(2_002, &mut payload);

        append_range_prefix(b"legacy", &[11], &mut payload);
        append_u64(2, &mut payload);
        append_f64(-0.5, &mut payload);
        append_f64(1.25, &mut payload);

        append_u64(2, &mut payload);
        append_u64(42, &mut payload);
        append_string(br#""\Y-\m-\d""#, &mut payload);
        append_u64(7, &mut payload);
        append_string(br"\L", &mut payload);
        payload
    }

    #[test]
    fn decodes_all_wire_encodings_and_explicit_pattern_ids() {
        let payload = representative_payload();
        let dictionary =
            TimestampDictionary::decode(payload.clone(), TimestampDictionaryLimits::default())
                .expect("valid timestamp dictionary");

        assert_eq!(payload, dictionary.encoded_bytes());
        assert_eq!(3, dictionary.ranges().len());
        assert_eq!("unknown", dictionary.authoritative_range().unwrap().key());
        assert_eq!(&[3], dictionary.ranges()[0].column_ids());
        assert_eq!(TimestampBounds::Unknown, dictionary.ranges()[0].bounds());
        assert_eq!(
            TimestampBounds::Epoch {
                start: -1_001,
                end: 2_002,
            },
            dictionary.ranges()[1].bounds()
        );
        assert_eq!(
            TimestampBounds::DoubleEpoch {
                start: -0.5,
                end: 1.25,
            },
            dictionary.ranges()[2].bounds()
        );
        assert_eq!(&[7, 9], dictionary.ranges()[1].column_ids());
        assert_eq!(2, dictionary.patterns().len());
        assert_eq!(42, dictionary.patterns()[0].id());
        assert_eq!(r#""\Y-\m-\d""#, dictionary.pattern(42).unwrap().raw());
        assert_eq!(r"\L", dictionary.pattern(7).unwrap().raw());
        assert!(dictionary.pattern(0).is_none());
    }

    #[test]
    fn rejects_every_truncated_prefix() {
        let payload = representative_payload();
        for end in 0..payload.len() {
            assert!(
                TimestampDictionary::decode(
                    payload[..end].to_vec(),
                    TimestampDictionaryLimits::default()
                )
                .is_err(),
                "prefix ending at {end} was accepted"
            );
        }
    }

    #[test]
    fn rejects_duplicate_pattern_ids() {
        let mut payload = Vec::new();
        append_u64(0, &mut payload);
        append_u64(2, &mut payload);
        for raw in [br"\L".as_slice(), br"\N".as_slice()] {
            append_u64(5, &mut payload);
            append_string(raw, &mut payload);
        }

        assert!(matches!(
            TimestampDictionary::decode(payload, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::DuplicatePatternId { id: 5 })
        ));
    }

    #[test]
    fn rejects_duplicate_column_ids() {
        let mut payload = Vec::new();
        append_u64(1, &mut payload);
        append_range_prefix(b"ts", &[4, 4], &mut payload);
        append_u64(0, &mut payload);
        append_u64(0, &mut payload);

        assert!(matches!(
            TimestampDictionary::decode(payload, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::DuplicateColumnId {
                range_index: 0,
                column_id: 4
            })
        ));
    }

    #[test]
    fn rejects_unknown_encoding() {
        let mut payload = Vec::new();
        append_u64(1, &mut payload);
        append_range_prefix(b"ts", &[1], &mut payload);
        append_u64(3, &mut payload);
        append_u64(0, &mut payload);

        assert!(matches!(
            TimestampDictionary::decode(payload, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::UnknownEncoding {
                range_index: 0,
                value: 3
            })
        ));
    }

    #[test]
    fn rejects_invalid_bounds() {
        let mut integer_payload = Vec::new();
        append_u64(1, &mut integer_payload);
        append_range_prefix(b"ts", &[1], &mut integer_payload);
        append_u64(1, &mut integer_payload);
        append_i64(2, &mut integer_payload);
        append_i64(1, &mut integer_payload);
        append_u64(0, &mut integer_payload);
        assert!(matches!(
            TimestampDictionary::decode(integer_payload, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::InvalidIntegerBounds { .. })
        ));

        let mut double_payload = Vec::new();
        append_u64(1, &mut double_payload);
        append_range_prefix(b"ts", &[1], &mut double_payload);
        append_u64(2, &mut double_payload);
        append_f64(f64::NAN, &mut double_payload);
        append_f64(1.0, &mut double_payload);
        append_u64(0, &mut double_payload);
        assert!(matches!(
            TimestampDictionary::decode(double_payload, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::InvalidDoubleBounds { .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf8_and_trailing_data() {
        let mut invalid_utf8 = Vec::new();
        append_u64(1, &mut invalid_utf8);
        append_range_prefix(&[0xff], &[1], &mut invalid_utf8);
        append_u64(0, &mut invalid_utf8);
        append_u64(0, &mut invalid_utf8);
        assert!(matches!(
            TimestampDictionary::decode(invalid_utf8, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::InvalidUtf8 {
                field: "timestamp range key",
                entry_index: 0,
                ..
            })
        ));

        let mut trailing = representative_payload();
        trailing.push(0);
        assert!(matches!(
            TimestampDictionary::decode(trailing, TimestampDictionaryLimits::default()),
            Err(TimestampDictionaryError::TrailingData { remaining: 1 })
        ));
    }

    #[test]
    fn enforces_limits_before_length_allocation() {
        let mut payload = Vec::new();
        append_u64(2, &mut payload);
        let limits = TimestampDictionaryLimits::new(1, u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        assert!(matches!(
            TimestampDictionary::decode(payload, limits),
            Err(TimestampDictionaryError::LimitExceeded {
                resource: TimestampDictionaryResource::RangeEntries,
                actual: 2,
                limit: 1
            })
        ));

        payload = Vec::new();
        append_u64(1, &mut payload);
        append_u64(u64::MAX, &mut payload);
        let limits = TimestampDictionaryLimits::new(u64::MAX, 8, u64::MAX, u64::MAX, u64::MAX);
        assert!(matches!(
            TimestampDictionary::decode(payload, limits),
            Err(TimestampDictionaryError::LimitExceeded {
                resource: TimestampDictionaryResource::KeyBytes,
                actual: u64::MAX,
                limit: 8
            })
        ));
    }
}
