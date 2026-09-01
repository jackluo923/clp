use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::iter::FusedIterator;

use super::dictionary::ArrayDictionary;
use super::dictionary::LogTypeDictionary;
use super::dictionary::LogTypeDictionaryEntry;
use super::dictionary::LogTypeVariableKind;
use super::dictionary::VariableDictionary;
use super::dictionary::VariableDictionaryEntry;
use super::schema::NodeType;
use super::schema_map::SchemaDefinition;
use super::schema_map::SchemaEntry;
use super::schema_tree::SchemaTree;
use super::timestamp_dictionary::TimestampDictionary;
use super::timestamp_dictionary::TimestampPatternEntry;

const U16_SIZE: usize = 2;
const U64_SIZE: usize = 8;
const LOGTYPE_ID_MASK: u64 = (1_u64 << 24) - 1;
const LOGTYPE_OFFSET_SHIFT: u32 = 24;
const MAX_ENCODED_VARIABLE_DOMAIN: u64 = 1_u64 << 40;
const ENCODED_FLOAT_UNUSED_BIT: u64 = 1_u64 << 62;
const ENCODED_FLOAT_DIGITS_MASK: u64 = (1_u64 << 54) - 1;
const MAX_ENCODED_FLOAT_DIGITS: u64 = 9_999_999_999_999_999;

/// Resource limits applied while decoding one decompressed schema table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnLimits {
    table_bytes: u64,
    messages: u64,
    columns: u64,
    encoded_variables_per_column: u64,
    total_encoded_variables: u64,
}

impl ColumnLimits {
    /// Creates explicit schema-table resource limits.
    #[must_use]
    pub const fn new(
        max_table_bytes: u64,
        max_messages: u64,
        max_columns: u64,
        max_encoded_variables_per_column: u64,
        max_total_encoded_variables: u64,
    ) -> Self {
        Self {
            table_bytes: max_table_bytes,
            messages: max_messages,
            columns: max_columns,
            encoded_variables_per_column: max_encoded_variables_per_column,
            total_encoded_variables: max_total_encoded_variables,
        }
    }

    /// Maximum bytes accepted in the complete decompressed table.
    #[must_use]
    pub const fn max_table_bytes(self) -> u64 {
        self.table_bytes
    }

    /// Maximum message count accepted from table metadata.
    #[must_use]
    pub const fn max_messages(self) -> u64 {
        self.messages
    }

    /// Maximum number of value-bearing columns accepted.
    #[must_use]
    pub const fn max_columns(self) -> u64 {
        self.columns
    }

    /// Maximum encoded-variable count accepted in one CLP string column.
    #[must_use]
    pub const fn max_encoded_variables_per_column(self) -> u64 {
        self.encoded_variables_per_column
    }

    /// Maximum cumulative encoded-variable count accepted across the table.
    #[must_use]
    pub const fn max_total_encoded_variables(self) -> u64 {
        self.total_encoded_variables
    }
}

impl Default for ColumnLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(
            1024 * MEBIBYTE,
            128 * 1024 * 1024,
            1024 * 1024,
            128 * 1024 * 1024,
            128 * 1024 * 1024,
        )
    }
}

/// A validated zero-copy view of one schema table.
#[derive(Clone, Debug)]
pub struct SchemaTable<'table, 'archive> {
    message_count: usize,
    columns: Vec<Column<'table, 'archive>>,
}

impl<'table, 'archive> SchemaTable<'table, 'archive> {
    /// Returns the number of records in every column.
    #[must_use]
    pub const fn message_count(&self) -> usize {
        self.message_count
    }

    /// Returns value-bearing columns in flattened schema-entry order.
    #[must_use]
    pub fn columns(&self) -> &[Column<'table, 'archive>] {
        &self.columns
    }

    /// Returns a value-bearing column by its table-local index.
    #[must_use]
    pub fn column(&self, column_index: usize) -> Option<&Column<'table, 'archive>> {
        self.columns.get(column_index)
    }

    /// Returns the number of value-bearing columns.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether the schema has no value-bearing columns.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// One value-bearing schema entry and its zero-copy column data.
#[derive(Clone, Copy, Debug)]
pub struct Column<'table, 'archive> {
    schema_entry_index: usize,
    node_id: u32,
    data: ColumnData<'table, 'archive>,
}

impl<'table, 'archive> Column<'table, 'archive> {
    /// Returns this column's index in the flattened schema definition.
    #[must_use]
    pub const fn schema_entry_index(self) -> usize {
        self.schema_entry_index
    }

    /// Returns the referenced schema-tree node ID.
    #[must_use]
    pub const fn node_id(self) -> u32 {
        self.node_id
    }

    /// Returns the typed zero-copy column data.
    #[must_use]
    pub const fn data(self) -> ColumnData<'table, 'archive> {
        self.data
    }

    /// Returns the stable schema-tree node type.
    #[must_use]
    pub const fn node_type(self) -> NodeType {
        self.data.node_type()
    }
}

/// Typed zero-copy data for one value-bearing CLP-S column.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ColumnData<'table, 'archive> {
    /// Raw signed 64-bit integers.
    Integer(I64Column<'table>),
    /// Delta-encoded signed 64-bit integers.
    DeltaInteger(DeltaI64Column<'table>),
    /// Finite binary64 values.
    Float(F64Column<'table>),
    /// Finite binary64 values paired with validated lexeme-format descriptors.
    FormattedFloat(FormattedFloatColumn<'table>),
    /// Numeric lexemes addressed through `/var.dict`.
    DictionaryFloat(DictionaryIdColumn<'table, 'archive>),
    /// Canonical Boolean bytes.
    Boolean(BooleanColumn<'table>),
    /// String values addressed through `/var.dict`.
    VarString(DictionaryIdColumn<'table, 'archive>),
    /// CLP logtype descriptors and encoded variables using `/log.dict`.
    ClpString(ClpStringColumn<'table, 'archive>),
    /// CLP logtype descriptors and encoded variables using `/array.dict`.
    UnstructuredArray(ClpStringColumn<'table, 'archive>),
    /// Legacy timestamp values and bit-cast pattern IDs.
    DeprecatedDateString(DeprecatedDateStringColumn<'table, 'archive>),
    /// Delta epoch-nanoseconds and current timestamp-pattern IDs.
    Timestamp(TimestampColumn<'table, 'archive>),
}

impl ColumnData<'_, '_> {
    /// Returns the stable schema-tree node type represented by this variant.
    #[must_use]
    pub const fn node_type(self) -> NodeType {
        match self {
            Self::Integer(_) => NodeType::Integer,
            Self::DeltaInteger(_) => NodeType::DeltaInteger,
            Self::Float(_) => NodeType::Float,
            Self::FormattedFloat(_) => NodeType::FormattedFloat,
            Self::DictionaryFloat(_) => NodeType::DictionaryFloat,
            Self::Boolean(_) => NodeType::Boolean,
            Self::VarString(_) => NodeType::VarString,
            Self::ClpString(_) => NodeType::ClpString,
            Self::UnstructuredArray(_) => NodeType::UnstructuredArray,
            Self::DeprecatedDateString(_) => NodeType::DeprecatedDateString,
            Self::Timestamp(_) => NodeType::Timestamp,
        }
    }
}

/// A zero-copy little-endian signed 64-bit column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I64Column<'a> {
    bytes: &'a [u8],
}

impl<'a> I64Column<'a> {
    /// Returns the number of values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.bytes.len() / U64_SIZE
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns one decoded little-endian value.
    #[must_use]
    pub fn get(self, index: usize) -> Option<i64> {
        read_i64_at(self.bytes, index)
    }

    /// Iterates decoded values without materializing an aligned copy.
    pub fn iter(
        self,
    ) -> impl ExactSizeIterator<Item = i64> + DoubleEndedIterator + FusedIterator + 'a {
        self.bytes
            .as_chunks::<U64_SIZE>()
            .0
            .iter()
            .copied()
            .map(i64::from_le_bytes)
    }

    /// Returns the exact little-endian bytes backing this view.
    #[must_use]
    pub const fn encoded_bytes(self) -> &'a [u8] {
        self.bytes
    }

    fn slice(self, start: usize, count: usize) -> Option<Self> {
        let byte_start = start.checked_mul(U64_SIZE)?;
        let byte_len = count.checked_mul(U64_SIZE)?;
        let byte_end = byte_start.checked_add(byte_len)?;
        Some(Self {
            bytes: self.bytes.get(byte_start..byte_end)?,
        })
    }
}

/// A zero-copy little-endian unsigned 64-bit column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct U64Column<'a> {
    bytes: &'a [u8],
}

impl<'a> U64Column<'a> {
    /// Returns the number of values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.bytes.len() / U64_SIZE
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns one decoded little-endian value.
    #[must_use]
    pub fn get(self, index: usize) -> Option<u64> {
        read_u64_at(self.bytes, index)
    }

    /// Iterates decoded values without materializing an aligned copy.
    pub fn iter(
        self,
    ) -> impl ExactSizeIterator<Item = u64> + DoubleEndedIterator + FusedIterator + 'a {
        self.bytes
            .as_chunks::<U64_SIZE>()
            .0
            .iter()
            .copied()
            .map(u64::from_le_bytes)
    }

    /// Returns the exact little-endian bytes backing this view.
    #[must_use]
    pub const fn encoded_bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// A zero-copy little-endian finite binary64 column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct F64Column<'a> {
    bytes: &'a [u8],
}

impl<'a> F64Column<'a> {
    /// Returns the number of values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.bytes.len() / U64_SIZE
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns one decoded finite binary64 value.
    #[must_use]
    pub fn get(self, index: usize) -> Option<f64> {
        self.bytes
            .get(index.checked_mul(U64_SIZE)?..index.checked_add(1)?.checked_mul(U64_SIZE)?)
            .map(decode_f64_chunk)
    }

    /// Iterates decoded values without materializing an aligned copy.
    pub fn iter(
        self,
    ) -> impl ExactSizeIterator<Item = f64> + DoubleEndedIterator + FusedIterator + 'a {
        self.bytes
            .as_chunks::<U64_SIZE>()
            .0
            .iter()
            .copied()
            .map(f64::from_le_bytes)
    }

    /// Returns the exact little-endian bytes backing this view.
    #[must_use]
    pub const fn encoded_bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// A validated delta-encoded signed 64-bit column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeltaI64Column<'a> {
    deltas: I64Column<'a>,
}

/// A forward iterator that reconstructs a delta column in one linear pass.
#[derive(Clone, Debug)]
pub struct DeltaI64Values<'a> {
    deltas: std::slice::Iter<'a, [u8; U64_SIZE]>,
    current: i64,
}

impl Iterator for DeltaI64Values<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        let delta = i64::from_le_bytes(*self.deltas.next()?);
        self.current = self.current.wrapping_add(delta);
        Some(self.current)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.deltas.size_hint()
    }
}

impl ExactSizeIterator for DeltaI64Values<'_> {}
impl FusedIterator for DeltaI64Values<'_> {}

impl<'a> DeltaI64Column<'a> {
    /// Returns the number of decoded values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.deltas.len()
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.deltas.is_empty()
    }

    /// Returns the stored deltas.
    #[must_use]
    pub const fn deltas(self) -> I64Column<'a> {
        self.deltas
    }

    /// Iterates reconstructed values in linear time.
    ///
    /// Overflow was rejected when this view was constructed.
    #[must_use]
    pub fn values(self) -> DeltaI64Values<'a> {
        DeltaI64Values {
            deltas: self.deltas.bytes.as_chunks::<U64_SIZE>().0.iter(),
            current: 0,
        }
    }

    /// Reconstructs one value. Repeated random access is linear in the requested index.
    #[must_use]
    pub fn get(self, index: usize) -> Option<i64> {
        self.values().nth(index)
    }
}

/// Scientific-notation marker in a formatted-float descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FloatNotation {
    /// Ordinary decimal notation.
    Decimal,
    /// Scientific notation with lowercase `e`.
    LowercaseScientific,
    /// Scientific notation with uppercase `E`.
    UppercaseScientific,
}

/// Exponent sign spelling in a formatted-float descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FloatExponentSign {
    /// No explicit exponent sign.
    None,
    /// An explicit plus sign.
    Plus,
    /// An explicit minus sign.
    Minus,
}

/// A structurally validated 16-bit formatted-float descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FloatFormat {
    raw: u16,
    notation: FloatNotation,
    exponent_sign: FloatExponentSign,
    exponent_digits: Option<u8>,
    significant_digits: u8,
}

impl FloatFormat {
    /// Returns the exact wire descriptor.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.raw
    }

    /// Returns the notation marker.
    #[must_use]
    pub const fn notation(self) -> FloatNotation {
        self.notation
    }

    /// Returns the exponent sign spelling.
    #[must_use]
    pub const fn exponent_sign(self) -> FloatExponentSign {
        self.exponent_sign
    }

    /// Returns the scientific exponent digit count, or `None` for decimal notation.
    #[must_use]
    pub const fn exponent_digits(self) -> Option<u8> {
        self.exponent_digits
    }

    /// Returns the significant digit count in the range 1 through 17.
    #[must_use]
    pub const fn significant_digits(self) -> u8 {
        self.significant_digits
    }
}

impl TryFrom<u16> for FloatFormat {
    type Error = FloatFormatErrorReason;

    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        if 0 != raw & 0x001f {
            return Err(FloatFormatErrorReason::ReservedBits);
        }
        let notation = match (raw >> 14) & 0b11 {
            0 => FloatNotation::Decimal,
            1 => FloatNotation::LowercaseScientific,
            3 => FloatNotation::UppercaseScientific,
            _ => return Err(FloatFormatErrorReason::UnknownNotation),
        };
        let exponent_sign = match (raw >> 12) & 0b11 {
            0 => FloatExponentSign::None,
            1 => FloatExponentSign::Plus,
            2 => FloatExponentSign::Minus,
            _ => return Err(FloatFormatErrorReason::UnknownExponentSign),
        };
        let encoded_exponent_digits = ((raw >> 10) & 0b11) as u8;
        let significant_digits = (((raw >> 5) & 0b1_1111) as u8) + 1;
        if significant_digits > 17 {
            return Err(FloatFormatErrorReason::SignificantDigitsOutOfRange);
        }
        if FloatNotation::Decimal == notation
            && (FloatExponentSign::None != exponent_sign || 0 != encoded_exponent_digits)
        {
            return Err(FloatFormatErrorReason::DecimalHasExponentMetadata);
        }
        Ok(Self {
            raw,
            notation,
            exponent_sign,
            exponent_digits: (FloatNotation::Decimal != notation)
                .then_some(encoded_exponent_digits + 1),
            significant_digits,
        })
    }
}

/// Reason a formatted-float descriptor is structurally invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FloatFormatErrorReason {
    /// Low reserved bits are nonzero.
    ReservedBits,
    /// Notation flag `10` is not emitted or understood.
    UnknownNotation,
    /// Exponent-sign flag `11` is not emitted or understood.
    UnknownExponentSign,
    /// Decimal notation carries scientific-only sign or digit metadata.
    DecimalHasExponentMetadata,
    /// The encoded significant digit count exceeds 17.
    SignificantDigitsOutOfRange,
}

impl Display for FloatFormatErrorReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReservedBits => "reserved formatted-float bits are nonzero",
            Self::UnknownNotation => "formatted-float notation flag is reserved",
            Self::UnknownExponentSign => "formatted-float exponent-sign flag is reserved",
            Self::DecimalHasExponentMetadata => {
                "decimal formatted-float descriptor has exponent metadata"
            }
            Self::SignificantDigitsOutOfRange => {
                "formatted-float significant digit count exceeds 17"
            }
        })
    }
}

impl Error for FloatFormatErrorReason {}

/// One finite formatted-float value and its original-lexeme descriptor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FormattedFloatValue {
    value: f64,
    format: FloatFormat,
}

impl FormattedFloatValue {
    /// Returns the finite binary64 value.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.value
    }

    /// Returns the validated formatting descriptor.
    #[must_use]
    pub const fn format(self) -> FloatFormat {
        self.format
    }
}

/// A zero-copy formatted-float column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FormattedFloatColumn<'a> {
    values: F64Column<'a>,
    formats: &'a [u8],
}

impl<'a> FormattedFloatColumn<'a> {
    /// Returns the number of values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.values.len()
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.values.is_empty()
    }

    /// Returns all finite binary64 values.
    #[must_use]
    pub const fn values(self) -> F64Column<'a> {
        self.values
    }

    /// Returns one decoded value and format.
    #[must_use]
    pub fn get(self, index: usize) -> Option<FormattedFloatValue> {
        let value = self.values.get(index)?;
        let format = FloatFormat::try_from(read_u16_at(self.formats, index)?).ok()?;
        Some(FormattedFloatValue { value, format })
    }

    /// Returns the exact little-endian descriptor bytes.
    #[must_use]
    pub const fn format_bytes(self) -> &'a [u8] {
        self.formats
    }
}

/// A zero-copy canonical Boolean column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BooleanColumn<'a> {
    bytes: &'a [u8],
}

impl<'a> BooleanColumn<'a> {
    /// Returns the number of values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.bytes.len()
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns one Boolean value.
    #[must_use]
    pub fn get(self, index: usize) -> Option<bool> {
        self.bytes.get(index).map(|value| 0 != *value)
    }

    /// Iterates Boolean values.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter(
        self,
    ) -> impl ExactSizeIterator<Item = bool> + DoubleEndedIterator + FusedIterator + 'a {
        self.bytes.iter().map(|value| 0 != *value)
    }

    /// Returns the canonical `0` or `1` bytes.
    #[must_use]
    pub const fn encoded_bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// A zero-copy variable-dictionary ID column.
#[derive(Clone, Copy, Debug)]
pub struct DictionaryIdColumn<'table, 'archive> {
    ids: U64Column<'table>,
    dictionary: &'archive VariableDictionary,
}

impl<'table, 'archive> DictionaryIdColumn<'table, 'archive> {
    /// Returns the number of IDs.
    #[must_use]
    pub const fn len(self) -> usize {
        self.ids.len()
    }

    /// Returns whether the column contains no IDs.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.ids.is_empty()
    }

    /// Returns the zero-copy ID view.
    #[must_use]
    pub const fn ids(self) -> U64Column<'table> {
        self.ids
    }

    /// Returns one dictionary ID.
    #[must_use]
    pub fn id(self, index: usize) -> Option<u64> {
        self.ids.get(index)
    }

    /// Returns one exact variable-dictionary byte string.
    #[must_use]
    pub fn value(self, index: usize) -> Option<&'archive [u8]> {
        self.dictionary
            .entry(self.id(index)?)
            .map(VariableDictionaryEntry::value)
    }

    /// Returns the validated variable dictionary.
    #[must_use]
    pub const fn dictionary(self) -> &'archive VariableDictionary {
        self.dictionary
    }
}

/// A decoded 24/40-bit CLP string descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClpDescriptor {
    raw: u64,
}

impl ClpDescriptor {
    /// Returns the exact 64-bit wire descriptor.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.raw
    }

    /// Returns the low 24-bit logtype dictionary ID.
    #[must_use]
    pub const fn logtype_id(self) -> u32 {
        (self.raw & LOGTYPE_ID_MASK) as u32
    }

    /// Returns the high 40-bit starting encoded-variable index.
    #[must_use]
    pub const fn encoded_variable_offset(self) -> u64 {
        self.raw >> LOGTYPE_OFFSET_SHIFT
    }
}

#[derive(Clone, Copy, Debug)]
enum LogTypeDictionaryRef<'a> {
    Log(&'a LogTypeDictionary),
    Array(&'a ArrayDictionary),
}

impl<'a> LogTypeDictionaryRef<'a> {
    fn entry(self, id: u64) -> Option<LogTypeDictionaryEntry<'a>> {
        match self {
            Self::Log(dictionary) => dictionary.entry(id),
            Self::Array(dictionary) => dictionary.entry(id),
        }
    }

    const fn section(self) -> LogTypeSection {
        match self {
            Self::Log(_) => LogTypeSection::Log,
            Self::Array(_) => LogTypeSection::Array,
        }
    }
}

/// One zero-copy CLP record span.
#[derive(Clone, Copy, Debug)]
pub struct ClpStringRecord<'table, 'archive> {
    descriptor: ClpDescriptor,
    logtype: LogTypeDictionaryEntry<'archive>,
    encoded_variables: I64Column<'table>,
}

impl<'table, 'archive> ClpStringRecord<'table, 'archive> {
    /// Returns the decoded 24/40-bit descriptor.
    #[must_use]
    pub const fn descriptor(self) -> ClpDescriptor {
        self.descriptor
    }

    /// Returns the referenced escaped logtype.
    #[must_use]
    pub const fn logtype(self) -> LogTypeDictionaryEntry<'archive> {
        self.logtype
    }

    /// Returns exactly the encoded variables consumed by this record's unescaped placeholders.
    #[must_use]
    pub const fn encoded_variables(self) -> I64Column<'table> {
        self.encoded_variables
    }
}

/// A zero-copy CLP string or unstructured-array column.
#[derive(Clone, Copy, Debug)]
pub struct ClpStringColumn<'table, 'archive> {
    descriptors: U64Column<'table>,
    encoded_variables: I64Column<'table>,
    variable_dictionary: &'archive VariableDictionary,
    logtype_dictionary: LogTypeDictionaryRef<'archive>,
}

impl<'table, 'archive> ClpStringColumn<'table, 'archive> {
    /// Returns the number of records.
    #[must_use]
    pub const fn len(self) -> usize {
        self.descriptors.len()
    }

    /// Returns whether the column contains no records.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.descriptors.is_empty()
    }

    /// Returns one decoded descriptor.
    #[must_use]
    pub fn descriptor(self, index: usize) -> Option<ClpDescriptor> {
        Some(ClpDescriptor {
            raw: self.descriptors.get(index)?,
        })
    }

    /// Returns the full encoded-variable array.
    #[must_use]
    pub const fn encoded_variables(self) -> I64Column<'table> {
        self.encoded_variables
    }

    /// Returns the variable dictionary used by dictionary placeholders.
    #[must_use]
    pub const fn variable_dictionary(self) -> &'archive VariableDictionary {
        self.variable_dictionary
    }

    /// Returns one validated logtype and encoded-variable span.
    #[must_use]
    pub fn record(self, index: usize) -> Option<ClpStringRecord<'table, 'archive>> {
        let descriptor = self.descriptor(index)?;
        let logtype = self
            .logtype_dictionary
            .entry(u64::from(descriptor.logtype_id()))?;
        let start = usize::try_from(descriptor.encoded_variable_offset()).ok()?;
        let count = usize::try_from(logtype.placeholder_counts().encoded_variables()).ok()?;
        Some(ClpStringRecord {
            descriptor,
            logtype,
            encoded_variables: self.encoded_variables.slice(start, count)?,
        })
    }
}

/// One legacy timestamp value and its bit-cast pattern ID.
#[derive(Clone, Copy, Debug)]
pub struct DeprecatedDateStringValue<'archive> {
    epoch: i64,
    pattern_id: u64,
    pattern: &'archive TimestampPatternEntry,
}

impl<'archive> DeprecatedDateStringValue<'archive> {
    /// Returns the raw legacy epoch value.
    #[must_use]
    pub const fn epoch(self) -> i64 {
        self.epoch
    }

    /// Returns the pattern ID obtained by bit-casting the stored signed value.
    #[must_use]
    pub const fn pattern_id(self) -> u64 {
        self.pattern_id
    }

    /// Returns the referenced raw timestamp pattern.
    #[must_use]
    pub const fn pattern(self) -> &'archive TimestampPatternEntry {
        self.pattern
    }
}

/// A zero-copy legacy date-string column.
#[derive(Clone, Copy, Debug)]
pub struct DeprecatedDateStringColumn<'table, 'archive> {
    epochs: I64Column<'table>,
    pattern_ids: I64Column<'table>,
    dictionary: &'archive TimestampDictionary,
}

impl<'table, 'archive> DeprecatedDateStringColumn<'table, 'archive> {
    /// Returns the number of values.
    #[must_use]
    pub const fn len(self) -> usize {
        self.epochs.len()
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.epochs.is_empty()
    }

    /// Returns the raw legacy epoch view.
    #[must_use]
    pub const fn epochs(self) -> I64Column<'table> {
        self.epochs
    }

    /// Returns the raw signed pattern-ID view.
    #[must_use]
    pub const fn encoded_pattern_ids(self) -> I64Column<'table> {
        self.pattern_ids
    }

    /// Returns one epoch and timestamp pattern.
    #[must_use]
    pub fn get(self, index: usize) -> Option<DeprecatedDateStringValue<'archive>> {
        let epoch = self.epochs.get(index)?;
        let pattern_id = u64::from_le_bytes(self.pattern_ids.get(index)?.to_le_bytes());
        Some(DeprecatedDateStringValue {
            epoch,
            pattern_id,
            pattern: self.dictionary.pattern(pattern_id)?,
        })
    }
}

/// One current timestamp value and pattern.
#[derive(Clone, Copy, Debug)]
pub struct TimestampValue<'archive> {
    epoch_nanoseconds: i64,
    pattern_id: u64,
    pattern: &'archive TimestampPatternEntry,
}

impl<'archive> TimestampValue<'archive> {
    /// Returns the reconstructed epoch-nanosecond value.
    #[must_use]
    pub const fn epoch_nanoseconds(self) -> i64 {
        self.epoch_nanoseconds
    }

    /// Returns the explicit pattern ID.
    #[must_use]
    pub const fn pattern_id(self) -> u64 {
        self.pattern_id
    }

    /// Returns the referenced raw timestamp pattern.
    #[must_use]
    pub const fn pattern(self) -> &'archive TimestampPatternEntry {
        self.pattern
    }
}

/// A zero-copy current timestamp column.
#[derive(Clone, Copy, Debug)]
pub struct TimestampColumn<'table, 'archive> {
    epochs: DeltaI64Column<'table>,
    pattern_ids: U64Column<'table>,
    dictionary: &'archive TimestampDictionary,
}

impl<'table, 'archive> TimestampColumn<'table, 'archive> {
    /// Returns the number of timestamps.
    #[must_use]
    pub const fn len(self) -> usize {
        self.epochs.len()
    }

    /// Returns whether the column contains no timestamps.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.epochs.is_empty()
    }

    /// Returns the validated delta epoch-nanosecond view.
    #[must_use]
    pub const fn epochs(self) -> DeltaI64Column<'table> {
        self.epochs
    }

    /// Returns the current unsigned pattern-ID view.
    #[must_use]
    pub const fn pattern_ids(self) -> U64Column<'table> {
        self.pattern_ids
    }

    /// Iterates epoch-nanosecond and pattern-ID pairs in one linear pass.
    ///
    /// This is the preferred extraction path. Unlike repeated [`Self::get`] calls, it does not
    /// rescan delta values from the beginning for every row.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn encoded_values(
        self,
    ) -> impl ExactSizeIterator<Item = (i64, u64)> + FusedIterator + 'table {
        self.epochs.values().zip(self.pattern_ids.iter())
    }

    /// Returns one reconstructed timestamp and pattern.
    #[must_use]
    pub fn get(self, index: usize) -> Option<TimestampValue<'archive>> {
        let epoch_nanoseconds = self.epochs.get(index)?;
        let pattern_id = self.pattern_ids.get(index)?;
        Some(TimestampValue {
            epoch_nanoseconds,
            pattern_id,
            pattern: self.dictionary.pattern(pattern_id)?,
        })
    }
}

/// Decodes and validates one complete decompressed schema table.
///
/// Values remain borrowed from `table_bytes`; dictionaries are borrowed only for validation and
/// zero-copy lookup accessors. Structural schema nodes and unordered-container delimiters do not
/// produce columns.
///
/// # Errors
///
/// Returns a resource error when configured limits are exceeded, a truncation or trailing-data
/// error when the table size disagrees with the schema, or a structured corruption error for an
/// invalid value, reference, descriptor, delta, or CLP encoded-variable span.
#[allow(clippy::too_many_arguments)]
pub fn decode_schema_table<'table, 'archive>(
    table_bytes: &'table [u8],
    schema: &SchemaDefinition,
    schema_tree: &SchemaTree,
    message_count: u64,
    variable_dictionary: &'archive VariableDictionary,
    logtype_dictionary: &'archive LogTypeDictionary,
    array_dictionary: &'archive ArrayDictionary,
    timestamp_dictionary: &'archive TimestampDictionary,
    limits: ColumnLimits,
) -> Result<SchemaTable<'table, 'archive>, ColumnError> {
    let table_size = u64::try_from(table_bytes.len()).map_err(|_| ColumnError::SizeOverflow)?;
    if table_size > limits.table_bytes {
        return Err(ColumnError::TableTooLarge {
            actual: table_size,
            limit: limits.table_bytes,
        });
    }
    if message_count > limits.messages {
        return Err(ColumnError::MessageCountTooLarge {
            actual: message_count,
            limit: limits.messages,
        });
    }
    let message_count = usize::try_from(message_count).map_err(|_| ColumnError::SizeOverflow)?;
    let column_count = count_value_columns(schema, schema_tree)?;
    let column_count_u64 = u64::try_from(column_count).map_err(|_| ColumnError::SizeOverflow)?;
    if column_count_u64 > limits.columns {
        return Err(ColumnError::ColumnCountTooLarge {
            actual: column_count_u64,
            limit: limits.columns,
        });
    }

    let mut columns = Vec::new();
    columns
        .try_reserve_exact(column_count)
        .map_err(|_| ColumnError::AllocationFailed {
            requested: column_count,
        })?;
    let mut cursor = TableCursor::new(table_bytes);
    let mut total_encoded_variables = 0_u64;

    for (schema_entry_index, entry) in schema.entries().iter().enumerate() {
        let SchemaEntry::Node(node_id) = *entry else {
            continue;
        };
        let node = schema_tree
            .get(usize::try_from(node_id).map_err(|_| ColumnError::SizeOverflow)?)
            .ok_or(ColumnError::UnknownSchemaNode { node_id })?;
        let node_type = node.node_type();
        if !is_value_bearing(node_type) {
            continue;
        }
        let context = ColumnContext {
            column_index: columns.len(),
            node_id,
        };
        let data = decode_column(
            &mut cursor,
            context,
            node_type,
            message_count,
            variable_dictionary,
            logtype_dictionary,
            array_dictionary,
            timestamp_dictionary,
            limits,
            &mut total_encoded_variables,
        )?;
        columns.push(Column {
            schema_entry_index,
            node_id,
            data,
        });
    }

    if 0 != cursor.remaining() {
        return Err(ColumnError::TrailingTableBytes {
            remaining: cursor.remaining(),
        });
    }
    Ok(SchemaTable {
        message_count,
        columns,
    })
}

#[derive(Clone, Copy)]
struct ColumnContext {
    column_index: usize,
    node_id: u32,
}

#[allow(clippy::too_many_arguments)]
fn decode_column<'table, 'archive>(
    cursor: &mut TableCursor<'table>,
    context: ColumnContext,
    node_type: NodeType,
    message_count: usize,
    variable_dictionary: &'archive VariableDictionary,
    logtype_dictionary: &'archive LogTypeDictionary,
    array_dictionary: &'archive ArrayDictionary,
    timestamp_dictionary: &'archive TimestampDictionary,
    limits: ColumnLimits,
    total_encoded_variables: &mut u64,
) -> Result<ColumnData<'table, 'archive>, ColumnError> {
    match node_type {
        NodeType::Integer => Ok(ColumnData::Integer(take_i64_column(
            cursor,
            context,
            message_count,
        )?)),
        NodeType::DeltaInteger => Ok(ColumnData::DeltaInteger(take_delta_column(
            cursor,
            context,
            message_count,
        )?)),
        NodeType::Float => Ok(ColumnData::Float(take_float_column(
            cursor,
            context,
            message_count,
        )?)),
        NodeType::FormattedFloat => Ok(ColumnData::FormattedFloat(take_formatted_float_column(
            cursor,
            context,
            message_count,
        )?)),
        NodeType::DictionaryFloat => Ok(ColumnData::DictionaryFloat(take_dictionary_id_column(
            cursor,
            context,
            message_count,
            variable_dictionary,
            true,
        )?)),
        NodeType::Boolean => Ok(ColumnData::Boolean(take_boolean_column(
            cursor,
            context,
            message_count,
        )?)),
        NodeType::VarString => Ok(ColumnData::VarString(take_dictionary_id_column(
            cursor,
            context,
            message_count,
            variable_dictionary,
            false,
        )?)),
        NodeType::ClpString => Ok(ColumnData::ClpString(take_clp_string_column(
            cursor,
            context,
            message_count,
            variable_dictionary,
            LogTypeDictionaryRef::Log(logtype_dictionary),
            limits,
            total_encoded_variables,
        )?)),
        NodeType::UnstructuredArray => Ok(ColumnData::UnstructuredArray(take_clp_string_column(
            cursor,
            context,
            message_count,
            variable_dictionary,
            LogTypeDictionaryRef::Array(array_dictionary),
            limits,
            total_encoded_variables,
        )?)),
        NodeType::DeprecatedDateString => Ok(ColumnData::DeprecatedDateString(
            take_deprecated_date_string_column(
                cursor,
                context,
                message_count,
                timestamp_dictionary,
            )?,
        )),
        NodeType::Timestamp => Ok(ColumnData::Timestamp(take_timestamp_column(
            cursor,
            context,
            message_count,
            timestamp_dictionary,
        )?)),
        NodeType::Object | NodeType::Null | NodeType::StructuredArray | NodeType::Metadata => {
            unreachable!("structural nodes are filtered before column decoding")
        }
    }
}

fn count_value_columns(
    schema: &SchemaDefinition,
    schema_tree: &SchemaTree,
) -> Result<usize, ColumnError> {
    let mut count = 0_usize;
    for entry in schema.entries() {
        let SchemaEntry::Node(node_id) = *entry else {
            continue;
        };
        let node = schema_tree
            .get(usize::try_from(node_id).map_err(|_| ColumnError::SizeOverflow)?)
            .ok_or(ColumnError::UnknownSchemaNode { node_id })?;
        if is_value_bearing(node.node_type()) {
            count = count.checked_add(1).ok_or(ColumnError::SizeOverflow)?;
        }
    }
    Ok(count)
}

const fn is_value_bearing(node_type: NodeType) -> bool {
    !matches!(
        node_type,
        NodeType::Object | NodeType::Null | NodeType::StructuredArray | NodeType::Metadata
    )
}

fn take_i64_column<'a>(
    cursor: &mut TableCursor<'a>,
    context: ColumnContext,
    count: usize,
) -> Result<I64Column<'a>, ColumnError> {
    Ok(I64Column {
        bytes: cursor.take_fixed(context, count, U64_SIZE)?,
    })
}

fn take_u64_column<'a>(
    cursor: &mut TableCursor<'a>,
    context: ColumnContext,
    count: usize,
) -> Result<U64Column<'a>, ColumnError> {
    Ok(U64Column {
        bytes: cursor.take_fixed(context, count, U64_SIZE)?,
    })
}

fn take_delta_column<'a>(
    cursor: &mut TableCursor<'a>,
    context: ColumnContext,
    count: usize,
) -> Result<DeltaI64Column<'a>, ColumnError> {
    let deltas = take_i64_column(cursor, context, count)?;
    validate_deltas(deltas, context)?;
    Ok(DeltaI64Column { deltas })
}

fn take_float_column<'a>(
    cursor: &mut TableCursor<'a>,
    context: ColumnContext,
    count: usize,
) -> Result<F64Column<'a>, ColumnError> {
    let values = F64Column {
        bytes: cursor.take_fixed(context, count, U64_SIZE)?,
    };
    validate_finite_floats(values, context)?;
    Ok(values)
}

fn take_formatted_float_column<'a>(
    cursor: &mut TableCursor<'a>,
    context: ColumnContext,
    count: usize,
) -> Result<FormattedFloatColumn<'a>, ColumnError> {
    let values = take_float_column(cursor, context, count)?;
    let formats = cursor.take_fixed(context, count, U16_SIZE)?;
    for (message_index, format) in formats.as_chunks::<U16_SIZE>().0.iter().enumerate() {
        let raw = u16::from_le_bytes(*format);
        if let Err(reason) = FloatFormat::try_from(raw) {
            return Err(corrupt(
                context,
                Some(message_index),
                ColumnCorruption::InvalidFloatFormat { raw, reason },
            ));
        }
    }
    Ok(FormattedFloatColumn { values, formats })
}

fn take_boolean_column<'a>(
    cursor: &mut TableCursor<'a>,
    context: ColumnContext,
    count: usize,
) -> Result<BooleanColumn<'a>, ColumnError> {
    let bytes = cursor.take_fixed(context, count, 1)?;
    for (message_index, value) in bytes.iter().copied().enumerate() {
        if value > 1 {
            return Err(corrupt(
                context,
                Some(message_index),
                ColumnCorruption::InvalidBoolean { actual: value },
            ));
        }
    }
    Ok(BooleanColumn { bytes })
}

fn take_dictionary_id_column<'table, 'archive>(
    cursor: &mut TableCursor<'table>,
    context: ColumnContext,
    count: usize,
    dictionary: &'archive VariableDictionary,
    validate_float: bool,
) -> Result<DictionaryIdColumn<'table, 'archive>, ColumnError> {
    let ids = take_u64_column(cursor, context, count)?;
    for (message_index, id) in ids.iter().enumerate() {
        let entry = dictionary.entry(id).ok_or_else(|| {
            corrupt(
                context,
                Some(message_index),
                ColumnCorruption::UnknownVariableDictionaryId { id },
            )
        })?;
        if validate_float {
            let finite = std::str::from_utf8(entry.value())
                .ok()
                .and_then(|value| value.parse::<f64>().ok())
                .is_some_and(f64::is_finite);
            if !finite {
                return Err(corrupt(
                    context,
                    Some(message_index),
                    ColumnCorruption::InvalidDictionaryFloat { id },
                ));
            }
        }
    }
    Ok(DictionaryIdColumn { ids, dictionary })
}

#[allow(clippy::too_many_arguments)]
fn take_clp_string_column<'table, 'archive>(
    cursor: &mut TableCursor<'table>,
    context: ColumnContext,
    message_count: usize,
    variable_dictionary: &'archive VariableDictionary,
    logtype_dictionary: LogTypeDictionaryRef<'archive>,
    limits: ColumnLimits,
    total_encoded_variables: &mut u64,
) -> Result<ClpStringColumn<'table, 'archive>, ColumnError> {
    let descriptors = take_u64_column(cursor, context, message_count)?;
    let encoded_variable_count = cursor.take_u64(context)?;
    if encoded_variable_count > limits.encoded_variables_per_column {
        return Err(ColumnError::EncodedVariableCountTooLarge {
            column_index: context.column_index,
            node_id: context.node_id,
            actual: encoded_variable_count,
            limit: limits.encoded_variables_per_column,
        });
    }
    if encoded_variable_count > MAX_ENCODED_VARIABLE_DOMAIN {
        return Err(corrupt(
            context,
            None,
            ColumnCorruption::EncodedVariableDomainTooLarge {
                count: encoded_variable_count,
            },
        ));
    }
    *total_encoded_variables = total_encoded_variables
        .checked_add(encoded_variable_count)
        .ok_or(ColumnError::SizeOverflow)?;
    if *total_encoded_variables > limits.total_encoded_variables {
        return Err(ColumnError::TotalEncodedVariableCountTooLarge {
            actual: *total_encoded_variables,
            limit: limits.total_encoded_variables,
        });
    }
    let encoded_variable_count =
        usize::try_from(encoded_variable_count).map_err(|_| ColumnError::SizeOverflow)?;
    let encoded_variables = take_i64_column(cursor, context, encoded_variable_count)?;
    validate_clp_descriptors(
        descriptors,
        encoded_variables,
        context,
        variable_dictionary,
        logtype_dictionary,
    )?;
    Ok(ClpStringColumn {
        descriptors,
        encoded_variables,
        variable_dictionary,
        logtype_dictionary,
    })
}

fn take_deprecated_date_string_column<'table, 'archive>(
    cursor: &mut TableCursor<'table>,
    context: ColumnContext,
    message_count: usize,
    dictionary: &'archive TimestampDictionary,
) -> Result<DeprecatedDateStringColumn<'table, 'archive>, ColumnError> {
    let epochs = take_i64_column(cursor, context, message_count)?;
    let pattern_ids = take_i64_column(cursor, context, message_count)?;
    for (message_index, signed_id) in pattern_ids.iter().enumerate() {
        let id = u64::from_le_bytes(signed_id.to_le_bytes());
        validate_timestamp_pattern(dictionary, context, message_index, id)?;
    }
    Ok(DeprecatedDateStringColumn {
        epochs,
        pattern_ids,
        dictionary,
    })
}

fn take_timestamp_column<'table, 'archive>(
    cursor: &mut TableCursor<'table>,
    context: ColumnContext,
    message_count: usize,
    dictionary: &'archive TimestampDictionary,
) -> Result<TimestampColumn<'table, 'archive>, ColumnError> {
    let epochs = take_delta_column(cursor, context, message_count)?;
    let pattern_ids = take_u64_column(cursor, context, message_count)?;
    for (message_index, id) in pattern_ids.iter().enumerate() {
        validate_timestamp_pattern(dictionary, context, message_index, id)?;
    }
    Ok(TimestampColumn {
        epochs,
        pattern_ids,
        dictionary,
    })
}

fn validate_timestamp_pattern(
    dictionary: &TimestampDictionary,
    context: ColumnContext,
    message_index: usize,
    id: u64,
) -> Result<(), ColumnError> {
    if dictionary.pattern(id).is_none() {
        return Err(corrupt(
            context,
            Some(message_index),
            ColumnCorruption::UnknownTimestampPattern { id },
        ));
    }
    Ok(())
}

fn validate_deltas(column: I64Column<'_>, context: ColumnContext) -> Result<(), ColumnError> {
    let mut previous = 0_i64;
    for (message_index, delta) in column.iter().enumerate() {
        previous = previous.checked_add(delta).ok_or_else(|| {
            corrupt(
                context,
                Some(message_index),
                ColumnCorruption::DeltaOverflow { previous, delta },
            )
        })?;
    }
    Ok(())
}

fn validate_finite_floats(
    column: F64Column<'_>,
    context: ColumnContext,
) -> Result<(), ColumnError> {
    for (message_index, value) in column.iter().enumerate() {
        if !value.is_finite() {
            return Err(corrupt(
                context,
                Some(message_index),
                ColumnCorruption::NonFiniteFloat,
            ));
        }
    }
    Ok(())
}

fn validate_clp_descriptors(
    descriptors: U64Column<'_>,
    encoded_variables: I64Column<'_>,
    context: ColumnContext,
    variable_dictionary: &VariableDictionary,
    logtype_dictionary: LogTypeDictionaryRef<'_>,
) -> Result<(), ColumnError> {
    let total = u64::try_from(encoded_variables.len()).map_err(|_| ColumnError::SizeOverflow)?;
    let mut expected_offset = 0_u64;
    // Descriptor IDs occupy only 24 bits, so this sentinel cannot alias a valid logtype. Cache
    // only the validation metadata used below instead of carrying the considerably larger
    // `LogTypeDictionaryEntry` through every iteration of a same-logtype run.
    let mut cached_logtype_id = u64::MAX;
    let mut cached_encoded_variable_count = 0_u64;
    let mut cached_variable_kinds = &[][..];
    let mut cached_needs_variable_validation = false;
    for (message_index, raw) in descriptors.iter().enumerate() {
        let descriptor = ClpDescriptor { raw };
        let logtype_id = u64::from(descriptor.logtype_id());
        if cached_logtype_id != logtype_id {
            let entry = logtype_dictionary.entry(logtype_id).ok_or_else(|| {
                corrupt(
                    context,
                    Some(message_index),
                    ColumnCorruption::UnknownLogType {
                        section: logtype_dictionary.section(),
                        id: logtype_id,
                    },
                )
            })?;
            let counts = entry.placeholder_counts();
            cached_logtype_id = logtype_id;
            cached_encoded_variable_count = counts.encoded_variables();
            cached_variable_kinds = entry.variable_kinds();
            cached_needs_variable_validation = 0 != counts.dictionary() || 0 != counts.float();
        }
        let offset = descriptor.encoded_variable_offset();
        if offset != expected_offset {
            return Err(corrupt(
                context,
                Some(message_index),
                ColumnCorruption::NonCanonicalEncodedVariableOffset {
                    expected: expected_offset,
                    actual: offset,
                },
            ));
        }
        let count = cached_encoded_variable_count;
        let end = offset.checked_add(count).ok_or(ColumnError::SizeOverflow)?;
        if end > total {
            return Err(corrupt(
                context,
                Some(message_index),
                ColumnCorruption::EncodedVariableSpanOutOfBounds {
                    offset,
                    count,
                    total,
                },
            ));
        }
        let offset = usize::try_from(offset).map_err(|_| ColumnError::SizeOverflow)?;
        let count = usize::try_from(count).map_err(|_| ColumnError::SizeOverflow)?;
        let variables = encoded_variables
            .slice(offset, count)
            .ok_or(ColumnError::SizeOverflow)?;
        validate_clp_variables(
            cached_variable_kinds,
            cached_needs_variable_validation,
            variables,
            context,
            message_index,
            offset,
            variable_dictionary,
        )?;
        expected_offset = end;
    }
    if expected_offset != total {
        return Err(corrupt(
            context,
            None,
            ColumnCorruption::EncodedVariableCountMismatch {
                referenced: expected_offset,
                declared: total,
            },
        ));
    }
    Ok(())
}

fn validate_clp_variables(
    variable_kinds: &[LogTypeVariableKind],
    needs_validation: bool,
    variables: I64Column<'_>,
    context: ColumnContext,
    message_index: usize,
    absolute_offset: usize,
    variable_dictionary: &VariableDictionary,
) -> Result<(), ColumnError> {
    if !needs_validation {
        return Ok(());
    }

    debug_assert_eq!(variables.len(), variable_kinds.len());
    for (variable_index, kind) in variable_kinds.iter().copied().enumerate() {
        match kind {
            LogTypeVariableKind::Integer => {}
            LogTypeVariableKind::Dictionary => {
                let raw = variables
                    .get(variable_index)
                    .ok_or(ColumnError::SizeOverflow)?;
                let id = u64::from_le_bytes(raw.to_le_bytes());
                if variable_dictionary.entry(id).is_none() {
                    return Err(corrupt(
                        context,
                        Some(message_index),
                        ColumnCorruption::UnknownDictionaryVariable {
                            encoded_variable_index: absolute_offset + variable_index,
                            id,
                        },
                    ));
                }
            }
            LogTypeVariableKind::Float => {
                let raw = variables
                    .get(variable_index)
                    .ok_or(ColumnError::SizeOverflow)?;
                if let Err(reason) = validate_encoded_float(raw) {
                    return Err(corrupt(
                        context,
                        Some(message_index),
                        ColumnCorruption::InvalidEncodedFloat {
                            encoded_variable_index: absolute_offset + variable_index,
                            reason,
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_encoded_float(raw: i64) -> Result<(), EncodedFloatErrorReason> {
    const EXCLUSIVE_DIGIT_LIMITS: [u64; 16] = [
        10,
        100,
        1_000,
        10_000,
        100_000,
        1_000_000,
        10_000_000,
        100_000_000,
        1_000_000_000,
        10_000_000_000,
        100_000_000_000,
        1_000_000_000_000,
        10_000_000_000_000,
        100_000_000_000_000,
        1_000_000_000_000_000,
        10_000_000_000_000_000,
    ];

    let bits = u64::from_le_bytes(raw.to_le_bytes());
    if 0 != bits & ENCODED_FLOAT_UNUSED_BIT {
        return Err(EncodedFloatErrorReason::ReservedBit);
    }
    let decimal_position = (bits & 0x0f) + 1;
    let digit_count = ((bits >> 4) & 0x0f) + 1;
    let digits = (bits >> 8) & ENCODED_FLOAT_DIGITS_MASK;
    if digits > MAX_ENCODED_FLOAT_DIGITS {
        return Err(EncodedFloatErrorReason::DigitsTooLarge);
    }
    if decimal_position > digit_count {
        return Err(EncodedFloatErrorReason::DecimalPositionExceedsDigits);
    }
    let limit_index = usize::try_from(digit_count - 1)
        .expect("the encoded-float digit count is a four-bit value");
    let exclusive_limit = EXCLUSIVE_DIGIT_LIMITS[limit_index];
    if digits >= exclusive_limit {
        return Err(EncodedFloatErrorReason::DigitValueExceedsDeclaredDigits);
    }
    Ok(())
}

/// Which physical logtype dictionary a CLP descriptor references.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LogTypeSection {
    /// `/log.dict` for ordinary CLP strings.
    Log,
    /// `/array.dict` for unstructured arrays.
    Array,
}

impl Display for LogTypeSection {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Log => "/log.dict",
            Self::Array => "/array.dict",
        })
    }
}

/// Reason an eight-byte CLP encoded float is structurally invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodedFloatErrorReason {
    /// The unused bit below the sign bit is set.
    ReservedBit,
    /// The 54-bit digit value exceeds the largest 16-digit decimal integer.
    DigitsTooLarge,
    /// The decimal point lies to the left of the declared digit domain.
    DecimalPositionExceedsDigits,
    /// The digit integer needs more decimal digits than declared.
    DigitValueExceedsDeclaredDigits,
}

impl Display for EncodedFloatErrorReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReservedBit => "encoded-float reserved bit is set",
            Self::DigitsTooLarge => "encoded-float digits exceed the 16-digit maximum",
            Self::DecimalPositionExceedsDigits => {
                "encoded-float decimal position exceeds its digit count"
            }
            Self::DigitValueExceedsDeclaredDigits => {
                "encoded-float value needs more digits than declared"
            }
        })
    }
}

impl Error for EncodedFloatErrorReason {}

/// Semantic corruption found in one otherwise length-valid column.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum ColumnCorruption {
    /// A raw or formatted binary64 value is NaN or infinite.
    NonFiniteFloat,
    /// A Boolean byte is not canonical `0` or `1`.
    InvalidBoolean {
        /// Invalid byte.
        actual: u8,
    },
    /// A formatted-float descriptor is invalid.
    InvalidFloatFormat {
        /// Exact descriptor.
        raw: u16,
        /// Structural failure.
        reason: FloatFormatErrorReason,
    },
    /// A dictionary-float entry is not a finite UTF-8 numeric lexeme.
    InvalidDictionaryFloat {
        /// Variable-dictionary ID.
        id: u64,
    },
    /// A variable or dictionary-float ID is absent from `/var.dict`.
    UnknownVariableDictionaryId {
        /// Missing ID.
        id: u64,
    },
    /// Reconstructing a delta-encoded value overflowed signed 64-bit range.
    DeltaOverflow {
        /// Previous reconstructed value.
        previous: i64,
        /// Next stored delta.
        delta: i64,
    },
    /// A timestamp pattern ID is absent from the timestamp dictionary.
    UnknownTimestampPattern {
        /// Missing pattern ID.
        id: u64,
    },
    /// A declared CLP encoded-variable array cannot be addressed by 40-bit offsets.
    EncodedVariableDomainTooLarge {
        /// Declared count.
        count: u64,
    },
    /// A CLP descriptor references an absent logtype.
    UnknownLogType {
        /// Dictionary ID space.
        section: LogTypeSection,
        /// Missing logtype ID.
        id: u64,
    },
    /// A descriptor offset differs from the canonical contiguous record layout.
    NonCanonicalEncodedVariableOffset {
        /// Required offset.
        expected: u64,
        /// Stored offset.
        actual: u64,
    },
    /// A logtype's encoded-variable span exceeds the declared array.
    EncodedVariableSpanOutOfBounds {
        /// Starting variable index.
        offset: u64,
        /// Variables required by the logtype.
        count: u64,
        /// Declared total variable count.
        total: u64,
    },
    /// Descriptor spans do not consume the complete declared encoded-variable array.
    EncodedVariableCountMismatch {
        /// Variables referenced by all records.
        referenced: u64,
        /// Declared total variable count.
        declared: u64,
    },
    /// A dictionary placeholder references an absent variable dictionary entry.
    UnknownDictionaryVariable {
        /// Absolute index in the column's encoded-variable array.
        encoded_variable_index: usize,
        /// Missing variable-dictionary ID.
        id: u64,
    },
    /// A float placeholder contains an invalid custom encoded float.
    InvalidEncodedFloat {
        /// Absolute index in the column's encoded-variable array.
        encoded_variable_index: usize,
        /// Structural failure.
        reason: EncodedFloatErrorReason,
    },
}

impl Display for ColumnCorruption {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteFloat => formatter.write_str("binary64 value is not finite"),
            Self::InvalidBoolean { actual } => write!(formatter, "Boolean byte is {actual}"),
            Self::InvalidFloatFormat { raw, reason } => {
                write!(formatter, "formatted-float descriptor {raw:#06x}: {reason}")
            }
            Self::InvalidDictionaryFloat { id } => {
                write!(
                    formatter,
                    "variable dictionary entry {id} is not a finite float"
                )
            }
            Self::UnknownVariableDictionaryId { id } => {
                write!(formatter, "variable dictionary has no entry {id}")
            }
            Self::DeltaOverflow { previous, delta } => {
                write!(
                    formatter,
                    "delta {delta} overflows previous value {previous}"
                )
            }
            Self::UnknownTimestampPattern { id } => {
                write!(formatter, "timestamp dictionary has no pattern {id}")
            }
            Self::EncodedVariableDomainTooLarge { count } => write!(
                formatter,
                "encoded-variable count {count} exceeds the 40-bit descriptor domain"
            ),
            Self::UnknownLogType { section, id } => {
                write!(formatter, "{section} has no entry {id}")
            }
            Self::NonCanonicalEncodedVariableOffset { expected, actual } => write!(
                formatter,
                "encoded-variable offset {actual} is not canonical offset {expected}"
            ),
            Self::EncodedVariableSpanOutOfBounds {
                offset,
                count,
                total,
            } => write!(
                formatter,
                "encoded-variable span {offset}+{count} exceeds declared count {total}"
            ),
            Self::EncodedVariableCountMismatch {
                referenced,
                declared,
            } => write!(
                formatter,
                "descriptors reference {referenced} encoded variables but {declared} were declared"
            ),
            Self::UnknownDictionaryVariable {
                encoded_variable_index,
                id,
            } => write!(
                formatter,
                "encoded variable {encoded_variable_index} references missing var.dict ID {id}"
            ),
            Self::InvalidEncodedFloat {
                encoded_variable_index,
                reason,
            } => write!(
                formatter,
                "encoded variable {encoded_variable_index} has invalid float: {reason}"
            ),
        }
    }
}

/// Failure to decode or structurally validate one schema table.
#[derive(Debug)]
#[non_exhaustive]
pub enum ColumnError {
    /// The complete decompressed table exceeds its configured byte limit.
    TableTooLarge {
        /// Actual bytes.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// The table metadata message count exceeds its configured limit.
    MessageCountTooLarge {
        /// Declared messages.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// The value-bearing schema column count exceeds its configured limit.
    ColumnCountTooLarge {
        /// Actual columns.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// One CLP column's encoded-variable count exceeds its configured limit.
    EncodedVariableCountTooLarge {
        /// Table-local column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Declared count.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// Cumulative CLP encoded variables exceed their configured limit.
    TotalEncodedVariableCountTooLarge {
        /// Cumulative count.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// Checked table size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded column-vector allocation could not be reserved.
    AllocationFailed {
        /// Elements requested.
        requested: usize,
    },
    /// The schema definition references a node absent from the supplied schema tree.
    UnknownSchemaNode {
        /// Missing node ID.
        node_id: u32,
    },
    /// A column needs more bytes than remain in the table.
    TruncatedColumn {
        /// Table-local value-bearing column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Bytes required by this read.
        needed: usize,
        /// Bytes remaining in the table.
        remaining: usize,
    },
    /// Bytes remain after every schema column was consumed.
    TrailingTableBytes {
        /// Unconsumed bytes.
        remaining: usize,
    },
    /// A value or cross-resource reference is semantically corrupt.
    Corrupt {
        /// Table-local value-bearing column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Record index, or `None` for a whole-column invariant.
        message_index: Option<usize>,
        /// Corruption reason.
        reason: ColumnCorruption,
    },
}

impl Display for ColumnError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "schema table size {actual} exceeds limit {limit}"
                )
            }
            Self::MessageCountTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "table message count {actual} exceeds limit {limit}"
                )
            }
            Self::ColumnCountTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "table column count {actual} exceeds limit {limit}"
                )
            }
            Self::EncodedVariableCountTooLarge {
                column_index,
                node_id,
                actual,
                limit,
            } => write!(
                formatter,
                "column {column_index} node {node_id} encoded-variable count {actual} exceeds \
                 limit {limit}"
            ),
            Self::TotalEncodedVariableCountTooLarge { actual, limit } => write!(
                formatter,
                "table encoded-variable count {actual} exceeds limit {limit}"
            ),
            Self::SizeOverflow => formatter.write_str("schema table size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve bounded schema-table allocation of {requested} columns"
            ),
            Self::UnknownSchemaNode { node_id } => {
                write!(formatter, "schema references missing tree node {node_id}")
            }
            Self::TruncatedColumn {
                column_index,
                node_id,
                needed,
                remaining,
            } => write!(
                formatter,
                "column {column_index} node {node_id} needs {needed} bytes; {remaining} remain"
            ),
            Self::TrailingTableBytes { remaining } => {
                write!(
                    formatter,
                    "{remaining} bytes follow the schema table columns"
                )
            }
            Self::Corrupt {
                column_index,
                node_id,
                message_index,
                reason,
            } => {
                write!(formatter, "column {column_index} node {node_id}")?;
                if let Some(message_index) = message_index {
                    write!(formatter, " message {message_index}")?;
                }
                write!(formatter, ": {reason}")
            }
        }
    }
}

impl Error for ColumnError {}

const fn corrupt(
    context: ColumnContext,
    message_index: Option<usize>,
    reason: ColumnCorruption,
) -> ColumnError {
    ColumnError::Corrupt {
        column_index: context.column_index,
        node_id: context.node_id,
        message_index,
        reason,
    }
}

struct TableCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> TableCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn take_fixed(
        &mut self,
        context: ColumnContext,
        count: usize,
        width: usize,
    ) -> Result<&'a [u8], ColumnError> {
        let size = count.checked_mul(width).ok_or(ColumnError::SizeOverflow)?;
        self.take(context, size)
    }

    fn take_u64(&mut self, context: ColumnContext) -> Result<u64, ColumnError> {
        Ok(decode_u64_chunk(self.take(context, U64_SIZE)?))
    }

    fn take(&mut self, context: ColumnContext, size: usize) -> Result<&'a [u8], ColumnError> {
        let remaining = self.remaining();
        if size > remaining {
            return Err(ColumnError::TruncatedColumn {
                column_index: context.column_index,
                node_id: context.node_id,
                needed: size,
                remaining,
            });
        }
        let end = self
            .position
            .checked_add(size)
            .ok_or(ColumnError::SizeOverflow)?;
        let bytes = &self.bytes[self.position..end];
        self.position = end;
        Ok(bytes)
    }
}

fn read_i64_at(bytes: &[u8], index: usize) -> Option<i64> {
    let start = index.checked_mul(U64_SIZE)?;
    let end = start.checked_add(U64_SIZE)?;
    Some(decode_i64_chunk(bytes.get(start..end)?))
}

fn read_u64_at(bytes: &[u8], index: usize) -> Option<u64> {
    let start = index.checked_mul(U64_SIZE)?;
    let end = start.checked_add(U64_SIZE)?;
    Some(decode_u64_chunk(bytes.get(start..end)?))
}

fn read_u16_at(bytes: &[u8], index: usize) -> Option<u16> {
    let start = index.checked_mul(U16_SIZE)?;
    let end = start.checked_add(U16_SIZE)?;
    Some(decode_u16_chunk(bytes.get(start..end)?))
}

fn decode_i64_chunk(bytes: &[u8]) -> i64 {
    i64::from_le_bytes(copy_array(bytes))
}

fn decode_u64_chunk(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(copy_array(bytes))
}

fn decode_f64_chunk(bytes: &[u8]) -> f64 {
    f64::from_bits(decode_u64_chunk(bytes))
}

fn decode_u16_chunk(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(copy_array(bytes))
}

fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut output = [0_u8; N];
    output.copy_from_slice(bytes);
    output
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Read as _;

    use super::super::dictionary::DictionaryLimits;
    use super::super::dictionary::decode_array_dictionary;
    use super::super::dictionary::decode_logtype_dictionary;
    use super::super::dictionary::decode_variable_dictionary;
    use super::super::schema_map::SchemaMapLimits;
    use super::super::schema_map::decode_schema_map;
    use super::super::schema_tree::SchemaTreeLimits;
    use super::super::schema_tree::decode_schema_tree;
    use super::super::timestamp_dictionary::TimestampDictionaryLimits;
    use super::*;

    const CPP_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    struct Resources {
        variable: VariableDictionary,
        logtype: LogTypeDictionary,
        array: ArrayDictionary,
        timestamp: TimestampDictionary,
    }

    fn take(bytes: &[u8]) -> std::io::Take<Cursor<&[u8]>> {
        Cursor::new(bytes).take(u64::try_from(bytes.len()).expect("test byte count fits u64"))
    }

    fn dictionary_section(entries: &[&[u8]]) -> Vec<u8> {
        let mut section = u64::try_from(entries.len())
            .expect("test dictionary count fits u64")
            .to_le_bytes()
            .to_vec();
        if entries.is_empty() {
            return section;
        }
        let mut payload = Vec::new();
        for entry in entries {
            payload.extend_from_slice(
                &u64::try_from(entry.len())
                    .expect("test entry length fits u64")
                    .to_le_bytes(),
            );
            payload.extend_from_slice(entry);
        }
        section.extend_from_slice(
            &zstd::stream::encode_all(payload.as_slice(), 3).expect("compress test dictionary"),
        );
        section
    }

    fn timestamp_dictionary(patterns: &[(u64, &str)]) -> TimestampDictionary {
        let mut payload = 0_u64.to_le_bytes().to_vec();
        payload.extend_from_slice(
            &u64::try_from(patterns.len())
                .expect("test pattern count fits u64")
                .to_le_bytes(),
        );
        for (id, pattern) in patterns {
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(
                &u64::try_from(pattern.len())
                    .expect("test pattern length fits u64")
                    .to_le_bytes(),
            );
            payload.extend_from_slice(pattern.as_bytes());
        }
        TimestampDictionary::decode(payload, TimestampDictionaryLimits::default())
            .expect("valid test timestamp dictionary")
    }

    fn resources(
        variable: &[&[u8]],
        logtype: &[&[u8]],
        array: &[&[u8]],
        patterns: &[(u64, &str)],
    ) -> Resources {
        let variable_section = dictionary_section(variable);
        let logtype_section = dictionary_section(logtype);
        let array_section = dictionary_section(array);
        Resources {
            variable: decode_variable_dictionary(
                take(&variable_section),
                DictionaryLimits::default(),
            )
            .expect("valid test variable dictionary"),
            logtype: decode_logtype_dictionary(take(&logtype_section), DictionaryLimits::default())
                .expect("valid test logtype dictionary"),
            array: decode_array_dictionary(take(&array_section), DictionaryLimits::default())
                .expect("valid test array dictionary"),
            timestamp: timestamp_dictionary(patterns),
        }
    }

    fn schema_tree(nodes: &[(i32, &[u8], NodeType)]) -> SchemaTree {
        let mut payload = u64::try_from(nodes.len())
            .expect("test node count fits u64")
            .to_le_bytes()
            .to_vec();
        for (parent, key, node_type) in nodes {
            payload.extend_from_slice(&parent.to_le_bytes());
            payload.extend_from_slice(
                &u64::try_from(key.len())
                    .expect("test key length fits u64")
                    .to_le_bytes(),
            );
            payload.extend_from_slice(key);
            payload.push(*node_type as u8);
        }
        let compressed =
            zstd::stream::encode_all(payload.as_slice(), 3).expect("compress test schema tree");
        decode_schema_tree(take(&compressed), SchemaTreeLimits::default())
            .expect("valid test schema tree")
    }

    fn flat_tree(types: &[NodeType]) -> SchemaTree {
        let keys = (0..types.len())
            .map(|index| format!("key-{index}").into_bytes())
            .collect::<Vec<_>>();
        let nodes = types
            .iter()
            .copied()
            .zip(&keys)
            .map(|(node_type, key)| (-1, key.as_slice(), node_type))
            .collect::<Vec<_>>();
        schema_tree(&nodes)
    }

    fn schema(tree: &SchemaTree, entries: &[u32]) -> SchemaDefinition {
        let raw = entries
            .iter()
            .map(|entry| entry.to_le_bytes())
            .collect::<Vec<_>>();
        schema_from_raw(tree, &raw, entries.len())
    }

    fn schema_from_raw(
        tree: &SchemaTree,
        entries: &[[u8; 4]],
        ordered_count: usize,
    ) -> SchemaDefinition {
        let mut payload = 1_u64.to_le_bytes().to_vec();
        payload.extend_from_slice(&7_i32.to_le_bytes());
        payload.extend_from_slice(
            &u32::try_from(entries.len())
                .expect("test schema size fits u32")
                .to_le_bytes(),
        );
        payload.extend_from_slice(
            &u32::try_from(ordered_count)
                .expect("test ordered size fits u32")
                .to_le_bytes(),
        );
        for entry in entries {
            payload.extend_from_slice(entry);
        }
        let compressed =
            zstd::stream::encode_all(payload.as_slice(), 3).expect("compress test schema map");
        decode_schema_map(take(&compressed), tree, SchemaMapLimits::default())
            .expect("valid test schema map")
            .get(7)
            .expect("test schema ID seven")
            .clone()
    }

    fn decode<'table, 'archive>(
        bytes: &'table [u8],
        tree: &SchemaTree,
        schema: &SchemaDefinition,
        message_count: u64,
        resources: &'archive Resources,
    ) -> Result<SchemaTable<'table, 'archive>, ColumnError> {
        decode_with_limits(
            bytes,
            tree,
            schema,
            message_count,
            resources,
            ColumnLimits::default(),
        )
    }

    fn decode_with_limits<'table, 'archive>(
        bytes: &'table [u8],
        tree: &SchemaTree,
        schema: &SchemaDefinition,
        message_count: u64,
        resources: &'archive Resources,
        limits: ColumnLimits,
    ) -> Result<SchemaTable<'table, 'archive>, ColumnError> {
        decode_schema_table(
            bytes,
            schema,
            tree,
            message_count,
            &resources.variable,
            &resources.logtype,
            &resources.array,
            &resources.timestamp,
            limits,
        )
    }

    fn single_column(
        node_type: NodeType,
        bytes: &[u8],
        message_count: u64,
        resources: &Resources,
    ) -> Result<(), ColumnError> {
        let tree = flat_tree(&[node_type]);
        let schema = schema(&tree, &[0]);
        decode(bytes, &tree, &schema, message_count, resources).map(|_| ())
    }

    fn push_i64(bytes: &mut Vec<u8>, value: i64) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_f64(bytes: &mut Vec<u8>, value: f64) {
        bytes.extend_from_slice(&value.to_bits().to_le_bytes());
    }

    fn encoded_float(digits: u64, digit_count: u8, decimal_position: u8) -> i64 {
        let bits =
            (digits << 8) | (u64::from(digit_count - 1) << 4) | u64::from(decimal_position - 1);
        i64::from_le_bytes(bits.to_le_bytes())
    }

    #[test]
    fn decodes_the_exact_cpp_oracle_57_byte_table() {
        let table_bytes = zstd::stream::decode_all(&CPP_FIXTURE[617..654])
            .expect("decompress committed C++ packed stream");
        assert_eq!(57, table_bytes.len());

        let tree = schema_tree(&[
            (-1, b"", NodeType::Metadata),
            (0, b"log_event_idx", NodeType::DeltaInteger),
            (-1, b"", NodeType::Object),
            (2, b"ts", NodeType::Timestamp),
            (2, b"level", NodeType::VarString),
            (2, b"message", NodeType::ClpString),
            (2, b"value", NodeType::Integer),
            (2, b"active", NodeType::Boolean),
        ]);
        let schema = schema(&tree, &[1, 3, 4, 5, 6, 7]);
        let resources = resources(&[b"INFO"], &[b"oracle fixture"], &[], &[(0, r"\L")]);
        let table = decode(&table_bytes, &tree, &schema, 1, &resources)
            .expect("decode exact C++ schema table");

        assert_eq!(1, table.message_count());
        assert_eq!(6, table.len());
        assert_eq!(1, table.columns()[0].node_id());
        let ColumnData::DeltaInteger(log_index) = table.columns()[0].data() else {
            panic!("expected delta log index")
        };
        let mut log_indexes = log_index.values();
        assert_eq!(1, log_indexes.len());
        assert_eq!((1, Some(1)), log_indexes.size_hint());
        assert_eq!(Some(0), log_indexes.next());
        assert_eq!(0, log_indexes.len());
        assert_eq!(None, log_indexes.next());
        assert_eq!(None, log_indexes.next());

        let ColumnData::Timestamp(timestamp) = table.columns()[1].data() else {
            panic!("expected timestamp")
        };
        assert_eq!(
            vec![(1_700_000_000_123_000_000, 0)],
            timestamp.encoded_values().collect::<Vec<_>>()
        );
        let timestamp = timestamp.get(0).expect("timestamp zero");
        assert_eq!(1_700_000_000_123_000_000, timestamp.epoch_nanoseconds());
        assert_eq!(0, timestamp.pattern_id());
        assert_eq!(r"\L", timestamp.pattern().raw());

        let ColumnData::VarString(level) = table.columns()[2].data() else {
            panic!("expected variable string")
        };
        assert_eq!(Some(b"INFO".as_slice()), level.value(0));

        let ColumnData::ClpString(message) = table.columns()[3].data() else {
            panic!("expected CLP string")
        };
        let record = message.record(0).expect("message record zero");
        assert_eq!(b"oracle fixture", record.logtype().escaped_value());
        assert!(record.encoded_variables().is_empty());

        let ColumnData::Integer(integer) = table.columns()[4].data() else {
            panic!("expected integer")
        };
        assert_eq!(Some(42), integer.get(0));
        let ColumnData::Boolean(boolean) = table.columns()[5].data() else {
            panic!("expected Boolean")
        };
        assert_eq!(Some(true), boolean.get(0));
    }

    #[test]
    fn exposes_all_remaining_column_types_without_value_allocations() {
        let types = [
            NodeType::Float,
            NodeType::FormattedFloat,
            NodeType::DictionaryFloat,
            NodeType::UnstructuredArray,
            NodeType::DeprecatedDateString,
        ];
        let tree = flat_tree(&types);
        let schema = schema(&tree, &[0, 1, 2, 3, 4]);
        let resources = resources(
            &[b"1.23456789012345678", b"word"],
            &[],
            &[b"[\x11,\x12,\x13]"],
            &[(4, "pattern")],
        );
        let mut bytes = Vec::new();
        push_f64(&mut bytes, 1.5);
        push_f64(&mut bytes, -12.5);
        bytes.extend_from_slice(&((3_u16 - 1) << 5).to_le_bytes());
        push_u64(&mut bytes, 0);
        push_u64(&mut bytes, 0);
        push_u64(&mut bytes, 3);
        push_i64(&mut bytes, 7);
        push_i64(&mut bytes, 1);
        push_i64(&mut bytes, encoded_float(1234, 4, 2));
        push_i64(&mut bytes, 123);
        push_i64(&mut bytes, 4);

        let table = decode(&bytes, &tree, &schema, 1, &resources).expect("valid all-type table");
        let ColumnData::Float(float) = table.columns()[0].data() else {
            panic!("float")
        };
        assert_eq!(Some(1.5), float.get(0));
        let ColumnData::FormattedFloat(formatted) = table.columns()[1].data() else {
            panic!("formatted float")
        };
        let formatted = formatted.get(0).expect("formatted value");
        assert_eq!((-12.5_f64).to_bits(), formatted.value().to_bits());
        assert_eq!(3, formatted.format().significant_digits());
        assert_eq!(FloatNotation::Decimal, formatted.format().notation());
        let ColumnData::DictionaryFloat(dictionary_float) = table.columns()[2].data() else {
            panic!("dictionary float")
        };
        assert_eq!(
            Some(b"1.23456789012345678".as_slice()),
            dictionary_float.value(0)
        );
        let ColumnData::UnstructuredArray(array) = table.columns()[3].data() else {
            panic!("array")
        };
        let record = array.record(0).expect("array record");
        assert_eq!(3, record.encoded_variables().len());
        assert_eq!(b"[\x11,\x12,\x13]", record.logtype().escaped_value());
        let ColumnData::DeprecatedDateString(deprecated) = table.columns()[4].data() else {
            panic!("deprecated date")
        };
        let deprecated = deprecated.get(0).expect("deprecated date value");
        assert_eq!(123, deprecated.epoch());
        assert_eq!(4, deprecated.pattern_id());
    }

    #[test]
    fn skips_structural_nodes_and_unordered_delimiters() {
        let tree = flat_tree(&[NodeType::Object, NodeType::Integer, NodeType::Boolean]);
        let delimiter = (((NodeType::Object as u32) << 24) | 2).to_le_bytes();
        let schema = schema_from_raw(
            &tree,
            &[delimiter, 1_u32.to_le_bytes(), 2_u32.to_le_bytes()],
            0,
        );
        let resources = resources(&[], &[], &[], &[]);
        let mut bytes = Vec::new();
        push_i64(&mut bytes, 9);
        bytes.push(1);
        let table = decode(&bytes, &tree, &schema, 1, &resources).expect("delimiter schema table");
        assert_eq!(2, table.len());
        assert_eq!(1, table.columns()[0].schema_entry_index());
        assert_eq!(2, table.columns()[1].schema_entry_index());
    }

    #[test]
    fn rejects_truncation_trailing_bytes_and_basic_limits() {
        let resources = resources(&[], &[], &[], &[]);
        assert!(matches!(
            single_column(NodeType::Integer, &[0; 7], 1, &resources),
            Err(ColumnError::TruncatedColumn {
                needed: 8,
                remaining: 7,
                ..
            })
        ));
        assert!(matches!(
            single_column(NodeType::Integer, &[0; 9], 1, &resources),
            Err(ColumnError::TrailingTableBytes { remaining: 1 })
        ));

        let tree = flat_tree(&[NodeType::Integer]);
        let schema = schema(&tree, &[0]);
        let bytes = [0_u8; 8];
        let table_limit = ColumnLimits::new(7, u64::MAX, 1, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_with_limits(&bytes, &tree, &schema, 1, &resources, table_limit),
            Err(ColumnError::TableTooLarge {
                actual: 8,
                limit: 7
            })
        ));
        let message_limit = ColumnLimits::new(u64::MAX, 0, 1, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_with_limits(&bytes, &tree, &schema, 1, &resources, message_limit),
            Err(ColumnError::MessageCountTooLarge {
                actual: 1,
                limit: 0
            })
        ));
        let column_limit = ColumnLimits::new(u64::MAX, 1, 0, u64::MAX, u64::MAX);
        assert!(matches!(
            decode_with_limits(&bytes, &tree, &schema, 1, &resources, column_limit),
            Err(ColumnError::ColumnCountTooLarge {
                actual: 1,
                limit: 0
            })
        ));
    }

    #[test]
    fn rejects_noncanonical_booleans_nonfinite_floats_and_delta_overflow() {
        let resources = resources(&[], &[], &[], &[]);
        assert!(matches!(
            single_column(NodeType::Boolean, &[2], 1, &resources),
            Err(ColumnError::Corrupt {
                message_index: Some(0),
                reason: ColumnCorruption::InvalidBoolean { actual: 2 },
                ..
            })
        ));
        assert!(matches!(
            single_column(
                NodeType::Float,
                &f64::NAN.to_bits().to_le_bytes(),
                1,
                &resources
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::NonFiniteFloat,
                ..
            })
        ));
        let mut deltas = i64::MAX.to_le_bytes().to_vec();
        deltas.extend_from_slice(&1_i64.to_le_bytes());
        assert!(matches!(
            single_column(NodeType::DeltaInteger, &deltas, 2, &resources),
            Err(ColumnError::Corrupt {
                message_index: Some(1),
                reason: ColumnCorruption::DeltaOverflow {
                    previous: i64::MAX,
                    delta: 1
                },
                ..
            })
        ));
    }

    #[test]
    fn rejects_every_invalid_formatted_float_descriptor_class() {
        let resources = resources(&[], &[], &[], &[]);
        let cases = [
            (0x0001, FloatFormatErrorReason::ReservedBits),
            (0x8000, FloatFormatErrorReason::UnknownNotation),
            (0x4000 | 0x3000, FloatFormatErrorReason::UnknownExponentSign),
            (0x1000, FloatFormatErrorReason::DecimalHasExponentMetadata),
            (
                17_u16 << 5,
                FloatFormatErrorReason::SignificantDigitsOutOfRange,
            ),
        ];
        for (raw, expected_reason) in cases {
            let mut bytes = 0_f64.to_bits().to_le_bytes().to_vec();
            bytes.extend_from_slice(&raw.to_le_bytes());
            assert!(matches!(
                single_column(NodeType::FormattedFloat, &bytes, 1, &resources),
                Err(ColumnError::Corrupt {
                    reason: ColumnCorruption::InvalidFloatFormat { reason, .. },
                    ..
                }) if reason == expected_reason
            ));
        }
    }

    #[test]
    fn rejects_invalid_variable_and_timestamp_references() {
        let resources = resources(&[b"not-a-number"], &[], &[], &[(0, "pattern")]);
        assert!(matches!(
            single_column(NodeType::VarString, &1_u64.to_le_bytes(), 1, &resources),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::UnknownVariableDictionaryId { id: 1 },
                ..
            })
        ));
        assert!(matches!(
            single_column(
                NodeType::DictionaryFloat,
                &0_u64.to_le_bytes(),
                1,
                &resources
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::InvalidDictionaryFloat { id: 0 },
                ..
            })
        ));
        let mut timestamp = 0_i64.to_le_bytes().to_vec();
        timestamp.extend_from_slice(&9_u64.to_le_bytes());
        assert!(matches!(
            single_column(NodeType::Timestamp, &timestamp, 1, &resources),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::UnknownTimestampPattern { id: 9 },
                ..
            })
        ));
        let mut deprecated = 0_i64.to_le_bytes().to_vec();
        deprecated.extend_from_slice(&(-1_i64).to_le_bytes());
        assert!(matches!(
            single_column(NodeType::DeprecatedDateString, &deprecated, 1, &resources),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::UnknownTimestampPattern { id: u64::MAX },
                ..
            })
        ));
    }

    fn clp_bytes(descriptor: u64, variables: &[i64]) -> Vec<u8> {
        let mut bytes = descriptor.to_le_bytes().to_vec();
        push_u64(
            &mut bytes,
            u64::try_from(variables.len()).expect("test variable count fits u64"),
        );
        for variable in variables {
            push_i64(&mut bytes, *variable);
        }
        bytes
    }

    #[test]
    fn rejects_invalid_clp_ids_offsets_spans_and_unused_variables() {
        let unknown_logtype = resources(&[], &[b"constant"], &[], &[]);
        assert!(matches!(
            single_column(NodeType::ClpString, &clp_bytes(1, &[]), 1, &unknown_logtype),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::UnknownLogType {
                    section: LogTypeSection::Log,
                    id: 1
                },
                ..
            })
        ));

        let integer_logtype = resources(&[], &[b"\x11"], &[], &[]);
        assert!(matches!(
            single_column(
                NodeType::ClpString,
                &clp_bytes(1_u64 << 24, &[7]),
                1,
                &integer_logtype
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::NonCanonicalEncodedVariableOffset {
                    expected: 0,
                    actual: 1
                },
                ..
            })
        ));
        assert!(matches!(
            single_column(NodeType::ClpString, &clp_bytes(0, &[]), 1, &integer_logtype),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::EncodedVariableSpanOutOfBounds {
                    offset: 0,
                    count: 1,
                    total: 0
                },
                ..
            })
        ));
        assert!(matches!(
            single_column(
                NodeType::ClpString,
                &clp_bytes(0, &[7]),
                1,
                &unknown_logtype
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::EncodedVariableCountMismatch {
                    referenced: 0,
                    declared: 1
                },
                ..
            })
        ));
    }

    #[test]
    fn rejects_invalid_clp_dictionary_and_float_variables() {
        let dictionary_logtype = resources(&[b"exists"], &[b"\x12"], &[], &[]);
        assert!(matches!(
            single_column(
                NodeType::ClpString,
                &clp_bytes(0, &[9]),
                1,
                &dictionary_logtype
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::UnknownDictionaryVariable {
                    encoded_variable_index: 0,
                    id: 9
                },
                ..
            })
        ));

        let float_logtype = resources(&[], &[b"\x13"], &[], &[]);
        let invalid = [
            (
                i64::from_le_bytes(ENCODED_FLOAT_UNUSED_BIT.to_le_bytes()),
                EncodedFloatErrorReason::ReservedBit,
            ),
            (
                encoded_float(1, 1, 2),
                EncodedFloatErrorReason::DecimalPositionExceedsDigits,
            ),
            (
                encoded_float(12, 1, 1),
                EncodedFloatErrorReason::DigitValueExceedsDeclaredDigits,
            ),
        ];
        for (value, expected_reason) in invalid {
            assert!(matches!(
                single_column(
                    NodeType::ClpString,
                    &clp_bytes(0, &[value]),
                    1,
                    &float_logtype
                ),
                Err(ColumnError::Corrupt {
                    reason: ColumnCorruption::InvalidEncodedFloat { reason, .. },
                    ..
                }) if reason == expected_reason
            ));
        }
    }

    #[test]
    fn validates_mixed_clp_variables_in_unescaped_placeholder_order() {
        let mixed_logtype = resources(&[b"exists"], &[b"prefix\x11\\\x12:\x13:\x12"], &[], &[]);
        let valid_float = encoded_float(1234, 4, 2);
        assert!(matches!(
            single_column(
                NodeType::ClpString,
                &clp_bytes(0, &[7, valid_float, 9]),
                1,
                &mixed_logtype
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::UnknownDictionaryVariable {
                    encoded_variable_index: 2,
                    id: 9
                },
                ..
            })
        ));

        let invalid_float = i64::from_le_bytes(ENCODED_FLOAT_UNUSED_BIT.to_le_bytes());
        assert!(matches!(
            single_column(
                NodeType::ClpString,
                &clp_bytes(0, &[7, invalid_float, 0]),
                1,
                &mixed_logtype
            ),
            Err(ColumnError::Corrupt {
                reason: ColumnCorruption::InvalidEncodedFloat {
                    encoded_variable_index: 1,
                    reason: EncodedFloatErrorReason::ReservedBit
                },
                ..
            })
        ));
    }

    #[test]
    fn enforces_clp_encoded_variable_limits() {
        let resources = resources(&[], &[b"constant"], &[], &[]);
        let tree = flat_tree(&[NodeType::ClpString]);
        let schema = schema(&tree, &[0]);
        let bytes = clp_bytes(0, &[1]);
        let limits = ColumnLimits::new(u64::MAX, 1, 1, 0, u64::MAX);
        assert!(matches!(
            decode_with_limits(&bytes, &tree, &schema, 1, &resources, limits),
            Err(ColumnError::EncodedVariableCountTooLarge {
                actual: 1,
                limit: 0,
                ..
            })
        ));
    }
}
