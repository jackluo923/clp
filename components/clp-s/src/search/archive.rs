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

    /// The schema nodes this sink writes for a matched row, or `None` when it may write any.
    ///
    /// The search loads only the dictionaries these nodes are read through before handing over
    /// matches. The default, `None`, loads them all.
    fn output_nodes(&self, schema_tree: &crate::archive::SchemaTree) -> Option<Vec<u32>> {
        let _ = schema_tree;
        None
    }
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
    let filter = filter_nodes(query, catalog.schema_tree());
    let output_nodes = sink.output_nodes(catalog.schema_tree());
    let columns = StreamColumns {
        filter: filter.columns(options.search),
    };
    let filter_dictionaries = filter.dictionaries(catalog.schema_tree());
    load_dictionaries(
        reader,
        &mut catalog,
        &options.catalog,
        &filter_dictionaries,
        &mut loaded,
    )?;

    // The output's dictionaries are worked out once the filter is compiled, from the tables it
    // can match. A projected output is narrow, so they load before the scan; whole records may
    // need many more, so those wait until a row matches.
    let mut output = None;
    let mut scan = ScanPosition::default();
    let mut sink_begun = false;
    loop {
        let compiled = query
            .compile_for_archive(&catalog, options.search)
            .map_err(ArchiveSearchError::Compile)?;
        let wanted = match output {
            Some(ref wanted) => Dictionaries::clone(wanted),
            None => output
                .insert(output_dictionaries(
                    &catalog,
                    &compiled,
                    output_nodes.as_deref(),
                )?)
                .clone(),
        };
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
        if output_nodes.is_some() && !loaded.covers(&wanted) {
            drop(compiled);
            load_dictionaries(reader, &mut catalog, &options.catalog, &wanted, &mut loaded)?;
            continue;
        }
        let outcome = scan_streams(
            reader,
            &catalog,
            &compiled,
            sink,
            options,
            ScanSettings {
                columns,
                all_dictionaries_loaded: loaded.covers(&wanted),
            },
            &mut scan,
        )?;
        drop(compiled);
        match outcome {
            ScanOutcome::Finished => break,
            // A match needs the dictionaries its output is read through. Load them and resume at
            // the stream holding the match, which wrote nothing before stopping.
            ScanOutcome::NeedsDictionaries => {
                load_dictionaries(reader, &mut catalog, &options.catalog, &wanted, &mut loaded)?;
            }
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

/// The columns a table can be matched from alone, or `None` when matching may read any.
#[derive(Clone, Copy)]
struct StreamColumns<'a> {
    filter: Option<&'a std::collections::BTreeSet<u32>>,
}

impl StreamColumns<'_> {
    /// For a separate-column stream, which stores one column per frame, the mask of the columns
    /// matching reads, in the stream's column order. `None` for a stream sharing one frame, or
    /// when the filter's columns are unknown.
    fn matching_mask(
        self,
        catalog: &ArchiveCatalog,
        table: &crate::archive::SchemaTableMetadata,
    ) -> Option<Vec<bool>> {
        use crate::archive::SchemaEntry;
        let filter = self.filter?;
        let frames = catalog
            .table_metadata()
            .separate_columns_for(table.stream_id())?
            .columns()
            .len();
        let schema = catalog.schema_map().get(table.schema_id())?;
        let tree = catalog.schema_tree();
        let mut mask = Vec::with_capacity(frames);
        for entry in schema.entries() {
            let SchemaEntry::Node(node) = *entry else {
                continue;
            };
            if crate::archive::is_value_bearing(tree.get(node as usize)?.node_type()) {
                mask.push(filter.contains(&node));
            }
        }
        (mask.len() == frames).then_some(mask)
    }
}

/// How one pass of a scan reads its streams.
#[derive(Clone, Copy)]
struct ScanSettings<'a> {
    columns: StreamColumns<'a>,
    /// Whether every dictionary a match is written through is loaded.
    all_dictionaries_loaded: bool,
}

/// What matching a separate-column stream's table from its filter columns alone found.
enum Prefilter {
    /// No row matched; the stream is recorded as scanned and the position moved past it.
    Unmatched,
    /// Rows matched. The filter-only stream and its bitmap are kept, so the whole read inflates
    /// only the columns still missing and does not evaluate the filter again.
    Matched {
        stream: crate::archive::DecodedPackedStream,
        bitmap: MatchBitmap,
    },
    /// The table was not the one expected; the whole read reports it.
    Unexpected,
}

/// Matches the one table of the stream at `position` from the columns `mask` selects, and when
/// no row matches, records the stream as scanned and moves past it.
fn prefilter_by_columns<A: ArchiveReader + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    compiled: &CompiledQuery<'_, '_>,
    options: &ArchiveSearchOptions,
    mask: &[bool],
    position: &mut ScanPosition,
) -> Result<Prefilter, ArchiveSearchError> {
    let stream_index = position.stream_index;
    let stream_id = u64::try_from(stream_index).map_err(|_| ArchiveSearchError::SizeOverflow)?;
    let stream = read_stream(reader, catalog, stream_index, Some(mask), options)?;
    let decoded = catalog
        .schema_tables(stream_id, &stream, options.columns)
        .map_err(|source| ArchiveSearchError::TableStream { stream_id, source })?
        .next()
        .transpose()
        .map_err(|source| ArchiveSearchError::TableStream { stream_id, source })?;
    // A table out of place is left for the whole read to report.
    let Some(decoded) =
        decoded.filter(|decoded| decoded.table_index() == position.expected_table_index)
    else {
        return Ok(Prefilter::Unexpected);
    };
    let bitmap =
        compiled
            .match_table(&decoded)
            .map_err(|source| ArchiveSearchError::TableMatch {
                stream_id,
                table_index: decoded.table_index(),
                schema_id: decoded.schema().id(),
                source,
            })?;
    drop(decoded);
    if 0 != bitmap.match_count() {
        return Ok(Prefilter::Matched { stream, bitmap });
    }
    position.stats.add_stream(stream.len())?;
    position.stats.add_table(bitmap.len(), 0)?;
    position.expected_table_index = position
        .expected_table_index
        .checked_add(1)
        .ok_or(ArchiveSearchError::SizeOverflow)?;
    position.stream_index += 1;
    Ok(Prefilter::Unmatched)
}

/// Completes a filter-only stream with the columns it did not load, or `None` when its layout
/// cannot be completed that way and the stream must be read whole.
fn complete_stream<A: ArchiveReader + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    stream_index: usize,
    filtered: crate::archive::DecodedPackedStream,
    options: &ArchiveSearchOptions,
) -> Result<Option<crate::archive::DecodedPackedStream>, ArchiveSearchError> {
    let Some(missing) = filtered
        .column_layout()
        .map(|layout| layout.iter().map(|slot| !slot.loaded).collect::<Vec<_>>())
    else {
        return Ok(None);
    };
    if !missing.contains(&true) {
        return Ok(Some(filtered));
    }
    let rest = read_stream(reader, catalog, stream_index, Some(&missing), options)?;
    Ok(filtered.merge_disjoint_columns(rest))
}

fn scan_streams<A: ArchiveReader + ?Sized, S: ArchiveMatchSink + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    compiled: &CompiledQuery<'_, '_>,
    sink: &mut S,
    options: &ArchiveSearchOptions,
    scan: ScanSettings<'_>,
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

        // A separate-column stream holds one table. The columns its filter reads are read alone
        // first, so a stream without a match costs only those. One with a match keeps them and
        // its bitmap, and reads only the remaining columns.
        let mut prefiltered = None;
        if let [table] = tables
            && let Some(mask) = scan.columns.matching_mask(catalog, table)
        {
            match prefilter_by_columns(reader, catalog, compiled, options, &mask, position)? {
                Prefilter::Unmatched => continue,
                Prefilter::Matched { stream, bitmap } => prefiltered = Some((stream, bitmap)),
                Prefilter::Unexpected => {}
            }
        }

        let (stream, mut cached_bitmap) = match prefiltered {
            Some((filtered, bitmap)) => {
                match complete_stream(reader, catalog, stream_index, filtered, options)? {
                    Some(stream) => (stream, Some(bitmap)),
                    None => (read_stream(reader, catalog, stream_index, None, options)?, None),
                }
            }
            None => (read_stream(reader, catalog, stream_index, None, options)?, None),
        };
        position.stats.add_stream(stream.len())?;
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
                let bitmap = match cached_bitmap.take() {
                    Some(bitmap) => bitmap,
                    None => compiled.match_table(&decoded).map_err(|source| {
                        ArchiveSearchError::TableMatch {
                            stream_id,
                            table_index,
                            schema_id,
                            source,
                        }
                    })?,
                };
                if 0 != bitmap.match_count() {
                    if !scan.all_dictionaries_loaded {
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

/// Reads a packed stream, only the columns `mask` selects when given, with its adaptive numeric
/// columns materialized.
fn read_stream<A: ArchiveReader + ?Sized>(
    reader: &mut A,
    catalog: &ArchiveCatalog,
    stream_index: usize,
    mask: Option<&[bool]>,
    options: &ArchiveSearchOptions,
) -> Result<crate::archive::DecodedPackedStream, ArchiveSearchError> {
    let stream_id = u64::try_from(stream_index).map_err(|_| ArchiveSearchError::SizeOverflow)?;
    let stream = match mask {
        Some(mask) => reader.read_packed_stream_projected(
            catalog.metadata(),
            catalog.table_metadata(),
            stream_index,
            mask,
            None,
            options.packed_stream,
        ),
        None => reader.read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            stream_index,
            options.packed_stream,
        ),
    }
    .map_err(|source| ArchiveSearchError::PackedStream { stream_id, source })?;
    catalog
        .materialize_adaptive_numerics(stream_id, stream, options.columns)
        .map_err(|source| ArchiveSearchError::TableStream { stream_id, source })
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

/// Which log-type dictionary entries a search needs: none, those of some CLP-string fields, or all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum LogTypeFields {
    #[default]
    None,
    Fields(std::collections::BTreeSet<u32>),
    All,
}

impl LogTypeFields {
    fn add(&mut self, node: u32) {
        match self {
            Self::None => *self = Self::Fields(std::iter::once(node).collect()),
            Self::Fields(fields) => {
                fields.insert(node);
            }
            Self::All => {}
        }
    }

    fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::None, other) | (other, Self::None) => other.clone(),
            (Self::Fields(left), Self::Fields(right)) => {
                Self::Fields(left.union(right).copied().collect())
            }
        }
    }

    fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::All, _) | (_, Self::None) => true,
            (Self::Fields(left), Self::Fields(right)) => right.is_subset(left),
            _ => false,
        }
    }
}

/// The dictionaries a search needs: the variable and array dictionaries whole, and the log-type
/// dictionary whole or for some fields, which an archive storing it per field loads alone.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Dictionaries {
    variable: bool,
    log_type: LogTypeFields,
    array: bool,
}

impl Dictionaries {
    const NONE: Self = Self {
        variable: false,
        log_type: LogTypeFields::None,
        array: false,
    };
    const ALL: Self = Self {
        variable: true,
        log_type: LogTypeFields::All,
        array: true,
    };

    /// Adds the dictionaries the column of `node` is read through.
    fn add_node(&mut self, node: u32, node_type: crate::archive::NodeType) {
        use crate::archive::NodeType;
        match node_type {
            NodeType::VarString | NodeType::DictionaryFloat => self.variable = true,
            NodeType::ClpString => {
                self.variable = true;
                self.log_type.add(node);
            }
            NodeType::UnstructuredArray => {
                self.variable = true;
                self.array = true;
            }
            _ => {}
        }
    }

    fn covers(&self, other: &Self) -> bool {
        (self.variable || !other.variable)
            && (self.array || !other.array)
            && self.log_type.covers(&other.log_type)
    }
}

/// Loads what `wanted` adds to `loaded`, recording it in `loaded`. A log-type dictionary is
/// reloaded for the union of the fields already loaded and those now wanted.
fn load_dictionaries<A: ArchiveReader + ?Sized>(
    reader: &mut A,
    catalog: &mut ArchiveCatalog,
    limits: &ArchiveCatalogLimits,
    wanted: &Dictionaries,
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
    if !loaded.log_type.covers(&wanted.log_type) {
        let fields = loaded.log_type.union(&wanted.log_type);
        let dictionary = match &fields {
            LogTypeFields::Fields(nodes) => {
                let nodes: Vec<u32> = nodes.iter().copied().collect();
                reader.read_log_type_dictionary_fields(
                    catalog.metadata(),
                    limits.log_type_dictionary(),
                    &nodes,
                )
            }
            _ => reader.read_log_type_dictionary(catalog.metadata(), limits.log_type_dictionary()),
        }
        .map_err(|source| {
            ArchiveSearchError::Catalog(ArchiveCatalogError::LogTypeDictionary(source))
        })?;
        // A dictionary stored as one sequence loads whole whatever was asked for, so it is
        // recorded as such and never reloaded.
        loaded.log_type = if dictionary.is_complete() {
            LogTypeFields::All
        } else {
            fields
        };
        catalog.set_log_type_dictionary(dictionary);
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

/// The schema nodes the query's filter reads.
struct FilterNodes {
    /// Every node the filter's paths reach, or `None` when they cannot be pinned down: a wildcard
    /// path, the auto-generated namespace, or an expression kind this does not know.
    nodes: Option<std::collections::BTreeSet<u32>>,
    /// Whether a predicate selects by range index, which reads the log-order column.
    range_index: bool,
}

impl FilterNodes {
    /// The dictionaries the filter compares against. Conservative: when the nodes are unknown,
    /// all three. Getting this wrong would be silent, since a filter compiled against a missing
    /// dictionary matches nothing, so it errs toward loading.
    fn dictionaries(&self, tree: &crate::archive::SchemaTree) -> Dictionaries {
        let Some(nodes) = &self.nodes else {
            return Dictionaries::ALL;
        };
        let mut wanted = Dictionaries::NONE;
        for node in nodes {
            if let Some(schema_node) = tree.get(*node as usize) {
                wanted.add_node(*node, schema_node.node_type());
            }
        }
        wanted
    }

    /// The columns a table can be matched from alone, or `None` when matching may read others:
    /// besides unknown nodes, a range-index selector or a timestamp bound reads columns the
    /// filter's paths do not name.
    const fn columns(&self, options: SearchOptions) -> Option<&std::collections::BTreeSet<u32>> {
        let range = options.authoritative_timestamp_range();
        if self.range_index
            || range.begin_milliseconds().is_some()
            || range.end_milliseconds().is_some()
        {
            return None;
        }
        self.nodes.as_ref()
    }
}

fn filter_nodes(query: &ParsedQuery, tree: &crate::archive::SchemaTree) -> FilterNodes {
    use super::ColumnNamespace;
    use super::ExpressionKind;
    let mut nodes = std::collections::BTreeSet::new();
    let mut range_index = false;
    for node in query.nodes() {
        let path = match node.kind() {
            ExpressionKind::Predicate(predicate) => predicate.path(),
            ExpressionKind::List(list) => list.path(),
            ExpressionKind::Not { .. } | ExpressionKind::Boolean { .. } => continue,
        };
        let unknown = FilterNodes {
            nodes: None,
            range_index,
        };
        match path.namespace() {
            ColumnNamespace::Default => {}
            ColumnNamespace::RangeIndex => {
                range_index = true;
                continue;
            }
            _ => return unknown,
        }
        if path.components().is_empty()
            || path
                .components()
                .iter()
                .any(super::PathComponent::is_wildcard)
        {
            return unknown;
        }
        add_path_nodes(tree, path.components(), &mut nodes);
    }
    FilterNodes {
        nodes: Some(nodes),
        range_index,
    }
}

/// The dictionaries a matched row's output is read through: those of the output's columns, or of
/// every column for whole records, held by a table whose schema the filter can match. A column
/// only other tables hold is never written.
fn output_dictionaries(
    catalog: &ArchiveCatalog,
    compiled: &CompiledQuery<'_, '_>,
    nodes: Option<&[u32]>,
) -> Result<Dictionaries, ArchiveSearchError> {
    use crate::archive::SchemaEntry;
    let tree = catalog.schema_tree();
    let mut held = vec![false; tree.len()];
    for table in catalog.table_metadata().schema_tables() {
        if !schema_may_match(catalog, compiled, table.schema_id())? {
            continue;
        }
        // A schema the map lacks is left to the table decoder to report; until then, any column
        // may be written.
        let Some(schema) = catalog.schema_map().get(table.schema_id()) else {
            held.fill(true);
            break;
        };
        for entry in schema.entries() {
            if let SchemaEntry::Node(node) = *entry
                && let Some(slot) = held.get_mut(node as usize)
            {
                *slot = true;
            }
        }
    }
    let mut output = Dictionaries::NONE;
    let mut add = |node: usize| {
        if held.get(node).copied().unwrap_or(false)
            && let (Some(schema_node), Ok(id)) = (tree.get(node), u32::try_from(node))
        {
            output.add_node(id, schema_node.node_type());
        }
    };
    match nodes {
        Some(nodes) => nodes.iter().for_each(|node| add(*node as usize)),
        None => (0..tree.len()).for_each(add),
    }
    Ok(output)
}

/// Adds every column a filter path can reach; see [`resolve_filter_path`].
fn add_path_nodes(
    tree: &crate::archive::SchemaTree,
    components: &[super::PathComponent],
    wanted: &mut std::collections::BTreeSet<u32>,
) {
    let components: Vec<&str> = components.iter().map(super::PathComponent::value).collect();
    wanted.extend(resolve_filter_path(tree, &components));
}

/// Returns every schema node a filter's comparison on a path, given as unescaped keys, can read.
///
/// The path resolves as the query compiler resolves it: a node with an empty key, such as a
/// structured array's element, consumes no component. That covers the nodes at the path's end,
/// an element one component short of it, every node beneath an object or structured array it
/// ends on, and an unstructured array it reaches, whose elements are matched inside the array's
/// text. The result is sorted.
#[must_use]
pub fn resolve_filter_path(tree: &crate::archive::SchemaTree, components: &[&str]) -> Vec<u32> {
    use crate::archive::NodeType;
    let mut wanted = std::collections::BTreeSet::new();
    let nodes = tree.nodes();
    let mut children = vec![Vec::new(); nodes.len()];
    for (node_id, node) in nodes.iter().enumerate() {
        if let Some(slot) = node.parent_id().and_then(|parent| children.get_mut(parent)) {
            slot.push(node_id);
        }
    }
    let mut pending: Vec<(usize, usize)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            node.parent_id().is_none()
                && NodeType::Metadata != node.node_type()
                && node.key_bytes().is_empty()
        })
        .flat_map(|(id, _)| children[id].iter().map(|child| (*child, 0)))
        .collect();
    let mut visited = std::collections::BTreeSet::new();
    while let Some((node_id, component)) = pending.pop() {
        if !visited.insert((node_id, component)) {
            continue;
        }
        let node = &nodes[node_id];
        let key = node.key_bytes();
        let matches_component = components
            .get(component)
            .is_some_and(|expected| key == expected.as_bytes());
        if NodeType::UnstructuredArray == node.node_type() {
            if matches_component && let Ok(id) = u32::try_from(node_id) {
                wanted.insert(id);
            }
            continue;
        }
        let next = if key.is_empty() {
            // The compiler also resolves an element one component short of the path's end, so
            // `a.b` reaches a scalar element of `a`.
            if component + 1 == components.len()
                && let Ok(id) = u32::try_from(node_id)
            {
                wanted.insert(id);
            }
            component
        } else if matches_component {
            component + 1
        } else {
            continue;
        };
        if next == components.len() {
            if let Ok(id) = u32::try_from(node_id) {
                wanted.insert(id);
            }
            if matches!(
                node.node_type(),
                NodeType::Object | NodeType::StructuredArray
            ) {
                add_descendant_nodes(tree, node_id, &mut wanted);
            }
            continue;
        }
        pending.extend(children[node_id].iter().map(|child| (*child, next)));
    }
    wanted.into_iter().collect()
}

/// Adds every column beneath `ancestor`. A node always follows its
/// parent, so one forward pass sees each parent first.
fn add_descendant_nodes(
    tree: &crate::archive::SchemaTree,
    ancestor: usize,
    wanted: &mut std::collections::BTreeSet<u32>,
) {
    let nodes = tree.nodes();
    let mut inside = vec![false; nodes.len()];
    for (node_id, node) in nodes.iter().enumerate() {
        let Some(parent) = node.parent_id() else {
            continue;
        };
        if parent == ancestor || inside.get(parent).copied().unwrap_or(false) {
            inside[node_id] = true;
            if let Ok(node_id) = u32::try_from(node_id) {
                wanted.insert(node_id);
            }
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
    fn dictionary_misses_prune_boolean_trees_without_inverting_negation() {
        for query in [
            "c:never-present AND (c:other OR NOT c:another)",
            "(c:other OR NOT c:another) AND c:never-present",
            "c:never-present OR c:also-never-present",
        ] {
            let mut sink = CoordinateSink::default();
            let stats = run_search(query, &mut sink, &ArchiveSearchOptions::default()).unwrap();
            assert_eq!(ArchiveSearchStats::default(), stats, "{query}");
            // Dictionary pruning follows sink setup, preserving the CLI's
            // observable empty-output behaviour from the C++ implementation.
            assert_eq!(1, sink.begin_calls, "{query}");
            assert!(sink.batches.is_empty(), "{query}");
        }
        for query in [
            "c:never-present OR NOT c:also-never-present",
            "NOT (c:never-present OR c:also-never-present)",
            "NOT c:never-present AND c:*",
        ] {
            let mut sink = CoordinateSink::default();
            let stats = run_search(query, &mut sink, &ArchiveSearchOptions::default()).unwrap();
            assert_eq!(1, stats.matching_rows(), "{query}");
            assert_eq!(1, stats.tables_scanned(), "{query}");
        }
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
