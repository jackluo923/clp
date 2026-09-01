//! Exact default-mode C++ unstructured-array lexemes.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str;

use super::WriterLimits;
use super::primitive::AppendError;
use super::primitive::AppendResource;
use super::primitive::FieldRef;
use super::primitive::ValueRef;
use crate::archive::NodeType;
use crate::archive::SchemaEntry;

const MAX_UNORDERED_CONTAINER_BODY_LEN: u64 = 0x00ff_ffff;
const LINEAR_DUPLICATE_FIELD_LIMIT: usize = 16;

/// One exact JSON array lexeme borrowed for a record append.
///
/// This models the current C++ default (`--structurize-arrays` disabled): the complete raw array,
/// including insignificant whitespace and nested values, is CLP encoded as one
/// `UnstructuredArray` column value. [`crate::writer::OpenArchive::append_record`] validates that
/// the bytes are exactly one UTF-8 JSON array before retaining any archive state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnstructuredArrayRef<'a> {
    raw_json: &'a [u8],
}

impl<'a> UnstructuredArrayRef<'a> {
    /// Borrows an exact raw JSON array lexeme for later validation and append.
    #[must_use]
    pub const fn new(raw_json: &'a [u8]) -> Self {
        Self { raw_json }
    }

    /// Returns the exact raw JSON bytes.
    #[must_use]
    pub const fn raw_json(self) -> &'a [u8] {
        self.raw_json
    }
}

/// JSON syntax class reported for an invalid unstructured-array lexeme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnstructuredArraySyntaxErrorKind {
    /// The lexeme does not begin with an array opening bracket.
    ExpectedArray,
    /// A JSON value was required.
    ExpectedValue,
    /// An object key was required.
    ExpectedObjectKey,
    /// A colon was required after an object key.
    ExpectedColon,
    /// A comma or the current container's closing delimiter was required.
    ExpectedCommaOrEnd,
    /// The lexeme ended before the current token or container was complete.
    UnexpectedEnd,
    /// A `true`, `false`, or `null` token was malformed.
    InvalidLiteral,
    /// A JSON number did not use the required grammar.
    InvalidNumber,
    /// A JSON string contained an unknown escape.
    InvalidStringEscape,
    /// A `\u` escape was malformed or contained an unpaired surrogate.
    InvalidUnicodeEscape,
    /// A JSON string contained an unescaped control byte.
    UnescapedControl,
    /// Bytes followed the root array's closing bracket.
    TrailingCharacters,
}

impl Display for UnstructuredArraySyntaxErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ExpectedArray => "expected a JSON array",
            Self::ExpectedValue => "expected a JSON value",
            Self::ExpectedObjectKey => "expected a JSON object key",
            Self::ExpectedColon => "expected ':' after a JSON object key",
            Self::ExpectedCommaOrEnd => "expected ',' or the container's closing delimiter",
            Self::UnexpectedEnd => "unexpected end of JSON array",
            Self::InvalidLiteral => "invalid JSON literal",
            Self::InvalidNumber => "invalid JSON number",
            Self::InvalidStringEscape => "invalid JSON string escape",
            Self::InvalidUnicodeEscape => "invalid JSON Unicode escape",
            Self::UnescapedControl => "unescaped JSON string control byte",
            Self::TrailingCharacters => "characters follow the JSON array",
        })
    }
}

/// Failure to validate one exact unstructured-array lexeme.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnstructuredArrayError {
    /// The raw bytes are not valid UTF-8, as required by the C++ JSON parser.
    InvalidUtf8 {
        /// Bytes known to be valid before the malformed sequence.
        valid_up_to: usize,
        /// Malformed sequence length, or `None` for a truncated sequence.
        error_len: Option<usize>,
    },
    /// The raw bytes are not exactly one syntactically valid JSON array.
    Syntax {
        /// Zero-based byte offset where validation failed.
        offset: usize,
        /// Syntax failure class.
        kind: UnstructuredArraySyntaxErrorKind,
    },
}

impl Display for UnstructuredArrayError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => write!(
                formatter,
                "array lexeme is not UTF-8 at byte {valid_up_to} (invalid length {error_len:?})"
            ),
            Self::Syntax { offset, kind } => {
                write!(formatter, "{kind} at byte {offset}")
            }
        }
    }
}

impl Error for UnstructuredArrayError {}

#[derive(Clone, Copy, Debug)]
enum Frame {
    Array(ArrayState),
    Object(ObjectState),
}

#[derive(Clone, Copy, Debug)]
enum ArrayState {
    FirstValueOrEnd,
    ValueAfterComma,
    CommaOrEnd,
}

#[derive(Clone, Copy, Debug)]
enum ObjectState {
    FirstKeyOrEnd,
    KeyAfterComma,
    Colon,
    Value,
    CommaOrEnd,
}

/// Reusable validation storage owned by an open writer.
#[derive(Debug, Default)]
pub(super) struct ArrayValidationScratch {
    stack: Vec<Frame>,
}

/// Limits applied while flattening one structured array into the unordered schema region.
///
/// `max_entries` includes container delimiters and structural leaf nodes. `max_nesting_depth`
/// counts from the record's implicit root object, so a root field containing an array has depth
/// two. `max_container_body_entries` is always capped by the 24-bit delimiter wire domain; keeping
/// it explicit also makes that otherwise implicit format boundary independently testable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StructuredArrayPlanLimits {
    entries: u64,
    nesting_depth: u64,
    container_body_entries: u64,
}

impl StructuredArrayPlanLimits {
    pub(super) const fn new(max_entries: u64, max_nesting_depth: u64) -> Self {
        Self {
            entries: max_entries,
            nesting_depth: max_nesting_depth,
            container_body_entries: MAX_UNORDERED_CONTAINER_BODY_LEN,
        }
    }

    #[cfg(test)]
    const fn with_max_container_body_entries(mut self, value: u64) -> Self {
        self.container_body_entries = if value < MAX_UNORDERED_CONTAINER_BODY_LEN {
            value
        } else {
            MAX_UNORDERED_CONTAINER_BODY_LEN
        };
        self
    }
}

/// A physical leaf occurrence in one flattened structured array.
///
/// Repeated node IDs are intentional: heterogeneous array elements reuse schema-tree nodes but
/// occupy distinct table columns in encounter order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct StructuredArrayValue<T> {
    node_id: u32,
    value: T,
}

impl<T: Copy> StructuredArrayValue<T> {
    pub(super) const fn node_id(self) -> u32 {
        self.node_id
    }

    pub(super) const fn value(self) -> T {
        self.value
    }
}

/// One resolved leaf, including whether it has a physical table value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ResolvedStructuredArrayValue<T> {
    pub(super) node_id: u32,
    pub(super) value: Option<T>,
}

impl<T> ResolvedStructuredArrayValue<T> {
    pub(super) const fn structural(node_id: u32) -> Self {
        Self {
            node_id,
            value: None,
        }
    }

    pub(super) const fn physical(node_id: u32, value: T) -> Self {
        Self {
            node_id,
            value: Some(value),
        }
    }
}

/// A fully bounded, borrowed plan for one structured-array field.
#[derive(Debug, PartialEq)]
pub(super) struct StructuredArrayPlan<T> {
    entries: Vec<SchemaEntry>,
    values: Vec<StructuredArrayValue<T>>,
}

impl<T> StructuredArrayPlan<T> {
    #[cfg(test)]
    pub(super) fn entries(&self) -> &[SchemaEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(super) fn values(&self) -> &[StructuredArrayValue<T>] {
        &self.values
    }

    pub(super) fn into_parts(self) -> (Vec<SchemaEntry>, Vec<StructuredArrayValue<T>>) {
        (self.entries, self.values)
    }
}

/// Planner resource used in structured allocation and limit diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StructuredArrayPlanResource {
    SchemaEntries,
    NestingDepth,
    TraversalStack,
    PhysicalValues,
    DuplicateFieldIndex,
}

/// Failure to flatten a structured array without mutating archive state.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum StructuredArrayPlanError<E> {
    Resolve(E),
    DuplicateField {
        object_depth: u64,
        previous_index: usize,
        field_index: usize,
    },
    LimitExceeded {
        resource: StructuredArrayPlanResource,
        actual: u64,
        limit: u64,
    },
    DelimiterBodyTooLong {
        node_type: NodeType,
        actual: u64,
        limit: u64,
    },
    SizeOverflow,
    AllocationFailed {
        resource: StructuredArrayPlanResource,
        requested: usize,
    },
}

/// Staged schema-tree operations required by [`plan_structured_array`].
///
/// Implementations must stage resolutions transactionally: `plan_structured_array` can report a
/// later structural or allocation failure after resolving an earlier node. The archive writer's
/// record plan already has exactly this property; tests use a disposable resolver.
pub(super) trait StructuredArrayNodeResolver<'a> {
    type Error;
    type Value;

    fn resolve_container(
        &mut self,
        parent: u32,
        node_type: NodeType,
        key: &'a [u8],
    ) -> Result<u32, Self::Error>;

    fn resolve_leaf(
        &mut self,
        parent: u32,
        key: &'a [u8],
        value: ValueRef<'a>,
    ) -> Result<ResolvedStructuredArrayValue<Self::Value>, Self::Error>;
}

#[derive(Clone, Copy, Debug)]
enum StructuredArrayFrame<'a> {
    Array {
        node_id: u32,
        values: &'a [ValueRef<'a>],
        next: usize,
        depth: u64,
        delimiter_index: usize,
        body_start: usize,
    },
    Object {
        node_id: u32,
        fields: &'a [FieldRef<'a>],
        next: usize,
        depth: u64,
        delimiter: Option<(usize, usize)>,
    },
}

impl StructuredArrayFrame<'_> {
    const fn node_type(self) -> NodeType {
        match self {
            Self::Array { .. } => NodeType::StructuredArray,
            Self::Object { .. } => NodeType::Object,
        }
    }

    const fn delimiter(self) -> Option<(usize, usize)> {
        match self {
            Self::Array {
                delimiter_index,
                body_start,
                ..
            } => Some((delimiter_index, body_start)),
            Self::Object { delimiter, .. } => delimiter,
        }
    }
}

#[derive(Clone, Copy)]
struct PendingStructuredValue<'a> {
    parent: u32,
    key: &'a [u8],
    value: ValueRef<'a>,
    parent_depth: u64,
    parent_is_array: bool,
}

/// Flattens one already-interned structured-array root using the canonical C++ encounter order.
///
/// Every non-empty array and every non-empty object directly contained by an array contributes a
/// delimiter followed by its flattened body. Named objects contained by other objects are schema
/// transparent, matching the C++ format. An empty container contributes its bare schema-tree node
/// ID. The returned leaves omit nulls and structural values, and intentionally retain repeated
/// node IDs as separate physical columns.
pub(super) fn plan_structured_array<'a, R>(
    values: &'a [ValueRef<'a>],
    root_node_id: u32,
    root_depth: u64,
    limits: StructuredArrayPlanLimits,
    resolver: &mut R,
) -> Result<StructuredArrayPlan<R::Value>, StructuredArrayPlanError<R::Error>>
where
    R: StructuredArrayNodeResolver<'a>, {
    check_structured_limit(
        StructuredArrayPlanResource::NestingDepth,
        root_depth,
        limits.nesting_depth,
    )?;
    let mut plan = StructuredArrayPlan {
        entries: Vec::new(),
        values: Vec::new(),
    };
    let mut stack = Vec::new();
    let mut duplicate_field_indexes = ahash::AHashMap::new();
    enter_array(
        &mut plan,
        &mut stack,
        root_node_id,
        values,
        root_depth,
        limits,
    )?;

    while !stack.is_empty() {
        let pending = next_structured_value(
            stack
                .last_mut()
                .expect("nonempty traversal stack has a current container"),
        );
        let Some(pending) = pending else {
            close_structured_container(
                &mut plan.entries,
                stack.pop().expect("open container"),
                limits,
            )?;
            continue;
        };
        visit_structured_value(
            &mut plan,
            &mut stack,
            &mut duplicate_field_indexes,
            pending,
            limits,
            resolver,
        )?;
    }
    Ok(plan)
}

fn next_structured_value<'a>(
    frame: &mut StructuredArrayFrame<'a>,
) -> Option<PendingStructuredValue<'a>> {
    match frame {
        StructuredArrayFrame::Array {
            node_id,
            values,
            next,
            depth,
            ..
        } => {
            let value = values.get(*next).copied()?;
            *next += 1;
            Some(PendingStructuredValue {
                parent: *node_id,
                key: b"",
                value,
                parent_depth: *depth,
                parent_is_array: true,
            })
        }
        StructuredArrayFrame::Object {
            node_id,
            fields,
            next,
            depth,
            ..
        } => {
            let field = fields.get(*next).copied()?;
            *next += 1;
            Some(PendingStructuredValue {
                parent: *node_id,
                key: field.key(),
                value: field.value(),
                parent_depth: *depth,
                parent_is_array: false,
            })
        }
    }
}

fn visit_structured_value<'a, R>(
    plan: &mut StructuredArrayPlan<R::Value>,
    stack: &mut Vec<StructuredArrayFrame<'a>>,
    duplicate_field_indexes: &mut ahash::AHashMap<&'a [u8], usize>,
    pending: PendingStructuredValue<'a>,
    limits: StructuredArrayPlanLimits,
    resolver: &mut R,
) -> Result<(), StructuredArrayPlanError<R::Error>>
where
    R: StructuredArrayNodeResolver<'a>, {
    match pending.value {
        ValueRef::Array(values) => {
            let depth = child_container_depth(pending.parent_depth, limits)?;
            let node_id = resolver
                .resolve_container(pending.parent, NodeType::StructuredArray, pending.key)
                .map_err(StructuredArrayPlanError::Resolve)?;
            enter_array(plan, stack, node_id, values, depth, limits)
        }
        ValueRef::Object(fields) => {
            let depth = child_container_depth(pending.parent_depth, limits)?;
            validate_structured_object_fields(fields, depth, duplicate_field_indexes)?;
            let node_id = resolver
                .resolve_container(pending.parent, NodeType::Object, pending.key)
                .map_err(StructuredArrayPlanError::Resolve)?;
            enter_object(
                plan,
                stack,
                node_id,
                fields,
                depth,
                pending.parent_is_array,
                limits,
            )
        }
        value => {
            let leaf = resolver
                .resolve_leaf(pending.parent, pending.key, value)
                .map_err(StructuredArrayPlanError::Resolve)?;
            push_structured_entry(&mut plan.entries, SchemaEntry::Node(leaf.node_id), limits)?;
            if let Some(value) = leaf.value {
                plan.values.try_reserve(1).map_err(|_| {
                    StructuredArrayPlanError::AllocationFailed {
                        resource: StructuredArrayPlanResource::PhysicalValues,
                        requested: 1,
                    }
                })?;
                plan.values.push(StructuredArrayValue {
                    node_id: leaf.node_id,
                    value,
                });
            }
            Ok(())
        }
    }
}

fn enter_array<'a, T, E>(
    plan: &mut StructuredArrayPlan<T>,
    stack: &mut Vec<StructuredArrayFrame<'a>>,
    node_id: u32,
    values: &'a [ValueRef<'a>],
    depth: u64,
    limits: StructuredArrayPlanLimits,
) -> Result<(), StructuredArrayPlanError<E>> {
    if values.is_empty() {
        return push_structured_entry(&mut plan.entries, SchemaEntry::Node(node_id), limits);
    }
    let (delimiter_index, body_start) =
        begin_structured_container(&mut plan.entries, NodeType::StructuredArray, limits)?;
    reserve_structured_frame(stack)?;
    stack.push(StructuredArrayFrame::Array {
        node_id,
        values,
        next: 0,
        depth,
        delimiter_index,
        body_start,
    });
    Ok(())
}

fn enter_object<'a, T, E>(
    plan: &mut StructuredArrayPlan<T>,
    stack: &mut Vec<StructuredArrayFrame<'a>>,
    node_id: u32,
    fields: &'a [FieldRef<'a>],
    depth: u64,
    needs_delimiter: bool,
    limits: StructuredArrayPlanLimits,
) -> Result<(), StructuredArrayPlanError<E>> {
    if fields.is_empty() {
        return push_structured_entry(&mut plan.entries, SchemaEntry::Node(node_id), limits);
    }
    let delimiter_bounds = if needs_delimiter {
        Some(begin_structured_container(
            &mut plan.entries,
            NodeType::Object,
            limits,
        )?)
    } else {
        None
    };
    reserve_structured_frame(stack)?;
    stack.push(StructuredArrayFrame::Object {
        node_id,
        fields,
        next: 0,
        depth,
        delimiter: delimiter_bounds,
    });
    Ok(())
}

fn begin_structured_container<E>(
    entries: &mut Vec<SchemaEntry>,
    node_type: NodeType,
    limits: StructuredArrayPlanLimits,
) -> Result<(usize, usize), StructuredArrayPlanError<E>> {
    let delimiter_index = entries.len();
    push_structured_entry(
        entries,
        SchemaEntry::UnorderedContainer {
            node_type,
            body_len: 0,
        },
        limits,
    )?;
    Ok((delimiter_index, entries.len()))
}

fn close_structured_container<E>(
    entries: &mut [SchemaEntry],
    frame: StructuredArrayFrame<'_>,
    limits: StructuredArrayPlanLimits,
) -> Result<(), StructuredArrayPlanError<E>> {
    let Some((delimiter_index, body_start)) = frame.delimiter() else {
        return Ok(());
    };
    let body_len = entries
        .len()
        .checked_sub(body_start)
        .ok_or(StructuredArrayPlanError::SizeOverflow)?;
    let body_len = u64::try_from(body_len).map_err(|_| StructuredArrayPlanError::SizeOverflow)?;
    let body_limit = limits
        .container_body_entries
        .min(MAX_UNORDERED_CONTAINER_BODY_LEN);
    if body_len > body_limit {
        return Err(StructuredArrayPlanError::DelimiterBodyTooLong {
            node_type: frame.node_type(),
            actual: body_len,
            limit: body_limit,
        });
    }
    entries[delimiter_index] = SchemaEntry::UnorderedContainer {
        node_type: frame.node_type(),
        body_len: u32::try_from(body_len).map_err(|_| StructuredArrayPlanError::SizeOverflow)?,
    };
    Ok(())
}

fn push_structured_entry<E>(
    entries: &mut Vec<SchemaEntry>,
    entry: SchemaEntry,
    limits: StructuredArrayPlanLimits,
) -> Result<(), StructuredArrayPlanError<E>> {
    let actual = u64::try_from(entries.len())
        .map_err(|_| StructuredArrayPlanError::SizeOverflow)?
        .checked_add(1)
        .ok_or(StructuredArrayPlanError::SizeOverflow)?;
    check_structured_limit(
        StructuredArrayPlanResource::SchemaEntries,
        actual,
        limits.entries,
    )?;
    entries
        .try_reserve(1)
        .map_err(|_| StructuredArrayPlanError::AllocationFailed {
            resource: StructuredArrayPlanResource::SchemaEntries,
            requested: 1,
        })?;
    entries.push(entry);
    Ok(())
}

fn reserve_structured_frame<E>(
    stack: &mut Vec<StructuredArrayFrame<'_>>,
) -> Result<(), StructuredArrayPlanError<E>> {
    stack
        .try_reserve(1)
        .map_err(|_| StructuredArrayPlanError::AllocationFailed {
            resource: StructuredArrayPlanResource::TraversalStack,
            requested: 1,
        })
}

fn child_container_depth<E>(
    parent_depth: u64,
    limits: StructuredArrayPlanLimits,
) -> Result<u64, StructuredArrayPlanError<E>> {
    let depth = parent_depth
        .checked_add(1)
        .ok_or(StructuredArrayPlanError::SizeOverflow)?;
    check_structured_limit(
        StructuredArrayPlanResource::NestingDepth,
        depth,
        limits.nesting_depth,
    )?;
    Ok(depth)
}

fn validate_structured_object_fields<'a, E>(
    fields: &'a [FieldRef<'a>],
    object_depth: u64,
    indexes: &mut ahash::AHashMap<&'a [u8], usize>,
) -> Result<(), StructuredArrayPlanError<E>> {
    if fields.len() <= LINEAR_DUPLICATE_FIELD_LIMIT {
        for (field_index, field) in fields.iter().enumerate() {
            if let Some(previous_index) = fields[..field_index]
                .iter()
                .position(|previous| previous.key() == field.key())
            {
                return Err(StructuredArrayPlanError::DuplicateField {
                    object_depth,
                    previous_index,
                    field_index,
                });
            }
        }
        return Ok(());
    }

    indexes.clear();
    indexes
        .try_reserve(fields.len())
        .map_err(|_| StructuredArrayPlanError::AllocationFailed {
            resource: StructuredArrayPlanResource::DuplicateFieldIndex,
            requested: fields.len(),
        })?;
    for (field_index, field) in fields.iter().enumerate() {
        if let Some(previous_index) = indexes.insert(field.key(), field_index) {
            return Err(StructuredArrayPlanError::DuplicateField {
                object_depth,
                previous_index,
                field_index,
            });
        }
    }
    Ok(())
}

const fn check_structured_limit<E>(
    resource: StructuredArrayPlanResource,
    actual: u64,
    limit: u64,
) -> Result<(), StructuredArrayPlanError<E>> {
    if actual > limit {
        Err(StructuredArrayPlanError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

pub(super) fn validate(
    value: UnstructuredArrayRef<'_>,
    limits: WriterLimits,
    scratch: &mut ArrayValidationScratch,
    node_id: u32,
) -> Result<(), AppendError> {
    let raw = value.raw_json();
    let raw_len = u64::try_from(raw.len()).map_err(|_| AppendError::SizeOverflow)?;
    check_limit(
        AppendResource::UnstructuredArrayLexemeBytes,
        raw_len,
        limits.max_unstructured_array_lexeme_bytes(),
    )?;
    if is_flat_ascii_string_array(raw) {
        return Ok(());
    }
    if let Err(error) = str::from_utf8(raw) {
        return Err(invalid(
            node_id,
            UnstructuredArrayError::InvalidUtf8 {
                valid_up_to: error.valid_up_to(),
                error_len: error.error_len(),
            },
        ));
    }

    scratch.stack.clear();
    let result = Parser {
        source: raw,
        offset: 0,
        max_depth: limits.max_unstructured_array_nesting_depth(),
        stack: &mut scratch.stack,
    }
    .parse();
    scratch.stack.clear();
    result.map_err(|failure| match failure {
        Failure::Syntax { offset, kind } => {
            invalid(node_id, UnstructuredArrayError::Syntax { offset, kind })
        }
        Failure::Limit { actual, limit } => AppendError::LimitExceeded {
            resource: AppendResource::UnstructuredArrayNestingDepth,
            actual,
            limit,
        },
        Failure::Allocation { requested } => AppendError::AllocationFailed {
            resource: AppendResource::ArrayValidationStack,
            requested,
        },
        Failure::SizeOverflow => AppendError::SizeOverflow,
    })
}

/// Recognizes the common tags-style array without allocating or running the general JSON state
/// machine. Any uncertain input falls through to the full validator, which retains its exact
/// structural diagnostics.
fn is_flat_ascii_string_array(raw: &[u8]) -> bool {
    if raw.first() != Some(&b'[') {
        return false;
    }
    let mut offset = 1_usize;
    skip_ascii_whitespace(raw, &mut offset);
    if consume_ascii(raw, &mut offset, b']') {
        return offset == raw.len();
    }

    loop {
        if !consume_ascii(raw, &mut offset, b'"') {
            return false;
        }
        loop {
            let Some(byte) = raw.get(offset).copied() else {
                return false;
            };
            match byte {
                b'"' => {
                    offset += 1;
                    break;
                }
                b'\\' | 0x00..=0x1f | 0x80..=u8::MAX => return false,
                _ => offset += 1,
            }
        }
        skip_ascii_whitespace(raw, &mut offset);
        if consume_ascii(raw, &mut offset, b']') {
            return offset == raw.len();
        }
        if !consume_ascii(raw, &mut offset, b',') {
            return false;
        }
        skip_ascii_whitespace(raw, &mut offset);
    }
}

fn skip_ascii_whitespace(raw: &[u8], offset: &mut usize) {
    while raw
        .get(*offset)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        *offset += 1;
    }
}

fn consume_ascii(raw: &[u8], offset: &mut usize, expected: u8) -> bool {
    if raw.get(*offset) != Some(&expected) {
        return false;
    }
    *offset += 1;
    true
}

struct Parser<'input, 'scratch> {
    source: &'input [u8],
    offset: usize,
    max_depth: u64,
    stack: &'scratch mut Vec<Frame>,
}

impl Parser<'_, '_> {
    fn parse(mut self) -> Result<(), Failure> {
        if self.peek() != Some(b'[') {
            return Err(self.syntax(UnstructuredArraySyntaxErrorKind::ExpectedArray));
        }
        self.open(Frame::Array(ArrayState::FirstValueOrEnd))?;
        loop {
            let Some(frame) = self.stack.last().copied() else {
                return if self.offset == self.source.len() {
                    Ok(())
                } else {
                    Err(self.syntax(UnstructuredArraySyntaxErrorKind::TrailingCharacters))
                };
            };
            match frame {
                Frame::Array(ArrayState::FirstValueOrEnd) => {
                    self.skip_whitespace();
                    if self.consume_if(b']') {
                        self.stack.pop();
                    } else {
                        self.replace_top(Frame::Array(ArrayState::CommaOrEnd));
                        self.parse_value()?;
                    }
                }
                Frame::Array(ArrayState::ValueAfterComma) => {
                    self.skip_whitespace();
                    self.replace_top(Frame::Array(ArrayState::CommaOrEnd));
                    self.parse_value()?;
                }
                Frame::Array(ArrayState::CommaOrEnd) => {
                    self.skip_whitespace();
                    if self.consume_if(b',') {
                        self.replace_top(Frame::Array(ArrayState::ValueAfterComma));
                    } else if self.consume_if(b']') {
                        self.stack.pop();
                    } else {
                        return Err(
                            self.syntax(UnstructuredArraySyntaxErrorKind::ExpectedCommaOrEnd)
                        );
                    }
                }
                Frame::Object(ObjectState::FirstKeyOrEnd) => {
                    self.skip_whitespace();
                    if self.consume_if(b'}') {
                        self.stack.pop();
                    } else {
                        self.parse_object_key()?;
                        self.replace_top(Frame::Object(ObjectState::Colon));
                    }
                }
                Frame::Object(ObjectState::KeyAfterComma) => {
                    self.skip_whitespace();
                    self.parse_object_key()?;
                    self.replace_top(Frame::Object(ObjectState::Colon));
                }
                Frame::Object(ObjectState::Colon) => {
                    self.skip_whitespace();
                    if !self.consume_if(b':') {
                        return Err(self.syntax(UnstructuredArraySyntaxErrorKind::ExpectedColon));
                    }
                    self.replace_top(Frame::Object(ObjectState::Value));
                }
                Frame::Object(ObjectState::Value) => {
                    self.replace_top(Frame::Object(ObjectState::CommaOrEnd));
                    self.parse_value()?;
                }
                Frame::Object(ObjectState::CommaOrEnd) => {
                    self.skip_whitespace();
                    if self.consume_if(b',') {
                        self.replace_top(Frame::Object(ObjectState::KeyAfterComma));
                    } else if self.consume_if(b'}') {
                        self.stack.pop();
                    } else {
                        return Err(
                            self.syntax(UnstructuredArraySyntaxErrorKind::ExpectedCommaOrEnd)
                        );
                    }
                }
            }
        }
    }

    fn parse_value(&mut self) -> Result<(), Failure> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.open(Frame::Object(ObjectState::FirstKeyOrEnd)),
            Some(b'[') => self.open(Frame::Array(ArrayState::FirstValueOrEnd)),
            Some(b'"') => self.parse_string(),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(_) => Err(self.syntax(UnstructuredArraySyntaxErrorKind::ExpectedValue)),
            None => Err(self.syntax(UnstructuredArraySyntaxErrorKind::UnexpectedEnd)),
        }
    }

    fn parse_object_key(&mut self) -> Result<(), Failure> {
        if self.peek() != Some(b'"') {
            return Err(self.syntax(UnstructuredArraySyntaxErrorKind::ExpectedObjectKey));
        }
        self.parse_string()
    }

    fn parse_string(&mut self) -> Result<(), Failure> {
        self.offset = self.offset.checked_add(1).ok_or(Failure::SizeOverflow)?;
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.syntax(UnstructuredArraySyntaxErrorKind::UnexpectedEnd));
            };
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(());
                }
                b'\\' => self.parse_escape()?,
                0x00..=0x1f => {
                    return Err(self.syntax(UnstructuredArraySyntaxErrorKind::UnescapedControl));
                }
                _ => self.offset += 1,
            }
        }
    }

    fn parse_escape(&mut self) -> Result<(), Failure> {
        let escape_offset = self.offset;
        self.offset = self.offset.checked_add(1).ok_or(Failure::SizeOverflow)?;
        let Some(escaped) = self.peek() else {
            return Err(self.syntax(UnstructuredArraySyntaxErrorKind::UnexpectedEnd));
        };
        if matches!(
            escaped,
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
        ) {
            self.offset += 1;
            return Ok(());
        }
        if b'u' != escaped {
            return Err(Failure::Syntax {
                offset: escape_offset,
                kind: UnstructuredArraySyntaxErrorKind::InvalidStringEscape,
            });
        }
        self.offset += 1;
        let first = self.parse_hex_quad(escape_offset)?;
        if (0xd800..=0xdbff).contains(&first) {
            let pair_end = self.offset.checked_add(2).ok_or(Failure::SizeOverflow)?;
            if self.source.get(self.offset..pair_end) != Some(br"\u") {
                return Err(Failure::Syntax {
                    offset: escape_offset,
                    kind: UnstructuredArraySyntaxErrorKind::InvalidUnicodeEscape,
                });
            }
            self.offset = pair_end;
            let second = self.parse_hex_quad(escape_offset)?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(Failure::Syntax {
                    offset: escape_offset,
                    kind: UnstructuredArraySyntaxErrorKind::InvalidUnicodeEscape,
                });
            }
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(Failure::Syntax {
                offset: escape_offset,
                kind: UnstructuredArraySyntaxErrorKind::InvalidUnicodeEscape,
            });
        }
        Ok(())
    }

    fn parse_hex_quad(&mut self, escape_offset: usize) -> Result<u16, Failure> {
        let end = self.offset.checked_add(4).ok_or(Failure::SizeOverflow)?;
        let Some(digits) = self.source.get(self.offset..end) else {
            return Err(self.syntax(UnstructuredArraySyntaxErrorKind::UnexpectedEnd));
        };
        let mut value = 0_u16;
        for digit in digits {
            let Some(nibble) = hex_nibble(*digit) else {
                return Err(Failure::Syntax {
                    offset: escape_offset,
                    kind: UnstructuredArraySyntaxErrorKind::InvalidUnicodeEscape,
                });
            };
            value = (value << 4) | u16::from(nibble);
        }
        self.offset = end;
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<(), Failure> {
        self.consume_if(b'-');
        match self.peek() {
            Some(b'0') => {
                self.offset += 1;
                if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(self.syntax(UnstructuredArraySyntaxErrorKind::InvalidNumber));
                }
            }
            Some(b'1'..=b'9') => {
                self.offset += 1;
                self.consume_ascii_digits();
            }
            Some(_) => {
                return Err(self.syntax(UnstructuredArraySyntaxErrorKind::InvalidNumber));
            }
            None => return Err(self.syntax(UnstructuredArraySyntaxErrorKind::UnexpectedEnd)),
        }
        if self.consume_if(b'.') {
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.syntax(if self.peek().is_none() {
                    UnstructuredArraySyntaxErrorKind::UnexpectedEnd
                } else {
                    UnstructuredArraySyntaxErrorKind::InvalidNumber
                }));
            }
            self.consume_ascii_digits();
        }
        if self.peek().is_some_and(|byte| matches!(byte, b'e' | b'E')) {
            self.offset += 1;
            if self.peek().is_some_and(|byte| matches!(byte, b'+' | b'-')) {
                self.offset += 1;
            }
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.syntax(if self.peek().is_none() {
                    UnstructuredArraySyntaxErrorKind::UnexpectedEnd
                } else {
                    UnstructuredArraySyntaxErrorKind::InvalidNumber
                }));
            }
            self.consume_ascii_digits();
        }
        Ok(())
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), Failure> {
        let end = self
            .offset
            .checked_add(literal.len())
            .ok_or(Failure::SizeOverflow)?;
        if self.source.get(self.offset..end) != Some(literal) {
            let kind = if end > self.source.len()
                && self
                    .source
                    .get(self.offset..)
                    .is_some_and(|suffix| literal.starts_with(suffix))
            {
                UnstructuredArraySyntaxErrorKind::UnexpectedEnd
            } else {
                UnstructuredArraySyntaxErrorKind::InvalidLiteral
            };
            return Err(self.syntax(kind));
        }
        self.offset = end;
        Ok(())
    }

    fn open(&mut self, frame: Frame) -> Result<(), Failure> {
        let depth = u64::try_from(self.stack.len())
            .map_err(|_| Failure::SizeOverflow)?
            .checked_add(1)
            .ok_or(Failure::SizeOverflow)?;
        if depth > self.max_depth {
            return Err(Failure::Limit {
                actual: depth,
                limit: self.max_depth,
            });
        }
        self.stack
            .try_reserve(1)
            .map_err(|_| Failure::Allocation { requested: 1 })?;
        self.offset = self.offset.checked_add(1).ok_or(Failure::SizeOverflow)?;
        self.stack.push(frame);
        Ok(())
    }

    fn replace_top(&mut self, frame: Frame) {
        *self
            .stack
            .last_mut()
            .expect("parser state transition requires an open container") = frame;
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.offset += 1;
        }
    }

    fn consume_ascii_digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek() != Some(expected) {
            return false;
        }
        self.offset += 1;
        true
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.offset).copied()
    }

    const fn syntax(&self, kind: UnstructuredArraySyntaxErrorKind) -> Failure {
        Failure::Syntax {
            offset: self.offset,
            kind,
        }
    }
}

enum Failure {
    Syntax {
        offset: usize,
        kind: UnstructuredArraySyntaxErrorKind,
    },
    Limit {
        actual: u64,
        limit: u64,
    },
    Allocation {
        requested: usize,
    },
    SizeOverflow,
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

const fn invalid(node_id: u32, reason: UnstructuredArrayError) -> AppendError {
    AppendError::UnstructuredArray { node_id, reason }
}

const fn check_limit(resource: AppendResource, actual: u64, limit: u64) -> Result<(), AppendError> {
    if actual > limit {
        Err(AppendError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct TestNode {
        parent: Option<u32>,
        node_type: NodeType,
        key: Vec<u8>,
    }

    #[derive(Debug, Default)]
    struct TestNodeResolver {
        nodes: Vec<TestNode>,
    }

    impl TestNodeResolver {
        fn with_record_root() -> Self {
            Self {
                nodes: vec![TestNode {
                    parent: None,
                    node_type: NodeType::Object,
                    key: Vec::new(),
                }],
            }
        }

        fn resolve(&mut self, parent: u32, node_type: NodeType, key: &[u8]) -> u32 {
            if let Some(index) = self.nodes.iter().position(|node| {
                node.parent == Some(parent) && node.node_type == node_type && node.key == key
            }) {
                return u32::try_from(index).expect("test schema node ID");
            }
            let id = u32::try_from(self.nodes.len()).expect("test schema node ID");
            self.nodes.push(TestNode {
                parent: Some(parent),
                node_type,
                key: key.to_vec(),
            });
            id
        }
    }

    impl<'a> StructuredArrayNodeResolver<'a> for TestNodeResolver {
        type Error = ();
        type Value = ValueRef<'a>;

        fn resolve_container(
            &mut self,
            parent: u32,
            node_type: NodeType,
            key: &'a [u8],
        ) -> Result<u32, Self::Error> {
            Ok(self.resolve(parent, node_type, key))
        }

        fn resolve_leaf(
            &mut self,
            parent: u32,
            key: &'a [u8],
            value: ValueRef<'a>,
        ) -> Result<ResolvedStructuredArrayValue<Self::Value>, Self::Error> {
            let node_type = match value {
                ValueRef::Null => NodeType::Null,
                ValueRef::I64(_) => NodeType::Integer,
                ValueRef::F64(_) => NodeType::Float,
                ValueRef::RetainedFloat(_) => NodeType::FormattedFloat,
                ValueRef::Bool(_) => NodeType::Boolean,
                ValueRef::String(value) if value.contains(&b' ') => NodeType::ClpString,
                ValueRef::String(_) => NodeType::VarString,
                ValueRef::UnstructuredArray(_) => NodeType::UnstructuredArray,
                ValueRef::Timestamp(_) | ValueRef::PrevalidatedTimestamp(_) => NodeType::Timestamp,
                ValueRef::Object(_) | ValueRef::Array(_) => {
                    unreachable!("containers use resolve_container")
                }
            };
            let node_id = self.resolve(parent, node_type, key);
            Ok(if matches!(value, ValueRef::Null) {
                ResolvedStructuredArrayValue::structural(node_id)
            } else {
                ResolvedStructuredArrayValue::physical(node_id, value)
            })
        }
    }

    const fn node(node_id: u32) -> SchemaEntry {
        SchemaEntry::Node(node_id)
    }

    const fn delimiter(node_type: NodeType, body_len: u32) -> SchemaEntry {
        SchemaEntry::UnorderedContainer {
            node_type,
            body_len,
        }
    }

    fn assert_structured_plan(
        plan: &StructuredArrayPlan<ValueRef<'_>>,
        expected_entries: &[SchemaEntry],
        expected_value_nodes: &[u32],
    ) {
        assert_eq!(expected_entries, plan.entries());
        assert_eq!(
            expected_value_nodes,
            plan.values()
                .iter()
                .map(|value| value.node_id())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn plans_exact_cpp_structured_array_oracle_layouts() {
        const LIMITS: StructuredArrayPlanLimits = StructuredArrayPlanLimits::new(256, 32);
        let mut resolver = TestNodeResolver::with_record_root();
        assert_eq!(1, resolver.resolve(0, NodeType::Integer, b"id"));

        let empty_values = [];
        let object_x = [FieldRef::new(b"x", ValueRef::I64(1))];
        let object_y = [FieldRef::new(b"y", ValueRef::I64(2))];
        let row_zero = [
            ValueRef::I64(1),
            ValueRef::I64(2),
            ValueRef::Null,
            ValueRef::Object(&object_x),
            ValueRef::Object(&object_y),
            ValueRef::Array(&empty_values),
        ];
        let items_node = resolver.resolve(0, NodeType::StructuredArray, b"items");
        let plan = plan_structured_array(&row_zero, items_node, 2, LIMITS, &mut resolver)
            .expect("plan oracle row zero");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 8),
                node(3),
                node(3),
                node(4),
                delimiter(NodeType::Object, 1),
                node(6),
                delimiter(NodeType::Object, 1),
                node(7),
                node(8),
            ],
            &[3, 3, 6, 7],
        );

        let first_xy = [
            FieldRef::new(b"x", ValueRef::I64(1)),
            FieldRef::new(b"y", ValueRef::I64(0)),
        ];
        let second_xy = [
            FieldRef::new(b"x", ValueRef::I64(0)),
            FieldRef::new(b"y", ValueRef::I64(2)),
        ];
        let row_one = [ValueRef::Object(&first_xy), ValueRef::Object(&second_xy)];
        let plan = plan_structured_array(&row_one, items_node, 2, LIMITS, &mut resolver)
            .expect("plan oracle row one");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 6),
                delimiter(NodeType::Object, 2),
                node(6),
                node(7),
                delimiter(NodeType::Object, 2),
                node(6),
                node(7),
            ],
            &[6, 7, 6, 7],
        );

        let x_fields = [FieldRef::new(b"x", ValueRef::I64(3))];
        let y_fields = [FieldRef::new(b"y", ValueRef::I64(4))];
        let first_nested_array = [ValueRef::Object(&x_fields)];
        let second_nested_array = [ValueRef::Object(&y_fields)];
        let row_two = [
            ValueRef::Array(&first_nested_array),
            ValueRef::Array(&second_nested_array),
        ];
        let plan = plan_structured_array(&row_two, items_node, 2, LIMITS, &mut resolver)
            .expect("plan oracle row two");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 6),
                delimiter(NodeType::StructuredArray, 2),
                delimiter(NodeType::Object, 1),
                node(10),
                delimiter(NodeType::StructuredArray, 2),
                delimiter(NodeType::Object, 1),
                node(11),
            ],
            &[10, 11],
        );

        let plan = plan_structured_array(&[], items_node, 2, LIMITS, &mut resolver)
            .expect("plan empty oracle row");
        assert_structured_plan(&plan, &[node(2)], &[]);

        let row_four = [ValueRef::Null];
        let plan = plan_structured_array(&row_four, items_node, 2, LIMITS, &mut resolver)
            .expect("plan null oracle row");
        assert_structured_plan(
            &plan,
            &[delimiter(NodeType::StructuredArray, 1), node(4)],
            &[],
        );

        let empty_object = [];
        let null_x = [FieldRef::new(b"x", ValueRef::Null)];
        let row_five = [ValueRef::Object(&empty_object), ValueRef::Object(&null_x)];
        let plan = plan_structured_array(&row_five, items_node, 2, LIMITS, &mut resolver)
            .expect("plan empty-object oracle row");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 3),
                node(5),
                delimiter(NodeType::Object, 1),
                node(12),
            ],
            &[],
        );

        let obj_node = resolver.resolve(0, NodeType::Object, b"obj");
        let nested_items_node = resolver.resolve(obj_node, NodeType::StructuredArray, b"items");
        let nested_object_x = [FieldRef::new(b"x", ValueRef::I64(5))];
        let row_six = [ValueRef::Object(&nested_object_x)];
        let plan = plan_structured_array(&row_six, nested_items_node, 3, LIMITS, &mut resolver)
            .expect("plan nested ordinary-object oracle row");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 2),
                delimiter(NodeType::Object, 1),
                node(16),
            ],
            &[16],
        );

        let yes = [FieldRef::new(b"z", ValueRef::String(b"yes"))];
        let no = [FieldRef::new(b"z", ValueRef::String(b"no"))];
        let first_nested = [FieldRef::new(b"nested", ValueRef::Object(&yes))];
        let second_nested = [FieldRef::new(b"nested", ValueRef::Object(&no))];
        let row_seven = [
            ValueRef::Object(&first_nested),
            ValueRef::Object(&second_nested),
        ];
        let plan = plan_structured_array(&row_seven, items_node, 2, LIMITS, &mut resolver)
            .expect("plan nested-object oracle row");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 4),
                delimiter(NodeType::Object, 1),
                node(18),
                delimiter(NodeType::Object, 1),
                node(18),
            ],
            &[18, 18],
        );

        let deep_z = [FieldRef::new(b"z", ValueRef::String(b"deep"))];
        let deep_object = [ValueRef::Object(&deep_z)];
        let nested_array_field = [FieldRef::new(b"nested", ValueRef::Array(&deep_object))];
        let row_eight = [ValueRef::Object(&nested_array_field)];
        let plan = plan_structured_array(&row_eight, items_node, 2, LIMITS, &mut resolver)
            .expect("plan nested-array oracle row");
        assert_structured_plan(
            &plan,
            &[
                delimiter(NodeType::StructuredArray, 4),
                delimiter(NodeType::Object, 3),
                delimiter(NodeType::StructuredArray, 2),
                delimiter(NodeType::Object, 1),
                node(21),
            ],
            &[21],
        );

        assert_eq!(
            &[
                TestNode {
                    parent: None,
                    node_type: NodeType::Object,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(0),
                    node_type: NodeType::Integer,
                    key: b"id".to_vec(),
                },
                TestNode {
                    parent: Some(0),
                    node_type: NodeType::StructuredArray,
                    key: b"items".to_vec(),
                },
                TestNode {
                    parent: Some(2),
                    node_type: NodeType::Integer,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(2),
                    node_type: NodeType::Null,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(2),
                    node_type: NodeType::Object,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(5),
                    node_type: NodeType::Integer,
                    key: b"x".to_vec(),
                },
                TestNode {
                    parent: Some(5),
                    node_type: NodeType::Integer,
                    key: b"y".to_vec(),
                },
                TestNode {
                    parent: Some(2),
                    node_type: NodeType::StructuredArray,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(8),
                    node_type: NodeType::Object,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(9),
                    node_type: NodeType::Integer,
                    key: b"x".to_vec(),
                },
                TestNode {
                    parent: Some(9),
                    node_type: NodeType::Integer,
                    key: b"y".to_vec(),
                },
                TestNode {
                    parent: Some(5),
                    node_type: NodeType::Null,
                    key: b"x".to_vec(),
                },
                TestNode {
                    parent: Some(0),
                    node_type: NodeType::Object,
                    key: b"obj".to_vec(),
                },
                TestNode {
                    parent: Some(13),
                    node_type: NodeType::StructuredArray,
                    key: b"items".to_vec(),
                },
                TestNode {
                    parent: Some(14),
                    node_type: NodeType::Object,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(15),
                    node_type: NodeType::Integer,
                    key: b"x".to_vec(),
                },
                TestNode {
                    parent: Some(5),
                    node_type: NodeType::Object,
                    key: b"nested".to_vec(),
                },
                TestNode {
                    parent: Some(17),
                    node_type: NodeType::VarString,
                    key: b"z".to_vec(),
                },
                TestNode {
                    parent: Some(5),
                    node_type: NodeType::StructuredArray,
                    key: b"nested".to_vec(),
                },
                TestNode {
                    parent: Some(19),
                    node_type: NodeType::Object,
                    key: Vec::new(),
                },
                TestNode {
                    parent: Some(20),
                    node_type: NodeType::VarString,
                    key: b"z".to_vec(),
                },
            ],
            resolver.nodes.as_slice()
        );
    }

    #[test]
    fn structured_array_planning_enforces_limits_and_duplicates() {
        let mut resolver = TestNodeResolver::with_record_root();
        assert!(matches!(
            plan_structured_array(
                &[],
                0,
                2,
                StructuredArrayPlanLimits::new(0, 8),
                &mut resolver,
            ),
            Err(StructuredArrayPlanError::LimitExceeded {
                resource: StructuredArrayPlanResource::SchemaEntries,
                actual: 1,
                limit: 0,
            })
        ));
        assert!(matches!(
            plan_structured_array(
                &[],
                0,
                2,
                StructuredArrayPlanLimits::new(8, 1),
                &mut resolver,
            ),
            Err(StructuredArrayPlanError::LimitExceeded {
                resource: StructuredArrayPlanResource::NestingDepth,
                actual: 2,
                limit: 1,
            })
        ));

        let empty = [];
        let nested = [ValueRef::Array(&empty)];
        assert!(matches!(
            plan_structured_array(
                &nested,
                0,
                2,
                StructuredArrayPlanLimits::new(8, 2),
                &mut resolver,
            ),
            Err(StructuredArrayPlanError::LimitExceeded {
                resource: StructuredArrayPlanResource::NestingDepth,
                actual: 3,
                limit: 2,
            })
        ));

        let three = [ValueRef::I64(1), ValueRef::I64(2), ValueRef::I64(3)];
        assert!(matches!(
            plan_structured_array(
                &three,
                0,
                2,
                StructuredArrayPlanLimits::new(8, 8).with_max_container_body_entries(2),
                &mut resolver,
            ),
            Err(StructuredArrayPlanError::DelimiterBodyTooLong {
                node_type: NodeType::StructuredArray,
                actual: 3,
                limit: 2,
            })
        ));

        let duplicate = [
            FieldRef::new(b"same", ValueRef::I64(1)),
            FieldRef::new(b"same", ValueRef::I64(2)),
        ];
        let object = [ValueRef::Object(&duplicate)];
        assert!(matches!(
            plan_structured_array(
                &object,
                0,
                2,
                StructuredArrayPlanLimits::new(8, 8),
                &mut resolver,
            ),
            Err(StructuredArrayPlanError::DuplicateField {
                object_depth: 3,
                previous_index: 0,
                field_index: 1,
            })
        ));
    }

    #[test]
    fn validates_nested_mixed_whitespace_and_escaped_arrays() {
        for raw in [
            b"[]".as_slice(),
            br#"["extraction-benchmark","shard-00"]"#,
            br#"[ "first" , "second" ]"#,
            br#"[1,true,null,"x",{"k":"v"},[2,3]]"#,
            br#"[ -7, 12.50 , "user=face", {"n": 9} ]"#,
            br#"["slash\\marker","\u0011","\uD83D\uDE00"]"#,
            "[\"é\",[],{},[{\"x\":[]}]]".as_bytes(),
        ] {
            let mut scratch = ArrayValidationScratch::default();
            validate(
                UnstructuredArrayRef::new(raw),
                WriterLimits::DEFAULT,
                &mut scratch,
                7,
            )
            .expect("valid exact array");
        }
    }

    #[test]
    fn rejects_non_arrays_and_json_grammar_failures_structurally() {
        const INVALID_UTF8: &[u8] = &[b'[', b'"', 0xff, b'"', b']'];
        let invalid = [
            b"".as_slice(),
            b"{}",
            b" []",
            b"[] ",
            b"[",
            b"[1,]",
            b"[,1]",
            b"[01]",
            b"[1.]",
            b"[truex]",
            b"[{x:1}]",
            b"[{\"x\" 1}]",
            b"[\"\\q\"]",
            b"[\"\\u12xx\"]",
            b"[\"\\uD800\"]",
            b"[\"\\uDC00\"]",
            b"[\"line\nfeed\"]",
            INVALID_UTF8,
        ];
        for raw in invalid {
            let mut scratch = ArrayValidationScratch::default();
            assert!(matches!(
                validate(
                    UnstructuredArrayRef::new(raw),
                    WriterLimits::DEFAULT,
                    &mut scratch,
                    7,
                ),
                Err(AppendError::UnstructuredArray { node_id: 7, .. })
            ));
        }
    }
}
