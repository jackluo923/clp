//! Columnar projected scanning over one archive.
//!
//! ABI v1's `clp_s_v1_search` delivers a complete JSON document per match. A caller that wants a
//! handful of scalar fields then pays to reconstruct, serialize, and reparse every other field in
//! the record. This module delivers typed values for the requested paths instead, reading them
//! straight out of the decoded columns.
//!
//! The scanner is a pull API over a push engine. `search_archive` drives a sink to completion,
//! which would force a caller to buffer an entire archive's matches, so the loop is reproduced
//! here one step at a time: the scanner owns the catalog and the current decompressed packed
//! stream, and materializes matched rows into an owned buffer until a batch is full. Nothing
//! borrowed from the catalog or the stream is retained across a call, which keeps the state
//! machine expressible without a self-referential struct, and no work happens on another thread.

use clp_s::ArchiveReader;
use clp_s::archive::ArchiveCatalog;
use clp_s::archive::ColumnData;
use clp_s::archive::ColumnTruncation;
use clp_s::archive::DecodedPackedStream;
use clp_s::archive::DecodedSchemaTable;
use clp_s::archive::NodeType;
use clp_s::archive::SchemaEntry;
use clp_s::archive::SchemaTableMetadata;
use clp_s::archive::SchemaTree;
use clp_s::archive::is_value_bearing;
use clp_s::LogOrderLocator;
use clp_s::archive::append_clp_message_bounded;
use clp_s::search::ArchiveSearchOptions;
use clp_s::search::DictionaryMatches;
use clp_s::search::ColumnNamespace;
use clp_s::search::ExpressionKind;
use clp_s::search::ParsedQuery;
use clp_s::search::PathComponent;

/// Rows materialized before a batch is handed back.
///
/// Large enough that recompiling the query once per batch is lost in the noise, small enough that
/// a query matching an entire stream does not hold the whole stream's values at once.
/// Rows buffered before a batch is handed back, as a ceiling checked between tables.
///
/// A batch ends on a table boundary rather than on an exact row count. Stopping mid-table would
/// force the next call to decode and re-match that table from the start, which is quadratic in a
/// table's matches; stopping between tables costs nothing, because `seek_to_table` resumes at the
/// next one without walking the tables before it. Memory is therefore bounded by one table's
/// matched rows, not by the whole query's.
const BATCH_ROWS: usize = 8192;

/// Bytes allowed for one reconstructed CLP string.
const CLP_SCRATCH_LIMIT: usize = 1024 * 1024;

/// Value kinds shared with the C header.
pub mod kind {
    pub const ABSENT: u32 = 0;
    pub const BOOLEAN: u32 = 1;
    pub const INTEGER: u32 = 2;
    pub const FLOAT: u32 = 3;
    pub const STRING: u32 = 4;
    pub const TIMESTAMP: u32 = 5;
    pub const UNSUPPORTED: u32 = 6;
}

/// One projected value, with text held as a span into the batch's arena.
///
/// `#[repr(C)]` because a batch of these is handed to C by pointer as `clp_s_v2_cell`; the two
/// declarations must agree field for field, which the size assertion below pins on this side.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Cell {
    pub kind: u32,
    pub integer: i64,
    pub real: f64,
    pub text_offset: usize,
    pub text_length: usize,
}

impl Cell {
    const fn absent() -> Self {
        Self {
            kind: kind::ABSENT,
            integer: 0,
            real: 0.0,
            text_offset: 0,
            text_length: 0,
        }
    }

    const fn scalar(kind: u32, integer: i64, real: f64) -> Self {
        Self {
            kind,
            integer,
            real,
            text_offset: 0,
            text_length: 0,
        }
    }
}

/// Failure while scanning one archive.
#[derive(Debug)]
pub enum ScanError {
    Archive(String),
    Projection(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Archive(message) | Self::Projection(message) => formatter.write_str(message),
        }
    }
}

/// Resolves one escaped dot path to every schema node that can carry its value.
///
/// A key may appear under more than one node type, so a path resolves to a list rather than a
/// single node: the same field can be an integer in one schema and a string in another. Object and
/// structured-array nodes are structural and carry no value, so they are skipped, which mirrors
/// how the core projection resolves the same descriptors.
fn resolve_path(tree: &SchemaTree, components: &[&str]) -> Vec<u32> {
    let mut resolved = Vec::new();
    let nodes = tree.nodes();
    let Some(mut current) = nodes.iter().position(|node| {
        node.parent_id().is_none()
            && NodeType::Metadata != node.node_type()
            && node.key_bytes().is_empty()
    }) else {
        return resolved;
    };
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        let mut next_object = None;
        for (child_id, child) in nodes.iter().enumerate() {
            if child.parent_id() != Some(current) || child.key_bytes() != component.as_bytes() {
                continue;
            }
            if last {
                if !matches!(
                    child.node_type(),
                    NodeType::Object | NodeType::StructuredArray
                ) && let Ok(id) = u32::try_from(child_id)
                {
                    resolved.push(id);
                }
            } else if NodeType::Object == child.node_type() {
                next_object = Some(child_id);
                break;
            }
        }
        if last {
            break;
        }
        let Some(next) = next_object else {
            break;
        };
        current = next;
    }
    resolved
}

/// Splits an escaped dot descriptor into its components.
///
/// A backslash escapes the following byte, so a key containing a literal dot survives the split.
fn split_descriptor(source: &str) -> Vec<String> {
    let mut components = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in source.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if '\\' == character {
            escaped = true;
        } else if '.' == character {
            components.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    components.push(current);
    components
}

/// A pull-based projected scan over one archive.
pub struct ProjectedScanner {
    reader: Box<dyn ArchiveReader>,
    catalog: ArchiveCatalog,
    parsed: ParsedQuery,
    options: ArchiveSearchOptions,
    /// Candidate schema nodes per requested field, in request order.
    field_nodes: Vec<Vec<u32>>,
    /// Sorted schema nodes the scan reads, or `None` to read every column.
    wanted_nodes: Option<Vec<u32>>,
    /// Dictionary matches this query has already resolved against this archive.
    ///
    /// The query is compiled once per packed stream and once per buffer refill, and resolving a
    /// string predicate reads every dictionary entry, so without this a string-filtered scan pays
    /// for the whole dictionary a dozen times over.
    dictionary_matches: DictionaryMatches,
    /// Log-event ranges the query's range-index selector accepts, when one governs every match.
    ///
    /// A row outside them cannot match, and rows are written in log order, so a column needs
    /// inflating only as far as the last row any range reaches.
    accepted_ranges: Option<Vec<(u64, u64)>>,
    field_count: usize,
    stream_index: usize,
    stream_count: usize,
    stream: Option<DecodedPackedStream>,
    /// Physical table index to resume from, archive-wide.
    table_cursor: usize,
    cells: Vec<Cell>,
    text: Vec<u8>,
    emit_cursor: usize,
    buffered_rows: usize,
    finished: bool,
}

/// Whether any node in `nodes` is read through a dictionary.
fn any_dictionary_backed(tree: &SchemaTree, nodes: &[u32]) -> bool {
    nodes.iter().any(|node_id| {
        tree.get(*node_id as usize).is_some_and(|node| {
            matches!(
                node.node_type(),
                NodeType::VarString
                    | NodeType::DictionaryFloat
                    | NodeType::ClpString
                    | NodeType::UnstructuredArray
            )
        })
    })
}

/// Every schema node the scan reads: the projected fields plus each node a predicate compares.
///
/// `None` means the set cannot be pinned down (a wildcard, an unfamiliar namespace or expression
/// kind), so the scan must load every column and every dictionary. A range-index selector reads
/// the log-order column, so that node joins the set when one is present.
fn wanted_nodes(parsed: &ParsedQuery, tree: &SchemaTree, field_nodes: &[Vec<u32>]) -> Option<Vec<u32>> {
    let mut nodes: Vec<u32> = field_nodes.iter().flatten().copied().collect();
    let mut needs_log_order = false;
    for node in parsed.nodes() {
        let path = match node.kind() {
            ExpressionKind::Predicate(predicate) => predicate.path(),
            ExpressionKind::List(list) => list.path(),
            ExpressionKind::Not { .. } | ExpressionKind::Boolean { .. } => continue,
            // ExpressionKind is non-exhaustive. A kind this code does not know
            // may read anything.
            _ => return None,
        };
        match path.namespace() {
            ColumnNamespace::Default => {}
            ColumnNamespace::RangeIndex => {
                needs_log_order = true;
                continue;
            }
            _ => return None,
        }
        if path.components().iter().any(PathComponent::is_wildcard) {
            return None;
        }
        let components: Vec<&str> = path.components().iter().map(PathComponent::value).collect();
        nodes.extend(resolve_path(tree, &components));
    }
    if needs_log_order {
        match LogOrderLocator::discover(tree) {
            Ok(Some(locator)) => nodes.push(locator.node_id()),
            _ => return None,
        }
    }
    nodes.sort_unstable();
    nodes.dedup();
    Some(nodes)
}

impl ProjectedScanner {
    pub fn open(
        reader: Box<dyn ArchiveReader>,
        parsed: ParsedQuery,
        options: &ArchiveSearchOptions,
        fields: &[String],
    ) -> Result<Self, ScanError> {
        let mut reader = reader;
        // Read everything but the dictionaries first: whether they are needed depends on the
        // schema tree, which is only known once the catalog is open.
        let catalog_limits = options.catalog().with_skip_dictionaries(true);
        let mut catalog = reader
            .read_catalog(catalog_limits)
            .map_err(|source| ScanError::Archive(format!("failed to read catalog: {source}")))?;
        let field_nodes = fields
            .iter()
            .map(|field| {
                let components = split_descriptor(field);
                let borrowed: Vec<&str> = components.iter().map(String::as_str).collect();
                resolve_path(catalog.schema_tree(), &borrowed)
            })
            .collect::<Vec<_>>();
        let mut options = *options;
        // A measurement hook: force the eager load so the two paths can be timed
        // in one binary. Not a supported setting.
        // Set-but-empty must read as off: a runner that forwards every variable
        // it knows passes the unset ones as empty strings.
        let force_eager = std::env::var("CLP_RUST_EAGER_DICTIONARIES")
            .is_ok_and(|value| !value.is_empty() && value != "0");
        let wanted_nodes = wanted_nodes(&parsed, catalog.schema_tree(), &field_nodes);
        // The dictionaries are decoded eagerly by default, and on a string-heavy archive that
        // costs more than reading the tables. A scan that only touches integers, floats,
        // booleans and timestamps never looks anything up, so it can leave them unloaded.
        let needs_dictionaries = wanted_nodes
            .as_deref()
            .is_none_or(|nodes| any_dictionary_backed(catalog.schema_tree(), nodes));
        if force_eager || needs_dictionaries {
            let limits = options.catalog();
            let variable = reader
                .read_variable_dictionary(catalog.metadata(), limits.variable_dictionary())
                .map_err(|source| ScanError::Archive(format!("failed to read var.dict: {source}")))?;
            let log_type = reader
                .read_log_type_dictionary(catalog.metadata(), limits.log_type_dictionary())
                .map_err(|source| ScanError::Archive(format!("failed to read log.dict: {source}")))?;
            let array = reader
                .read_array_dictionary(catalog.metadata(), limits.array_dictionary())
                .map_err(|source| ScanError::Archive(format!("failed to read array.dict: {source}")))?;
            catalog.set_dictionaries(variable, log_type, array);
        } else {
            // Nothing will look an ID up, so decoding need not check them either.
            options = options.with_columns(options.columns().with_dictionary_validation(false));
        }
        // Compiling here answers which source files the selector accepts once, rather than once
        // per stream, and the answer outlives the compiled query it came from.
        let accepted_ranges = parsed
            .compile_for_archive(&catalog, options.search())
            .map_err(|source| ScanError::Archive(format!("failed to compile query: {source}")))?
            .accepted_log_event_ranges()
            .map_err(|source| {
                ScanError::Archive(format!("failed to read the range index: {source}"))
            })?;
        let stream_count = catalog.table_metadata().packed_streams().len();
        Ok(Self {
            reader,
            catalog,
            parsed,
            options,
            field_count: fields.len(),
            field_nodes,
            wanted_nodes,
            dictionary_matches: DictionaryMatches::new(),
            accepted_ranges,
            stream_index: 0,
            stream_count,
            stream: None,
            table_cursor: 0,
            cells: Vec::new(),
            text: Vec::new(),
            emit_cursor: 0,
            buffered_rows: 0,
            finished: false,
        })
    }

    /// Returns every buffered row not yet emitted, refilling first if none remain.
    ///
    /// The same rows `next_row` would return one at a time. A caller that consumes the batch
    /// must not mix the two, since both advance the same cursor.
    pub fn next_batch(&mut self) -> Result<Option<ProjectedBatch<'_>>, ScanError> {
        while self.emit_cursor >= self.buffered_rows {
            if self.finished {
                return Ok(None);
            }
            self.fill_batch()?;
        }
        let start = self.emit_cursor * self.field_count;
        let end = self.buffered_rows * self.field_count;
        self.emit_cursor = self.buffered_rows;
        Ok(Some((&self.cells[start..end], &self.text)))
    }

    /// Returns the next row's cells, or `None` once the archive is exhausted.
    pub fn next_row(&mut self) -> Result<Option<ProjectedRow<'_>>, ScanError> {
        while self.emit_cursor >= self.buffered_rows {
            if self.finished {
                return Ok(None);
            }
            self.fill_batch()?;
        }
        let start = self.emit_cursor * self.field_count;
        let end = start + self.field_count;
        self.emit_cursor += 1;
        Ok(Some((&self.cells[start..end], &self.text)))
    }

    /// Materializes up to `BATCH_ROWS` matched rows into the owned buffer.
    ///
    /// The query is recompiled once per call rather than once per archive, because a compiled
    /// query borrows the catalog and cannot be held across an FFI boundary. Amortized over a
    /// batch, that compile is not measurable next to decompressing a packed stream.
    fn fill_batch(&mut self) -> Result<(), ScanError> {
        self.cells.clear();
        self.text.clear();
        self.emit_cursor = 0;
        self.buffered_rows = 0;

        while self.buffered_rows < BATCH_ROWS {
            if self.stream.is_none() {
                if self.stream_index >= self.stream_count {
                    self.finished = true;
                    return Ok(());
                }
                if !self.load_next_stream()? {
                    continue;
                }
            }
            if self.drain_current_stream()? {
                // The stream still has rows but the buffer is full.
                return Ok(());
            }
            self.stream = None;
            self.stream_index += 1;
        }
        Ok(())
    }

    /// Reads the next packed stream.
    ///
    /// Every stream is read. A range-index predicate cannot rule one out from metadata alone: an
    /// entry's bounds are log event indices, while a stream covers a physical span, and the two
    /// orders differ as soon as an archive holds more than one schema. Deciding without reading
    /// would need each row's `log_event_idx`, which is inside the stream being skipped.
    fn load_next_stream(&mut self) -> Result<bool, ScanError> {
        let stream_id = self.stream_index as u64;
        let mask = self.projection_mask(stream_id);
        let truncate = match mask.as_deref() {
            Some(wanted) => self.plan_truncation(stream_id, wanted),
            None => None,
        };
        let total_rows = self
            .stream_table(stream_id)
            .and_then(|table| usize::try_from(table.message_count()).ok())
            .unwrap_or(0);
        let stream = match mask.as_deref() {
            Some(wanted) => self.reader.read_packed_stream_projected(
                self.catalog.metadata(),
                self.catalog.table_metadata(),
                self.stream_index,
                wanted,
                truncate
                    .as_ref()
                    .map(|(log_order_column, limit, mask)| ColumnTruncation {
                        log_order_column: *log_order_column,
                        limit: *limit,
                        total_rows,
                        truncatable: mask,
                    }),
                self.options.packed_stream(),
            ),
            None => self.reader.read_packed_stream(
                self.catalog.metadata(),
                self.catalog.table_metadata(),
                self.stream_index,
                self.options.packed_stream(),
            ),
        }
        .map_err(|source| {
            ScanError::Archive(format!(
                "failed to read packed stream {stream_id}: {source}"
            ))
        })?;
        self.stream = Some(stream);
        Ok(true)
    }

    /// Which value-bearing columns of a separate-column stream the scan reads, in column order.
    ///
    /// `None` when the stream shares one frame across its tables, or when the wanted set is
    /// unknown, in which case every column is inflated.
    /// Plans how far into this stream's columns the scan needs to read.
    ///
    /// Costs nothing but metadata: the reader stops each column at the row where the log-order
    /// column, which it reads first either way, reaches the first log event no accepted source
    /// file covers.
    fn plan_truncation(&self, stream_id: u64, wanted: &[bool]) -> Option<(usize, u64, Vec<bool>)> {
        let accepted = self.accepted_ranges.as_ref()?;
        let limit = accepted.iter().map(|(_, end)| *end).max()?;
        // A selector reaching the newest source file leaves nothing after it to skip, which is the
        // usual shape of a dashboard query, so it plans nothing rather than paying to find out.
        let last = self
            .catalog
            .metadata()
            .range_index()
            .map(|index| {
                index
                    .entries()
                    .iter()
                    .map(clp_s::archive::RangeIndexEntry::end_index)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        if limit >= last {
            return None;
        }
        let nodes = self.column_nodes(stream_id)?;
        let tree = self.catalog.schema_tree();
        let log_order = LogOrderLocator::discover(tree).ok()??.node_id();
        let log_order_column = nodes.iter().position(|node| *node == log_order)?;
        let mut truncatable = Vec::with_capacity(nodes.len());
        for (index, node_id) in nodes.iter().enumerate() {
            let fixed = tree
                .get(*node_id as usize)
                .is_some_and(|node| Self::truncatable(node.node_type()));
            truncatable.push(
                fixed && index != log_order_column && wanted.get(index).copied() == Some(true),
            );
        }
        truncatable
            .iter()
            .any(|value| *value)
            .then_some((log_order_column, limit, truncatable))
    }

    fn projection_mask(&self, stream_id: u64) -> Option<Vec<bool>> {
        let wanted = self.wanted_nodes.as_deref()?;
        self.column_nodes(stream_id)
            .map(|nodes| nodes.iter().map(|id| wanted.binary_search(id).is_ok()).collect())
    }

    /// Returns the schema node behind each value-bearing column of a separate-column stream's
    /// single table, in the order the stream stores them.
    fn column_nodes(&self, stream_id: u64) -> Option<Vec<u32>> {
        let table_metadata = self.catalog.table_metadata();
        table_metadata.separate_columns_for(stream_id)?;
        let table = self.stream_table(stream_id)?;
        let schema = self.catalog.schema_map().get(table.schema_id())?;
        let tree = self.catalog.schema_tree();
        let mut nodes = Vec::new();
        for entry in schema.entries() {
            let SchemaEntry::Node(node_id) = *entry else {
                continue;
            };
            let node = tree.get(node_id as usize)?;
            if is_value_bearing(node.node_type()) {
                nodes.push(node_id);
            }
        }
        Some(nodes)
    }

    fn stream_table(&self, stream_id: u64) -> Option<&SchemaTableMetadata> {
        let tables = self.catalog.table_metadata().schema_tables();
        let first = tables.partition_point(|table| table.stream_id() < stream_id);
        tables
            .get(first)
            .filter(|table| table.stream_id() == stream_id)
    }

    /// Whether a column of this type can be read as a prefix of its bytes.
    ///
    /// Three things have to hold. A row's byte offset must follow from the column's size, so the
    /// layout has to be one fixed-width value per row. That layout must be a single run of values,
    /// since a column stored as two sections back to back would have its second section cut away
    /// entirely rather than shortened, which is why timestamps, stored as deltas then pattern IDs,
    /// and formatted floats, stored as values then descriptors, are read whole. And the unread
    /// tail must stay structurally valid as zeroes, which rules out the dictionary-backed columns
    /// whose IDs are checked against a dictionary while decoding.
    const fn truncatable(node_type: NodeType) -> bool {
        matches!(
            node_type,
            NodeType::Integer | NodeType::DeltaInteger | NodeType::Float | NodeType::Boolean
        )
    }

    /// Projects the current stream's matched rows in one forward pass.
    ///
    /// Returns whether the stream still holds unread rows, which happens only when the buffer
    /// ceiling is reached part way through.
    fn drain_current_stream(&mut self) -> Result<bool, ScanError> {
        let stream_id = self.stream_index as u64;
        let Some(stream) = self.stream.as_ref() else {
            return Ok(false);
        };
        let compiled = self
            .parsed
            .compile_for_archive_reusing(
                &self.catalog,
                self.options.search(),
                &mut self.dictionary_matches,
            )
            .map_err(|source| ScanError::Archive(format!("failed to compile query: {source}")))?;
        let mut tables = self
            .catalog
            .schema_tables(stream_id, stream, self.options.columns())
            .map_err(|source| {
                ScanError::Archive(format!("failed to open stream {stream_id}: {source}"))
            })?;
        // Resume where the last batch stopped without decoding the tables in between.
        if !tables.seek_to_table(self.table_cursor) {
            return Ok(false);
        }

        let mut cells = std::mem::take(&mut self.cells);
        let mut text = std::mem::take(&mut self.text);
        let mut buffered = self.buffered_rows;
        let mut table_cursor = self.table_cursor;
        let mut stream_has_more = false;

        for decoded in tables {
            let decoded = decoded.map_err(|source| {
                ScanError::Archive(format!("failed to decode table: {source}"))
            })?;
            let bitmap = compiled
                .match_table(&decoded)
                .map_err(|source| ScanError::Archive(format!("failed to match table: {source}")))?;
            if 0 == bitmap.match_count() {
                table_cursor = decoded.table_index() + 1;
                continue;
            }
            let columns = self.column_slots(&decoded);
            for row in bitmap.matching_rows() {
                project_row(&decoded, &columns, row, &mut cells, &mut text)?;
                buffered += 1;
            }
            table_cursor = decoded.table_index() + 1;
            if buffered >= BATCH_ROWS {
                stream_has_more = true;
                break;
            }
        }

        self.cells = cells;
        self.text = text;
        self.buffered_rows = buffered;
        self.table_cursor = table_cursor;
        Ok(stream_has_more)
    }

    /// Maps each requested field to a column index in this table, when it has one.
    fn column_slots(&self, decoded: &DecodedSchemaTable<'_, '_>) -> Vec<Option<usize>> {
        let table = decoded.table();
        self.field_nodes
            .iter()
            .map(|candidates| {
                table
                    .columns()
                    .iter()
                    .position(|column| candidates.iter().any(|node| *node == column.node_id()))
            })
            .collect()
    }
}

/// Reads one row's projected values out of the decoded columns.
fn project_row(
    decoded: &DecodedSchemaTable<'_, '_>,
    columns: &[Option<usize>],
    row: usize,
    cells: &mut Vec<Cell>,
    text: &mut Vec<u8>,
) -> Result<(), ScanError> {
    let table = decoded.table();
    for slot in columns {
        let Some(column_index) = *slot else {
            cells.push(Cell::absent());
            continue;
        };
        let Some(column) = table.column(column_index) else {
            cells.push(Cell::absent());
            continue;
        };
        let cell = match column.data() {
            ColumnData::Integer(values) => values.get(row).map_or_else(Cell::absent, |value| {
                Cell::scalar(kind::INTEGER, value, 0.0)
            }),
            ColumnData::DeltaInteger(values) => {
                values.get(row).map_or_else(Cell::absent, |value| {
                    Cell::scalar(kind::INTEGER, value, 0.0)
                })
            }
            ColumnData::Float(values) => values
                .get(row)
                .map_or_else(Cell::absent, |value| Cell::scalar(kind::FLOAT, 0, value)),
            ColumnData::FormattedFloat(values) => {
                values.get(row).map_or_else(Cell::absent, |value| {
                    Cell::scalar(kind::FLOAT, 0, value.value())
                })
            }
            ColumnData::Boolean(values) => values.get(row).map_or_else(Cell::absent, |value| {
                Cell::scalar(kind::BOOLEAN, i64::from(value), 0.0)
            }),
            ColumnData::Timestamp(values) => values.get(row).map_or_else(Cell::absent, |value| {
                Cell::scalar(kind::TIMESTAMP, value.epoch_nanoseconds(), 0.0)
            }),
            ColumnData::VarString(values) | ColumnData::DictionaryFloat(values) => values
                .value(row)
                .map_or_else(Cell::absent, |bytes| push_text(kind::STRING, bytes, text)),
            ColumnData::ClpString(values) | ColumnData::UnstructuredArray(values) => {
                match values.record(row) {
                    Some(record) => {
                        let start = text.len();
                        append_clp_message_bounded(
                            record.logtype(),
                            values.variable_dictionary(),
                            &record.encoded_variables(),
                            text,
                            CLP_SCRATCH_LIMIT,
                        )
                        .map_err(|source| {
                            ScanError::Projection(format!("failed to decode CLP string: {source}"))
                        })?;
                        Cell {
                            kind: kind::STRING,
                            integer: 0,
                            real: 0.0,
                            text_offset: start,
                            text_length: text.len() - start,
                        }
                    }
                    None => Cell::absent(),
                }
            }
            // Legacy date strings and variants added upstream carry no value this ABI can
            // represent faithfully. Reporting them as unsupported keeps "absent" meaning "the
            // path is not here" and prevents new column types from looking like missing paths.
            _ => Cell::scalar(kind::UNSUPPORTED, 0, 0.0),
        };
        cells.push(cell);
    }
    Ok(())
}

pub type ProjectedRow<'a> = (&'a [Cell], &'a [u8]);

/// A run of projected rows: `cells` holds `row_count * field_count` cells in row-major order.
pub type ProjectedBatch<'a> = (&'a [Cell], &'a [u8]);

const _: () = assert!(std::mem::size_of::<Cell>() == 40, "Cell layout is part of the C ABI");

/// Appends borrowed bytes to the batch arena and returns a cell spanning them.
fn push_text(kind: u32, bytes: &[u8], text: &mut Vec<u8>) -> Cell {
    let start = text.len();
    text.extend_from_slice(bytes);
    Cell {
        kind,
        integer: 0,
        real: 0.0,
        text_offset: start,
        text_length: bytes.len(),
    }
}
