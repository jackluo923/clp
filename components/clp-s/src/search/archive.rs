//! High-level physical-order search orchestration over a format-independent archive reader.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::iter::FusedIterator;

use super::CompiledQuery;
use super::MatchBitmap;
use super::MatchingRows;
use super::ParsedQuery;
use super::SearchError;
use super::SearchOptions;
use crate::ArchiveReader;
use crate::archive::ArchiveCatalog;
use crate::archive::ArchiveCatalogError;
use crate::archive::ArchiveCatalogLimits;
use crate::archive::ColumnLimits;
use crate::archive::DecodedSchemaTable;
use crate::archive::PackedStreamError;
use crate::archive::PackedStreamLimits;
use crate::archive::TableStreamError;

/// Limits and semantic options for one archive-wide search.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveSearchOptions {
    catalog: ArchiveCatalogLimits,
    packed_stream: PackedStreamLimits,
    columns: ColumnLimits,
    search: SearchOptions,
}

impl ArchiveSearchOptions {
    /// Replaces the limits for loading and cross-validating non-table archive state.
    #[must_use]
    pub const fn with_catalog(mut self, limits: ArchiveCatalogLimits) -> Self {
        self.catalog = limits;
        self
    }

    /// Replaces the per-packed-stream decompression limits.
    #[must_use]
    pub const fn with_packed_stream(mut self, limits: PackedStreamLimits) -> Self {
        self.packed_stream = limits;
        self
    }

    /// Replaces the per-schema-table column-decoding limits.
    #[must_use]
    pub const fn with_columns(mut self, limits: ColumnLimits) -> Self {
        self.columns = limits;
        self
    }

    /// Replaces archive semantic compilation and matching options.
    #[must_use]
    pub const fn with_search(mut self, options: SearchOptions) -> Self {
        self.search = options;
        self
    }

    /// Returns the non-table catalog limits.
    #[must_use]
    pub const fn catalog(self) -> ArchiveCatalogLimits {
        self.catalog
    }

    /// Returns the packed-stream decompression limits.
    #[must_use]
    pub const fn packed_stream(self) -> PackedStreamLimits {
        self.packed_stream
    }

    /// Returns the schema-table column limits.
    #[must_use]
    pub const fn columns(self) -> ColumnLimits {
        self.columns
    }

    /// Returns archive semantic compilation and matching options.
    #[must_use]
    pub const fn search(self) -> SearchOptions {
        self.search
    }
}

/// Aggregate work completed by a successful archive-wide search.
///
/// A query proven impossible from catalog metadata returns all-zero work statistics because no
/// packed stream, table, or physical row was evaluated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveSearchStats {
    streams_scanned: u64,
    decoded_stream_bytes: u64,
    tables_scanned: u64,
    rows_scanned: u64,
    matching_rows: u64,
}

impl ArchiveSearchStats {
    /// Returns the number of packed streams read and decoded. A stream none of whose tables can
    /// match is skipped unread.
    #[must_use]
    pub const fn streams_scanned(self) -> u64 {
        self.streams_scanned
    }

    /// Returns the sum of decoded packed-stream byte lengths.
    #[must_use]
    pub const fn decoded_stream_bytes(self) -> u64 {
        self.decoded_stream_bytes
    }

    /// Returns the number of schema tables evaluated. A table whose schema cannot match is not.
    #[must_use]
    pub const fn tables_scanned(self) -> u64 {
        self.tables_scanned
    }

    /// Returns the number of physical rows covered: evaluated, or ruled out with their table
    /// because its schema cannot match.
    #[must_use]
    pub const fn rows_scanned(self) -> u64 {
        self.rows_scanned
    }

    /// Returns the number of rows accepted by the query.
    #[must_use]
    pub const fn matching_rows(self) -> u64 {
        self.matching_rows
    }

    fn add_stream(&mut self, decoded_bytes: usize) -> Result<(), ArchiveSearchError> {
        self.streams_scanned = self
            .streams_scanned
            .checked_add(1)
            .ok_or(ArchiveSearchError::SizeOverflow)?;
        self.decoded_stream_bytes = self
            .decoded_stream_bytes
            .checked_add(
                u64::try_from(decoded_bytes).map_err(|_| ArchiveSearchError::SizeOverflow)?,
            )
            .ok_or(ArchiveSearchError::SizeOverflow)?;
        Ok(())
    }

    /// Counts a table whose schema the query cannot match: its rows are covered without being
    /// evaluated, so record positions after it stay correct.
    fn add_skipped_table(&mut self, rows: u64) -> Result<(), ArchiveSearchError> {
        self.rows_scanned = self
            .rows_scanned
            .checked_add(rows)
            .ok_or(ArchiveSearchError::SizeOverflow)?;
        Ok(())
    }

    fn add_table(&mut self, rows: usize, matches: usize) -> Result<(), ArchiveSearchError> {
        self.tables_scanned = self
            .tables_scanned
            .checked_add(1)
            .ok_or(ArchiveSearchError::SizeOverflow)?;
        self.rows_scanned = self
            .rows_scanned
            .checked_add(u64::try_from(rows).map_err(|_| ArchiveSearchError::SizeOverflow)?)
            .ok_or(ArchiveSearchError::SizeOverflow)?;
        self.matching_rows = self
            .matching_rows
            .checked_add(u64::try_from(matches).map_err(|_| ArchiveSearchError::SizeOverflow)?)
            .ok_or(ArchiveSearchError::SizeOverflow)?;
        Ok(())
    }
}

/// One matching physical row and its borrowed zero-copy table.
#[derive(Clone, Copy, Debug)]
pub struct ArchiveRowRef<'table, 'stream, 'archive> {
    catalog: &'archive ArchiveCatalog,
    table: &'table DecodedSchemaTable<'stream, 'archive>,
    stream_id: u64,
    row_index: usize,
}

impl<'table, 'stream, 'archive> ArchiveRowRef<'table, 'stream, 'archive> {
    /// Returns the validated archive catalog backing this row.
    #[must_use]
    pub const fn catalog(self) -> &'archive ArchiveCatalog {
        self.catalog
    }

    /// Returns the decoded zero-copy schema table backing this row.
    #[must_use]
    pub const fn table(self) -> &'table DecodedSchemaTable<'stream, 'archive> {
        self.table
    }

    /// Returns the packed-stream ID containing this row.
    #[must_use]
    pub const fn stream_id(self) -> u64 {
        self.stream_id
    }

    /// Returns the table's global index in physical archive order.
    #[must_use]
    pub const fn table_index(self) -> usize {
        self.table.table_index()
    }

    /// Returns the opaque schema ID of the containing table.
    #[must_use]
    pub const fn schema_id(self) -> i32 {
        self.table.schema().id()
    }

    /// Returns the physical row index within the containing schema table.
    #[must_use]
    pub const fn row_index(self) -> usize {
        self.row_index
    }
}

/// Iterator over matching borrowed rows in one physical schema table.
#[derive(Clone, Debug)]
pub struct ArchiveMatchingRows<'table, 'stream, 'archive> {
    catalog: &'archive ArchiveCatalog,
    table: &'table DecodedSchemaTable<'stream, 'archive>,
    stream_id: u64,
    rows: MatchingRows<'table>,
}

impl<'table, 'stream, 'archive> Iterator for ArchiveMatchingRows<'table, 'stream, 'archive> {
    type Item = ArchiveRowRef<'table, 'stream, 'archive>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next().map(|row_index| ArchiveRowRef {
            catalog: self.catalog,
            table: self.table,
            stream_id: self.stream_id,
            row_index,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl ExactSizeIterator for ArchiveMatchingRows<'_, '_, '_> {}
impl FusedIterator for ArchiveMatchingRows<'_, '_, '_> {}

/// Borrowed match result for one physical schema table.
///
/// The batch is valid only for the synchronous sink call. It gives projection and aggregation
/// layers direct access to typed zero-copy columns; the search engine never serializes a row as
/// JSON or retains an archive-sized coordinate collection.
#[derive(Clone, Copy, Debug)]
pub struct ArchiveTableMatches<'table, 'stream, 'archive> {
    catalog: &'archive ArchiveCatalog,
    table: &'table DecodedSchemaTable<'stream, 'archive>,
    bitmap: &'table MatchBitmap,
    stream_id: u64,
    archive_record_start: u64,
}

impl<'table, 'stream, 'archive> ArchiveTableMatches<'table, 'stream, 'archive> {
    /// Returns the validated archive catalog backing this table.
    #[must_use]
    pub const fn catalog(self) -> &'archive ArchiveCatalog {
        self.catalog
    }

    /// Returns the decoded zero-copy schema table.
    #[must_use]
    pub const fn table(self) -> &'table DecodedSchemaTable<'stream, 'archive> {
        self.table
    }

    /// Returns the packed-stream ID containing this table.
    #[must_use]
    pub const fn stream_id(self) -> u64 {
        self.stream_id
    }

    /// Returns the table's global index in physical archive order.
    #[must_use]
    pub const fn table_index(self) -> usize {
        self.table.table_index()
    }

    /// Returns the opaque schema ID of the table.
    #[must_use]
    pub const fn schema_id(self) -> i32 {
        self.table.schema().id()
    }

    /// Returns the first row's zero-based position in physical archive table order.
    #[must_use]
    pub const fn archive_record_start(self) -> u64 {
        self.archive_record_start
    }

    /// Returns the complete table-local match bitmap.
    #[must_use]
    pub const fn bitmap(self) -> &'table MatchBitmap {
        self.bitmap
    }

    /// Iterates borrowed matching rows in table-local physical order without allocation.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn matching_rows(self) -> ArchiveMatchingRows<'table, 'stream, 'archive> {
        ArchiveMatchingRows {
            catalog: self.catalog,
            table: self.table,
            stream_id: self.stream_id,
            rows: self.bitmap.matching_rows(),
        }
    }
}

/// Synchronous borrowed match destination used by archive-wide search.
///
/// After metadata, timestamp-index, and schema preflight accepts an archive, the engine invokes
/// [`ArchiveMatchSink::begin_archive`] exactly once. It then invokes the sink once for each
/// physical schema table containing at least one match, in packed-stream and table order.
/// Implementations may project typed columns, update reducers, lazily open an output, or bridge
/// rows into an FFI callback. Borrowed archive data cannot outlive the call.
pub trait ArchiveMatchSink {
    /// Begins an archive that survived every pre-output metadata and schema check.
    ///
    /// This hook runs before packed-stream decoding and before stronger dictionary-backed value
    /// pruning. Its default implementation is a no-op, preserving source compatibility for sinks
    /// that need only nonempty match batches.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when per-archive output setup fails.
    fn begin_archive(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Consumes one nonempty table-local match batch.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the destination cannot accept the batch. The sink may already
    /// have accepted matches from earlier tables.
    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()>;
}

/// Searches every physical schema table in one archive without materializing decoded JSON.
///
/// The parsed query is archive-independent and may be reused across calls. This function loads
/// one validated catalog, compiles the query exactly once against it, and then retains only one
/// decompressed packed stream plus one table bitmap at a time. Results follow the C++ physical
/// packed-stream, schema-table, and row ordering rather than log-event order. A direct root
/// predicate proven impossible from schema and dictionary metadata returns before packed-stream
/// decompression, as does a wholly unresolved predicate directly below root `NOT`. The sink's
/// begin hook separates C++'s pre-output metadata/schema pruning from later dictionary pruning;
/// all expressions that cannot be proven empty take the ordinary evaluator path.
///
/// # Errors
///
/// Returns a contextual error for catalog loading, archive semantic compilation, packed-stream or
/// schema-table decoding, table matching, physical-order inconsistencies, checked arithmetic, or
/// sink output.
pub fn search_archive<A: ArchiveReader + ?Sized, S: ArchiveMatchSink + ?Sized>(
    reader: &mut A,
    query: &ParsedQuery,
    sink: &mut S,
    options: &ArchiveSearchOptions,
) -> Result<ArchiveSearchStats, ArchiveSearchError> {
    // The dictionaries are usually most of an archive, and a query rarely needs all of them: the
    // filter needs only those its compared columns are read through, and the rest matter only
    // once a row matches and must be written out. So the catalog is read without them, the
    // filter's are loaded before compiling, and the others when the first match appears.
    let mut catalog = reader
        .read_catalog(options.catalog.with_skip_dictionaries(true))
        .map_err(ArchiveSearchError::Catalog)?;
    let mut loaded = Dictionaries::NONE;
    let filter = filter_dictionaries(query, catalog.schema_tree());
    load_dictionaries(reader, &mut catalog, &options.catalog, filter, &mut loaded)?;

    let mut scan = ScanPosition::default();
    let mut sink_begun = false;
    loop {
        let compiled = query
            .compile_for_archive(&catalog, options.search)
            .map_err(ArchiveSearchError::Compile)?;
        if !sink_begun {
            if !compiled
                .reaches_match_sink()
                .map_err(ArchiveSearchError::Compile)?
            {
                return Ok(ArchiveSearchStats::default());
            }
            sink.begin_archive()
                .map_err(ArchiveSearchError::BeginSink)?;
            sink_begun = true;
            if !compiled.may_match_archive() {
                return Ok(ArchiveSearchStats::default());
            }
        }
        let outcome = scan_streams(
            reader,
            &catalog,
            &compiled,
            sink,
            options,
            Dictionaries::ALL == loaded,
            &mut scan,
        )?;
        drop(compiled);
        match outcome {
            ScanOutcome::Finished => break,
            // A match needs every dictionary to be written out. Load the rest and resume at the
            // stream holding the match, which wrote nothing before stopping.
            ScanOutcome::NeedsDictionaries => load_dictionaries(
                reader,
                &mut catalog,
                &options.catalog,
                Dictionaries::ALL,
                &mut loaded,
            )?,
        }
    }

    let ScanPosition {
        stats,
        expected_table_index,
        ..
    } = scan;
    let expected_tables = catalog.table_metadata().schema_tables().len();
    if expected_table_index != expected_tables {
        return Err(ArchiveSearchError::TableCountMismatch {
            expected: expected_tables,
            actual: expected_table_index,
        });
    }
    let expected_rows = catalog.table_metadata().record_count();
    if stats.rows_scanned != expected_rows {
        return Err(ArchiveSearchError::RecordCountMismatch {
            expected: expected_rows,
            actual: stats.rows_scanned,
        });
    }
    Ok(stats)
}

/// Where a scan has reached, kept across a restart so finished streams are not read again.
#[derive(Clone, Copy, Default)]
struct ScanPosition {
    stream_index: usize,
    stats: ArchiveSearchStats,
    expected_table_index: usize,
}

enum ScanOutcome {
    Finished,
    /// A table in the stream at the saved position matched while dictionaries were missing. The
    /// position is that stream's start, and nothing from it has reached the sink.
    NeedsDictionaries,
}

fn scan_streams<A: ArchiveReader + ?Sized, S: ArchiveMatchSink + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    compiled: &CompiledQuery<'_, '_>,
    sink: &mut S,
    options: &ArchiveSearchOptions,
    all_dictionaries_loaded: bool,
    position: &mut ScanPosition,
) -> Result<ScanOutcome, ArchiveSearchError> {
    let table_metadata = catalog.table_metadata();
    let stream_count = table_metadata.packed_streams().len();
    while position.stream_index < stream_count {
        let stream_index = position.stream_index;
        let stream_id =
            u64::try_from(stream_index).map_err(|_| ArchiveSearchError::SizeOverflow)?;
        let stream_start = *position;

        // A stream whose every table has a schema the query cannot match is never decompressed.
        let tables = stream_tables(table_metadata.schema_tables(), stream_id);
        let mut any_may_match = false;
        for table in tables {
            if schema_may_match(catalog, compiled, table.schema_id())? {
                any_may_match = true;
                break;
            }
        }
        if !any_may_match {
            for table in tables {
                position.stats.add_skipped_table(table.message_count())?;
                position.expected_table_index = position
                    .expected_table_index
                    .checked_add(1)
                    .ok_or(ArchiveSearchError::SizeOverflow)?;
            }
            position.stream_index += 1;
            continue;
        }

        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                table_metadata,
                stream_index,
                options.packed_stream,
            )
            .map_err(|source| ArchiveSearchError::PackedStream { stream_id, source })?;
        position.stats.add_stream(stream.len())?;
        let stream = catalog
            .materialize_adaptive_numerics(stream_id, stream, options.columns)
            .map_err(|source| ArchiveSearchError::TableStream { stream_id, source })?;
        let decoded_tables = catalog
            .schema_tables(stream_id, &stream, options.columns)
            .map_err(|source| ArchiveSearchError::TableStream { stream_id, source })?;
        for decoded in decoded_tables {
            let decoded =
                decoded.map_err(|source| ArchiveSearchError::TableStream { stream_id, source })?;
            if decoded.table_index() != position.expected_table_index {
                return Err(ArchiveSearchError::PhysicalTableOrder {
                    expected: position.expected_table_index,
                    actual: decoded.table_index(),
                });
            }
            let table_index = decoded.table_index();
            let schema_id = decoded.schema().id();
            if schema_may_match(catalog, compiled, schema_id)? {
                let bitmap = compiled.match_table(&decoded).map_err(|source| {
                    ArchiveSearchError::TableMatch {
                        stream_id,
                        table_index,
                        schema_id,
                        source,
                    }
                })?;
                if 0 != bitmap.match_count() {
                    if !all_dictionaries_loaded {
                        *position = stream_start;
                        return Ok(ScanOutcome::NeedsDictionaries);
                    }
                    sink.write_matches(ArchiveTableMatches {
                        catalog,
                        table: &decoded,
                        bitmap: &bitmap,
                        stream_id,
                        archive_record_start: position.stats.rows_scanned,
                    })
                    .map_err(|source| ArchiveSearchError::Sink {
                        stream_id,
                        table_index,
                        schema_id,
                        source,
                    })?;
                }
                position
                    .stats
                    .add_table(bitmap.len(), bitmap.match_count())?;
            } else {
                position
                    .stats
                    .add_skipped_table(decoded.metadata().message_count())?;
            }
            position.expected_table_index = position
                .expected_table_index
                .checked_add(1)
                .ok_or(ArchiveSearchError::SizeOverflow)?;
        }
        position.stream_index += 1;
    }
    Ok(ScanOutcome::Finished)
}

/// The tables stored in one packed stream, which table metadata keeps grouped by stream.
fn stream_tables(
    tables: &[crate::archive::SchemaTableMetadata],
    stream_id: u64,
) -> &[crate::archive::SchemaTableMetadata] {
    let first = tables.partition_point(|table| table.stream_id() < stream_id);
    let end = first + tables[first..].partition_point(|table| table.stream_id() == stream_id);
    &tables[first..end]
}

fn schema_may_match(
    catalog: &ArchiveCatalog,
    compiled: &CompiledQuery<'_, '_>,
    schema_id: i32,
) -> Result<bool, ArchiveSearchError> {
    // A schema the map lacks is left to the table decoder to report.
    let Some(schema) = catalog.schema_map().get(schema_id) else {
        return Ok(true);
    };
    compiled
        .may_match_schema(schema)
        .map_err(ArchiveSearchError::Compile)
}

/// A set of the three archive dictionaries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Dictionaries {
    variable: bool,
    log_type: bool,
    array: bool,
}

impl Dictionaries {
    const NONE: Self = Self {
        variable: false,
        log_type: false,
        array: false,
    };
    const ALL: Self = Self {
        variable: true,
        log_type: true,
        array: true,
    };

    /// The dictionaries a column of this type is read through.
    const fn add_node_type(&mut self, node_type: crate::archive::NodeType) {
        use crate::archive::NodeType;
        match node_type {
            NodeType::VarString | NodeType::DictionaryFloat => self.variable = true,
            NodeType::ClpString => {
                self.variable = true;
                self.log_type = true;
            }
            NodeType::UnstructuredArray => {
                self.variable = true;
                self.array = true;
            }
            _ => {}
        }
    }
}

/// Loads the dictionaries in `wanted` that `loaded` lacks, recording them in `loaded`.
fn load_dictionaries<A: ArchiveReader + ?Sized>(
    reader: &mut A,
    catalog: &mut ArchiveCatalog,
    limits: &ArchiveCatalogLimits,
    wanted: Dictionaries,
    loaded: &mut Dictionaries,
) -> Result<(), ArchiveSearchError> {
    if wanted.variable && !loaded.variable {
        let dictionary = reader
            .read_variable_dictionary(catalog.metadata(), limits.variable_dictionary())
            .map_err(|source| {
                ArchiveSearchError::Catalog(ArchiveCatalogError::VariableDictionary(source))
            })?;
        catalog.set_variable_dictionary(dictionary);
        loaded.variable = true;
    }
    if wanted.log_type && !loaded.log_type {
        let dictionary = reader
            .read_log_type_dictionary(catalog.metadata(), limits.log_type_dictionary())
            .map_err(|source| {
                ArchiveSearchError::Catalog(ArchiveCatalogError::LogTypeDictionary(source))
            })?;
        catalog.set_log_type_dictionary(dictionary);
        loaded.log_type = true;
    }
    if wanted.array && !loaded.array {
        let dictionary = reader
            .read_array_dictionary(catalog.metadata(), limits.array_dictionary())
            .map_err(|source| {
                ArchiveSearchError::Catalog(ArchiveCatalogError::ArrayDictionary(source))
            })?;
        catalog.set_array_dictionary(dictionary);
        loaded.array = true;
    }
    Ok(())
}

/// The dictionaries the query's filter compares against.
///
/// Conservative: anything this cannot pin down to schema nodes, such as a wildcard path, the
/// auto-generated namespace, or an expression kind it does not know, asks for all three. Getting
/// this wrong would be silent, since a filter compiled against a missing dictionary matches
/// nothing, so it errs toward loading.
fn filter_dictionaries(query: &ParsedQuery, tree: &crate::archive::SchemaTree) -> Dictionaries {
    use super::ColumnNamespace;
    use super::ExpressionKind;
    let mut wanted = Dictionaries::NONE;
    for node in query.nodes() {
        let path = match node.kind() {
            ExpressionKind::Predicate(predicate) => predicate.path(),
            ExpressionKind::List(list) => list.path(),
            ExpressionKind::Not { .. } | ExpressionKind::Boolean { .. } => continue,
        };
        match path.namespace() {
            ColumnNamespace::Default => {}
            ColumnNamespace::RangeIndex => continue,
            _ => return Dictionaries::ALL,
        }
        if path.components().is_empty()
            || path
                .components()
                .iter()
                .any(super::PathComponent::is_wildcard)
        {
            return Dictionaries::ALL;
        }
        add_path_dictionaries(tree, path.components(), &mut wanted);
        if Dictionaries::ALL == wanted {
            break;
        }
    }
    wanted
}

/// Adds the dictionaries read by every column a path can reach: the columns at its end, any
/// column beneath an object or structured array it ends on, and an unstructured array it passes
/// through, whose elements are matched inside the array's text.
fn add_path_dictionaries(
    tree: &crate::archive::SchemaTree,
    components: &[super::PathComponent],
    wanted: &mut Dictionaries,
) {
    use crate::archive::NodeType;
    let nodes = tree.nodes();
    let mut current: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            node.parent_id().is_none()
                && NodeType::Metadata != node.node_type()
                && node.key_bytes().is_empty()
        })
        .map(|(id, _)| id)
        .collect();
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        let mut next = Vec::new();
        for (child_id, child) in nodes.iter().enumerate() {
            if !child
                .parent_id()
                .is_some_and(|parent| current.contains(&parent))
                || child.key_bytes() != component.value().as_bytes()
            {
                continue;
            }
            match child.node_type() {
                NodeType::Object if last => add_descendant_dictionaries(tree, child_id, wanted),
                NodeType::Object => next.push(child_id),
                NodeType::StructuredArray => add_descendant_dictionaries(tree, child_id, wanted),
                NodeType::UnstructuredArray => wanted.add_node_type(NodeType::UnstructuredArray),
                node_type if last => wanted.add_node_type(node_type),
                _ => {}
            }
        }
        if next.is_empty() {
            break;
        }
        current = next;
    }
}

/// Adds the dictionaries read by every column beneath `ancestor`. A node always follows its
/// parent, so one forward pass sees each parent first.
fn add_descendant_dictionaries(
    tree: &crate::archive::SchemaTree,
    ancestor: usize,
    wanted: &mut Dictionaries,
) {
    let nodes = tree.nodes();
    let mut inside = vec![false; nodes.len()];
    for (node_id, node) in nodes.iter().enumerate() {
        let Some(parent) = node.parent_id() else {
            continue;
        };
        if parent == ancestor || inside.get(parent).copied().unwrap_or(false) {
            inside[node_id] = true;
            wanted.add_node_type(node.node_type());
        }
    }
}

/// Failure while orchestrating one archive-wide search.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArchiveSearchError {
    /// Loading or cross-validating non-table archive state failed.
    Catalog(ArchiveCatalogError),
    /// Compiling the parsed query against the archive catalog failed.
    Compile(SearchError),
    /// The match sink rejected per-archive setup after preflight.
    BeginSink(io::Error),
    /// Reading or decoding one packed stream failed.
    PackedStream {
        /// Zero-based packed-stream ID.
        stream_id: u64,
        /// Stream failure.
        source: PackedStreamError,
    },
    /// Selecting or decoding a table in one packed stream failed.
    TableStream {
        /// Zero-based packed-stream ID.
        stream_id: u64,
        /// Table-stream failure.
        source: TableStreamError,
    },
    /// A decoded table was not yielded in canonical physical order.
    PhysicalTableOrder {
        /// Required next table index.
        expected: usize,
        /// Actual yielded table index.
        actual: usize,
    },
    /// Matching one decoded schema table failed.
    TableMatch {
        /// Zero-based packed-stream ID.
        stream_id: u64,
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
        /// Semantic matching failure.
        source: SearchError,
    },
    /// The match sink rejected one nonempty table batch.
    Sink {
        /// Zero-based packed-stream ID.
        stream_id: u64,
        /// Global physical table index.
        table_index: usize,
        /// Opaque schema ID.
        schema_id: i32,
        /// Sink failure.
        source: io::Error,
    },
    /// Fewer or more tables were decoded than catalog metadata advertised.
    TableCountMismatch {
        /// Advertised table count.
        expected: usize,
        /// Decoded table count.
        actual: usize,
    },
    /// Scanned rows disagreed with the catalog's aggregate record count.
    RecordCountMismatch {
        /// Advertised record count.
        expected: u64,
        /// Scanned record count.
        actual: u64,
    },
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
}

impl Display for ArchiveSearchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(source) => write!(formatter, "failed to load archive catalog: {source}"),
            Self::Compile(source) => {
                write!(formatter, "failed to compile query for archive: {source}")
            }
            Self::BeginSink(source) => write!(formatter, "match sink setup failed: {source}"),
            Self::PackedStream { stream_id, source } => {
                write!(
                    formatter,
                    "failed to decode packed stream {stream_id}: {source}"
                )
            }
            Self::TableStream { stream_id, source } => write!(
                formatter,
                "failed to decode a schema table in packed stream {stream_id}: {source}"
            ),
            Self::PhysicalTableOrder { expected, actual } => write!(
                formatter,
                "schema table {actual} was yielded where physical table {expected} was expected"
            ),
            Self::TableMatch {
                stream_id,
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "failed to match packed stream {stream_id}, table {table_index}, schema \
                 {schema_id}: {source}"
            ),
            Self::Sink {
                stream_id,
                table_index,
                schema_id,
                source,
            } => write!(
                formatter,
                "match sink failed for packed stream {stream_id}, table {table_index}, schema \
                 {schema_id}: {source}"
            ),
            Self::TableCountMismatch { expected, actual } => write!(
                formatter,
                "archive advertised {expected} schema tables but search decoded {actual}"
            ),
            Self::RecordCountMismatch { expected, actual } => write!(
                formatter,
                "archive advertised {expected} records but search scanned {actual}"
            ),
            Self::SizeOverflow => formatter.write_str("archive search size arithmetic overflow"),
        }
    }
}

impl Error for ArchiveSearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(source) => Some(source),
            Self::Compile(source) | Self::TableMatch { source, .. } => Some(source),
            Self::PackedStream { source, .. } => Some(source),
            Self::TableStream { source, .. } => Some(source),
            Self::BeginSink(source) | Self::Sink { source, .. } => Some(source),
            Self::PhysicalTableOrder { .. }
            | Self::TableCountMismatch { .. }
            | Self::RecordCountMismatch { .. }
            | Self::SizeOverflow => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::LogOrderLocator;
    use crate::archive::SingleFileArchiveReader;
    use crate::search::AuthoritativeTimestampRange;
    use crate::search::KqlLimits;
    use crate::search::SearchLimits;
    use crate::search::parse_kql;

    const CPP_MULTI_SCHEMA_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-log-order-cpp.bin");
    const CPP_MINIMAL_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    struct ObservedBatch {
        stream_id: u64,
        table_index: usize,
        archive_record_start: u64,
        rows: Vec<usize>,
    }

    #[derive(Default)]
    struct CoordinateSink {
        begin_calls: usize,
        batches: Vec<ObservedBatch>,
    }

    impl ArchiveMatchSink for CoordinateSink {
        fn begin_archive(&mut self) -> io::Result<()> {
            self.begin_calls += 1;
            Ok(())
        }

        fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
            let rows = matches
                .matching_rows()
                .map(|row| {
                    assert_eq!(matches.stream_id(), row.stream_id());
                    assert_eq!(matches.table_index(), row.table_index());
                    assert_eq!(matches.schema_id(), row.schema_id());
                    assert!(std::ptr::eq(matches.catalog(), row.catalog()));
                    assert!(std::ptr::eq(matches.table(), row.table()));
                    row.row_index()
                })
                .collect();
            self.batches.push(ObservedBatch {
                stream_id: matches.stream_id(),
                table_index: matches.table_index(),
                archive_record_start: matches.archive_record_start(),
                rows,
            });
            Ok(())
        }
    }

    fn run_search(
        source: &str,
        sink: &mut impl ArchiveMatchSink,
        options: &ArchiveSearchOptions,
    ) -> Result<ArchiveSearchStats, ArchiveSearchError> {
        let query = parse_kql(source, KqlLimits::default()).expect("parse test query");
        let mut reader = SingleFileArchiveReader::open(Cursor::new(CPP_MULTI_SCHEMA_FIXTURE))
            .expect("open committed C++ multi-schema fixture");
        search_archive(&mut reader, &query, sink, options)
    }

    #[test]
    fn multi_table_matches_follow_cpp_physical_order() {
        let mut sink = CoordinateSink::default();
        let stats = run_search(
            "a:* OR b:* OR c:*",
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("search physical tables");

        assert_eq!(1, stats.streams_scanned());
        assert!(0 < stats.decoded_stream_bytes());
        assert_eq!(3, stats.tables_scanned());
        assert_eq!(6, stats.rows_scanned());
        assert_eq!(6, stats.matching_rows());
        assert_eq!(1, sink.begin_calls);
        assert_eq!(3, sink.batches.len());
        assert_batch(&sink.batches[0], 0, 0, &[0, 1, 2]);
        assert_batch(&sink.batches[1], 1, 3, &[0, 1]);
        assert_batch(&sink.batches[2], 2, 5, &[0]);
    }

    fn assert_batch(
        batch: &ObservedBatch,
        table_index: usize,
        archive_record_start: u64,
        rows: &[usize],
    ) {
        assert_eq!(0, batch.stream_id);
        assert_eq!(table_index, batch.table_index);
        assert_eq!(archive_record_start, batch.archive_record_start);
        assert_eq!(rows, batch.rows);
    }

    #[derive(Default)]
    struct LogIndexProjectionSink {
        projected: Vec<i64>,
    }

    impl ArchiveMatchSink for LogIndexProjectionSink {
        fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
            let locator = LogOrderLocator::discover(matches.catalog().schema_tree())
                .expect("valid log-order schema tree")
                .expect("fixture has log order");
            let column = locator
                .locate(matches.table().schema(), matches.table().table())
                .expect("locate log-order column")
                .expect("fixture table has log order");
            self.projected.extend(
                column
                    .cursor()
                    .zip(matches.bitmap().as_bytes())
                    .filter_map(|(value, matched)| (0 != *matched).then_some(value)),
            );
            Ok(())
        }
    }

    #[test]
    fn matching_is_projection_neutral_and_keeps_typed_tables_borrowed() {
        let mut sink = LogIndexProjectionSink::default();
        let stats = run_search(
            "a:20 OR b:false",
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("search before typed projection");

        assert_eq!(2, stats.matching_rows());
        assert_eq!([2, 4], sink.projected.as_slice());
    }

    #[test]
    fn impossible_archive_predicates_skip_packed_streams_conservatively() {
        let mut sink = CoordinateSink::default();
        let stats = run_search(
            "c:not-in-the-archive-dictionary",
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("compile an impossible dictionary predicate");
        assert_eq!(ArchiveSearchStats::default(), stats);
        assert_eq!(1, sink.begin_calls);
        assert!(sink.batches.is_empty());

        let negated = run_search(
            "NOT c:not-in-the-archive-dictionary",
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("negation must retain ordinary three-valued evaluation");
        assert_eq!(1, negated.matching_rows());
        // Only the table holding `c` is evaluated; the other two schemas lack the column, so their
        // tables are ruled out unread, but their rows still count toward the archive's total.
        assert_eq!(1, negated.tables_scanned());
        assert_eq!(6, negated.rows_scanned());
        assert_eq!(2, sink.begin_calls);
        assert_eq!(1, sink.batches.len());

        let missing = run_search(
            "NOT wholly_missing:*",
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("a wholly unresolved path under NOT must remain schema-pruned like C++");
        assert_eq!(ArchiveSearchStats::default(), missing);
        assert_eq!(2, sink.begin_calls);
        assert_eq!(1, sink.batches.len());
    }

    #[test]
    fn schema_preflight_requires_one_schema_to_satisfy_each_conjunct() {
        let mut sink = CoordinateSink::default();
        let stats = run_search("a:* AND b:*", &mut sink, &ArchiveSearchOptions::default())
            .expect("schema-disjoint conjunction is an early success");

        assert_eq!(ArchiveSearchStats::default(), stats);
        assert_eq!(0, sink.begin_calls);
        assert!(sink.batches.is_empty());
    }

    #[test]
    fn resolved_empty_lists_keep_their_boolean_identities_during_preflight() {
        let mut any_sink = CoordinateSink::default();
        let any = run_search("a:()", &mut any_sink, &ArchiveSearchOptions::default())
            .expect("empty ANY is a constant false search");
        assert_eq!(ArchiveSearchStats::default(), any);
        assert_eq!(0, any_sink.begin_calls);
        assert!(any_sink.batches.is_empty());

        for query in ["a:(AND)", "a:(NOT)"] {
            let mut sink = CoordinateSink::default();
            let stats = run_search(query, &mut sink, &ArchiveSearchOptions::default())
                .expect("empty ALL and NONE are constant true searches");
            assert_eq!(1, sink.begin_calls, "{query}");
            assert_eq!(6, stats.matching_rows(), "{query}");
            assert_eq!(3, sink.batches.len(), "{query}");
        }
    }

    #[test]
    fn metadata_and_timestamp_preflight_skip_sink_setup() {
        let mut metadata_sink = CoordinateSink::default();
        let metadata = run_search(
            "$_filename:NOPE",
            &mut metadata_sink,
            &ArchiveSearchOptions::default(),
        )
        .expect("nonmatching range metadata is an early success");
        assert_eq!(ArchiveSearchStats::default(), metadata);
        assert_eq!(0, metadata_sink.begin_calls);

        let query = parse_kql("*:*", KqlLimits::default()).expect("parse wildcard query");
        let mut reader = SingleFileArchiveReader::open(Cursor::new(CPP_MINIMAL_FIXTURE))
            .expect("open committed C++ minimal fixture");
        let range = AuthoritativeTimestampRange::new(Some(1_700_000_000_124), None);
        let options = ArchiveSearchOptions::default()
            .with_search(SearchOptions::default().with_authoritative_timestamp_range(range));
        let mut timestamp_sink = CoordinateSink::default();
        let timestamp = search_archive(&mut reader, &query, &mut timestamp_sink, &options)
            .expect("disjoint timestamp range is an early success");
        assert_eq!(ArchiveSearchStats::default(), timestamp);
        assert_eq!(0, timestamp_sink.begin_calls);
    }

    struct BeginFailingSink;

    impl ArchiveMatchSink for BeginFailingSink {
        fn begin_archive(&mut self) -> io::Result<()> {
            Err(io::Error::other("could not create output"))
        }

        fn write_matches(&mut self, _matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
            unreachable!("a failing begin hook prevents table output")
        }
    }

    #[test]
    fn begin_failure_is_reported_before_packed_stream_scanning() {
        let error = run_search(
            "a:*",
            &mut BeginFailingSink,
            &ArchiveSearchOptions::default(),
        )
        .expect_err("sink setup must fail");

        assert!(matches!(
            error,
            ArchiveSearchError::BeginSink(source)
                if "could not create output" == source.to_string()
        ));
    }

    struct FailingSink {
        accepted_batches: usize,
    }

    impl ArchiveMatchSink for FailingSink {
        fn write_matches(&mut self, _matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
            if 1 == self.accepted_batches {
                return Err(io::Error::other("projection failed"));
            }
            self.accepted_batches += 1;
            Ok(())
        }
    }

    #[test]
    fn sink_and_match_failures_name_the_physical_table() {
        let mut sink = FailingSink {
            accepted_batches: 0,
        };
        let sink_error = run_search(
            "a:* OR b:* OR c:*",
            &mut sink,
            &ArchiveSearchOptions::default(),
        )
        .expect_err("second table sink must fail");
        assert!(matches!(
            sink_error,
            ArchiveSearchError::Sink {
                stream_id: 0,
                table_index: 1,
                source,
                ..
            } if "projection failed" == source.to_string()
        ));

        let defaults = SearchLimits::default();
        let limited = SearchLimits::new(
            defaults.max_schema_nodes(),
            defaults.max_archive_tables(),
            defaults.max_resolved_nodes(),
            defaults.max_path_states(),
            defaults.max_dictionary_entries_scanned(),
            defaults.max_dictionary_matches(),
            2,
        );
        let mut sink = CoordinateSink::default();
        let match_error = run_search(
            "a:* OR b:* OR c:*",
            &mut sink,
            &ArchiveSearchOptions::default().with_search(SearchOptions::new(false, limited)),
        )
        .expect_err("three-row table must exceed bitmap limit");
        assert!(matches!(
            match_error,
            ArchiveSearchError::TableMatch {
                stream_id: 0,
                table_index: 0,
                source: SearchError::LimitExceeded { .. },
                ..
            }
        ));
    }
}
