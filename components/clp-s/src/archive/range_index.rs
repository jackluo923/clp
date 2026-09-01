use std::collections::BTreeMap;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::Range;
use std::str::Utf8Error;

/// Resource limits applied while decoding a range-index metadata packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeIndexLimits {
    entries: u32,
    fields_per_range: u32,
    collection_entries: u32,
    nesting_depth: u32,
    string_bytes: u32,
    binary_bytes: u32,
    total_values: u64,
}

impl RangeIndexLimits {
    /// Default limits used by archive metadata decoding.
    pub const DEFAULT: Self = Self::new(
        65_536,
        4_096,
        1_048_576,
        64,
        1024 * 1024,
        16 * 1024 * 1024,
        4_194_304,
    );

    /// Creates explicit range-index resource limits.
    #[must_use]
    pub const fn new(
        max_entries: u32,
        max_fields_per_range: u32,
        max_collection_entries: u32,
        max_nesting_depth: u32,
        max_string_bytes: u32,
        max_binary_bytes: u32,
        max_total_values: u64,
    ) -> Self {
        Self {
            entries: max_entries,
            fields_per_range: max_fields_per_range,
            collection_entries: max_collection_entries,
            nesting_depth: max_nesting_depth,
            string_bytes: max_string_bytes,
            binary_bytes: max_binary_bytes,
            total_values: max_total_values,
        }
    }

    /// Maximum number of ranges in one packet.
    #[must_use]
    pub const fn max_entries(self) -> u32 {
        self.entries
    }

    /// Maximum number of metadata fields in one range.
    #[must_use]
    pub const fn max_fields_per_range(self) -> u32 {
        self.fields_per_range
    }

    /// Maximum number of elements in one nested array or map.
    #[must_use]
    pub const fn max_collection_entries(self) -> u32 {
        self.collection_entries
    }

    /// Maximum number of nested array/map levels in a metadata value.
    #[must_use]
    pub const fn max_nesting_depth(self) -> u32 {
        self.nesting_depth
    }

    /// Maximum UTF-8 bytes in one string or map key.
    #[must_use]
    pub const fn max_string_bytes(self) -> u32 {
        self.string_bytes
    }

    /// Maximum bytes in one binary value.
    #[must_use]
    pub const fn max_binary_bytes(self) -> u32 {
        self.binary_bytes
    }

    /// Maximum total metadata values decoded across the packet.
    #[must_use]
    pub const fn max_total_values(self) -> u64 {
        self.total_values
    }
}

impl Default for RangeIndexLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A JSON/`MessagePack` value retained from a range's metadata map.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RangeIndexValue {
    /// JSON null.
    Null,
    /// A Boolean value.
    Boolean(bool),
    /// A signed integer value.
    Signed(i64),
    /// An unsigned integer value.
    Unsigned(u64),
    /// A binary32 or binary64 floating-point value, represented as binary64.
    Float(f64),
    /// A UTF-8 string.
    String(String),
    /// A `MessagePack` binary value.
    Binary(Vec<u8>),
    /// An ordered JSON array.
    Array(Vec<Self>),
    /// A JSON object. `MessagePack` map order is intentionally not retained.
    Object(BTreeMap<String, Self>),
}

impl RangeIndexValue {
    /// Returns this value as a string when it has string type.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Returns this value as a nonnegative integer when representable by `u64`.
    #[must_use]
    pub const fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Unsigned(value) => Some(*value),
            Self::Signed(value) if 0 <= *value => Some(value.cast_unsigned()),
            _ => None,
        }
    }
}

/// One structurally validated range-index entry.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeIndexEntry {
    range: Range<u64>,
    fields: BTreeMap<String, RangeIndexValue>,
}

impl RangeIndexEntry {
    /// Returns the inclusive start index.
    #[must_use]
    pub const fn start_index(&self) -> u64 {
        self.range.start
    }

    /// Returns the exclusive end index.
    #[must_use]
    pub const fn end_index(&self) -> u64 {
        self.range.end
    }

    /// Returns the half-open archive-local log-event range.
    #[must_use]
    pub fn range(&self) -> Range<u64> {
        self.range.clone()
    }

    /// Returns the entry's semantic metadata object.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<String, RangeIndexValue> {
        &self.fields
    }

    /// Looks up one metadata field by name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&RangeIndexValue> {
        self.fields.get(name)
    }
}

/// A structurally validated v0.5 archive range index.
///
/// Packet decoding validates shape, types, configured resource limits, half-open bounds, and
/// monotonic non-overlap. Call [`Self::validate_record_domain`] once the archive's table metadata
/// establishes the log-event count.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeIndex {
    encoded: Vec<u8>,
    entries: Vec<RangeIndexEntry>,
}

impl RangeIndex {
    /// Decodes an owned `MessagePack` range-index payload without duplicating its raw bytes.
    ///
    /// # Errors
    ///
    /// Returns [`RangeIndexError`] for invalid `MessagePack`, a malformed range or metadata object,
    /// non-monotonic ranges, trailing bytes, or a configured resource-limit violation.
    pub fn decode(encoded: Vec<u8>, limits: RangeIndexLimits) -> Result<Self, RangeIndexError> {
        let entries = decode_entries(&encoded, limits)?;
        Ok(Self { encoded, entries })
    }

    /// Returns the exact decompressed packet bytes.
    #[must_use]
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.encoded
    }

    /// Returns entries in serialized range order.
    #[must_use]
    pub fn entries(&self) -> &[RangeIndexEntry] {
        &self.entries
    }

    /// Returns whether the packet contains no ranges.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Validates all range ends against the known archive-local log-event count.
    ///
    /// This check is explicit because the record domain is stored in table metadata, not in the
    /// range-index packet.
    ///
    /// # Errors
    ///
    /// Returns [`RangeIndexError::RangeOutOfDomain`] when any exclusive end exceeds
    /// `record_count`.
    pub fn validate_record_domain(&self, record_count: u64) -> Result<(), RangeIndexError> {
        for (entry_index, entry) in (0_u64..).zip(&self.entries) {
            if entry.end_index() > record_count {
                return Err(RangeIndexError::RangeOutOfDomain {
                    entry_index,
                    end: entry.end_index(),
                    record_count,
                });
            }
        }
        Ok(())
    }
}

fn decode_entries(
    encoded: &[u8],
    limits: RangeIndexLimits,
) -> Result<Vec<RangeIndexEntry>, RangeIndexError> {
    let mut reader = MessagePackReader::new(encoded, limits);
    let entry_count = reader.read_array_len("range-index packet")?;
    check_limit(
        RangeIndexResource::Entries,
        u64::from(entry_count),
        u64::from(limits.entries),
    )?;
    reader.check_element_bytes("range-index entries", entry_count, 1)?;

    let capacity = usize::try_from(entry_count).map_err(|_| RangeIndexError::SizeOverflow {
        context: "range-index entry count",
    })?;
    let mut entries = Vec::with_capacity(capacity);
    let mut previous_end = None;
    for entry_index in 0..u64::from(entry_count) {
        let entry = reader.read_entry(entry_index)?;
        if let Some(end) = previous_end
            && entry.start_index() < end
        {
            return Err(RangeIndexError::NonMonotonicRange {
                entry_index,
                previous_end: end,
                start: entry.start_index(),
            });
        }
        previous_end = Some(entry.end_index());
        entries.push(entry);
    }

    if 0 != reader.remaining() {
        return Err(RangeIndexError::TrailingData {
            remaining: reader.remaining(),
        });
    }
    Ok(entries)
}

struct MessagePackReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    limits: RangeIndexLimits,
    value_count: u64,
}

impl<'a> MessagePackReader<'a> {
    const fn new(bytes: &'a [u8], limits: RangeIndexLimits) -> Self {
        Self {
            bytes,
            offset: 0,
            limits,
            value_count: 0,
        }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn read_entry(&mut self, entry_index: u64) -> Result<RangeIndexEntry, RangeIndexError> {
        let member_count = self.read_map_len("range-index entry")?;
        check_limit(
            RangeIndexResource::CollectionEntries,
            u64::from(member_count),
            u64::from(self.limits.collection_entries),
        )?;
        self.check_element_bytes("range-index entry members", member_count, 2)?;

        let mut keys = HashSet::new();
        let mut start = None;
        let mut end = None;
        let mut fields = None;
        for _ in 0..member_count {
            let key = self.read_string("range-index entry key")?;
            if !keys.insert(key.clone()) {
                return Err(RangeIndexError::DuplicateKey {
                    context: "range-index entry",
                    key,
                });
            }
            match key.as_str() {
                "s" => start = Some(self.read_index(entry_index, "s")?),
                "e" => end = Some(self.read_index(entry_index, "e")?),
                "f" => fields = Some(self.read_fields()?),
                _ => {
                    self.read_value(0)?;
                }
            }
        }

        let start = start.ok_or(RangeIndexError::MissingEntryField {
            entry_index,
            field: "s",
        })?;
        let end = end.ok_or(RangeIndexError::MissingEntryField {
            entry_index,
            field: "e",
        })?;
        let fields = fields.ok_or(RangeIndexError::MissingEntryField {
            entry_index,
            field: "f",
        })?;
        if start > end {
            return Err(RangeIndexError::ReversedRange {
                entry_index,
                start,
                end,
            });
        }
        Ok(RangeIndexEntry {
            range: start..end,
            fields,
        })
    }

    fn read_fields(&mut self) -> Result<BTreeMap<String, RangeIndexValue>, RangeIndexError> {
        let field_count = self.read_map_len("range metadata fields")?;
        check_limit(
            RangeIndexResource::FieldsPerRange,
            u64::from(field_count),
            u64::from(self.limits.fields_per_range),
        )?;
        self.check_element_bytes("range metadata fields", field_count, 2)?;

        let mut fields = BTreeMap::new();
        for _ in 0..field_count {
            let key = self.read_string("range metadata field name")?;
            let value = self.read_value(0)?;
            if fields.insert(key.clone(), value).is_some() {
                return Err(RangeIndexError::DuplicateKey {
                    context: "range metadata fields",
                    key,
                });
            }
        }
        Ok(fields)
    }

    fn read_value(&mut self, nesting_depth: u32) -> Result<RangeIndexValue, RangeIndexError> {
        self.value_count =
            self.value_count
                .checked_add(1)
                .ok_or(RangeIndexError::SizeOverflow {
                    context: "range metadata value count",
                })?;
        check_limit(
            RangeIndexResource::TotalValues,
            self.value_count,
            self.limits.total_values,
        )?;

        let (marker, marker_offset) = self.read_marker("range metadata value")?;
        match marker {
            0x00..=0x7f => Ok(RangeIndexValue::Unsigned(u64::from(marker))),
            0x80..=0x8f | 0xde | 0xdf => {
                let depth = self.next_depth(nesting_depth)?;
                let length = self.map_len_from_marker(marker, marker_offset)?;
                self.read_object(length, depth)
            }
            0x90..=0x9f | 0xdc | 0xdd => {
                let depth = self.next_depth(nesting_depth)?;
                let length = self.array_len_from_marker(marker, marker_offset)?;
                self.read_array(length, depth)
            }
            0xa0..=0xbf | 0xd9..=0xdb => self
                .read_string_from_marker(marker, marker_offset, "range metadata string")
                .map(RangeIndexValue::String),
            0xc0 => Ok(RangeIndexValue::Null),
            0xc2 => Ok(RangeIndexValue::Boolean(false)),
            0xc3 => Ok(RangeIndexValue::Boolean(true)),
            0xc4..=0xc6 => self
                .read_binary_from_marker(marker, marker_offset)
                .map(RangeIndexValue::Binary),
            0xca => validate_float(
                f64::from(f32::from_bits(self.read_be_u32("binary32 value")?)),
                marker_offset,
            ),
            0xcb => validate_float(
                f64::from_bits(self.read_be_u64("binary64 value")?),
                marker_offset,
            ),
            0xcc => Ok(RangeIndexValue::Unsigned(u64::from(
                self.read_u8("u8 value")?,
            ))),
            0xcd => Ok(RangeIndexValue::Unsigned(u64::from(
                self.read_be_u16("u16 value")?,
            ))),
            0xce => Ok(RangeIndexValue::Unsigned(u64::from(
                self.read_be_u32("u32 value")?,
            ))),
            0xcf => Ok(RangeIndexValue::Unsigned(self.read_be_u64("u64 value")?)),
            0xd0 => Ok(RangeIndexValue::Signed(i64::from(
                self.read_i8("i8 value")?,
            ))),
            0xd1 => Ok(RangeIndexValue::Signed(i64::from(
                self.read_be_i16("i16 value")?,
            ))),
            0xd2 => Ok(RangeIndexValue::Signed(i64::from(
                self.read_be_i32("i32 value")?,
            ))),
            0xd3 => Ok(RangeIndexValue::Signed(self.read_be_i64("i64 value")?)),
            0xe0..=0xff => Ok(RangeIndexValue::Signed(i64::from(marker.cast_signed()))),
            _ => Err(RangeIndexError::UnsupportedMarker {
                marker,
                offset: marker_offset,
            }),
        }
    }

    fn read_array(
        &mut self,
        length: u32,
        nesting_depth: u32,
    ) -> Result<RangeIndexValue, RangeIndexError> {
        check_limit(
            RangeIndexResource::CollectionEntries,
            u64::from(length),
            u64::from(self.limits.collection_entries),
        )?;
        self.check_element_bytes("range metadata array", length, 1)?;
        let capacity = usize::try_from(length).map_err(|_| RangeIndexError::SizeOverflow {
            context: "range metadata array length",
        })?;
        let mut values = Vec::with_capacity(capacity);
        for _ in 0..length {
            values.push(self.read_value(nesting_depth)?);
        }
        Ok(RangeIndexValue::Array(values))
    }

    fn read_object(
        &mut self,
        length: u32,
        nesting_depth: u32,
    ) -> Result<RangeIndexValue, RangeIndexError> {
        check_limit(
            RangeIndexResource::CollectionEntries,
            u64::from(length),
            u64::from(self.limits.collection_entries),
        )?;
        self.check_element_bytes("range metadata object", length, 2)?;
        let mut values = BTreeMap::new();
        for _ in 0..length {
            let key = self.read_string("range metadata object key")?;
            let value = self.read_value(nesting_depth)?;
            if values.insert(key.clone(), value).is_some() {
                return Err(RangeIndexError::DuplicateKey {
                    context: "range metadata object",
                    key,
                });
            }
        }
        Ok(RangeIndexValue::Object(values))
    }

    fn next_depth(&self, current: u32) -> Result<u32, RangeIndexError> {
        let next = current
            .checked_add(1)
            .ok_or(RangeIndexError::SizeOverflow {
                context: "range metadata nesting depth",
            })?;
        check_limit(
            RangeIndexResource::NestingDepth,
            u64::from(next),
            u64::from(self.limits.nesting_depth),
        )?;
        Ok(next)
    }

    fn read_index(
        &mut self,
        entry_index: u64,
        field: &'static str,
    ) -> Result<u64, RangeIndexError> {
        let (marker, marker_offset) = self.read_marker("range index bound")?;
        let signed = match marker {
            0x00..=0x7f => return Ok(u64::from(marker)),
            0xcc => return Ok(u64::from(self.read_u8("range index u8")?)),
            0xcd => return Ok(u64::from(self.read_be_u16("range index u16")?)),
            0xce => return Ok(u64::from(self.read_be_u32("range index u32")?)),
            0xcf => return self.read_be_u64("range index u64"),
            0xd0 => i64::from(self.read_i8("range index i8")?),
            0xd1 => i64::from(self.read_be_i16("range index i16")?),
            0xd2 => i64::from(self.read_be_i32("range index i32")?),
            0xd3 => self.read_be_i64("range index i64")?,
            0xe0..=0xff => i64::from(marker.cast_signed()),
            _ => {
                return Err(RangeIndexError::UnexpectedType {
                    context: "range index bound",
                    expected: "integer",
                    marker,
                    offset: marker_offset,
                });
            }
        };
        if signed < 0 {
            return Err(RangeIndexError::NegativeIndex {
                entry_index,
                field,
                value: signed,
            });
        }
        Ok(signed.cast_unsigned())
    }

    fn read_array_len(&mut self, context: &'static str) -> Result<u32, RangeIndexError> {
        let (marker, marker_offset) = self.read_marker(context)?;
        self.array_len_from_marker(marker, marker_offset)
    }

    fn array_len_from_marker(
        &mut self,
        marker: u8,
        marker_offset: usize,
    ) -> Result<u32, RangeIndexError> {
        match marker {
            0x90..=0x9f => Ok(u32::from(marker & 0x0f)),
            0xdc => Ok(u32::from(self.read_be_u16("array length")?)),
            0xdd => self.read_be_u32("array length"),
            _ => Err(RangeIndexError::UnexpectedType {
                context: "range-index packet",
                expected: "array",
                marker,
                offset: marker_offset,
            }),
        }
    }

    fn read_map_len(&mut self, context: &'static str) -> Result<u32, RangeIndexError> {
        let (marker, marker_offset) = self.read_marker(context)?;
        self.map_len_from_marker(marker, marker_offset)
    }

    fn map_len_from_marker(
        &mut self,
        marker: u8,
        marker_offset: usize,
    ) -> Result<u32, RangeIndexError> {
        match marker {
            0x80..=0x8f => Ok(u32::from(marker & 0x0f)),
            0xde => Ok(u32::from(self.read_be_u16("map length")?)),
            0xdf => self.read_be_u32("map length"),
            _ => Err(RangeIndexError::UnexpectedType {
                context: "MessagePack map",
                expected: "map",
                marker,
                offset: marker_offset,
            }),
        }
    }

    fn read_string(&mut self, context: &'static str) -> Result<String, RangeIndexError> {
        let (marker, marker_offset) = self.read_marker(context)?;
        self.read_string_from_marker(marker, marker_offset, context)
    }

    fn read_string_from_marker(
        &mut self,
        marker: u8,
        marker_offset: usize,
        context: &'static str,
    ) -> Result<String, RangeIndexError> {
        let length = match marker {
            0xa0..=0xbf => u32::from(marker & 0x1f),
            0xd9 => u32::from(self.read_u8("string length")?),
            0xda => u32::from(self.read_be_u16("string length")?),
            0xdb => self.read_be_u32("string length")?,
            _ => {
                return Err(RangeIndexError::UnexpectedType {
                    context,
                    expected: "UTF-8 string",
                    marker,
                    offset: marker_offset,
                });
            }
        };
        check_limit(
            RangeIndexResource::StringBytes,
            u64::from(length),
            u64::from(self.limits.string_bytes),
        )?;
        let length = self.check_byte_length(context, length)?;
        let offset = self.offset;
        let bytes = self.take(length, context)?;
        let value = std::str::from_utf8(bytes).map_err(|source| RangeIndexError::InvalidUtf8 {
            context,
            offset,
            source,
        })?;
        Ok(value.to_owned())
    }

    fn read_binary_from_marker(
        &mut self,
        marker: u8,
        marker_offset: usize,
    ) -> Result<Vec<u8>, RangeIndexError> {
        let length = match marker {
            0xc4 => u32::from(self.read_u8("binary length")?),
            0xc5 => u32::from(self.read_be_u16("binary length")?),
            0xc6 => self.read_be_u32("binary length")?,
            _ => {
                return Err(RangeIndexError::UnexpectedType {
                    context: "range metadata binary",
                    expected: "binary",
                    marker,
                    offset: marker_offset,
                });
            }
        };
        check_limit(
            RangeIndexResource::BinaryBytes,
            u64::from(length),
            u64::from(self.limits.binary_bytes),
        )?;
        let length = self.check_byte_length("range metadata binary", length)?;
        Ok(self.take(length, "range metadata binary")?.to_owned())
    }

    fn check_element_bytes(
        &self,
        context: &'static str,
        count: u32,
        minimum_bytes: u32,
    ) -> Result<(), RangeIndexError> {
        let minimum = u64::from(count)
            .checked_mul(u64::from(minimum_bytes))
            .ok_or(RangeIndexError::SizeOverflow { context })?;
        if minimum > self.remaining_u64() {
            return Err(RangeIndexError::LengthOutOfBounds {
                context,
                declared: u64::from(count),
                remaining: self.remaining(),
            });
        }
        Ok(())
    }

    fn check_byte_length(
        &self,
        context: &'static str,
        length: u32,
    ) -> Result<usize, RangeIndexError> {
        if u64::from(length) > self.remaining_u64() {
            return Err(RangeIndexError::LengthOutOfBounds {
                context,
                declared: u64::from(length),
                remaining: self.remaining(),
            });
        }
        usize::try_from(length).map_err(|_| RangeIndexError::SizeOverflow { context })
    }

    fn remaining_u64(&self) -> u64 {
        u64::try_from(self.remaining()).unwrap_or(u64::MAX)
    }

    fn read_marker(&mut self, context: &'static str) -> Result<(u8, usize), RangeIndexError> {
        let offset = self.offset;
        Ok((self.read_u8(context)?, offset))
    }

    fn read_u8(&mut self, context: &'static str) -> Result<u8, RangeIndexError> {
        Ok(self.take(1, context)?[0])
    }

    fn read_i8(&mut self, context: &'static str) -> Result<i8, RangeIndexError> {
        Ok(i8::from_be_bytes([self.read_u8(context)?]))
    }

    fn read_be_u16(&mut self, context: &'static str) -> Result<u16, RangeIndexError> {
        let mut bytes = [0_u8; 2];
        bytes.copy_from_slice(self.take(2, context)?);
        Ok(u16::from_be_bytes(bytes))
    }

    fn read_be_i16(&mut self, context: &'static str) -> Result<i16, RangeIndexError> {
        let mut bytes = [0_u8; 2];
        bytes.copy_from_slice(self.take(2, context)?);
        Ok(i16::from_be_bytes(bytes))
    }

    fn read_be_u32(&mut self, context: &'static str) -> Result<u32, RangeIndexError> {
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(self.take(4, context)?);
        Ok(u32::from_be_bytes(bytes))
    }

    fn read_be_i32(&mut self, context: &'static str) -> Result<i32, RangeIndexError> {
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(self.take(4, context)?);
        Ok(i32::from_be_bytes(bytes))
    }

    fn read_be_u64(&mut self, context: &'static str) -> Result<u64, RangeIndexError> {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(self.take(8, context)?);
        Ok(u64::from_be_bytes(bytes))
    }

    fn read_be_i64(&mut self, context: &'static str) -> Result<i64, RangeIndexError> {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(self.take(8, context)?);
        Ok(i64::from_be_bytes(bytes))
    }

    fn take(&mut self, size: usize, context: &'static str) -> Result<&'a [u8], RangeIndexError> {
        let end = self
            .offset
            .checked_add(size)
            .ok_or(RangeIndexError::SizeOverflow { context })?;
        let remaining = self.remaining();
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(RangeIndexError::Truncated {
                context,
                offset: self.offset,
                needed: size,
                remaining,
            })?;
        self.offset = end;
        Ok(bytes)
    }
}

const fn validate_float(value: f64, offset: usize) -> Result<RangeIndexValue, RangeIndexError> {
    if value.is_finite() {
        Ok(RangeIndexValue::Float(value))
    } else {
        Err(RangeIndexError::NonFiniteFloat { value, offset })
    }
}

const fn check_limit(
    resource: RangeIndexResource,
    actual: u64,
    limit: u64,
) -> Result<(), RangeIndexError> {
    if actual > limit {
        Err(RangeIndexError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

/// A range-index resource controlled by [`RangeIndexLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RangeIndexResource {
    /// Number of range entries.
    Entries,
    /// Number of metadata fields in one range.
    FieldsPerRange,
    /// Number of elements in one nested array or map.
    CollectionEntries,
    /// Number of nested array/map levels.
    NestingDepth,
    /// UTF-8 bytes in one string or map key.
    StringBytes,
    /// Bytes in one binary value.
    BinaryBytes,
    /// Total metadata values decoded across the packet.
    TotalValues,
}

impl Display for RangeIndexResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Entries => "entries",
            Self::FieldsPerRange => "fields per range",
            Self::CollectionEntries => "collection entries",
            Self::NestingDepth => "nesting depth",
            Self::StringBytes => "string bytes",
            Self::BinaryBytes => "binary bytes",
            Self::TotalValues => "total values",
        })
    }
}

/// Failure to decode or validate a v0.5 range-index packet.
#[derive(Debug)]
#[non_exhaustive]
pub enum RangeIndexError {
    /// An encoded value ended before its fixed or declared size.
    Truncated {
        /// Field or value being decoded.
        context: &'static str,
        /// Byte offset at which the value data began.
        offset: usize,
        /// Bytes required at the offset.
        needed: usize,
        /// Bytes available at the offset.
        remaining: usize,
    },
    /// A collection or byte sequence cannot fit in the remaining payload.
    LengthOutOfBounds {
        /// Collection or byte sequence being decoded.
        context: &'static str,
        /// Declared element count or byte length.
        declared: u64,
        /// Bytes remaining after its declaration.
        remaining: usize,
    },
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow {
        /// Field whose size overflowed.
        context: &'static str,
    },
    /// A configured resource limit was exceeded.
    LimitExceeded {
        /// Limited resource.
        resource: RangeIndexResource,
        /// Value declared or observed in the packet.
        actual: u64,
        /// Configured maximum value.
        limit: u64,
    },
    /// A `MessagePack` marker had the wrong semantic type.
    UnexpectedType {
        /// Value being decoded.
        context: &'static str,
        /// Required semantic type.
        expected: &'static str,
        /// `MessagePack` marker byte.
        marker: u8,
        /// Marker byte offset.
        offset: usize,
    },
    /// A marker unsupported by the JSON-compatible range metadata model appeared.
    UnsupportedMarker {
        /// Unsupported `MessagePack` marker byte.
        marker: u8,
        /// Marker byte offset.
        offset: usize,
    },
    /// A floating-point metadata value was not representable in JSON.
    NonFiniteFloat {
        /// Non-finite value decoded from the packet.
        value: f64,
        /// Marker byte offset.
        offset: usize,
    },
    /// A `MessagePack` string or map key was not UTF-8.
    InvalidUtf8 {
        /// String being decoded.
        context: &'static str,
        /// String data byte offset.
        offset: usize,
        /// UTF-8 validation failure.
        source: Utf8Error,
    },
    /// A semantic `MessagePack` map repeated a key.
    DuplicateKey {
        /// Map whose key was repeated.
        context: &'static str,
        /// Repeated key.
        key: String,
    },
    /// A range entry omitted one of `s`, `e`, or `f`.
    MissingEntryField {
        /// Zero-based range entry index.
        entry_index: u64,
        /// Missing wire field.
        field: &'static str,
    },
    /// A range bound was a negative signed integer.
    NegativeIndex {
        /// Zero-based range entry index.
        entry_index: u64,
        /// `s` or `e`.
        field: &'static str,
        /// Negative encoded value.
        value: i64,
    },
    /// A half-open range's start exceeded its end.
    ReversedRange {
        /// Zero-based range entry index.
        entry_index: u64,
        /// Inclusive start index.
        start: u64,
        /// Exclusive end index.
        end: u64,
    },
    /// A range began before the preceding range ended.
    NonMonotonicRange {
        /// Zero-based range entry index.
        entry_index: u64,
        /// Preceding range's exclusive end.
        previous_end: u64,
        /// Current range's inclusive start.
        start: u64,
    },
    /// Bytes followed the one range-index `MessagePack` value.
    TrailingData {
        /// Unconsumed payload bytes.
        remaining: usize,
    },
    /// A range's exclusive end exceeded the known archive record domain.
    RangeOutOfDomain {
        /// Zero-based range entry index.
        entry_index: u64,
        /// Range's exclusive end.
        end: u64,
        /// Number of archive-local log events.
        record_count: u64,
    },
}

impl Display for RangeIndexError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                context,
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "range index {context} at offset {offset} needs {needed} bytes; {remaining} remain"
            ),
            Self::LengthOutOfBounds {
                context,
                declared,
                remaining,
            } => write!(
                formatter,
                "range index {context} declares {declared}; only {remaining} bytes remain"
            ),
            Self::SizeOverflow { context } => {
                write!(formatter, "range index {context} size overflow")
            }
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "range index {resource} value {actual} exceeds limit {limit}"
            ),
            Self::UnexpectedType {
                context,
                expected,
                marker,
                offset,
            } => write!(
                formatter,
                "range index {context} at offset {offset} has MessagePack marker {marker:#04x}; \
                 expected {expected}"
            ),
            Self::UnsupportedMarker { marker, offset } => write!(
                formatter,
                "range index value at offset {offset} uses unsupported MessagePack marker \
                 {marker:#04x}"
            ),
            Self::NonFiniteFloat { value, offset } => fmt_non_finite(*value, *offset, formatter),
            Self::InvalidUtf8 {
                context,
                offset,
                source,
            } => write!(
                formatter,
                "range index {context} at offset {offset} is not UTF-8: {source}"
            ),
            Self::DuplicateKey { context, key } => {
                write!(formatter, "range index {context} repeats key {key:?}")
            }
            Self::MissingEntryField { entry_index, field } => {
                write!(
                    formatter,
                    "range index entry {entry_index} is missing {field:?}"
                )
            }
            Self::NegativeIndex {
                entry_index,
                field,
                value,
            } => write!(
                formatter,
                "range index entry {entry_index} field {field:?} is negative ({value})"
            ),
            Self::ReversedRange {
                entry_index,
                start,
                end,
            } => write!(
                formatter,
                "range index entry {entry_index} has reversed range {start}..{end}"
            ),
            Self::NonMonotonicRange {
                entry_index,
                previous_end,
                start,
            } => write!(
                formatter,
                "range index entry {entry_index} starts at {start} before the preceding end \
                 {previous_end}"
            ),
            Self::TrailingData { remaining } => {
                write!(formatter, "range index has {remaining} trailing bytes")
            }
            Self::RangeOutOfDomain {
                entry_index,
                end,
                record_count,
            } => write!(
                formatter,
                "range index entry {entry_index} ends at {end} outside record domain \
                 0..{record_count}"
            ),
        }
    }
}

fn fmt_non_finite(value: f64, offset: usize, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(
        formatter,
        "range index value at offset {offset} is non-finite ({value})"
    )
}

impl Error for RangeIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8 { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    struct WireEntry<T> {
        #[serde(rename = "s")]
        start: T,
        #[serde(rename = "e")]
        end: T,
        #[serde(rename = "f")]
        fields: BTreeMap<String, String>,
    }

    fn encode<T: Serialize>(entries: &[WireEntry<T>]) -> Vec<u8> {
        rmp_serde::to_vec_named(entries).expect("encode range-index test packet")
    }

    fn entry<T>(start: T, end: T) -> WireEntry<T> {
        WireEntry {
            start,
            end,
            fields: BTreeMap::from([("name".to_owned(), "input.jsonl".to_owned())]),
        }
    }

    #[test]
    fn decodes_semantic_map_order_and_valid_ranges() {
        let bytes = encode(&[entry(0_u64, 2), entry(2, 5)]);
        let range_index = RangeIndex::decode(bytes.clone(), RangeIndexLimits::default())
            .expect("valid range index");

        assert_eq!(bytes, range_index.encoded_bytes());
        assert_eq!(2, range_index.entries().len());
        assert_eq!(0..2, range_index.entries()[0].range());
        assert_eq!(2..5, range_index.entries()[1].range());
        assert_eq!(
            Some("input.jsonl"),
            range_index.entries()[0]
                .field("name")
                .and_then(RangeIndexValue::as_str)
        );
        range_index
            .validate_record_domain(5)
            .expect("ranges fit known record domain");
    }

    #[test]
    fn decodes_all_supported_metadata_value_types() {
        let bytes = [
            0x91, // one range
            0x83, 0xa1, b's', 0x00, 0xa1, b'e', 0x01, 0xa1, b'f',
            0x89, // nine metadata fields
            0xa1, b'n', 0xc0, // null
            0xa1, b'b', 0xc3, // Boolean
            0xa1, b'i', 0xd0, 0xff, // signed -1
            0xa1, b'u', 0xcc, 0x80, // unsigned 128
            0xa1, b'x', 0xcb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 1.5
            0xa1, b's', 0xa1, b'v', // string
            0xa1, b'z', 0xc4, 0x02, 0x00, 0xff, // binary
            0xa1, b'a', 0x92, 0xc2, 0x01, // array
            0xa1, b'o', 0x81, 0xa1, b'k', 0xa1, b'v', // object
        ];
        let range_index = RangeIndex::decode(bytes.to_vec(), RangeIndexLimits::default())
            .expect("all JSON/MessagePack value types decode");
        let entry = &range_index.entries()[0];

        assert_eq!(Some(&RangeIndexValue::Null), entry.field("n"));
        assert_eq!(Some(&RangeIndexValue::Boolean(true)), entry.field("b"));
        assert_eq!(Some(&RangeIndexValue::Signed(-1)), entry.field("i"));
        assert_eq!(Some(&RangeIndexValue::Unsigned(128)), entry.field("u"));
        assert_eq!(Some(&RangeIndexValue::Float(1.5)), entry.field("x"));
        assert_eq!(
            Some(&RangeIndexValue::String("v".to_owned())),
            entry.field("s")
        );
        assert_eq!(
            Some(&RangeIndexValue::Binary(vec![0x00, 0xff])),
            entry.field("z")
        );
        assert_eq!(
            Some(&RangeIndexValue::Array(vec![
                RangeIndexValue::Boolean(false),
                RangeIndexValue::Unsigned(1),
            ])),
            entry.field("a")
        );
        assert_eq!(
            Some(&RangeIndexValue::Object(BTreeMap::from([(
                "k".to_owned(),
                RangeIndexValue::String("v".to_owned()),
            )]))),
            entry.field("o")
        );
    }

    #[test]
    fn accepts_adjacent_and_repeated_empty_ranges() {
        let bytes = encode(&[entry(1_u64, 1), entry(1, 1), entry(1, 3)]);
        RangeIndex::decode(bytes, RangeIndexLimits::default())
            .expect("empty and adjacent ranges are legal");
    }

    #[test]
    fn rejects_non_finite_metadata_floats() {
        for encoded_float in [
            [0xcb, 0x7f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            [0xcb, 0x7f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        ] {
            let mut bytes = vec![
                0x91, 0x83, 0xa1, b's', 0x00, 0xa1, b'e', 0x01, 0xa1, b'f', 0x81, 0xa1, b'x',
            ];
            bytes.extend_from_slice(&encoded_float);
            assert!(matches!(
                RangeIndex::decode(bytes, RangeIndexLimits::default()),
                Err(RangeIndexError::NonFiniteFloat { .. })
            ));
        }
    }

    #[test]
    fn rejects_negative_and_reversed_bounds() {
        let negative = encode(&[entry(-1_i64, 0)]);
        assert!(matches!(
            RangeIndex::decode(negative, RangeIndexLimits::default()),
            Err(RangeIndexError::NegativeIndex {
                entry_index: 0,
                field: "s",
                value: -1
            })
        ));

        let reversed = encode(&[entry(3_u64, 2)]);
        assert!(matches!(
            RangeIndex::decode(reversed, RangeIndexLimits::default()),
            Err(RangeIndexError::ReversedRange {
                entry_index: 0,
                start: 3,
                end: 2
            })
        ));
    }

    #[test]
    fn rejects_overlapping_or_non_monotonic_ranges() {
        let bytes = encode(&[entry(0_u64, 3), entry(2, 4)]);
        assert!(matches!(
            RangeIndex::decode(bytes, RangeIndexLimits::default()),
            Err(RangeIndexError::NonMonotonicRange {
                entry_index: 1,
                previous_end: 3,
                start: 2
            })
        ));
    }

    #[test]
    fn defers_record_domain_validation() {
        let range_index =
            RangeIndex::decode(encode(&[entry(0_u64, 4)]), RangeIndexLimits::default())
                .expect("structurally valid without record count");

        assert!(matches!(
            range_index.validate_record_domain(3),
            Err(RangeIndexError::RangeOutOfDomain {
                entry_index: 0,
                end: 4,
                record_count: 3
            })
        ));
    }

    #[test]
    fn applies_entry_limit_before_allocating() {
        let bytes = [0xdd, 0xff, 0xff, 0xff, 0xff];
        let limits = RangeIndexLimits::new(
            1,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            u64::MAX,
        );
        assert!(matches!(
            RangeIndex::decode(bytes.to_vec(), limits),
            Err(RangeIndexError::LimitExceeded {
                resource: RangeIndexResource::Entries,
                actual: 4_294_967_295,
                limit: 1
            })
        ));
    }

    #[test]
    fn validates_nested_metadata_and_depth() {
        let bytes = [
            0x91, 0x83, 0xa1, b's', 0x00, 0xa1, b'e', 0x01, 0xa1, b'f', 0x81, 0xa1, b'x', 0x91,
            0x91, 0xc0,
        ];
        let limits = RangeIndexLimits::new(
            u32::MAX,
            u32::MAX,
            u32::MAX,
            1,
            u32::MAX,
            u32::MAX,
            u64::MAX,
        );
        assert!(matches!(
            RangeIndex::decode(bytes.to_vec(), limits),
            Err(RangeIndexError::LimitExceeded {
                resource: RangeIndexResource::NestingDepth,
                actual: 2,
                limit: 1
            })
        ));
    }

    #[test]
    fn rejects_duplicate_semantic_keys_and_trailing_data() {
        let duplicate_entry_key = [
            0x91, 0x84, 0xa1, b's', 0x00, 0xa1, b's', 0x00, 0xa1, b'e', 0x00, 0xa1, b'f', 0x80,
        ];
        assert!(matches!(
            RangeIndex::decode(
                duplicate_entry_key.to_vec(),
                RangeIndexLimits::default()
            ),
            Err(RangeIndexError::DuplicateKey { key, .. }) if key == "s"
        ));

        let mut trailing = encode(&[entry(0_u64, 0)]);
        trailing.push(0xc0);
        assert!(matches!(
            RangeIndex::decode(trailing, RangeIndexLimits::default()),
            Err(RangeIndexError::TrailingData { remaining: 1 })
        ));
    }

    #[test]
    fn rejects_non_map_fields_and_extension_values() {
        let non_map_fields = [
            0x91, 0x83, 0xa1, b's', 0x00, 0xa1, b'e', 0x00, 0xa1, b'f', 0x90,
        ];
        assert!(matches!(
            RangeIndex::decode(non_map_fields.to_vec(), RangeIndexLimits::default()),
            Err(RangeIndexError::UnexpectedType {
                expected: "map",
                ..
            })
        ));

        let extension_value = [
            0x91, 0x83, 0xa1, b's', 0x00, 0xa1, b'e', 0x00, 0xa1, b'f', 0x81, 0xa1, b'x', 0xd4,
            0x00, 0x00,
        ];
        assert!(matches!(
            RangeIndex::decode(extension_value.to_vec(), RangeIndexLimits::default()),
            Err(RangeIndexError::UnsupportedMarker { marker: 0xd4, .. })
        ));
    }
}
