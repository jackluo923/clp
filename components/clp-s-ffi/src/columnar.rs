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
use clp_s::archive::append_logtype_template;
use clp_s::archive::append_parsed_logtype_template;
use clp_s::search::ArchiveSearchOptions;
use clp_s::search::DictionaryMatches;
use clp_s::search::ColumnNamespace;
use clp_s::search::{ComparisonOperator, Literal};
use clp_s::search::ExpressionKind;
use clp_s::search::FieldForm;
use clp_s::search::ParsedQuery;
use clp_s::search::PathComponent;
use clp_s::search::resolve_filter_path;
use clp_s::timestamp_catalog::TimestampPatternCatalog;

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
    // v4 column layouts only, never v2 cell kinds or v3 dense columns.
    pub const CONSTANT_INTEGER: u32 = 7;
    pub const CONSTANT_FLOAT: u32 = 8;
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

/// A v3 projected column. Numeric data is native-endian, potentially unaligned;
/// consumers must copy bytes rather than dereference an aligned typed pointer.
/// `kind` is INTEGER/FLOAT for compact 8-byte values, ABSENT for an all-null
/// column, or UNSUPPORTED for a column of legacy Cells (including text).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ColumnView {
    pub kind: u32,
    pub reserved: u32,
    pub data: *const u8,
}

#[derive(Default)]
struct ColumnBuffer {
    numeric: Vec<u64>,
    cells: Vec<Cell>,
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
    let Some(mut current) = user_root(tree) else {
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

/// Returns the default namespace's object root, the parent of every top-level user key.
fn user_root(tree: &SchemaTree) -> Option<usize> {
    tree.nodes().iter().position(|node| {
        node.parent_id().is_none()
            && NodeType::Metadata != node.node_type()
            && node.key_bytes().is_empty()
    })
}

/// One schema node a dot path can address: the path, escaped the way a projection descriptor is,
/// and the archive's node type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveFieldType {
    pub path: Vec<u8>,
    pub node_type: NodeType,
}

/// Lists every schema node a dot path can address, with its escaped path and type.
///
/// A node is addressable when every ancestor below the user root is an object with a non-empty
/// key: an array's children describe its elements rather than further dot paths, and an empty key
/// has no dot spelling. A key stored under several node types yields one entry per type, which is
/// what lets a caller tell a homogeneous path from a mixed one. A validated tree lists every
/// parent before its children, so one forward pass suffices.
#[must_use]
pub fn archive_field_types(tree: &SchemaTree) -> Vec<ArchiveFieldType> {
    let nodes = tree.nodes();
    let Some(root) = user_root(tree) else {
        return Vec::new();
    };
    // The dot path of every object a later node may hang off, by node ID.
    let mut object_paths: Vec<Option<Vec<u8>>> = vec![None; nodes.len()];
    object_paths[root] = Some(Vec::new());
    let mut types = Vec::new();
    for (node_id, node) in nodes.iter().enumerate() {
        if node.key_bytes().is_empty() {
            continue;
        }
        let Some(parent_path) = node
            .parent_id()
            .and_then(|parent| object_paths.get(parent))
            .and_then(Option::as_ref)
        else {
            continue;
        };
        let mut path = parent_path.clone();
        if !path.is_empty() {
            path.push(b'.');
        }
        crate::kv_ir_columnar::push_escaped_component(&mut path, node.key_bytes());
        if NodeType::Object == node.node_type() {
            object_paths[node_id] = Some(path.clone());
        }
        types.push(ArchiveFieldType {
            path,
            node_type: node.node_type(),
        });
    }
    types
}

/// Splits an escaped dot descriptor into its components.
///
/// A backslash escapes the following character, so a key containing a literal dot survives the
/// split, and `\uXXXX` names a character by its code, as a search projection's descriptor may;
/// `_shape` writes a comma this way.
fn split_descriptor(source: &str) -> Vec<String> {
    let mut components = Vec::new();
    let mut current = String::new();
    let mut characters = source.chars();
    while let Some(character) = characters.next() {
        if '\\' == character {
            match characters.next() {
                Some('u') => {
                    let hex: String = characters.clone().take(4).collect();
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(decoded) if 4 == hex.len() => {
                            current.push(decoded);
                            characters.nth(3);
                        }
                        _ => current.push('u'),
                    }
                }
                Some(escaped) => current.push(escaped),
                None => {}
            }
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
    /// How each requested field is written: its value, or its CLP strings as templates.
    field_shapes: Vec<FieldForm>,
    /// Sorted schema nodes the filter reads, or `None` when they cannot be pinned down.
    filter_nodes: Option<Vec<u32>>,
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
    column_buffers: Vec<ColumnBuffer>,
    column_views: Vec<ColumnView>,
}

/// Which of the three dictionaries a scan reads through: variable, log-type, array. `log_type`
/// lists the CLP-string fields read, which an archive storing the dictionary per field loads alone.
#[derive(Clone, Default, Eq, PartialEq)]
struct Dictionaries {
    variable: bool,
    log_type: Vec<u32>,
    log_type_all: bool,
    array: bool,
}

impl Dictionaries {
    const ALL: Self = Self {
        variable: true,
        log_type: Vec::new(),
        log_type_all: true,
        array: true,
    };

    const fn all(&self) -> bool {
        self.variable && self.log_type_all && self.array
    }

    fn union(&self, other: &Self) -> Self {
        let mut log_type = self.log_type.clone();
        log_type.extend_from_slice(&other.log_type);
        log_type.sort_unstable();
        log_type.dedup();
        Self {
            variable: self.variable || other.variable,
            log_type,
            log_type_all: self.log_type_all || other.log_type_all,
            array: self.array || other.array,
        }
    }

    /// Whether everything `other` needs is already here.
    fn covers(&self, other: &Self) -> bool {
        (self.variable || !other.variable)
            && (self.array || !other.array)
            && (self.log_type_all
                || (!other.log_type_all
                    && other
                        .log_type
                        .iter()
                        .all(|node| self.log_type.binary_search(node).is_ok())))
    }
}

/// Loads what `wanted` adds to `loaded`, recording it in `loaded`. A log-type dictionary is
/// reloaded for the union of the fields already loaded and those now wanted.
fn load_dictionaries(
    reader: &mut dyn ArchiveReader,
    catalog: &mut ArchiveCatalog,
    options: ArchiveSearchOptions,
    wanted: &Dictionaries,
    loaded: &mut Dictionaries,
) -> Result<(), ScanError> {
    let limits = options.catalog();
    if wanted.variable && !loaded.variable {
        let variable = reader
            .read_variable_dictionary(catalog.metadata(), limits.variable_dictionary())
            .map_err(|source| ScanError::Archive(format!("failed to read var.dict: {source}")))?;
        catalog.set_variable_dictionary(variable);
        loaded.variable = true;
    }
    let log_type_loaded = Dictionaries {
        variable: true,
        array: true,
        ..loaded.clone()
    };
    let log_type_wanted = Dictionaries {
        variable: false,
        array: false,
        ..wanted.clone()
    };
    if !log_type_loaded.covers(&log_type_wanted) {
        let fields = loaded.union(&log_type_wanted);
        let log_type = if fields.log_type_all {
            reader.read_log_type_dictionary(catalog.metadata(), limits.log_type_dictionary())
        } else {
            reader.read_log_type_dictionary_fields(
                catalog.metadata(),
                limits.log_type_dictionary(),
                &fields.log_type,
            )
        }
        .map_err(|source| ScanError::Archive(format!("failed to read log.dict: {source}")))?;
        // A dictionary stored as one sequence loads whole whatever was asked for.
        loaded.log_type_all = fields.log_type_all || log_type.is_complete();
        loaded.log_type = fields.log_type;
        catalog.set_log_type_dictionary(log_type);
    }
    if wanted.array && !loaded.array {
        let array = reader
            .read_array_dictionary(catalog.metadata(), limits.array_dictionary())
            .map_err(|source| ScanError::Archive(format!("failed to read array.dict: {source}")))?;
        catalog.set_array_dictionary(array);
        loaded.array = true;
    }
    Ok(())
}

/// For each schema node, whether a table whose schema the filter can match holds it.
fn nodes_of_matchable_tables(
    catalog: &ArchiveCatalog,
    compiled: &clp_s::search::CompiledQuery<'_, '_>,
) -> Result<Vec<bool>, ScanError> {
    let mut held = vec![false; catalog.schema_tree().len()];
    for table in catalog.table_metadata().schema_tables() {
        // A schema the map lacks is left to the table decoder to report; until then, any column
        // may be written.
        let Some(schema) = catalog.schema_map().get(table.schema_id()) else {
            held.fill(true);
            break;
        };
        if !compiled
            .may_match_schema(schema)
            .map_err(|source| ScanError::Archive(format!("failed to compile query: {source}")))?
        {
            continue;
        }
        for entry in schema.entries() {
            if let SchemaEntry::Node(node) = *entry
                && let Some(slot) = held.get_mut(node as usize)
            {
                *slot = true;
            }
        }
    }
    Ok(held)
}

/// The dictionaries the columns behind `nodes` are read through. A string is a variable ID, a CLP
/// string a logtype whose variables may be IDs, and an unstructured array an array-dictionary
/// logtype whose variables may be IDs; numbers, Booleans and timestamps need none. A CLP string
/// among `shaped` is written as its logtype template, which reads no variable.
fn dictionaries_read_by(tree: &SchemaTree, nodes: &[u32], shaped: &[u32]) -> Dictionaries {
    let mut needed = Dictionaries::default();
    for (node_id, node) in shaped
        .iter()
        .filter_map(|node_id| Some((*node_id, tree.get(*node_id as usize)?)))
    {
        if NodeType::ClpString == node.node_type() {
            needed.log_type.push(node_id);
        } else {
            needed = needed.union(&dictionaries_read_by(tree, &[node_id], &[]));
        }
    }
    for (node_id, node) in nodes
        .iter()
        .filter_map(|node_id| Some((*node_id, tree.get(*node_id as usize)?)))
    {
        match node.node_type() {
            NodeType::VarString | NodeType::DictionaryFloat => needed.variable = true,
            NodeType::ClpString => {
                needed.variable = true;
                needed.log_type.push(node_id);
            }
            NodeType::UnstructuredArray => {
                needed.variable = true;
                needed.array = true;
            }
            _ => {}
        }
    }
    needed
}

/// Every schema node the query's filter reads, sorted: each node a predicate compares, and the
/// log-order column when a range-index selector places rows by walking it.
///
/// Bare default `*` existence needs only schema metadata. Other wildcard paths,
/// unfamiliar namespaces and expression kinds remain conservative: `None` loads
/// every column and dictionary.
fn filter_nodes(parsed: &ParsedQuery, tree: &SchemaTree) -> Option<Vec<u32>> {
    let mut nodes: Vec<u32> = Vec::new();
    let mut needs_log_order = false;
    for node in parsed.nodes() {
        let path = match node.kind() {
            ExpressionKind::Predicate(predicate)
                if predicate.path().is_default_wildcard()
                    && predicate.operator() == ComparisonOperator::Equal
                    && matches!(predicate.value(), Literal::String(value)
                        if value.wildcard_pattern() == "*") =>
            {
                // Bare `*` is existence, not a value search. The semantic evaluator
                // checks schema nodes and terminal array presence without reading
                // values. Keep that evaluator (including NOT/empty-schema behavior),
                // but do not load payload columns or dictionaries for this leaf.
                continue;
            }
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
        nodes.extend(resolve_filter_path(tree, &components));
    }
    if needs_log_order {
        match LogOrderLocator::discover(tree) {
            // Column present: a range selector places rows by walking it, so it joins the set.
            Ok(Some(locator)) => nodes.push(locator.node_id()),
            // Span-based member slicing: no per-row column exists; the selector resolves through
            // the range-index physical spans and needs no extra column read, so keep projecting the
            // query's own columns rather than falling back to reading every column.
            Ok(None) => {}
            // Discovery genuinely failed: fall back to reading every column.
            Err(_) => return None,
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
        let parsed_fields = fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                FieldForm::parse(field).map_err(|error| {
                    ScanError::Projection(format!("projected field {index} `{field}`: {error}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let field_shapes: Vec<FieldForm> = parsed_fields.iter().map(|(_, form)| *form).collect();
        let field_nodes = parsed_fields
            .iter()
            .map(|(descriptor, _)| {
                let components = split_descriptor(descriptor);
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
        let filter_nodes = filter_nodes(&parsed, catalog.schema_tree());
        let wanted_nodes = filter_nodes.as_ref().map(|filter| {
            let mut nodes: Vec<u32> = field_nodes.iter().flatten().copied().collect();
            nodes.extend_from_slice(filter);
            nodes.sort_unstable();
            nodes.dedup();
            nodes
        });
        // The dictionaries are usually most of an archive, and on a string-heavy one decoding them
        // costs more than reading the tables. Only those the read columns go through are loaded:
        // a scan of numbers, Booleans and timestamps loads none, and one of plain strings only the
        // variable dictionary. The filter's load first; the fields' then only for the tables the
        // filter can match, since a column no such table holds is never written.
        let mut loaded = Dictionaries::default();
        let filter_needed = match filter_nodes.as_deref() {
            _ if force_eager => Dictionaries::ALL,
            None => Dictionaries::ALL,
            Some(nodes) => dictionaries_read_by(catalog.schema_tree(), nodes, &[]),
        };
        load_dictionaries(
            &mut *reader,
            &mut catalog,
            options,
            &filter_needed,
            &mut loaded,
        )?;
        // Compiling here answers which source files the selector accepts once, rather than once
        // per stream, and the answer outlives the compiled query it came from.
        let (accepted_ranges, held) = {
            let compiled = parsed
                .compile_for_archive(&catalog, options.search())
                .map_err(|source| {
                    ScanError::Archive(format!("failed to compile query: {source}"))
                })?;
            let accepted_ranges = compiled.accepted_log_event_ranges().map_err(|source| {
                ScanError::Archive(format!("failed to read the range index: {source}"))
            })?;
            (
                accepted_ranges,
                nodes_of_matchable_tables(&catalog, &compiled)?,
            )
        };
        let shaped: Vec<u32> = field_nodes
            .iter()
            .zip(&field_shapes)
            .filter(|(_, shape)| shape.is_shape())
            .flat_map(|(nodes, _)| nodes.iter().copied())
            .collect();
        let plain: Vec<u32> = field_nodes
            .iter()
            .zip(&field_shapes)
            .filter(|(_, shape)| !shape.is_shape())
            .flat_map(|(nodes, _)| nodes.iter().copied())
            .filter(|node| held.get(*node as usize).copied().unwrap_or(false))
            .collect();
        let shaped: Vec<u32> = shaped
            .into_iter()
            .filter(|node| held.get(*node as usize).copied().unwrap_or(false))
            .collect();
        let field_needed = if force_eager {
            Dictionaries::ALL
        } else {
            dictionaries_read_by(catalog.schema_tree(), &plain, &shaped)
        };
        load_dictionaries(
            &mut *reader,
            &mut catalog,
            options,
            &field_needed,
            &mut loaded,
        )?;
        if !loaded.all() {
            // A column whose dictionary was left unloaded is never read, so decoding must not
            // check its IDs against the empty one.
            options = options.with_columns(options.columns().with_dictionary_validation(false));
        }
        let stream_count = catalog.table_metadata().packed_streams().len();
        Ok(Self {
            reader,
            catalog,
            parsed,
            options,
            field_count: fields.len(),
            field_nodes,
            field_shapes,
            filter_nodes,
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
            column_buffers: (0..fields.len()).map(|_| ColumnBuffer::default()).collect(),
            column_views: Vec::with_capacity(fields.len()),
        })
    }

    /// Returns one schema table's matching rows as compact columns.
    ///
    /// Views borrow the scanner until the next call or free. Unfiltered numeric
    /// columns borrow the decoded stream directly; filtered columns gather only
    /// matches. Other types keep the v2 projection semantics. Must not be mixed
    /// with `next_row` or `next_batch` on the same scanner.
    pub fn next_columns(&mut self) -> Result<Option<(&[ColumnView], &[u8], usize)>, ScanError> {
        self.next_columns_with_constants(false)
    }

    pub fn next_columns_with_constants(&mut self, constants: bool) -> Result<Option<(&[ColumnView], &[u8], usize)>, ScanError> {
        self.column_views.clear();
        self.text.clear();
        loop {
            if self.stream.is_none() {
                if self.stream_index >= self.stream_count {
                    return Ok(None);
                }
                if !self.load_next_stream()? {
                    continue;
                }
            }
            if let Some(rows) = self.project_next_table_columns(constants)? {
                return Ok(Some((&self.column_views, &self.text, rows)));
            }
            self.stream = None;
            self.stream_index += 1;
        }
    }

    fn project_next_table_columns(&mut self, constants: bool) -> Result<Option<usize>, ScanError> {
        let stream = self.stream.as_ref().expect("stream is loaded");
        let compiled = self.parsed.compile_for_archive_reusing(
            &self.catalog, self.options.search(), &mut self.dictionary_matches,
        ).map_err(|e| ScanError::Archive(e.to_string()))?;
        let mut tables = self.catalog.schema_tables(
            self.stream_index as u64, stream, self.options.columns(),
        ).map_err(|e| ScanError::Archive(e.to_string()))?;
        if !tables.seek_to_table(self.table_cursor) {
            return Ok(None);
        }
        for decoded in tables {
            let decoded = decoded.map_err(|e| ScanError::Archive(e.to_string()))?;
            self.table_cursor = decoded.table_index() + 1;
            if !compiled.may_match_schema(decoded.schema())
                .map_err(|e| ScanError::Archive(e.to_string()))? {
                continue;
            }
            let bitmap = compiled.match_table(&decoded)
                .map_err(|e| ScanError::Archive(e.to_string()))?;
            let rows = bitmap.match_count();
            if rows == 0 {
                continue;
            }
            // Resolve fields without borrowing all of self: compiled borrows
            // the reusable dictionary matches for this table.
            let columns: Vec<Option<usize>> = self.field_nodes.iter().map(|nodes| {
                decoded.table().columns().iter().position(|c| nodes.contains(&c.node_id()))
            }).collect();
            for (field, slot) in columns.iter().enumerate() {
                let buffer = &mut self.column_buffers[field];
                buffer.numeric.clear();
                buffer.cells.clear();
                let data = slot.and_then(|i| decoded.table().column(i)).map(|c| c.data());
                let (kind, bytes, constant) = match data {
                    Some(ColumnData::Integer(values)) => (kind::INTEGER, values.canonical_bytes(), values.constant_value().map(|v| v as u64)),
                    Some(ColumnData::Float(values)) => (kind::FLOAT, values.canonical_bytes(), values.constant_value().map(f64::to_bits)),
                    _ => (kind::UNSUPPORTED, None, None),
                };
                let view = if constants && let Some(value) = constant {
                    buffer.numeric.push(value);
                    ColumnView {
                        kind: if kind == kind::INTEGER { kind::CONSTANT_INTEGER } else { kind::CONSTANT_FLOAT },
                        reserved: 0, data: buffer.numeric.as_ptr().cast(),
                    }
                } else if let Some(bytes) = bytes {
                    // A projected member may have a truncated column. Only
                    // borrow when it covers the complete physical row domain.
                    if cfg!(target_endian = "little")
                        && rows == bitmap.len() && bytes.len() / 8 == bitmap.len() {
                        ColumnView { kind, reserved: 0, data: bytes.as_ptr() }
                    } else {
                        for row in bitmap.matching_rows() {
                            let start = row.checked_mul(8).ok_or_else(|| ScanError::Projection("row offset overflow".into()))?;
                            let value = bytes.get(start..start + 8).ok_or_else(|| ScanError::Projection("selected row outside numeric column".into()))?;
                            buffer.numeric.push(u64::from_le_bytes(value.try_into().expect("8 bytes")));
                        }
                        ColumnView { kind, reserved: 0, data: buffer.numeric.as_ptr().cast() }
                    }
                } else if matches!(kind, kind::INTEGER | kind::FLOAT) {
                    // Compact dictionaries/runs expand only the selected output. A v3 caller
                    // also gets dense constants, preserving that API's original contract.
                    if rows == bitmap.len() {
                        match data {
                            Some(ColumnData::Integer(values)) => values.append_bits(&mut buffer.numeric),
                            Some(ColumnData::Float(values)) => values.append_bits(&mut buffer.numeric),
                            _ => unreachable!(),
                        }
                    } else {
                        for row in bitmap.matching_rows() {
                            let bits = match data {
                                Some(ColumnData::Integer(values)) => values.get(row).map(|v| v as u64),
                                Some(ColumnData::Float(values)) => values.get(row).map(f64::to_bits),
                                _ => unreachable!(),
                            }.ok_or_else(|| ScanError::Projection("selected row outside compact column".into()))?;
                            buffer.numeric.push(bits);
                        }
                    }
                    ColumnView { kind, reserved: 0, data: buffer.numeric.as_ptr().cast() }
                } else if data.is_none() {
                    ColumnView { kind: kind::ABSENT, reserved: 0, data: std::ptr::null() }
                } else {
                    for row in bitmap.matching_rows() {
                        project_row(
                            &decoded, &[*slot], &self.field_shapes[field..field + 1],
                            self.catalog.timestamp_patterns(), row, &mut buffer.cells, &mut self.text,
                        )?;
                    }
                    ColumnView { kind: kind::UNSUPPORTED, reserved: 0, data: buffer.cells.as_ptr().cast() }
                };
                self.column_views.push(view);
            }
            return Ok(Some(rows));
        }
        Ok(None)
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
        // A stream none of whose tables has a schema the query can match holds no result row, so it
        // is skipped unread. The cursor moves past its tables, since resuming before a stream's
        // first table would read that stream as finished.
        let (first_table, end_table) = self.stream_table_range(stream_id);
        if !self.any_schema_may_match(first_table..end_table)? {
            self.table_cursor = end_table;
            self.stream_index += 1;
            return Ok(false);
        }
        let mask = self.projection_mask(stream_id);
        let truncate = match mask.as_deref() {
            Some(wanted) => self.plan_truncation(stream_id, wanted),
            None => None,
        };
        // The columns the filter reads are read alone first when the fields add others, so a
        // stream without a match costs only those.
        if truncate.is_none()
            && let Some(wanted) = mask.as_deref()
            && let Some(matching) = self.filter_mask(stream_id)
            && matching != wanted
            && !self.matches_from_columns(stream_id, first_table, &matching)?
        {
            self.table_cursor = end_table;
            self.stream_index += 1;
            return Ok(false);
        }
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
        // Rewrite adaptive integer columns back to plain 8-byte values so the fixed-stride table
        // decoder and random-access cell reader are unchanged (a no-op for 0.5.x archives).
        let stream = self
            .catalog
            .materialize_adaptive_numerics(stream_id, stream, self.options.columns())
            .map_err(|source| {
                ScanError::Archive(format!(
                    "failed to materialize adaptive columns in stream {stream_id}: {source}"
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
        // Adaptive numeric columns are variable-width, so their bytes cannot be prefix-cut by the
        // fixed-stride truncation; they are always read whole and materialized afterwards.
        let adaptive = self.catalog.version().supports_adaptive_numeric();
        let mut truncatable = Vec::with_capacity(nodes.len());
        for (index, node_id) in nodes.iter().enumerate() {
            let node_type = tree.get(*node_id as usize).map(|node| node.node_type());
            let adaptive_numeric = adaptive
                && matches!(node_type, Some(NodeType::Integer | NodeType::Float));
            let fixed = node_type.is_some_and(Self::truncatable) && !adaptive_numeric;
            truncatable.push(
                fixed && index != log_order_column && wanted.get(index).copied() == Some(true),
            );
        }
        truncatable
            .iter()
            .any(|value| *value)
            .then_some((log_order_column, limit, truncatable))
    }

    /// Which columns of a separate-column stream the filter reads, in column order, or `None`
    /// when its columns are unknown or the stream shares one frame across its tables.
    fn filter_mask(&self, stream_id: u64) -> Option<Vec<bool>> {
        let filter = self.filter_nodes.as_deref()?;
        self.column_nodes(stream_id).map(|nodes| {
            nodes
                .iter()
                .map(|id| filter.binary_search(id).is_ok())
                .collect()
        })
    }

    /// Whether any row of the one table in a separate-column stream matches, judged from the
    /// columns `mask` selects. A table out of place answers `true`, leaving the whole read to
    /// report it.
    fn matches_from_columns(
        &mut self,
        stream_id: u64,
        table_index: usize,
        mask: &[bool],
    ) -> Result<bool, ScanError> {
        let stream = self
            .reader
            .read_packed_stream_projected(
                self.catalog.metadata(),
                self.catalog.table_metadata(),
                self.stream_index,
                mask,
                None,
                self.options.packed_stream(),
            )
            .map_err(|source| {
                ScanError::Archive(format!(
                    "failed to read packed stream {stream_id}: {source}"
                ))
            })?;
        let stream = self
            .catalog
            .materialize_adaptive_numerics(stream_id, stream, self.options.columns())
            .map_err(|source| {
                ScanError::Archive(format!(
                    "failed to materialize adaptive columns in stream {stream_id}: {source}"
                ))
            })?;
        let compiled = self
            .parsed
            .compile_for_archive_reusing(
                &self.catalog,
                self.options.search(),
                &mut self.dictionary_matches,
            )
            .map_err(|source| ScanError::Archive(format!("failed to compile query: {source}")))?;
        let Some(decoded) = self
            .catalog
            .schema_tables(stream_id, &stream, self.options.columns())
            .map_err(|source| {
                ScanError::Archive(format!("failed to open stream {stream_id}: {source}"))
            })?
            .next()
            .transpose()
            .map_err(|source| ScanError::Archive(format!("failed to decode table: {source}")))?
            .filter(|decoded| decoded.table_index() == table_index)
        else {
            return Ok(true);
        };
        let bitmap = compiled
            .match_table(&decoded)
            .map_err(|source| ScanError::Archive(format!("failed to match table: {source}")))?;
        Ok(0 != bitmap.match_count())
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

    /// The archive-wide indexes of the tables stored in one packed stream, which table metadata
    /// keeps grouped by stream.
    fn stream_table_range(&self, stream_id: u64) -> (usize, usize) {
        let tables = self.catalog.table_metadata().schema_tables();
        let first = tables.partition_point(|table| table.stream_id() < stream_id);
        let end = first + tables[first..].partition_point(|table| table.stream_id() == stream_id);
        (first, end)
    }

    /// Whether the query could match a row of any table in `tables`, judged from schemas alone.
    fn any_schema_may_match(&mut self, tables: std::ops::Range<usize>) -> Result<bool, ScanError> {
        let compiled = self
            .parsed
            .compile_for_archive_reusing(
                &self.catalog,
                self.options.search(),
                &mut self.dictionary_matches,
            )
            .map_err(|source| ScanError::Archive(format!("failed to compile query: {source}")))?;
        let metadata = self.catalog.table_metadata().schema_tables();
        for table in &metadata[tables] {
            // A schema the map lacks is left to the table decoder to report.
            let Some(schema) = self.catalog.schema_map().get(table.schema_id()) else {
                return Ok(true);
            };
            if compiled.may_match_schema(schema).map_err(|source| {
                ScanError::Archive(format!("failed to compile query: {source}"))
            })? {
                return Ok(true);
            }
        }
        Ok(false)
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
            // A table whose schema the query cannot match is passed over without evaluating it.
            if !compiled
                .may_match_schema(decoded.schema())
                .map_err(|source| ScanError::Archive(format!("failed to match table: {source}")))?
            {
                table_cursor = decoded.table_index() + 1;
                continue;
            }
            let bitmap = compiled
                .match_table(&decoded)
                .map_err(|source| ScanError::Archive(format!("failed to match table: {source}")))?;
            if 0 == bitmap.match_count() {
                table_cursor = decoded.table_index() + 1;
                continue;
            }
            let columns = self.column_slots(&decoded);
            for row in bitmap.matching_rows() {
                project_row(
                    &decoded,
                    &columns,
                    &self.field_shapes,
                    self.catalog.timestamp_patterns(),
                    row,
                    &mut cells,
                    &mut text,
                )?;
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

/// Reads one row's projected values out of the decoded columns. A field `shapes` marks as a shape
/// writes a CLP string as its template, as its dictionary entry holds it or parsed (see
/// [`FieldForm`]), rather than its value, and a timestamp as the text it was stored from, as a
/// whole record would show it, since such a field has no record to read it from.
fn project_row(
    decoded: &DecodedSchemaTable<'_, '_>,
    columns: &[Option<usize>],
    shapes: &[FieldForm],
    timestamps: &TimestampPatternCatalog,
    row: usize,
    cells: &mut Vec<Cell>,
    text: &mut Vec<u8>,
) -> Result<(), ScanError> {
    let table = decoded.table();
    for (field, slot) in columns.iter().enumerate() {
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
            ColumnData::Timestamp(values)
                if shapes.get(field).is_some_and(|form| form.is_shape()) =>
            {
                match values.get(row) {
                    Some(value) => {
                        let mut formatted = String::new();
                        if timestamps
                            .append_epoch_nanoseconds(
                                value.pattern_id(),
                                value.epoch_nanoseconds(),
                                &mut formatted,
                            )
                            .is_ok()
                        {
                            // The text is a JSON literal: a quoted string, or a bare number.
                            if let Some(inner) = formatted
                                .strip_prefix('"')
                                .and_then(|rest| rest.strip_suffix('"'))
                            {
                                push_text(kind::STRING, inner.as_bytes(), text)
                            } else if let Ok(integer) = formatted.parse::<i64>() {
                                Cell::scalar(kind::INTEGER, integer, 0.0)
                            } else if let Ok(real) = formatted.parse::<f64>() {
                                Cell::scalar(kind::FLOAT, 0, real)
                            } else {
                                Cell::scalar(kind::TIMESTAMP, value.epoch_nanoseconds(), 0.0)
                            }
                        } else {
                            Cell::scalar(kind::TIMESTAMP, value.epoch_nanoseconds(), 0.0)
                        }
                    }
                    None => Cell::absent(),
                }
            }
            ColumnData::Timestamp(values) => values.get(row).map_or_else(Cell::absent, |value| {
                Cell::scalar(kind::TIMESTAMP, value.epoch_nanoseconds(), 0.0)
            }),
            ColumnData::VarString(values) | ColumnData::DictionaryFloat(values) => values
                .value(row)
                .map_or_else(Cell::absent, |bytes| push_text(kind::STRING, bytes, text)),
            ColumnData::ClpString(values)
                if shapes.get(field).is_some_and(|form| form.is_shape()) =>
            {
                match values.record(row) {
                    Some(record) => {
                        let start = text.len();
                        if Some(&FieldForm::ParsedShape) == shapes.get(field) {
                            append_parsed_logtype_template(record.logtype(), text);
                        } else {
                            append_logtype_template(record.logtype(), text);
                        }
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

#[cfg(test)]
mod tests {
    use super::split_descriptor;

    #[test]
    fn descriptors_split_on_unescaped_dots_and_decode_escapes() {
        assert_eq!(vec!["a", "b"], split_descriptor("a.b"));
        assert_eq!(vec!["a.b", "c"], split_descriptor("a\\.b.c"));
        assert_eq!(vec!["back\\slash"], split_descriptor("back\\\\slash"));
        assert_eq!(vec!["x, all"], split_descriptor("x\\u002c all"));
        assert_eq!(vec!["my key "], split_descriptor("my key "));
        assert_eq!(
            vec!["u002"],
            split_descriptor("\\u002"),
            "an incomplete code is the letter it starts with"
        );
    }
}

#[cfg(test)]
mod wildcard_projection_tests {
    use super::*;
    use std::io::Cursor;
    use clp_s::archive::SingleFileArchiveReader;
    use clp_s::search::{parse_kql, KqlLimits};

    #[test]
    fn existence_loads_only_projected_columns_and_no_dictionaries() {
        let mut writer = clp_s::writer::OpenArchive::new(Cursor::new(Vec::new()),
            clp_s::writer::WriterOptions::default().with_separate_columns_min_size(1)
                .with_adaptive_numeric_columns(true));
        let source = b"{\"n\":1,\"text\":\"hello 12 world\",\"tag\":\"alpha\"}\n{\"n\":2,\"text\":\"hello 34 world\",\"tag\":\"beta\"}\n";
        let mut parser = clp_s::ingest::ParseManyReader::new(&source[..], Default::default());
        parser.read_to_end(&mut clp_s::ingest::JsonArchiveSink::new(&mut writer, Default::default())).unwrap();
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        for fields in [vec!["n".to_owned()], Vec::new()] {
            let reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone())).unwrap();
            let parsed = parse_kql("*", KqlLimits::default()).unwrap();
            let mut scan = ProjectedScanner::open(Box::new(reader), parsed, &Default::default(), &fields).unwrap();
            assert_eq!(Some(vec![]), scan.filter_nodes);
            assert!(scan.catalog.variable_dictionary().is_empty());
            assert!(scan.catalog.log_type_dictionary().is_empty());
            assert!(scan.catalog.array_dictionary().is_empty());
            let (_, _, rows) = scan.next_columns().unwrap().unwrap();
            assert_eq!(2, rows);
            let layout = scan.stream.as_ref().unwrap().column_layout().unwrap();
            assert_eq!(fields.len(), layout.iter().filter(|slot| slot.loaded).count());
            assert_eq!(fields.len() * rows * 8, scan.stream.as_ref().unwrap().len());
            // Value-search wildcards still require their payloads.
            let parsed = parse_kql("*:alpha", KqlLimits::default()).unwrap();
            assert!(filter_nodes(&parsed, scan.catalog.schema_tree()).is_none());
        }
    }
}

#[cfg(test)]
mod encoded_numeric_tests {
    use super::*;
    use std::io::Cursor;
    use clp_s::archive::{NumericEncoding, SingleFileArchiveReader};
    use clp_s::search::{parse_kql, KqlLimits};

    #[test]
    fn constants_stay_compact_and_v3_remains_dense() {
        let mut writer = clp_s::writer::OpenArchive::new(Cursor::new(Vec::new()),
            clp_s::writer::WriterOptions::default().with_separate_columns_min_size(1)
                .with_adaptive_numeric_columns(true));
        let source = "{\"i\":-42,\"f\":2.5}\n".repeat(10000);
        let mut parser = clp_s::ingest::ParseManyReader::new(source.as_bytes(), Default::default());
        parser.read_to_end(&mut clp_s::ingest::JsonArchiveSink::new(&mut writer,
            clp_s::ingest::JsonArchiveOptions::default().with_retain_float_format(false))).unwrap();
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        for constants in [false, true] {
            for query in ["*", "i:-42 AND f >= 2.5", "NOT i:1 OR f < 0", "i:1 OR f >= 2.5"] {
                let reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone())).unwrap();
                let parsed = parse_kql(query, KqlLimits::default()).unwrap();
                let mut scan = ProjectedScanner::open(Box::new(reader), parsed, &Default::default(),
                    &["i".to_owned(), "f".to_owned()]).unwrap();
                let (columns, _, rows) = scan.next_columns_with_constants(constants).unwrap().unwrap();
                assert_eq!(10000, rows);
                assert_eq!(columns[0].kind, if constants { kind::CONSTANT_INTEGER } else { kind::INTEGER });
                assert_eq!(columns[1].kind, if constants { kind::CONSTANT_FLOAT } else { kind::FLOAT });
                // SAFETY: The borrowed views remain live until the next scanner call.
                unsafe {
                    for row in 0..if constants { 1 } else { rows } {
                        assert_eq!(-42, columns[0].data.cast::<i64>().add(row).read_unaligned());
                        assert_eq!(2.5f64.to_bits(), columns[1].data.cast::<u64>().add(row).read_unaligned());
                    }
                }
                let stream = scan.stream.as_ref().unwrap();
                assert_eq!(16, stream.len());
                assert!(stream.column_layout().unwrap().iter().filter(|slot| slot.loaded)
                    .all(|slot| slot.numeric_encoding == Some(NumericEncoding::Constant)));
                for buffer in &scan.column_buffers { assert_eq!(if constants { 1 } else { rows }, buffer.numeric.len()); }
            }
        }
    }
}
