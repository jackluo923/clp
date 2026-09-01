//! Reusable JSON-record extraction programs for decoded CLP-S schema tables.
//!
//! [`RecordProgram`] compiles structural punctuation and escaped schema keys once, then
//! [`RecordWriter`] replays that literal tape around typed column values. Delta columns are read
//! through persistent forward iterators, so extracting a table is linear in its row count.
//!
//! Record appends are transactional: an error restores the caller's output length and does not
//! advance any column cursor. [`JsonBytePolicy::StrictUtf8`] is the default. The explicit
//! [`JsonBytePolicy::PreserveInvalidUtf8`] mode reproduces the C++ extractor's byte-preserving
//! behavior, which can intentionally produce output that is not valid UTF-8 JSON.
//! Pre-v0.5 deprecated date-pattern semantics are intentionally rejected when binding a table.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use crate::ExtractionOp;
use crate::ExtractionPlan;
use crate::ExtractionPosition;
use crate::archive::BooleanColumn;
use crate::archive::ClpStringColumn;
use crate::archive::ColumnData;
use crate::archive::DeltaI64Values;
use crate::archive::DictionaryIdColumn;
use crate::archive::EncodedVariableError;
use crate::archive::F64Column;
use crate::archive::FormattedFloatColumn;
use crate::archive::FormattedFloatError;
use crate::archive::I64Column;
use crate::archive::MAX_FORMATTED_FLOAT_BYTES;
use crate::archive::NodeType;
use crate::archive::SchemaTable;
use crate::archive::SchemaTree;
use crate::archive::TimestampColumn;
use crate::archive::U64Column;
use crate::archive::append_clp_message_bounded;
use crate::archive::append_formatted_float;
use crate::json::JsonBytePolicy;
use crate::json::JsonEscapeError;
use crate::json::JsonEscapeLimits;
use crate::json::append_json_key_bytes;
use crate::json::append_json_string_bytes;
use crate::json_number::JsonNumberError;
use crate::json_number::MAX_F64_JSON_BYTES;
use crate::json_number::MAX_I64_JSON_BYTES;
use crate::json_number::append_json_f64;
use crate::json_number::append_json_i64;
use crate::timestamp_catalog::TimestampCatalogFormatError;
use crate::timestamp_catalog::TimestampPatternCatalog;

const MEBIBYTE: usize = 1024 * 1024;

/// Resource limits shared by record-program compilation and row serialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordLimits {
    columns: usize,
    operations: usize,
    nesting_depth: usize,
    program_bytes: usize,
    record_bytes: usize,
    scratch_bytes: usize,
}

impl RecordLimits {
    /// Creates explicit program and serialization limits.
    #[must_use]
    pub const fn new(
        max_columns: usize,
        max_operations: usize,
        max_nesting_depth: usize,
        max_program_bytes: usize,
        max_record_bytes: usize,
        max_scratch_bytes: usize,
    ) -> Self {
        Self {
            columns: max_columns,
            operations: max_operations,
            nesting_depth: max_nesting_depth,
            program_bytes: max_program_bytes,
            record_bytes: max_record_bytes,
            scratch_bytes: max_scratch_bytes,
        }
    }

    /// Maximum physical columns in a bound table.
    #[must_use]
    pub const fn max_columns(self) -> usize {
        self.columns
    }

    /// Maximum extraction operations compiled into one program.
    #[must_use]
    pub const fn max_operations(self) -> usize {
        self.operations
    }

    /// Maximum explicit object/array nesting below the implicit outer object.
    #[must_use]
    pub const fn max_nesting_depth(self) -> usize {
        self.nesting_depth
    }

    /// Maximum bytes in the program's compiled literal tape.
    #[must_use]
    pub const fn max_program_bytes(self) -> usize {
        self.program_bytes
    }

    /// Maximum bytes appended for one complete JSON record.
    #[must_use]
    pub const fn max_record_bytes(self) -> usize {
        self.record_bytes
    }

    /// Maximum bytes materialized while restoring one dynamic value.
    #[must_use]
    pub const fn max_scratch_bytes(self) -> usize {
        self.scratch_bytes
    }
}

impl Default for RecordLimits {
    fn default() -> Self {
        Self::new(
            1024 * 1024,
            16 * 1024 * 1024,
            256,
            256 * MEBIBYTE,
            256 * MEBIBYTE,
            64 * MEBIBYTE,
        )
    }
}

/// A resource governed by [`RecordLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordResource {
    /// Physical table columns and column-indexed state.
    Columns,
    /// Extraction operations and value steps.
    Operations,
    /// Open structured-container stack.
    NestingDepth,
    /// Compiled punctuation and escaped keys.
    ProgramBytes,
    /// Bytes in one emitted JSON record.
    RecordBytes,
    /// Bytes used to restore one number, CLP value, or timestamp.
    ScratchBytes,
    /// Transactional per-column cursors.
    ColumnCursors,
}

impl Display for RecordResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Columns => "record columns",
            Self::Operations => "record operations",
            Self::NestingDepth => "record nesting depth",
            Self::ProgramBytes => "compiled record bytes",
            Self::RecordBytes => "JSON record bytes",
            Self::ScratchBytes => "record scratch bytes",
            Self::ColumnCursors => "transactional column cursors",
        })
    }
}

/// A value-independent, reusable JSON extraction program for one schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordProgram {
    schema_id: i32,
    column_count: usize,
    literals: Vec<u8>,
    values: Vec<ValueStep>,
    expected_columns: Vec<Option<ExpectedColumn>>,
    byte_policy: JsonBytePolicy,
    limits: RecordLimits,
}

impl RecordProgram {
    /// Compiles an extraction plan using strict UTF-8 validation for every archive byte string.
    ///
    /// # Errors
    ///
    /// Returns a bounded-allocation error or a structured inconsistency between the plan and its
    /// schema tree. Object keys are validated and escaped during this call.
    pub fn compile(
        plan: &ExtractionPlan,
        schema_tree: &SchemaTree,
        limits: RecordLimits,
    ) -> Result<Self, RecordCompileError> {
        Self::compile_with_byte_policy(plan, schema_tree, JsonBytePolicy::StrictUtf8, limits)
    }

    /// Compiles an extraction plan with an explicit archive-byte policy.
    ///
    /// [`JsonBytePolicy::PreserveInvalidUtf8`] matches C++ byte escaping, but records containing
    /// malformed input bytes are not valid UTF-8 JSON.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::compile`].
    pub fn compile_with_byte_policy(
        plan: &ExtractionPlan,
        schema_tree: &SchemaTree,
        byte_policy: JsonBytePolicy,
        limits: RecordLimits,
    ) -> Result<Self, RecordCompileError> {
        let source = SchemaTreeNodeSource(schema_tree);
        compile_parts(
            PlanParts {
                schema_id: plan.schema_id(),
                root_node_id: plan.root_node_id(),
                column_count: plan.column_count(),
                operations: plan.operations(),
            },
            &source,
            byte_policy,
            limits,
        )
    }

    /// Returns the opaque schema ID represented by this program.
    #[must_use]
    pub const fn schema_id(&self) -> i32 {
        self.schema_id
    }

    /// Returns the complete physical column count, including omitted metadata columns.
    #[must_use]
    pub const fn column_count(&self) -> usize {
        self.column_count
    }

    /// Returns the number of dynamic values emitted per record.
    #[must_use]
    pub const fn value_count(&self) -> usize {
        self.values.len()
    }

    /// Returns the archive-byte policy fixed when the program was compiled.
    #[must_use]
    pub const fn byte_policy(&self) -> JsonBytePolicy {
        self.byte_policy
    }

    /// Returns the limits applied to this program and every writer created from it.
    #[must_use]
    pub const fn limits(&self) -> RecordLimits {
        self.limits
    }

    /// Binds this program to one decoded table and timestamp catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the table does not match the compiled schema, cursor allocation
    /// fails, configured scratch cannot support a referenced fixed-size formatter, or a
    /// referenced pre-v0.5 deprecated date column is encountered.
    pub fn writer<'program, 'table, 'archive, 'catalog>(
        &'program self,
        table: &SchemaTable<'table, 'archive>,
        timestamps: &'catalog TimestampPatternCatalog,
    ) -> Result<RecordWriter<'program, 'table, 'archive, 'catalog>, RecordBindError> {
        self.writer_with_scratch(table, timestamps, RecordScratch::new())
    }

    /// Binds this program while taking ownership of caller-warmed reusable scratch buffers.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::writer`].
    pub fn writer_with_scratch<'program, 'table, 'archive, 'catalog>(
        &'program self,
        table: &SchemaTable<'table, 'archive>,
        timestamps: &'catalog TimestampPatternCatalog,
        scratch: RecordScratch,
    ) -> Result<RecordWriter<'program, 'table, 'archive, 'catalog>, RecordBindError> {
        bind_writer(self, table, timestamps, scratch)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedColumn {
    node_id: u32,
    node_type: NodeType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValueStep {
    literal_end: usize,
    column_index: usize,
    node_id: u32,
}

/// Scratch buffers retained across records and reclaimable from a [`RecordWriter`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecordScratch {
    bytes: Vec<u8>,
    text: String,
}

impl RecordScratch {
    /// Creates empty scratch buffers that grow only through bounded record operations.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            text: String::new(),
        }
    }

    /// Returns the retained byte-buffer capacity.
    #[must_use]
    pub const fn byte_capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Returns the retained UTF-8 text-buffer capacity.
    #[must_use]
    pub const fn text_capacity(&self) -> usize {
        self.text.capacity()
    }

    /// Releases buffered contents while retaining capacity for a later writer.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.text.clear();
    }
}

/// A table-bound, forward-only JSON record writer.
///
/// The writer keeps a committed and a trial cursor set. Failed records leave both the output
/// contents and committed delta state untouched, allowing callers to inspect the error and retry.
pub struct RecordWriter<'program, 'table, 'archive, 'catalog> {
    program: &'program RecordProgram,
    timestamps: &'catalog TimestampPatternCatalog,
    current: Vec<ColumnCursor<'table, 'archive>>,
    trial: Vec<ColumnCursor<'table, 'archive>>,
    scratch: RecordScratch,
    next_row: usize,
    row_count: usize,
}

impl RecordWriter<'_, '_, '_, '_> {
    /// Appends the next complete JSON object, without a trailing newline.
    ///
    /// Returns `Ok(false)` without modifying `output` after all rows have been emitted. On error,
    /// `output` is restored to its original length and all column cursors remain at the failing
    /// row.
    ///
    /// # Errors
    ///
    /// Returns a structured column, formatting, UTF-8, allocation, or resource-limit error.
    pub fn append_next_record(&mut self, output: &mut Vec<u8>) -> Result<bool, RecordError> {
        if self.next_row == self.row_count {
            return Ok(false);
        }
        let committed_next_row = self
            .next_row
            .checked_add(1)
            .ok_or(RecordError::SizeOverflow)?;

        for (trial, current) in self.trial.iter_mut().zip(&self.current) {
            trial.clone_from(current);
        }
        let original_len = output.len();
        let mut context = RowContext {
            program: self.program,
            timestamps: self.timestamps,
            cursors: &mut self.trial,
            scratch: &mut self.scratch,
            row_index: self.next_row,
            output,
            output_start: original_len,
        };
        let result = append_row(&mut context);
        if let Err(error) = result {
            output.truncate(original_len);
            return Err(error);
        }

        std::mem::swap(&mut self.current, &mut self.trial);
        self.next_row = committed_next_row;
        Ok(true)
    }

    /// Advances past the next row without formatting JSON.
    ///
    /// This is the forward-only companion to [`Self::append_next_record`] for search and
    /// projection pipelines that already know a row will not be emitted. Stateful delta and
    /// timestamp cursors advance transactionally; random-access columns require no work. Returns
    /// `Ok(false)` when the writer is exhausted.
    ///
    /// # Errors
    ///
    /// Returns [`RecordError::MissingColumnValue`] if a validated stateful column unexpectedly
    /// ends before the table's advertised row count. The committed row and all committed cursors
    /// remain unchanged, so callers may inspect or retry the failure.
    pub fn skip_next_record(&mut self) -> Result<bool, RecordError> {
        self.skip_records(1)
    }

    pub(crate) fn skip_records(&mut self, count: usize) -> Result<bool, RecordError> {
        if count > self.remaining() {
            return Ok(false);
        }
        if 0 == count {
            return Ok(true);
        }
        let committed_next_row = self
            .next_row
            .checked_add(count)
            .ok_or(RecordError::SizeOverflow)?;

        for (trial, current) in self.trial.iter_mut().zip(&self.current) {
            trial.clone_from(current);
        }
        for (column_index, cursor) in self.trial.iter_mut().enumerate() {
            let available = match cursor {
                ColumnCursor::DeltaInteger(values) => values.len(),
                ColumnCursor::Timestamp {
                    epochs,
                    pattern_ids,
                } => epochs
                    .len()
                    .min(pattern_ids.len().saturating_sub(self.next_row)),
                _ => continue,
            };
            if available < count {
                let node_id = self
                    .program
                    .expected_columns
                    .get(column_index)
                    .copied()
                    .flatten()
                    .ok_or(RecordError::SizeOverflow)?
                    .node_id;
                return Err(RecordError::MissingColumnValue {
                    column_index,
                    node_id,
                    row_index: self
                        .next_row
                        .checked_add(available)
                        .ok_or(RecordError::SizeOverflow)?,
                });
            }
            match cursor {
                ColumnCursor::DeltaInteger(values) => {
                    let _ = values.nth(count - 1);
                }
                ColumnCursor::Timestamp { epochs, .. } => {
                    let _ = epochs.nth(count - 1);
                }
                _ => unreachable!("only stateful cursors compute an available row count"),
            }
        }

        std::mem::swap(&mut self.current, &mut self.trial);
        self.next_row = committed_next_row;
        Ok(true)
    }

    /// Returns the zero-based row that will be emitted next.
    #[must_use]
    pub const fn next_row_index(&self) -> usize {
        self.next_row
    }

    /// Returns the number of records not yet emitted.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.row_count - self.next_row
    }

    /// Returns the complete row count of the bound schema table.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Consumes the writer and returns its warmed scratch buffers.
    #[must_use]
    pub fn into_scratch(self) -> RecordScratch {
        self.scratch
    }
}

#[derive(Clone, Debug)]
enum ColumnCursor<'table, 'archive> {
    Unused,
    Integer(I64Column<'table>),
    DeltaInteger(DeltaI64Values<'table>),
    Float(F64Column<'table>),
    FormattedFloat(FormattedFloatColumn<'table>),
    DictionaryFloat(DictionaryIdColumn<'table, 'archive>),
    Boolean(BooleanColumn<'table>),
    VarString(DictionaryIdColumn<'table, 'archive>),
    ClpString(ClpStringColumn<'table, 'archive>),
    UnstructuredArray(ClpStringColumn<'table, 'archive>),
    Timestamp {
        epochs: DeltaI64Values<'table>,
        pattern_ids: U64Column<'table>,
    },
}

struct RowContext<'writer, 'program, 'table, 'archive, 'catalog> {
    program: &'program RecordProgram,
    timestamps: &'catalog TimestampPatternCatalog,
    cursors: &'writer mut [ColumnCursor<'table, 'archive>],
    scratch: &'writer mut RecordScratch,
    row_index: usize,
    output: &'writer mut Vec<u8>,
    output_start: usize,
}

fn append_row(context: &mut RowContext<'_, '_, '_, '_, '_>) -> Result<(), RecordError> {
    let mut output = BoundedRecordOutput {
        bytes: &mut *context.output,
        start: context.output_start,
        limit: context.program.limits.record_bytes,
    };
    let mut literal_start = 0_usize;
    for step in &context.program.values {
        let literal = context
            .program
            .literals
            .get(literal_start..step.literal_end)
            .ok_or(RecordError::SizeOverflow)?;
        output.append(literal)?;
        let cursor =
            context
                .cursors
                .get_mut(step.column_index)
                .ok_or(RecordError::MissingColumnValue {
                    column_index: step.column_index,
                    node_id: step.node_id,
                    row_index: context.row_index,
                })?;
        append_dynamic_value(
            cursor,
            *step,
            context.row_index,
            context.timestamps,
            context.program.byte_policy,
            context.program.limits.scratch_bytes,
            context.scratch,
            &mut output,
        )?;
        literal_start = step.literal_end;
    }
    let tail = context
        .program
        .literals
        .get(literal_start..)
        .ok_or(RecordError::SizeOverflow)?;
    output.append(tail)
}

struct BoundedRecordOutput<'a> {
    bytes: &'a mut Vec<u8>,
    start: usize,
    limit: usize,
}

impl BoundedRecordOutput<'_> {
    fn append(&mut self, bytes: &[u8]) -> Result<(), RecordError> {
        let current = self
            .bytes
            .len()
            .checked_sub(self.start)
            .ok_or(RecordError::SizeOverflow)?;
        let required = current
            .checked_add(bytes.len())
            .ok_or(RecordError::SizeOverflow)?;
        if required > self.limit {
            return Err(RecordError::LimitExceeded {
                resource: RecordResource::RecordBytes,
                required,
                limit: self.limit,
            });
        }
        self.bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(RecordError::SizeOverflow)?;
        self.bytes
            .try_reserve_exact(bytes.len())
            .map_err(|_| RecordError::AllocationFailed {
                resource: RecordResource::RecordBytes,
                requested: bytes.len(),
            })?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn append_json_string(
        &mut self,
        source: &[u8],
        policy: JsonBytePolicy,
        step: ValueStep,
    ) -> Result<(), RecordError> {
        let current = self
            .bytes
            .len()
            .checked_sub(self.start)
            .ok_or(RecordError::SizeOverflow)?;
        let remaining = self
            .limit
            .checked_sub(current)
            .ok_or(RecordError::LimitExceeded {
                resource: RecordResource::RecordBytes,
                required: current,
                limit: self.limit,
            })?;
        let limits = JsonEscapeLimits::new(source.len(), remaining);
        append_json_string_bytes(source, self.bytes, policy, limits).map_err(|source| {
            RecordError::JsonString {
                column_index: step.column_index,
                node_id: step.node_id,
                source,
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn append_dynamic_value(
    cursor: &mut ColumnCursor<'_, '_>,
    step: ValueStep,
    row_index: usize,
    timestamps: &TimestampPatternCatalog,
    byte_policy: JsonBytePolicy,
    scratch_limit: usize,
    scratch: &mut RecordScratch,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    match cursor {
        ColumnCursor::Integer(column) => {
            let value = column
                .get(row_index)
                .ok_or_else(|| missing(step, row_index))?;
            append_integer(value, step, scratch, output)
        }
        ColumnCursor::DeltaInteger(values) => {
            let value = values.next().ok_or_else(|| missing(step, row_index))?;
            append_integer(value, step, scratch, output)
        }
        ColumnCursor::Float(column) => {
            let value = column
                .get(row_index)
                .ok_or_else(|| missing(step, row_index))?;
            append_float(value, step, scratch, output)
        }
        ColumnCursor::FormattedFloat(column) => {
            append_original_float(*column, step, row_index, scratch, output)
        }
        ColumnCursor::DictionaryFloat(column) => {
            let value = column
                .value(row_index)
                .ok_or_else(|| missing(step, row_index))?;
            append_raw_bytes(value, step, byte_policy, output)
        }
        ColumnCursor::Boolean(column) => {
            let value = column
                .get(row_index)
                .ok_or_else(|| missing(step, row_index))?;
            output.append(if value { b"true" } else { b"false" })
        }
        ColumnCursor::VarString(column) => {
            let value = column
                .value(row_index)
                .ok_or_else(|| missing(step, row_index))?;
            output.append_json_string(value, byte_policy, step)
        }
        ColumnCursor::ClpString(column) => append_clp_value(
            *column,
            step,
            row_index,
            true,
            byte_policy,
            scratch_limit,
            scratch,
            output,
        ),
        ColumnCursor::UnstructuredArray(column) => append_clp_value(
            *column,
            step,
            row_index,
            false,
            byte_policy,
            scratch_limit,
            scratch,
            output,
        ),
        ColumnCursor::Timestamp {
            epochs,
            pattern_ids,
        } => append_timestamp(
            epochs,
            *pattern_ids,
            step,
            row_index,
            timestamps,
            scratch_limit,
            scratch,
            output,
        ),
        ColumnCursor::Unused => Err(missing(step, row_index)),
    }
}

fn append_integer(
    value: i64,
    step: ValueStep,
    scratch: &mut RecordScratch,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    scratch.bytes.clear();
    append_json_i64(value, &mut scratch.bytes).map_err(|source| RecordError::JsonNumber {
        column_index: step.column_index,
        node_id: step.node_id,
        source,
    })?;
    output.append(&scratch.bytes)
}

fn append_float(
    value: f64,
    step: ValueStep,
    scratch: &mut RecordScratch,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    scratch.bytes.clear();
    append_json_f64(value, &mut scratch.bytes).map_err(|source| RecordError::JsonNumber {
        column_index: step.column_index,
        node_id: step.node_id,
        source,
    })?;
    output.append(&scratch.bytes)
}

fn append_original_float(
    column: FormattedFloatColumn<'_>,
    step: ValueStep,
    row_index: usize,
    scratch: &mut RecordScratch,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    let formatted = column
        .get(row_index)
        .ok_or_else(|| missing(step, row_index))?;
    scratch.text.clear();
    append_formatted_float(formatted.value(), formatted.format(), &mut scratch.text).map_err(
        |source| RecordError::FormattedFloat {
            column_index: step.column_index,
            node_id: step.node_id,
            source,
        },
    )?;
    output.append(scratch.text.as_bytes())
}

#[allow(clippy::too_many_arguments)]
fn append_clp_value(
    column: ClpStringColumn<'_, '_>,
    step: ValueStep,
    row_index: usize,
    quoted: bool,
    byte_policy: JsonBytePolicy,
    scratch_limit: usize,
    scratch: &mut RecordScratch,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    let record = column
        .record(row_index)
        .ok_or_else(|| missing(step, row_index))?;
    scratch.bytes.clear();
    append_clp_message_bounded(
        record.logtype(),
        column.variable_dictionary(),
        &record.encoded_variables(),
        &mut scratch.bytes,
        scratch_limit,
    )
    .map_err(|source| RecordError::ClpValue {
        column_index: step.column_index,
        node_id: step.node_id,
        source,
    })?;
    if quoted {
        output.append_json_string(&scratch.bytes, byte_policy, step)
    } else {
        append_raw_bytes(&scratch.bytes, step, byte_policy, output)
    }
}

#[allow(clippy::too_many_arguments)]
fn append_timestamp(
    epochs: &mut DeltaI64Values<'_>,
    pattern_ids: U64Column<'_>,
    step: ValueStep,
    row_index: usize,
    timestamps: &TimestampPatternCatalog,
    scratch_limit: usize,
    scratch: &mut RecordScratch,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    let epoch_nanoseconds = epochs.next().ok_or_else(|| missing(step, row_index))?;
    let pattern_id = pattern_ids
        .get(row_index)
        .ok_or_else(|| missing(step, row_index))?;
    let pattern = timestamps
        .pattern(pattern_id)
        .ok_or(RecordError::Timestamp {
            column_index: step.column_index,
            node_id: step.node_id,
            source: TimestampCatalogFormatError::UnknownPatternId { pattern_id },
        })?;
    let required = pattern.max_formatted_size();
    if required > scratch_limit {
        return Err(RecordError::LimitExceeded {
            resource: RecordResource::ScratchBytes,
            required,
            limit: scratch_limit,
        });
    }

    scratch.text.clear();
    timestamps
        .append_epoch_nanoseconds(pattern_id, epoch_nanoseconds, &mut scratch.text)
        .map_err(|source| RecordError::Timestamp {
            column_index: step.column_index,
            node_id: step.node_id,
            source,
        })?;
    output.append(scratch.text.as_bytes())
}

fn append_raw_bytes(
    value: &[u8],
    step: ValueStep,
    byte_policy: JsonBytePolicy,
    output: &mut BoundedRecordOutput<'_>,
) -> Result<(), RecordError> {
    if matches!(byte_policy, JsonBytePolicy::StrictUtf8) {
        std::str::from_utf8(value).map_err(|source| RecordError::InvalidRawUtf8 {
            column_index: step.column_index,
            node_id: step.node_id,
            valid_up_to: source.valid_up_to(),
            error_len: source.error_len(),
        })?;
    }
    output.append(value)
}

const fn missing(step: ValueStep, row_index: usize) -> RecordError {
    RecordError::MissingColumnValue {
        column_index: step.column_index,
        node_id: step.node_id,
        row_index,
    }
}

fn bind_writer<'program, 'table, 'archive, 'catalog>(
    program: &'program RecordProgram,
    table: &SchemaTable<'table, 'archive>,
    timestamps: &'catalog TimestampPatternCatalog,
    scratch: RecordScratch,
) -> Result<RecordWriter<'program, 'table, 'archive, 'catalog>, RecordBindError> {
    if table.len() != program.column_count {
        return Err(RecordBindError::ColumnCountMismatch {
            expected: program.column_count,
            actual: table.len(),
        });
    }
    if program.literals.len() > program.limits.record_bytes {
        return Err(RecordBindError::LimitExceeded {
            resource: RecordResource::RecordBytes,
            required: program.literals.len(),
            limit: program.limits.record_bytes,
        });
    }

    let mut current = reserve_cursor_vec(program.column_count)?;
    let mut trial = reserve_cursor_vec(program.column_count)?;
    for (column_index, (column, expected)) in table
        .columns()
        .iter()
        .copied()
        .zip(&program.expected_columns)
        .enumerate()
    {
        let cursor = match expected {
            Some(expected) => bind_column(
                column_index,
                column.node_id(),
                column.node_type(),
                column.data(),
                *expected,
                program.limits.scratch_bytes,
            )?,
            None => ColumnCursor::Unused,
        };
        trial.push(cursor.clone());
        current.push(cursor);
    }

    Ok(RecordWriter {
        program,
        timestamps,
        current,
        trial,
        scratch,
        next_row: 0,
        row_count: table.message_count(),
    })
}

fn reserve_cursor_vec<'table, 'archive>(
    count: usize,
) -> Result<Vec<ColumnCursor<'table, 'archive>>, RecordBindError> {
    let mut cursors = Vec::new();
    cursors
        .try_reserve_exact(count)
        .map_err(|_| RecordBindError::AllocationFailed {
            resource: RecordResource::ColumnCursors,
            requested: count,
        })?;
    Ok(cursors)
}

#[allow(clippy::too_many_arguments)]
fn bind_column<'table, 'archive>(
    column_index: usize,
    actual_node_id: u32,
    actual_type: NodeType,
    data: ColumnData<'table, 'archive>,
    expected: ExpectedColumn,
    scratch_limit: usize,
) -> Result<ColumnCursor<'table, 'archive>, RecordBindError> {
    if actual_node_id != expected.node_id {
        return Err(RecordBindError::NodeIdMismatch {
            column_index,
            expected: expected.node_id,
            actual: actual_node_id,
        });
    }
    if actual_type != expected.node_type {
        return Err(RecordBindError::NodeTypeMismatch {
            column_index,
            node_id: actual_node_id,
            expected: expected.node_type,
            actual: actual_type,
        });
    }

    let required_scratch = fixed_scratch_requirement(data);
    if required_scratch > scratch_limit {
        return Err(RecordBindError::LimitExceeded {
            resource: RecordResource::ScratchBytes,
            required: required_scratch,
            limit: scratch_limit,
        });
    }
    match data {
        ColumnData::Integer(column) => Ok(ColumnCursor::Integer(column)),
        ColumnData::DeltaInteger(column) => Ok(ColumnCursor::DeltaInteger(column.values())),
        ColumnData::Float(column) => Ok(ColumnCursor::Float(column)),
        ColumnData::FormattedFloat(column) => Ok(ColumnCursor::FormattedFloat(column)),
        ColumnData::DictionaryFloat(column) => Ok(ColumnCursor::DictionaryFloat(column)),
        ColumnData::Boolean(column) => Ok(ColumnCursor::Boolean(column)),
        ColumnData::VarString(column) => Ok(ColumnCursor::VarString(column)),
        ColumnData::ClpString(column) => Ok(ColumnCursor::ClpString(column)),
        ColumnData::UnstructuredArray(column) => Ok(ColumnCursor::UnstructuredArray(column)),
        ColumnData::Timestamp(column) => Ok(timestamp_cursor(column)),
        ColumnData::DeprecatedDateString(_) => {
            Err(RecordBindError::UnsupportedDeprecatedDateString {
                column_index,
                node_id: actual_node_id,
            })
        }
    }
}

const fn fixed_scratch_requirement(data: ColumnData<'_, '_>) -> usize {
    match data {
        ColumnData::Integer(_) | ColumnData::DeltaInteger(_) => MAX_I64_JSON_BYTES,
        ColumnData::Float(_) => MAX_F64_JSON_BYTES,
        ColumnData::FormattedFloat(_) => MAX_FORMATTED_FLOAT_BYTES,
        _ => 0,
    }
}

fn timestamp_cursor<'table, 'archive>(
    column: TimestampColumn<'table, 'archive>,
) -> ColumnCursor<'table, 'archive> {
    ColumnCursor::Timestamp {
        epochs: column.epochs().values(),
        pattern_ids: column.pattern_ids(),
    }
}

#[derive(Clone, Copy)]
struct PlanParts<'a> {
    schema_id: i32,
    root_node_id: Option<u32>,
    column_count: usize,
    operations: &'a [ExtractionOp],
}

#[derive(Clone, Copy)]
struct NodeView<'a> {
    parent_id: Option<usize>,
    key: &'a [u8],
    node_type: NodeType,
}

trait NodeSource {
    fn get(&self, node_id: usize) -> Option<NodeView<'_>>;
}

struct SchemaTreeNodeSource<'a>(&'a SchemaTree);

impl NodeSource for SchemaTreeNodeSource<'_> {
    fn get(&self, node_id: usize) -> Option<NodeView<'_>> {
        self.0.get(node_id).map(|node| NodeView {
            parent_id: node.parent_id(),
            key: node.key_bytes(),
            node_type: node.node_type(),
        })
    }
}

struct ProgramCompiler<'a> {
    schema_id: i32,
    column_count: usize,
    source: &'a dyn NodeSource,
    byte_policy: JsonBytePolicy,
    limits: RecordLimits,
    literals: Vec<u8>,
    values: Vec<ValueStep>,
    expected_columns: Vec<Option<ExpectedColumn>>,
    containers: Vec<ContainerFrame>,
}

#[derive(Clone, Copy)]
struct ContainerFrame {
    node_type: NodeType,
    has_item: bool,
}

fn compile_parts(
    parts: PlanParts<'_>,
    source: &dyn NodeSource,
    byte_policy: JsonBytePolicy,
    limits: RecordLimits,
) -> Result<RecordProgram, RecordCompileError> {
    check_compile_limit(RecordResource::Columns, parts.column_count, limits.columns)?;
    check_compile_limit(
        RecordResource::Operations,
        parts.operations.len(),
        limits.operations,
    )?;
    validate_root(parts.root_node_id, parts.operations, source)?;

    let mut compiler = ProgramCompiler::new(
        parts.schema_id,
        parts.column_count,
        parts.operations.len(),
        source,
        byte_policy,
        limits,
    )?;
    compiler.append_literal(b"{")?;
    for (operation_index, operation) in parts.operations.iter().copied().enumerate() {
        compiler.compile_operation(operation_index, operation)?;
    }
    if 1 != compiler.containers.len() {
        return Err(RecordCompileError::UnclosedContainer {
            operation_index: parts.operations.len(),
            open_depth: compiler.containers.len() - 1,
        });
    }
    compiler.append_literal(b"}")?;
    Ok(compiler.finish())
}

fn validate_root(
    root_node_id: Option<u32>,
    operations: &[ExtractionOp],
    source: &dyn NodeSource,
) -> Result<(), RecordCompileError> {
    let Some(root_node_id) = root_node_id else {
        if operations.is_empty() {
            return Ok(());
        }
        return Err(RecordCompileError::MissingDefaultRoot);
    };
    let node_index = usize::try_from(root_node_id).map_err(|_| RecordCompileError::SizeOverflow)?;
    let root = source
        .get(node_index)
        .ok_or(RecordCompileError::UnknownNode {
            operation_index: None,
            node_id: root_node_id,
        })?;
    if root.parent_id.is_some() || NodeType::Object != root.node_type || !root.key.is_empty() {
        return Err(RecordCompileError::InvalidDefaultRoot {
            node_id: root_node_id,
            node_type: root.node_type,
        });
    }
    Ok(())
}

impl<'a> ProgramCompiler<'a> {
    fn new(
        schema_id: i32,
        column_count: usize,
        operation_count: usize,
        source: &'a dyn NodeSource,
        byte_policy: JsonBytePolicy,
        limits: RecordLimits,
    ) -> Result<Self, RecordCompileError> {
        let mut values = Vec::new();
        reserve_compile(
            &mut values,
            operation_count.min(column_count),
            RecordResource::Operations,
        )?;
        let mut expected_columns = Vec::new();
        reserve_compile(&mut expected_columns, column_count, RecordResource::Columns)?;
        expected_columns.resize(column_count, None);
        let mut containers = Vec::new();
        reserve_compile(
            &mut containers,
            operation_count.min(limits.nesting_depth).saturating_add(1),
            RecordResource::NestingDepth,
        )?;
        containers.push(ContainerFrame {
            node_type: NodeType::Object,
            has_item: false,
        });
        Ok(Self {
            schema_id,
            column_count,
            source,
            byte_policy,
            limits,
            literals: Vec::new(),
            values,
            expected_columns,
            containers,
        })
    }

    fn compile_operation(
        &mut self,
        operation_index: usize,
        operation: ExtractionOp,
    ) -> Result<(), RecordCompileError> {
        match operation {
            ExtractionOp::BeginObject { node_id, position } => {
                self.begin_container(operation_index, node_id, position, NodeType::Object, b'{')
            }
            ExtractionOp::EndObject => self.end_container(operation_index, NodeType::Object, b'}'),
            ExtractionOp::BeginArray { node_id, position } => self.begin_container(
                operation_index,
                node_id,
                position,
                NodeType::StructuredArray,
                b'[',
            ),
            ExtractionOp::EndArray => {
                self.end_container(operation_index, NodeType::StructuredArray, b']')
            }
            ExtractionOp::Value {
                column_index,
                node_id,
                position,
            } => self.value(operation_index, column_index, node_id, position),
            ExtractionOp::Null { node_id, position } => {
                let node = require_node(self.source, operation_index, node_id)?;
                Self::expect_node_type(operation_index, node_id, node, NodeType::Null)?;
                self.prepare_item(operation_index, node_id, position, node.key)?;
                self.append_literal(b"null")
            }
        }
    }

    fn begin_container(
        &mut self,
        operation_index: usize,
        node_id: u32,
        position: ExtractionPosition,
        expected_type: NodeType,
        opening: u8,
    ) -> Result<(), RecordCompileError> {
        let node = require_node(self.source, operation_index, node_id)?;
        Self::expect_node_type(operation_index, node_id, node, expected_type)?;
        self.prepare_item(operation_index, node_id, position, node.key)?;
        self.append_literal(&[opening])?;
        let explicit_depth = self.containers.len();
        let next_depth = explicit_depth
            .checked_add(1)
            .ok_or(RecordCompileError::SizeOverflow)?;
        if explicit_depth > self.limits.nesting_depth {
            return Err(RecordCompileError::LimitExceeded {
                resource: RecordResource::NestingDepth,
                actual: explicit_depth,
                limit: self.limits.nesting_depth,
            });
        }
        self.containers
            .try_reserve_exact(usize::from(next_depth > self.containers.capacity()))
            .map_err(|_| RecordCompileError::AllocationFailed {
                resource: RecordResource::NestingDepth,
                requested: next_depth,
            })?;
        self.containers.push(ContainerFrame {
            node_type: expected_type,
            has_item: false,
        });
        Ok(())
    }

    fn end_container(
        &mut self,
        operation_index: usize,
        expected_type: NodeType,
        closing: u8,
    ) -> Result<(), RecordCompileError> {
        let actual = self.containers.last().map(|frame| frame.node_type);
        if self.containers.len() <= 1 || actual != Some(expected_type) {
            return Err(RecordCompileError::UnexpectedContainerEnd {
                operation_index,
                expected: expected_type,
                actual,
            });
        }
        self.append_literal(&[closing])?;
        self.containers.pop();
        Ok(())
    }

    fn value(
        &mut self,
        operation_index: usize,
        raw_column_index: u32,
        node_id: u32,
        position: ExtractionPosition,
    ) -> Result<(), RecordCompileError> {
        let column_index =
            usize::try_from(raw_column_index).map_err(|_| RecordCompileError::SizeOverflow)?;
        if column_index >= self.column_count {
            return Err(RecordCompileError::ColumnIndexOutOfBounds {
                operation_index,
                column_index,
                column_count: self.column_count,
            });
        }
        let node = require_node(self.source, operation_index, node_id)?;
        if !is_value_node(node.node_type) {
            return Err(RecordCompileError::InvalidValueNodeType {
                operation_index,
                node_id,
                node_type: node.node_type,
            });
        }
        if self.expected_columns[column_index].is_some() {
            return Err(RecordCompileError::DuplicateColumn {
                operation_index,
                column_index,
            });
        }
        self.prepare_item(operation_index, node_id, position, node.key)?;
        self.expected_columns[column_index] = Some(ExpectedColumn {
            node_id,
            node_type: node.node_type,
        });
        self.values.push(ValueStep {
            literal_end: self.literals.len(),
            column_index,
            node_id,
        });
        Ok(())
    }

    fn prepare_item(
        &mut self,
        operation_index: usize,
        node_id: u32,
        position: ExtractionPosition,
        key: &[u8],
    ) -> Result<(), RecordCompileError> {
        let parent_type = self.containers.last().map(|frame| frame.node_type).ok_or(
            RecordCompileError::UnexpectedContainerEnd {
                operation_index,
                expected: NodeType::Object,
                actual: None,
            },
        )?;
        let position_matches = matches!(
            (parent_type, position),
            (NodeType::Object, ExtractionPosition::ObjectField)
                | (NodeType::StructuredArray, ExtractionPosition::ArrayElement)
        );
        if !position_matches {
            return Err(RecordCompileError::PositionMismatch {
                operation_index,
                node_id,
                parent_type,
                position,
            });
        }
        let has_item = self.containers.last().is_some_and(|frame| frame.has_item);
        if has_item {
            self.append_literal(b",")?;
        }
        if let Some(frame) = self.containers.last_mut() {
            frame.has_item = true;
        }
        if matches!(position, ExtractionPosition::ObjectField) {
            self.append_key(operation_index, node_id, key)?;
        }
        Ok(())
    }

    fn expect_node_type(
        operation_index: usize,
        node_id: u32,
        node: NodeView<'_>,
        expected: NodeType,
    ) -> Result<(), RecordCompileError> {
        if node.node_type != expected {
            return Err(RecordCompileError::NodeTypeMismatch {
                operation_index,
                node_id,
                expected,
                actual: node.node_type,
            });
        }
        Ok(())
    }

    fn append_key(
        &mut self,
        operation_index: usize,
        node_id: u32,
        key: &[u8],
    ) -> Result<(), RecordCompileError> {
        let remaining = self
            .limits
            .program_bytes
            .checked_sub(self.literals.len())
            .ok_or(RecordCompileError::LimitExceeded {
                resource: RecordResource::ProgramBytes,
                actual: self.literals.len(),
                limit: self.limits.program_bytes,
            })?;
        append_json_key_bytes(
            key,
            &mut self.literals,
            self.byte_policy,
            JsonEscapeLimits::new(key.len(), remaining),
        )
        .map_err(|source| RecordCompileError::JsonKey {
            operation_index,
            node_id,
            source,
        })
    }

    fn append_literal(&mut self, bytes: &[u8]) -> Result<(), RecordCompileError> {
        let actual = self
            .literals
            .len()
            .checked_add(bytes.len())
            .ok_or(RecordCompileError::SizeOverflow)?;
        if actual > self.limits.program_bytes {
            return Err(RecordCompileError::LimitExceeded {
                resource: RecordResource::ProgramBytes,
                actual,
                limit: self.limits.program_bytes,
            });
        }
        self.literals.try_reserve_exact(bytes.len()).map_err(|_| {
            RecordCompileError::AllocationFailed {
                resource: RecordResource::ProgramBytes,
                requested: bytes.len(),
            }
        })?;
        self.literals.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(self) -> RecordProgram {
        RecordProgram {
            schema_id: self.schema_id,
            column_count: self.column_count,
            literals: self.literals,
            values: self.values,
            expected_columns: self.expected_columns,
            byte_policy: self.byte_policy,
            limits: self.limits,
        }
    }
}

fn require_node(
    source: &dyn NodeSource,
    operation_index: usize,
    node_id: u32,
) -> Result<NodeView<'_>, RecordCompileError> {
    let index = usize::try_from(node_id).map_err(|_| RecordCompileError::SizeOverflow)?;
    source.get(index).ok_or(RecordCompileError::UnknownNode {
        operation_index: Some(operation_index),
        node_id,
    })
}

const fn is_value_node(node_type: NodeType) -> bool {
    matches!(
        node_type,
        NodeType::Integer
            | NodeType::Float
            | NodeType::ClpString
            | NodeType::VarString
            | NodeType::Boolean
            | NodeType::UnstructuredArray
            | NodeType::DeprecatedDateString
            | NodeType::DeltaInteger
            | NodeType::FormattedFloat
            | NodeType::DictionaryFloat
            | NodeType::Timestamp
    )
}

fn reserve_compile<T>(
    vector: &mut Vec<T>,
    requested: usize,
    resource: RecordResource,
) -> Result<(), RecordCompileError> {
    vector
        .try_reserve_exact(requested)
        .map_err(|_| RecordCompileError::AllocationFailed {
            resource,
            requested,
        })
}

const fn check_compile_limit(
    resource: RecordResource,
    actual: usize,
    limit: usize,
) -> Result<(), RecordCompileError> {
    if actual > limit {
        return Err(RecordCompileError::LimitExceeded {
            resource,
            actual,
            limit,
        });
    }
    Ok(())
}

/// Failure to compile an [`ExtractionPlan`] into a [`RecordProgram`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordCompileError {
    /// A configured compilation bound was exceeded.
    LimitExceeded {
        /// Bounded resource.
        resource: RecordResource,
        /// Actual or next required amount.
        actual: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A bounded compiler allocation failed.
    AllocationFailed {
        /// State being allocated.
        resource: RecordResource,
        /// Requested element or byte count.
        requested: usize,
    },
    /// Checked arithmetic or an index conversion overflowed.
    SizeOverflow,
    /// A non-empty operation stream has no implicit default-namespace object root.
    MissingDefaultRoot,
    /// The declared default root is not a root-level, empty-key object.
    InvalidDefaultRoot {
        /// Declared root node ID.
        node_id: u32,
        /// Actual node type.
        node_type: NodeType,
    },
    /// An operation references an absent schema-tree node.
    UnknownNode {
        /// Operation index, or `None` for the implicit root.
        operation_index: Option<usize>,
        /// Missing node ID.
        node_id: u32,
    },
    /// A structural operation disagrees with its schema-tree node type.
    NodeTypeMismatch {
        /// Operation index.
        operation_index: usize,
        /// Referenced node ID.
        node_id: u32,
        /// Type required by the operation.
        expected: NodeType,
        /// Type stored in the schema tree.
        actual: NodeType,
    },
    /// A value operation references a structural or unsupported node type.
    InvalidValueNodeType {
        /// Operation index.
        operation_index: usize,
        /// Referenced node ID.
        node_id: u32,
        /// Invalid value type.
        node_type: NodeType,
    },
    /// A named field appears in an array or an unnamed element appears in an object.
    PositionMismatch {
        /// Operation index.
        operation_index: usize,
        /// Referenced node ID.
        node_id: u32,
        /// Active parent container type.
        parent_type: NodeType,
        /// Requested placement.
        position: ExtractionPosition,
    },
    /// A close operation does not match the active explicit container.
    UnexpectedContainerEnd {
        /// Operation index.
        operation_index: usize,
        /// Container type requested by the close operation.
        expected: NodeType,
        /// Active type, or `None` when no container exists.
        actual: Option<NodeType>,
    },
    /// Explicit containers remain open after the final operation.
    UnclosedContainer {
        /// Index immediately after the final operation.
        operation_index: usize,
        /// Number of unclosed explicit containers.
        open_depth: usize,
    },
    /// A value operation's physical column index is absent.
    ColumnIndexOutOfBounds {
        /// Operation index.
        operation_index: usize,
        /// Referenced column index.
        column_index: usize,
        /// Complete physical column count.
        column_count: usize,
    },
    /// More than one value operation references the same physical column.
    DuplicateColumn {
        /// Later operation index.
        operation_index: usize,
        /// Reused physical column index.
        column_index: usize,
    },
    /// A schema key could not be validated or escaped within the program bound.
    JsonKey {
        /// Operation index.
        operation_index: usize,
        /// Key-bearing node ID.
        node_id: u32,
        /// JSON byte-string failure.
        source: JsonEscapeError,
    },
    /// This library does not understand a future extraction-operation variant.
    UnsupportedOperation {
        /// Operation index.
        operation_index: usize,
    },
}

impl Display for RecordCompileError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "{resource} requirement {actual} exceeds limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("record-program size overflow"),
            Self::MissingDefaultRoot => {
                formatter.write_str("non-empty record plan has no default object root")
            }
            Self::InvalidDefaultRoot { node_id, node_type } => write!(
                formatter,
                "record root node {node_id} is not a root-level empty-key object ({node_type:?})"
            ),
            Self::UnknownNode {
                operation_index,
                node_id,
            } => write!(
                formatter,
                "record operation {operation_index:?} references unknown node {node_id}"
            ),
            operation_error => fmt_compile_operation_error(operation_error, formatter),
        }
    }
}

fn fmt_compile_operation_error(
    error: &RecordCompileError,
    formatter: &mut Formatter<'_>,
) -> fmt::Result {
    match error {
        RecordCompileError::NodeTypeMismatch {
            operation_index,
            node_id,
            expected,
            actual,
        } => write!(
            formatter,
            "record operation {operation_index} expects node {node_id} to be {expected:?}, not \
             {actual:?}"
        ),
        RecordCompileError::InvalidValueNodeType {
            operation_index,
            node_id,
            node_type,
        } => write!(
            formatter,
            "record value operation {operation_index} uses non-value node {node_id} \
             ({node_type:?})"
        ),
        RecordCompileError::PositionMismatch {
            operation_index,
            node_id,
            parent_type,
            position,
        } => write!(
            formatter,
            "record operation {operation_index} places node {node_id} as {position:?} in \
             {parent_type:?}"
        ),
        RecordCompileError::UnexpectedContainerEnd {
            operation_index,
            expected,
            actual,
        } => write!(
            formatter,
            "record close operation {operation_index} expects {expected:?}, active={actual:?}"
        ),
        RecordCompileError::UnclosedContainer {
            operation_index,
            open_depth,
        } => write!(
            formatter,
            "record program ends at operation {operation_index} with {open_depth} open containers"
        ),
        RecordCompileError::ColumnIndexOutOfBounds {
            operation_index,
            column_index,
            column_count,
        } => write!(
            formatter,
            "record operation {operation_index} references column {column_index}, but only \
             {column_count} columns exist"
        ),
        RecordCompileError::DuplicateColumn {
            operation_index,
            column_index,
        } => write!(
            formatter,
            "record operation {operation_index} reuses column {column_index}"
        ),
        RecordCompileError::JsonKey {
            operation_index,
            node_id,
            source,
        } => write!(
            formatter,
            "record operation {operation_index} cannot encode key for node {node_id}: {source}"
        ),
        RecordCompileError::UnsupportedOperation { operation_index } => write!(
            formatter,
            "record operation {operation_index} uses an unsupported future variant"
        ),
        _ => Err(fmt::Error),
    }
}

impl Error for RecordCompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::JsonKey { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Failure to bind a compiled program to one decoded schema table.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordBindError {
    /// The table's physical column count disagrees with the extraction plan.
    ColumnCountMismatch {
        /// Compiled physical column count.
        expected: usize,
        /// Decoded table column count.
        actual: usize,
    },
    /// A referenced physical column belongs to a different schema-tree node.
    NodeIdMismatch {
        /// Physical column index.
        column_index: usize,
        /// Compiled node ID.
        expected: u32,
        /// Decoded node ID.
        actual: u32,
    },
    /// A referenced physical column has a different value type.
    NodeTypeMismatch {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Compiled node type.
        expected: NodeType,
        /// Decoded column type.
        actual: NodeType,
    },
    /// A configured writer bound cannot support the table or compiled literals.
    LimitExceeded {
        /// Bounded resource.
        resource: RecordResource,
        /// Required bytes or elements.
        required: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Transactional cursor allocation failed.
    AllocationFailed {
        /// State being allocated.
        resource: RecordResource,
        /// Requested element count.
        requested: usize,
    },
    /// The pre-v0.5 legacy date-pattern semantics are intentionally not implemented.
    UnsupportedDeprecatedDateString {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
    },
    /// This library does not understand a future decoded-column variant.
    UnsupportedColumnType {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Stable node type reported by the column.
        node_type: NodeType,
    },
}

impl Display for RecordBindError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ColumnCountMismatch { expected, actual } => write!(
                formatter,
                "record program expects {expected} columns, table has {actual}"
            ),
            Self::NodeIdMismatch {
                column_index,
                expected,
                actual,
            } => write!(
                formatter,
                "record column {column_index} expects node {expected}, table uses node {actual}"
            ),
            Self::NodeTypeMismatch {
                column_index,
                node_id,
                expected,
                actual,
            } => write!(
                formatter,
                "record column {column_index} for node {node_id} expects {expected:?}, table uses \
                 {actual:?}"
            ),
            Self::LimitExceeded {
                resource,
                required,
                limit,
            } => write!(
                formatter,
                "{resource} requirement {required} exceeds limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} units for {resource}"
            ),
            Self::UnsupportedDeprecatedDateString {
                column_index,
                node_id,
            } => write!(
                formatter,
                "record column {column_index} (node {node_id}) uses unsupported pre-v0.5 \
                 deprecated date semantics"
            ),
            Self::UnsupportedColumnType {
                column_index,
                node_id,
                node_type,
            } => write!(
                formatter,
                "record column {column_index} (node {node_id}) has unsupported type {node_type:?}"
            ),
        }
    }
}

impl Error for RecordBindError {}

/// Failure while appending one table row as JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordError {
    /// A configured output or scratch bound was exceeded.
    LimitExceeded {
        /// Bounded resource.
        resource: RecordResource,
        /// Required bytes.
        required: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A bounded output reservation failed.
    AllocationFailed {
        /// State being allocated.
        resource: RecordResource,
        /// Requested additional byte count.
        requested: usize,
    },
    /// Checked output-size or row-index arithmetic overflowed.
    SizeOverflow,
    /// A validated column unexpectedly has no value at the requested row.
    MissingColumnValue {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Requested row index.
        row_index: usize,
    },
    /// A quoted archive byte string could not be escaped.
    JsonString {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// JSON byte-string failure.
        source: JsonEscapeError,
    },
    /// An ordinary integer or binary64 value could not be formatted.
    JsonNumber {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Scalar-number failure.
        source: JsonNumberError,
    },
    /// An original formatted-float lexeme could not be restored.
    FormattedFloat {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Formatted-float failure.
        source: FormattedFloatError,
    },
    /// A CLP string or unstructured-array value could not be restored.
    ClpValue {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Encoded-variable failure.
        source: EncodedVariableError,
    },
    /// A current v0.5 timestamp could not be formatted.
    Timestamp {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Catalog lookup or timestamp-format failure.
        source: TimestampCatalogFormatError,
    },
    /// A raw dictionary-float or unstructured-array lexeme is not UTF-8 in strict mode.
    InvalidRawUtf8 {
        /// Physical column index.
        column_index: usize,
        /// Schema-tree node ID.
        node_id: u32,
        /// Bytes valid before the malformed sequence.
        valid_up_to: usize,
        /// Malformed sequence length, or `None` when truncated.
        error_len: Option<usize>,
    },
}

impl Display for RecordError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                resource,
                required,
                limit,
            } => write!(
                formatter,
                "{resource} requirement {required} exceeds limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} bytes for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("JSON record size overflow"),
            Self::MissingColumnValue {
                column_index,
                node_id,
                row_index,
            } => write!(
                formatter,
                "record column {column_index} (node {node_id}) has no row {row_index}"
            ),
            Self::JsonString {
                column_index,
                node_id,
                source,
            } => write!(
                formatter,
                "record string column {column_index} (node {node_id}) failed: {source}"
            ),
            Self::JsonNumber {
                column_index,
                node_id,
                source,
            } => write!(
                formatter,
                "record number column {column_index} (node {node_id}) failed: {source}"
            ),
            Self::FormattedFloat {
                column_index,
                node_id,
                source,
            } => write!(
                formatter,
                "record formatted-float column {column_index} (node {node_id}) failed: {source}"
            ),
            Self::ClpValue {
                column_index,
                node_id,
                source,
            } => write!(
                formatter,
                "record CLP column {column_index} (node {node_id}) failed: {source}"
            ),
            Self::Timestamp {
                column_index,
                node_id,
                source,
            } => write!(
                formatter,
                "record timestamp column {column_index} (node {node_id}) failed: {source}"
            ),
            Self::InvalidRawUtf8 {
                column_index,
                node_id,
                valid_up_to,
                error_len,
            } => write!(
                formatter,
                "raw record column {column_index} (node {node_id}) contains invalid UTF-8 at byte \
                 {valid_up_to} (length {error_len:?})"
            ),
        }
    }
}

impl Error for RecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::JsonString { source, .. } => Some(source),
            Self::JsonNumber { source, .. } => Some(source),
            Self::FormattedFloat { source, .. } => Some(source),
            Self::ClpValue { source, .. } => Some(source),
            Self::Timestamp { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Cursor;
    use std::path::PathBuf;

    use serde::Serialize;

    use super::*;
    use crate::ExtractionPlanLimits;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::ArchiveHeader;
    use crate::archive::ColumnLimits;
    use crate::archive::PackedStreamLimits;
    use crate::archive::SFA_SECTION_NAMES;
    use crate::archive::SingleFileArchiveReader;

    #[derive(Clone, Copy)]
    struct TestNode<'a> {
        parent_id: Option<usize>,
        key: &'a [u8],
        node_type: NodeType,
    }

    struct TestNodes<'a>(&'a [TestNode<'a>]);

    impl NodeSource for TestNodes<'_> {
        fn get(&self, node_id: usize) -> Option<NodeView<'_>> {
            self.0.get(node_id).map(|node| NodeView {
                parent_id: node.parent_id,
                key: node.key,
                node_type: node.node_type,
            })
        }
    }

    fn render_markers(program: &RecordProgram) -> Vec<u8> {
        let mut output = Vec::new();
        let mut start = 0;
        for step in &program.values {
            output.extend_from_slice(&program.literals[start..step.literal_end]);
            output.extend_from_slice(format!("<{}>", step.column_index).as_bytes());
            start = step.literal_end;
        }
        output.extend_from_slice(&program.literals[start..]);
        output
    }

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sfa-v0.5.0-minimal-cpp.bin")
    }

    #[derive(Serialize)]
    struct TestArchiveInfo {
        num_segments: u64,
    }

    #[derive(Serialize)]
    struct TestFileInfo<'a> {
        files: [TestFile<'a>; SFA_SECTION_NAMES.len()],
    }

    #[derive(Serialize)]
    struct TestFile<'a> {
        #[serde(rename = "n")]
        name: &'a str,
        #[serde(rename = "o")]
        offset: u64,
    }

    fn push_packet(output: &mut Vec<u8>, packet_type: u8, payload: &[u8]) {
        output.push(packet_type);
        output.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("test packet size fits u32")
                .to_le_bytes(),
        );
        output.extend_from_slice(payload);
    }

    fn zstd(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 1).expect("compress synthetic test section")
    }

    fn dictionary(entries: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for entry in entries {
            body.extend_from_slice(
                &u64::try_from(entry.len())
                    .expect("test entry size fits u64")
                    .to_le_bytes(),
            );
            body.extend_from_slice(entry);
        }
        let mut section = u64::try_from(entries.len())
            .expect("test entry count fits u64")
            .to_le_bytes()
            .to_vec();
        section.extend_from_slice(&zstd(&body));
        section
    }

    fn push_tree_node(bytes: &mut Vec<u8>, parent: i32, key: &[u8], node_type: NodeType) {
        bytes.extend_from_slice(&parent.to_le_bytes());
        bytes.extend_from_slice(
            &u64::try_from(key.len())
                .expect("test key size fits u64")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(key);
        bytes.push(node_type as u8);
    }

    fn all_value_schema_tree() -> Vec<u8> {
        let nodes = [
            (b"integer".as_slice(), NodeType::Integer),
            (b"delta".as_slice(), NodeType::DeltaInteger),
            (b"float".as_slice(), NodeType::Float),
            (b"formatted".as_slice(), NodeType::FormattedFloat),
            (b"dict_float".as_slice(), NodeType::DictionaryFloat),
            (b"boolean".as_slice(), NodeType::Boolean),
            (b"string".as_slice(), NodeType::VarString),
            (b"clp".as_slice(), NodeType::ClpString),
            (b"array".as_slice(), NodeType::UnstructuredArray),
            (b"timestamp".as_slice(), NodeType::Timestamp),
        ];
        let mut bytes = u64::try_from(nodes.len() + 1)
            .expect("test node count fits u64")
            .to_le_bytes()
            .to_vec();
        push_tree_node(&mut bytes, -1, b"", NodeType::Object);
        for (key, node_type) in nodes {
            push_tree_node(&mut bytes, 0, key, node_type);
        }
        bytes
    }

    fn all_value_schema_map() -> Vec<u8> {
        const VALUE_COUNT: u32 = 10;
        let mut bytes = 1_u64.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&VALUE_COUNT.to_le_bytes());
        bytes.extend_from_slice(&VALUE_COUNT.to_le_bytes());
        for node_id in 1_i32..=i32::try_from(VALUE_COUNT).expect("value count fits i32") {
            bytes.extend_from_slice(&node_id.to_le_bytes());
        }
        bytes
    }

    fn all_value_table() -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in [-1_i64, i64::MAX] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for delta in [10_i64, -3] {
            bytes.extend_from_slice(&delta.to_le_bytes());
        }
        for value in [1.25_f64, -0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in [8.25_f64, 8.25] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for descriptor in [(5_u16 - 1) << 5, (3_u16 - 1) << 5] {
            bytes.extend_from_slice(&descriptor.to_le_bytes());
        }
        for id in [0_u64, 1] {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes.extend_from_slice(&[1, 0]);
        for id in [2_u64, 3] {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        for descriptor in [0_u64, 1_u64 << 24] {
            bytes.extend_from_slice(&descriptor.to_le_bytes());
        }
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        for encoded in [42_i64, -7] {
            bytes.extend_from_slice(&encoded.to_le_bytes());
        }
        for descriptor in [0_u64, 0] {
            bytes.extend_from_slice(&descriptor.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        for delta in [1_700_000_000_123_000_000_i64, 1_000_000] {
            bytes.extend_from_slice(&delta.to_le_bytes());
        }
        for pattern_id in [0_u64, 0] {
            bytes.extend_from_slice(&pattern_id.to_le_bytes());
        }
        bytes
    }

    fn timestamp_dictionary() -> Vec<u8> {
        let mut bytes = 0_u64.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(br"\L");
        bytes
    }

    fn all_value_archive() -> Vec<u8> {
        let table = all_value_table();
        let packed_stream = zstd(&table);
        let mut table_metadata = 1_u64.to_le_bytes().to_vec();
        table_metadata.extend_from_slice(&0_u64.to_le_bytes());
        table_metadata.extend_from_slice(
            &u64::try_from(table.len())
                .expect("test table size fits u64")
                .to_le_bytes(),
        );
        table_metadata.extend_from_slice(&0_u64.to_le_bytes());
        table_metadata.extend_from_slice(&1_u64.to_le_bytes());
        table_metadata.extend_from_slice(&0_u64.to_le_bytes());
        table_metadata.extend_from_slice(&0_u64.to_le_bytes());
        table_metadata.extend_from_slice(&0_i32.to_le_bytes());
        table_metadata.extend_from_slice(&2_u64.to_le_bytes());

        let sections = [
            zstd(&all_value_schema_tree()),
            zstd(&all_value_schema_map()),
            zstd(&table_metadata),
            dictionary(&[b"1e+03", b"-0.0", b"a\"b", "snowman:\u{2603}".as_bytes()]),
            dictionary(&[b"msg \x11"]),
            dictionary(&[b"[1,true]"]),
            packed_stream,
        ];
        assemble_archive(&sections, &timestamp_dictionary())
    }

    fn assemble_archive(
        sections: &[Vec<u8>; SFA_SECTION_NAMES.len()],
        timestamp_dictionary: &[u8],
    ) -> Vec<u8> {
        let mut offset = 0_u64;
        let offsets: [u64; SFA_SECTION_NAMES.len()] = std::array::from_fn(|index| {
            let current = offset;
            offset += u64::try_from(sections[index].len()).expect("test section size fits u64");
            current
        });
        let archive_info = rmp_serde::to_vec_named(&TestArchiveInfo { num_segments: 1 })
            .expect("encode test archive info");
        let files = std::array::from_fn(|index| TestFile {
            name: SFA_SECTION_NAMES[index],
            offset: offsets[index],
        });
        let file_info =
            rmp_serde::to_vec_named(&TestFileInfo { files }).expect("encode test file info");
        let mut metadata_body = vec![3_u8];
        push_packet(&mut metadata_body, 0, &archive_info);
        push_packet(&mut metadata_body, 1, &file_info);
        push_packet(&mut metadata_body, 2, timestamp_dictionary);
        let metadata = zstd(&metadata_body);
        let archive_size =
            64_u64 + u64::try_from(metadata.len()).expect("test metadata size fits u64") + offset;
        let header = ArchiveHeader::new(
            0,
            archive_size,
            u32::try_from(metadata.len()).expect("test metadata size fits u32"),
        );
        let mut archive = header.encode().to_vec();
        archive.extend_from_slice(&metadata);
        for section in sections {
            archive.extend_from_slice(section);
        }
        archive
    }

    #[test]
    fn compiles_nested_punctuation_and_escaped_keys_once() {
        let nodes = [
            TestNode {
                parent_id: None,
                key: b"",
                node_type: NodeType::Object,
            },
            TestNode {
                parent_id: Some(0),
                key: b"ob\"j",
                node_type: NodeType::Object,
            },
            TestNode {
                parent_id: Some(1),
                key: b"x",
                node_type: NodeType::Integer,
            },
            TestNode {
                parent_id: Some(0),
                key: b"arr",
                node_type: NodeType::StructuredArray,
            },
            TestNode {
                parent_id: Some(3),
                key: b"",
                node_type: NodeType::Null,
            },
            TestNode {
                parent_id: Some(3),
                key: b"",
                node_type: NodeType::VarString,
            },
        ];
        let operations = [
            ExtractionOp::BeginObject {
                node_id: 1,
                position: ExtractionPosition::ObjectField,
            },
            ExtractionOp::Value {
                column_index: 0,
                node_id: 2,
                position: ExtractionPosition::ObjectField,
            },
            ExtractionOp::EndObject,
            ExtractionOp::BeginArray {
                node_id: 3,
                position: ExtractionPosition::ObjectField,
            },
            ExtractionOp::Null {
                node_id: 4,
                position: ExtractionPosition::ArrayElement,
            },
            ExtractionOp::Value {
                column_index: 1,
                node_id: 5,
                position: ExtractionPosition::ArrayElement,
            },
            ExtractionOp::EndArray,
        ];
        let program = compile_parts(
            PlanParts {
                schema_id: 9,
                root_node_id: Some(0),
                column_count: 2,
                operations: &operations,
            },
            &TestNodes(&nodes),
            JsonBytePolicy::StrictUtf8,
            RecordLimits::default(),
        )
        .expect("compile synthetic record program");

        assert_eq!(9, program.schema_id());
        assert_eq!(2, program.value_count());
        assert_eq!(
            br#"{"ob\"j":{"x":<0>},"arr":[null,<1>]}"#,
            render_markers(&program).as_slice()
        );
    }

    #[test]
    fn strict_keys_reject_invalid_utf8_and_preserve_mode_is_explicit() {
        let nodes = [
            TestNode {
                parent_id: None,
                key: b"",
                node_type: NodeType::Object,
            },
            TestNode {
                parent_id: Some(0),
                key: b"bad\xffkey",
                node_type: NodeType::Integer,
            },
        ];
        let operations = [ExtractionOp::Value {
            column_index: 0,
            node_id: 1,
            position: ExtractionPosition::ObjectField,
        }];
        let parts = || PlanParts {
            schema_id: 0,
            root_node_id: Some(0),
            column_count: 1,
            operations: &operations,
        };

        assert!(matches!(
            compile_parts(
                parts(),
                &TestNodes(&nodes),
                JsonBytePolicy::StrictUtf8,
                RecordLimits::default()
            ),
            Err(RecordCompileError::JsonKey {
                source: JsonEscapeError::InvalidUtf8 { valid_up_to: 3, .. },
                ..
            })
        ));
        let preserved = compile_parts(
            parts(),
            &TestNodes(&nodes),
            JsonBytePolicy::PreserveInvalidUtf8,
            RecordLimits::default(),
        )
        .expect("explicit C++ byte-preserving key mode");
        assert_eq!(
            b"{\"bad\xffkey\":<0>}",
            render_markers(&preserved).as_slice()
        );
    }

    #[test]
    fn extracts_the_committed_cpp_oracle_record_transactionally() {
        let fixture = File::open(fixture_path()).expect("open C++ oracle fixture");
        let mut archive = SingleFileArchiveReader::open(fixture).expect("open C++ archive");
        let catalog = archive
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read C++ archive catalog");
        let stream = archive
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read C++ packed stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open schema-table stream");
        let decoded = tables.next().expect("one table").expect("decode table");
        let plan = ExtractionPlan::compile(
            decoded.schema(),
            catalog.schema_tree(),
            ExtractionPlanLimits::default(),
        )
        .expect("compile extraction plan");
        let defaults = RecordLimits::default();
        let limited = RecordLimits::new(
            defaults.max_columns(),
            defaults.max_operations(),
            defaults.max_nesting_depth(),
            defaults.max_program_bytes(),
            47,
            defaults.max_scratch_bytes(),
        );
        let limited_program = RecordProgram::compile(&plan, catalog.schema_tree(), limited)
            .expect("compile output-limited record program");
        let mut limited_writer = limited_program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("static literals fit the record bound");
        let mut unchanged = b"prefix:".to_vec();
        for _ in 0..2 {
            assert!(limited_writer.append_next_record(&mut unchanged).is_err());
            assert_eq!(b"prefix:", unchanged.as_slice());
            assert_eq!(0, limited_writer.next_row_index());
        }

        let program = RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default())
            .expect("compile record program");
        let mut writer = program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("bind record writer");

        let mut output = b"prefix:".to_vec();
        assert!(
            writer
                .append_next_record(&mut output)
                .expect("extract C++ oracle row")
        );
        assert_eq!(
            include_bytes!("../tests/fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl")
                .strip_suffix(b"\n")
                .expect("fixture is JSONL"),
            &output[b"prefix:".len()..]
        );
        let complete = output.clone();
        assert!(
            !writer
                .append_next_record(&mut output)
                .expect("writer is exhausted")
        );
        assert_eq!(complete, output);
        assert_eq!(1, writer.next_row_index());
        assert_eq!(0, writer.remaining());
    }

    #[test]
    fn extracts_every_current_value_column_with_sequential_deltas() {
        let mut archive = SingleFileArchiveReader::open(Cursor::new(all_value_archive()))
            .expect("open synthetic all-value archive");
        let catalog = archive
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read synthetic all-value catalog");
        let stream = archive
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read synthetic all-value packed stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("open synthetic schema-table stream");
        let decoded = tables
            .next()
            .expect("one synthetic table")
            .expect("decode synthetic table");
        let plan = ExtractionPlan::compile(
            decoded.schema(),
            catalog.schema_tree(),
            ExtractionPlanLimits::default(),
        )
        .expect("compile all-value extraction plan");
        let program = RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default())
            .expect("compile all-value record program");
        let mut writer = program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("bind all-value writer");
        let expected = [
            concat!(
                r#"{"integer":-1,"delta":10,"float":1.250000,"formatted":8.2500,"#,
                r#""dict_float":1e+03,"boolean":true,"string":"a\"b","clp":"msg 42","#,
                r#""array":[1,true],"timestamp":1700000000123}"#,
            )
            .as_bytes(),
            concat!(
                r#"{"integer":9223372036854775807,"delta":7,"float":-0.000000,"#,
                r#""formatted":8.25,"dict_float":-0.0,"boolean":false,"#,
                r#""string":"snowman:☃","clp":"msg -7","array":[1,true],"#,
                r#""timestamp":1700000000124}"#,
            )
            .as_bytes(),
        ];

        let mut output = Vec::new();
        for expected_record in expected {
            assert!(
                writer
                    .append_next_record(&mut output)
                    .expect("extract current value kinds")
            );
            assert_eq!(expected_record, output.as_slice());
            output.clear();
        }
        assert!(
            !writer
                .append_next_record(&mut output)
                .expect("all-value writer exhausted")
        );

        let mut skipping_writer = program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("bind a second all-value writer");
        assert!(
            skipping_writer
                .skip_next_record()
                .expect("skip the first physical row")
        );
        assert_eq!(1, skipping_writer.next_row_index());
        assert_eq!(1, skipping_writer.remaining());
        assert!(
            skipping_writer
                .append_next_record(&mut output)
                .expect("format after advancing delta and timestamp cursors")
        );
        assert_eq!(expected[1], output.as_slice());
        output.clear();
        assert!(
            !skipping_writer
                .skip_next_record()
                .expect("skip reports exhaustion")
        );

        let mut bulk_skipping_writer = program
            .writer(decoded.table(), catalog.timestamp_patterns())
            .expect("bind a third all-value writer");
        assert!(
            bulk_skipping_writer
                .skip_records(2)
                .expect("skip both physical rows in one transaction")
        );
        assert_eq!(2, bulk_skipping_writer.next_row_index());
        assert_eq!(0, bulk_skipping_writer.remaining());
        assert!(
            !bulk_skipping_writer
                .skip_records(1)
                .expect("bulk skip reports exhaustion")
        );
        assert_eq!(2, bulk_skipping_writer.next_row_index());
    }

    #[test]
    fn raw_utf8_policy_and_record_limit_are_transactional() {
        let step = ValueStep {
            literal_end: 0,
            column_index: 4,
            node_id: 7,
        };
        let invalid = b"1\xff";
        let mut bytes = b"prefix".to_vec();
        let start = bytes.len();
        let mut output = BoundedRecordOutput {
            bytes: &mut bytes,
            start,
            limit: 8,
        };
        assert!(matches!(
            append_raw_bytes(invalid, step, JsonBytePolicy::StrictUtf8, &mut output),
            Err(RecordError::InvalidRawUtf8 { valid_up_to: 1, .. })
        ));
        assert_eq!(b"prefix", output.bytes.as_slice());
        append_raw_bytes(
            invalid,
            step,
            JsonBytePolicy::PreserveInvalidUtf8,
            &mut output,
        )
        .expect("preserve raw invalid bytes explicitly");
        assert_eq!(b"prefix1\xff", output.bytes.as_slice());
        assert!(matches!(
            output.append(b"1234567"),
            Err(RecordError::LimitExceeded {
                resource: RecordResource::RecordBytes,
                required: 9,
                limit: 8
            })
        ));
        assert_eq!(b"prefix1\xff", output.bytes.as_slice());
    }
}
