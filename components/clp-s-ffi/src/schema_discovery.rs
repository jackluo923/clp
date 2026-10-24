//! Queryable schema inventories, selected by field names and exact time-window presence.
//! No event JSON is reconstructed. Boundary windows read only the time column.
use super::*;
use clp_s::archive::{ArchiveCatalog, NodeType, SchemaEntry};
use clp_s::numeric_summary::{NumericSummaries, integer};
use clp_s::timestamp_chunks::TimestampChunks;
use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;

#[repr(C)]
pub struct Selector {
    pub path: *const u8,
    pub length: usize,
    /// 0 = exact escaped dot path; 1 = SQL LIKE (% and _, case sensitive, no escape clause).
    pub pattern: u32,
}

#[repr(C)]
pub struct Options {
    pub selectors: *const Selector,
    pub selector_count: usize,
    pub match_all: u32,
    /// 0 = no window; 1 = timestamp epoch ns; 2 = ordinary signed integer values.
    pub time_kind: u32,
    pub time_path: *const u8,
    pub time_path_length: usize,
    /// Bit 0: inclusive lower bound; bit 1: exclusive upper bound.
    pub bounds: u32,
    pub lower: i64,
    pub upper: i64,
    pub max_compressed: u64,
    pub max_decompressed: u64,
}

#[repr(C)]
pub struct Field {
    pub path: *const u8,
    pub path_length: usize,
    pub typ: *const u8,
    pub type_length: usize,
}

#[repr(C)]
pub struct Schema {
    pub fields: *const Field,
    pub field_count: usize,
    pub matching_rows: u64,
    pub total_rows: u64,
    pub scanned: u32,
}

#[derive(Default)]
struct Inventory {
    fields: BTreeSet<(String, &'static str)>,
    rows: u64,
    total_rows: u64,
    scanned: bool,
}

struct Selection {
    fields: Vec<(String, bool)>,
    all: bool,
    time_kind: u32,
    time_path: String,
    lower: Option<i64>,
    upper: Option<i64>,
}

fn failure(message: impl ToString) -> ApiError {
    ApiError::Archive(message.to_string())
}

/// SQL LIKE over Unicode characters. Backslashes are literal; exact selectors can name %/_ keys.
fn like(value: &str, pattern: &str) -> bool {
    let value: Vec<char> = value.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut v, mut p, mut retry) = (0, 0, None);
    while v < value.len() {
        if p < pattern.len() && pattern[p] == '%' {
            retry = Some((p + 1, v));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '_' || pattern[p] == value[v]) {
            v += 1;
            p += 1;
        } else if let Some((next, previous)) = retry {
            v = previous + 1;
            p = next;
            retry = Some((next, v));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '%' {
        p += 1;
    }
    p == pattern.len()
}

impl Selection {
    fn matches(&self, inventory: &Inventory) -> bool {
        let matches = |(selector, pattern): &(String, bool)| {
            inventory.fields.iter().any(|(path, _)| {
                if *pattern {
                    like(path, selector)
                } else {
                    path == selector
                }
            })
        };
        self.fields.is_empty()
            || if self.all {
                self.fields.iter().all(matches)
            } else {
                self.fields.iter().any(matches)
            }
    }

    /// True/false only when extrema prove every/no value passes. Overlap is not presence.
    fn coverage(&self, lo: i128, hi: i128) -> Option<bool> {
        if lo > hi {
            return None;
        }
        if self.lower.is_some_and(|v| hi < i128::from(v))
            || self.upper.is_some_and(|v| lo >= i128::from(v))
        {
            Some(false)
        } else if self.lower.is_none_or(|v| lo >= i128::from(v))
            && self.upper.is_none_or(|v| hi < i128::from(v))
        {
            Some(true)
        } else {
            None
        }
    }

    fn query(&self) -> Result<clp_s::search::ParsedQuery, ApiError> {
        // Re-encode decoded path components, not the descriptor's existing escapes. Doubling
        // `\.` would change a literal dot into a path separator. Unicode escapes also protect
        // literal wildcard/namespace characters and quotes from the KQL grammar.
        let components = columnar::split_descriptor(&self.time_path);
        if components.iter().any(String::is_empty) {
            return Err(ApiError::InvalidArgument(
                "time_path has an empty component".into(),
            ));
        }
        let path = components
            .iter()
            .map(|component| {
                let mut escaped = String::new();
                for character in component.chars() {
                    if character.is_ascii()
                        && !character.is_ascii_alphanumeric()
                        && character != '_'
                    {
                        use std::fmt::Write;
                        write!(escaped, "\\u{:04x}", character as u32).expect("string formatting");
                    } else {
                        escaped.push(character);
                    }
                }
                escaped
            })
            .collect::<Vec<_>>()
            .join(".");
        let path = format!("\"{path}\"");
        let mut clauses = Vec::new();
        if let Some(lower) = self.lower {
            clauses.push(format!("{path} >= {lower}"));
        }
        if let Some(upper) = self.upper {
            clauses.push(format!("{path} < {upper}"));
        }
        let parsed = parse_kql(&clauses.join(" AND "), KqlLimits::default()).map_err(failure)?;
        // A time path must identify one scalar, never a namespace or a wildcard search.
        for node in parsed.nodes() {
            if let clp_s::search::ExpressionKind::Predicate(predicate) = node.kind() {
                if predicate.path().namespace() != clp_s::search::ColumnNamespace::Default
                    || predicate
                        .path()
                        .components()
                        .iter()
                        .map(|c| c.value())
                        .ne(components.iter().map(String::as_str))
                    || predicate
                        .path()
                        .components()
                        .iter()
                        .any(clp_s::search::PathComponent::is_wildcard)
                {
                    return Err(ApiError::InvalidArgument(
                        "schema discovery time_path must be an exact field path".into(),
                    ));
                }
            }
        }
        Ok(parsed)
    }
}

fn kind(node: NodeType) -> &'static str {
    match node {
        NodeType::Integer | NodeType::DeltaInteger => "integer",
        NodeType::MixedInteger => "integer_or_unsigned",
        NodeType::Float | NodeType::FormattedFloat | NodeType::DictionaryFloat => "float",
        NodeType::ClpString | NodeType::VarString | NodeType::DeprecatedDateString => "string",
        NodeType::Boolean => "boolean",
        NodeType::Object => "object",
        NodeType::StructuredArray | NodeType::UnstructuredArray => "array",
        NodeType::Null => "null",
        NodeType::Timestamp => "timestamp",
        _ => "unsupported",
    }
}

fn discover(
    reader: Box<dyn ArchiveReader>,
    catalog: ArchiveCatalog,
    selection: &Selection,
    options: &ArchiveSearchOptions,
    check: impl Fn() -> Result<(), ApiError>,
) -> Result<Vec<Inventory>, ApiError> {
    check()?;
    // Canonical inventories intentionally describe queryable dot paths. Arrays are containers,
    // not separate schemas for every element count; physical encodings/key order are not types.
    let paths = columnar::archive_field_types(catalog.schema_tree());
    let paths: HashMap<_, _> = paths
        .into_iter()
        .map(|field| (field.node_id, field))
        .collect();
    let summaries = catalog
        .metadata()
        .numeric_summaries()
        .and_then(|b| NumericSummaries::decode(b).ok());
    let mut summary_index = HashMap::new();
    if let Some(summaries) = &summaries {
        for column in &summaries.columns {
            // Array elements can repeat a node within a physical schema. A duplicate key
            // cannot certify one scalar column's extrema/count, so do not use that entry.
            summary_index
                .entry((column.schema, column.node))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(column));
        }
    }
    let chunks = catalog
        .metadata()
        .timestamp_chunks()
        .and_then(|b| TimestampChunks::decode(b).ok());
    let mut chunk_index = HashMap::new();
    if let Some(chunks) = &chunks {
        for column in &chunks.columns {
            chunk_index
                .entry((column.schema, column.node))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(column));
        }
    }
    let time_nodes = columnar::resolve_descriptor_path(catalog.schema_tree(), &selection.time_path);
    let mut result = Vec::new();
    let mut scan_tables = Vec::new();
    let empty_window = selection
        .lower
        .zip(selection.upper)
        .is_some_and(|(lo, hi)| lo == hi);
    // Validate the time descriptor even when metadata happens to answer the complete query.
    let query = if selection.time_kind == 0 {
        None
    } else {
        Some(selection.query()?)
    };
    for table in catalog.table_metadata().schema_tables() {
        check()?;
        let mut inventory = Inventory {
            total_rows: table.message_count(),
            ..Default::default()
        };
        let schema = catalog
            .schema_map()
            .get(table.schema_id())
            .ok_or_else(|| failure("missing schema"))?;
        let mut present = BTreeSet::new();
        for entry in schema.entries() {
            if let SchemaEntry::Node(id) = entry {
                let mut current = Some(*id as usize);
                // Object parents are implicit in some schema entries. Include them, but not
                // unrelated children from another schema of the same archive.
                while let Some(id) = current {
                    if !present.insert(id) {
                        break;
                    }
                    current = catalog.schema_tree().get(id).and_then(|n| n.parent_id());
                }
            }
        }
        for id in &present {
            let Some(field) = paths.get(id) else {
                continue;
            };
            let path = str::from_utf8(&field.path).map_err(failure)?.to_owned();
            if field.node_type == NodeType::MixedInteger
                && let Some(summary) = summary_index
                    .get(&(table.schema_id(), *id as u32))
                    .copied()
                    .flatten()
                && summary
                    .negative
                    .count
                    .checked_add(summary.nonnegative.count)
                    .and_then(|n| n.checked_add(summary.unsigned.count))
                    == Some(table.message_count())
                && summary.float.count == 0
            {
                if summary.negative.count != 0 || summary.nonnegative.count != 0 {
                    inventory.fields.insert((path.clone(), "integer"));
                }
                if summary.unsigned.count != 0 {
                    inventory.fields.insert((path, "unsigned"));
                }
            } else {
                inventory.fields.insert((path, kind(field.node_type)));
            }
        }
        let selected = selection.matches(&inventory) && !empty_window && table.message_count() != 0;
        let mut scan = false;
        if selected && selection.time_kind == 0 {
            inventory.rows = table.message_count();
        } else if selected {
            let matching: Vec<_> = schema
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    SchemaEntry::Node(id) if time_nodes.contains(id) => Some(*id),
                    _ => None,
                })
                .collect();
            if matching.len() > 1 {
                return Err(failure("ambiguous repeated time field in schema"));
            }
            if let Some(&node) = matching.first() {
                let typ = catalog
                    .schema_tree()
                    .get(node as usize)
                    .ok_or_else(|| failure("missing node"))?
                    .node_type();
                let eligible = if selection.time_kind == 1 {
                    typ == NodeType::Timestamp
                } else {
                    matches!(
                        typ,
                        NodeType::Integer | NodeType::DeltaInteger | NodeType::MixedInteger
                    )
                };
                if eligible {
                    scan = true;
                    let key = (table.schema_id(), node);
                    if selection.time_kind == 1 {
                        if let (Some(chunks), Some(column)) =
                            (&chunks, chunk_index.get(&key).copied().flatten())
                        {
                            let stride = u64::from(chunks.chunk_rows);
                            if column.bounds.len() as u64 == table.message_count().div_ceil(stride)
                            {
                                let mut count = Some(0);
                                for (i, &[lo, hi]) in column.bounds.iter().enumerate() {
                                    count = count
                                        .zip(selection.coverage(i128::from(lo), i128::from(hi)))
                                        .map(|(n, all)| {
                                            n + if all {
                                                stride
                                                    .min(table.message_count() - i as u64 * stride)
                                            } else {
                                                0
                                            }
                                        });
                                }
                                if let Some(n) = count {
                                    inventory.rows = n;
                                    scan = false;
                                }
                            }
                        }
                    } else if let Some(column) = summary_index.get(&key).copied().flatten() {
                        let total = column
                            .negative
                            .count
                            .checked_add(column.nonnegative.count)
                            .and_then(|n| n.checked_add(column.unsigned.count));
                        if total == Some(table.message_count()) && column.float.count == 0 {
                            let mut count = Some(0);
                            // A BIGINT time window only admits signed integer rows. UInt64-only
                            // values are excluded, exactly as the typed mapping would exclude them.
                            for bucket in [&column.negative, &column.nonnegative] {
                                if bucket.count != 0 {
                                    let (lo, hi) = (integer(bucket.min), integer(bucket.max));
                                    if lo < i128::from(i64::MIN) || hi > i128::from(i64::MAX) {
                                        count = None;
                                    }
                                    count = count
                                        .zip(selection.coverage(lo, hi))
                                        .map(|(n, all)| n + if all { bucket.count } else { 0 });
                                }
                            }
                            if let Some(n) = count {
                                inventory.rows = n;
                                scan = false;
                            }
                        }
                    }
                }
            }
        }
        inventory.scanned = scan;
        if !scan && inventory.rows == 0 {
            inventory.fields.clear();
        }
        scan_tables.push(scan);
        result.push(inventory);
    }
    if scan_tables.iter().any(|v| *v) {
        let mut scanner = columnar::ProjectedScanner::open_with_catalog(
            reader,
            catalog,
            query.expect("window scan has predicate"),
            options,
            &[],
        )
        .map_err(failure)?;
        // Keep per-table accounting: the ordinary zero-projection count fast path can combine
        // several all-match tables in one stream into one batch.
        scanner
            .set_numeric_plans(
                scan_tables
                    .iter()
                    .map(|_| columnar::NumericColumnPlan {
                        fields: vec![],
                        all_rows: false,
                    })
                    .collect(),
            )
            .map_err(failure)?;
        scanner.restrict_tables(&scan_tables).map_err(failure)?;
        loop {
            check()?;
            let Some((table, _, _, rows)) = scanner.next_numeric_columns().map_err(failure)? else {
                break;
            };
            result[table].rows = result[table]
                .rows
                .checked_add(rows as u64)
                .ok_or_else(|| failure("row count overflow"))?;
        }
    }
    check()?;
    result.retain(|row| row.rows != 0);
    Ok(result)
}

/// Synchronous callback; all returned byte slices live only until that callback returns.
///
/// # Safety
/// Inputs must be readable for their lengths and callbacks/context/error valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v8_discover_schemas(
    path: *const u8,
    path_len: usize,
    options: *const Options,
    emit: Option<unsafe extern "C" fn(*mut c_void, *const Schema) -> u32>,
    cancel: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    context: *mut c_void,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    unsafe {
        ffi_entry(error, || {
            let options = options
                .as_ref()
                .ok_or_else(|| ApiError::InvalidArgument("null schema options".into()))?;
            let emit =
                emit.ok_or_else(|| ApiError::InvalidArgument("null schema callback".into()))?;
            if options.match_all > 1
                || options.time_kind > 2
                || options.bounds > 3
                || (options.time_kind == 0) != (options.bounds == 0)
                || options.selector_count > 1024
                || (options.selector_count != 0 && options.selectors.is_null())
            {
                return Err(ApiError::InvalidArgument(
                    "invalid schema discovery options".into(),
                ));
            }
            let mut selection = Selection {
                fields: vec![],
                all: options.match_all != 0,
                time_kind: options.time_kind,
                time_path: str::from_utf8(borrowed_bytes(
                    options.time_path,
                    options.time_path_length,
                    "time path",
                )?)
                .map_err(failure)?
                .into(),
                lower: (options.bounds & 1 != 0).then_some(options.lower),
                upper: (options.bounds & 2 != 0).then_some(options.upper),
            };
            if selection
                .lower
                .zip(selection.upper)
                .is_some_and(|(lo, hi)| lo > hi)
                || (selection.time_kind != 0 && selection.time_path.is_empty())
            {
                return Err(ApiError::InvalidArgument(
                    "invalid schema discovery time window".into(),
                ));
            }
            for i in 0..options.selector_count {
                let field = &*options.selectors.add(i);
                if field.pattern > 1 || field.length > 4096 {
                    return Err(ApiError::InvalidArgument(
                        "invalid schema field selector".into(),
                    ));
                }
                let name = str::from_utf8(borrowed_bytes(field.path, field.length, "selector")?)
                    .map_err(failure)?;
                selection.fields.push((name.to_owned(), field.pattern != 0));
            }
            let check = || {
                if cancel.is_some_and(|f| f(context) != 0) {
                    Err(ApiError::Cancelled("schema discovery interrupted".into()))
                } else {
                    Ok(())
                }
            };
            check()?;
            let path = str::from_utf8(borrowed_bytes(path, path_len, "path")?).map_err(failure)?;
            let mut reader = ClpSArchive::locate(PathBuf::from(path))?.open_reader()?;
            let catalog = reader
                .read_catalog(ArchiveCatalogLimits::default().with_skip_dictionaries(true))
                .map_err(failure)?;
            let scan = ArchiveSearchOptions::default();
            let limits = scan.packed_stream();
            let scan = scan.with_packed_stream(PackedStreamLimits::new(
                if options.max_compressed == 0 {
                    limits.max_compressed_size()
                } else {
                    options.max_compressed
                },
                if options.max_decompressed == 0 {
                    limits.max_decompressed_size()
                } else {
                    options.max_decompressed
                },
            ));
            for inventory in discover(reader, catalog, &selection, &scan, check)? {
                check()?;
                let fields: Vec<_> = inventory
                    .fields
                    .iter()
                    .map(|(path, typ)| Field {
                        path: path.as_ptr(),
                        path_length: path.len(),
                        typ: typ.as_ptr(),
                        type_length: typ.len(),
                    })
                    .collect();
                let row = Schema {
                    fields: fields.as_ptr(),
                    field_count: fields.len(),
                    matching_rows: inventory.rows,
                    total_rows: inventory.total_rows,
                    scanned: u32::from(inventory.scanned),
                };
                if emit(context, &row) != 0 {
                    return Err(ApiError::Callback("schema callback stopped".into()));
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clp_s::archive::SingleFileArchiveReader;
    use clp_s::writer::{FieldRef, OpenArchive, RecordRef, TimestampRef, ValueRef, WriterOptions};
    use std::io::Cursor;

    #[test]
    fn like_patterns_match_characters_and_literal_backslashes() {
        for (v, p, expected) in [
            ("nested.温度", "nested.__", true),
            ("abc", "%b%", true),
            ("abc", "a_d", false),
            ("", "%", true),
            ("", "_", false),
            ("a\\.b", "a\\.%", true),
            ("abc", "%bc%z", false),
            ("a", "%%", true),
        ] {
            assert_eq!(like(v, p), expected, "{v} LIKE {p}");
        }
    }

    #[test]
    fn exact_time_descriptors_round_trip_without_kql_injection() {
        for path in [
            r"clock\.tick",
            "nested.ts",
            "t*",
            "*",
            "@timestamp",
            r"a\\b",
            "a\"b",
            "a\nb",
            "温度",
        ] {
            let selection = Selection {
                fields: vec![],
                all: false,
                time_kind: 2,
                time_path: path.into(),
                lower: Some(1),
                upper: Some(2),
            };
            selection.query().unwrap(); // query() certifies decoded components against the source path.
        }
        assert_eq!(std::mem::size_of::<Options>(), 80);
        assert_eq!(std::mem::size_of::<Schema>(), 40);
        assert_eq!(std::mem::size_of::<Field>(), 32);
    }

    fn fixture(summaries: bool) -> (Box<dyn ArchiveReader>, ArchiveCatalog) {
        let mut writer = OpenArchive::new(
            Cursor::new(Vec::new()),
            WriterOptions::default()
                .with_numeric_summaries(summaries)
                .with_separate_columns_min_size(1),
        );
        for (epoch, label) in [(0, b"a"), (100, b"a"), (50, b"b")] {
            let raw = epoch.to_string();
            writer
                .append_record(RecordRef::new(&[
                    FieldRef::new(
                        b"ts",
                        ValueRef::Timestamp(TimestampRef::new(epoch, &raw, r"\N", "ts")),
                    ),
                    FieldRef::new(label, ValueRef::I64(epoch)),
                ]))
                .unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        let mut reader = Box::new(SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap());
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default().with_skip_dictionaries(true))
            .unwrap();
        (reader, catalog)
    }

    #[test]
    fn metadata_overlap_never_proves_presence_and_old_archives_scan() {
        for summaries in [false, true] {
            for (start, end, expected) in [
                (40, 60, 1),
                (0, 101, 3),
                (100, 101, 1),
                (100, 100, 0),
                (-1, 0, 0),
            ] {
                let (reader, catalog) = fixture(summaries);
                let selection = Selection {
                    fields: vec![],
                    all: false,
                    time_kind: 1,
                    time_path: "ts".into(),
                    lower: Some(start),
                    upper: Some(end),
                };
                let actual = discover(
                    reader,
                    catalog,
                    &selection,
                    &ArchiveSearchOptions::default(),
                    || Ok(()),
                )
                .unwrap();
                assert_eq!(
                    actual.iter().map(|i| i.rows).sum::<u64>(),
                    expected,
                    "{summaries} {start} {end}"
                );
                if start == 40 {
                    assert_eq!(actual.len(), 1);
                    assert!(actual[0].fields.contains(&("b".into(), "integer")));
                }
                if start == 0 {
                    assert_eq!(actual.iter().any(|i| i.scanned), !summaries);
                }
            }
        }
    }

    #[test]
    fn field_selection_happens_before_scanning_and_cancellation_propagates() {
        let selection = Selection {
            fields: vec![("missing".into(), false)],
            all: false,
            time_kind: 1,
            time_path: "ts".into(),
            lower: Some(40),
            upper: Some(60),
        };
        let (reader, catalog) = fixture(false);
        // A deliberately unusable scan limit is harmless when metadata excludes every schema.
        let options =
            ArchiveSearchOptions::default().with_packed_stream(PackedStreamLimits::new(1, 1));
        assert!(
            discover(reader, catalog, &selection, &options, || Ok(()))
                .unwrap()
                .is_empty()
        );
        let (reader, catalog) = fixture(true);
        assert!(matches!(
            discover(reader, catalog, &selection, &options, || Err(
                ApiError::Cancelled("test".into())
            )),
            Err(ApiError::Cancelled(_))
        ));
    }

    #[test]
    fn disjoint_fully_covered_chunks_count_without_opening_streams() {
        let stride = clp_s::timestamp_chunks::CHUNK_ROWS as usize;
        let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for epoch in std::iter::repeat_n(0, stride)
            .chain(std::iter::repeat_n(100, stride))
            .chain(std::iter::repeat_n(0, 7))
        {
            let raw = epoch.to_string();
            writer
                .append_record(RecordRef::new(&[FieldRef::new(
                    b"ts",
                    ValueRef::Timestamp(TimestampRef::new(epoch, &raw, r"\N", "ts")),
                )]))
                .unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        let mut reader = Box::new(SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap());
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default().with_skip_dictionaries(true))
            .unwrap();
        let selection = Selection {
            fields: vec![],
            all: false,
            time_kind: 1,
            time_path: "ts".into(),
            lower: Some(-1),
            upper: Some(1),
        };
        let options =
            ArchiveSearchOptions::default().with_packed_stream(PackedStreamLimits::new(1, 1));
        let actual = discover(reader, catalog, &selection, &options, || Ok(())).unwrap();
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].rows, stride as u64 + 7);
        assert!(!actual[0].scanned);
    }
}
