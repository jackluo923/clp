//! Whole-archive typed numeric partials. Summary misses scan inside Rust, never emit rows to C.
use super::*;
use clp_s::archive::{NodeType, SchemaEntry, SchemaTableMetadata};
use clp_s::numeric_summary::{FloatSummary, IntegerSummary, NumericSummaries, integer};

#[derive(Default, Clone, Copy)]
#[repr(C)]
pub struct ClpNumericSummary {
    pub count: u64,
    pub sum: [u64; 2],
    pub min: [u64; 2],
    pub max: [u64; 2],
    pub float_sum: f64,
    pub float_min: f64,
    pub float_max: f64,
    pub from_metadata: u64,
    pub supported: u64,
}
pub(super) fn bounds(typ: u32) -> Option<(i128, i128)> {
    Some(match typ {
        0 => (i128::from(i64::MIN), i128::from(i64::MAX)),
        1 => (0, i128::from(u64::MAX)),
        2 => (i128::from(i64::MIN), i128::from(u64::MAX)),
        4 => (i128::from(i8::MIN), i128::from(i8::MAX)),
        5 => (i128::from(i16::MIN), i128::from(i16::MAX)),
        6 => (i128::from(i32::MIN), i128::from(i32::MAX)),
        _ => return None,
    })
}
pub(super) fn failure(message: impl ToString) -> ApiError {
    ApiError::Archive(message.to_string())
}
fn combine(a: &mut IntegerSummary, b: &IntegerSummary, lo: i128, hi: i128) -> Option<bool> {
    if b.count == 0 {
        return Some(true);
    }
    let min = integer(b.min);
    let max = integer(b.max);
    if min > max {
        return None;
    }
    if max < lo || min > hi {
        return Some(true);
    }
    if min < lo || max > hi {
        return Some(false);
    }
    a.merge(b)?;
    Some(true)
}
/// Summary lookup and fallback scanning share the same opened source and catalog.
/// Directory archive contents must remain immutable for the operation's lifetime.
fn open_numeric_archive(
    path: &str,
) -> Result<(Box<dyn ArchiveReader>, clp_s::archive::ArchiveCatalog), ApiError> {
    let mut reader = ClpSArchive::locate(PathBuf::from(path))?.open_reader()?;
    let catalog = reader
        .read_catalog(ArchiveCatalogLimits::default().with_skip_dictionaries(true))
        .map_err(failure)?;
    Ok((reader, catalog))
}

struct SummaryContext<'a> {
    catalog: &'a clp_s::archive::ArchiveCatalog,
    summaries: NumericSummaries,
    index: std::collections::HashMap<(i32, u32), usize>,
}
impl<'a> SummaryContext<'a> {
    fn from_catalog(catalog: &'a clp_s::archive::ArchiveCatalog) -> Result<Option<Self>, ApiError> {
        let summaries = catalog
            .metadata()
            .numeric_summaries()
            .and_then(|bytes| NumericSummaries::decode(bytes).ok())
            .unwrap_or(NumericSummaries {
                version: 1,
                columns: vec![],
            });
        let mut index = std::collections::HashMap::new();
        for (i, column) in summaries.columns.iter().enumerate() {
            if index.insert((column.schema, column.node), i).is_some() {
                return Ok(None);
            }
        }
        Ok(Some(Self {
            catalog,
            summaries,
            index,
        }))
    }
    fn field(&self, field: &str, typ: u32) -> Result<Option<ClpNumericSummary>, ApiError> {
        let nodes = columnar::resolve_descriptor_path(self.catalog.schema_tree(), field);
        self.field_tables(&nodes, typ, self.catalog.table_metadata().schema_tables())
    }
    fn field_tables<'b>(
        &self, nodes: &[u32], typ: u32,
        tables: impl IntoIterator<Item = &'b SchemaTableMetadata>,
    ) -> Result<Option<ClpNumericSummary>, ApiError> {
        let catalog = self.catalog;
        let mut ints = IntegerSummary::default();
        let mut floats = FloatSummary::default();
        for table in tables {
            let schema = catalog
                .schema_map()
                .get(table.schema_id())
                .ok_or_else(|| failure("missing schema"))?;
            let matching: Vec<_> = schema
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    SchemaEntry::Node(id) if nodes.contains(id) => Some(*id),
                    _ => None,
                })
                .collect();
            // Repeated keys or array layouts must use the existing projection semantics.
            if matching.len() > 1 {
                return Ok(None);
            }
            for node in matching {
                let kind = catalog
                    .schema_tree()
                    .get(node as usize)
                    .ok_or_else(|| failure("missing node"))?
                    .node_type();
                if kind == NodeType::Timestamp {
                    return Ok(Some(ClpNumericSummary::default()));
                }
                if kind == NodeType::DictionaryFloat {
                    return Ok(None);
                }
                let needed = if typ == 3 {
                    matches!(kind, NodeType::Float | NodeType::FormattedFloat)
                } else {
                    matches!(kind, NodeType::Integer | NodeType::MixedInteger)
                };
                if !needed {
                    continue;
                }
                let Some(position) = self.index.get(&(table.schema_id(), node)) else {
                    return Ok(None);
                };
                let summary = &self.summaries.columns[*position];
                let n = summary
                    .negative
                    .count
                    .checked_add(summary.nonnegative.count)
                    .and_then(|n| n.checked_add(summary.unsigned.count))
                    .and_then(|n| n.checked_add(summary.float.count));
                if n != Some(table.message_count()) {
                    return Ok(None);
                }
                if typ == 3 {
                    floats
                        .merge(&summary.float)
                        .ok_or_else(|| failure("summary count overflow"))?;
                } else {
                    let (lo, hi) =
                        bounds(typ).ok_or_else(|| failure("unsupported numeric mapping"))?;
                    for bucket in [&summary.negative, &summary.nonnegative, &summary.unsigned] {
                        if combine(&mut ints, bucket, lo, hi) != Some(true) {
                            return Ok(None);
                        }
                    }
                }
            }
        }
        Ok(Some(result(ints, floats, typ, true)))
    }
}
fn metadata_summary(
    catalog: &clp_s::archive::ArchiveCatalog,
    field: &str,
    typ: u32,
) -> Result<Option<ClpNumericSummary>, ApiError> {
    match SummaryContext::from_catalog(catalog)? {
        Some(ctx) => ctx.field(field, typ),
        None => Ok(None),
    }
}
pub(super) fn result(
    ints: IntegerSummary,
    floats: FloatSummary,
    typ: u32,
    metadata: bool,
) -> ClpNumericSummary {
    ClpNumericSummary {
        count: if typ == 3 { floats.count } else { ints.count },
        sum: ints.sum,
        min: ints.min,
        max: ints.max,
        float_sum: floats.sum,
        float_min: floats.min,
        float_max: floats.max,
        from_metadata: u64::from(metadata),
        supported: 1,
    }
}

/// Compute an unfiltered typed summary. Type: 0 BIGINT, 1 UBIGINT, 2 HUGEINT,
/// 3 DOUBLE, 4 TINYINT, 5 SMALLINT, 6 INTEGER. `force_scan` is a validation hook.
///
/// # Safety
/// Byte regions must be readable, output/error writable, and cancel/context live for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v5_numeric_summary(
    path: *const u8,
    path_len: usize,
    field: *const u8,
    field_len: usize,
    typ: u32,
    max_compressed: u64,
    max_decompressed: u64,
    force_scan: u32,
    cancel: Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> u32>,
    context: *mut std::ffi::c_void,
    out: *mut ClpNumericSummary,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    unsafe {
        ffi_entry(error, || {
            if out.is_null() {
                return Err(ApiError::InvalidArgument("null summary output".into()));
            }
            out.write(ClpNumericSummary::default());
            if typ != 3 && bounds(typ).is_none() {
                return Err(ApiError::InvalidArgument("unsupported summary type".into()));
            }
            let path = str::from_utf8(borrowed_bytes(path, path_len, "path")?).map_err(failure)?;
            let field =
                str::from_utf8(borrowed_bytes(field, field_len, "field")?).map_err(failure)?;
            let check = || -> Result<(), ApiError> {
                if cancel.is_some_and(|f| f(context) != 0) {
                    Err(failure("aggregate interrupted"))
                } else {
                    Ok(())
                }
            };
            check()?;
            let (reader, catalog) = open_numeric_archive(path)?;
            check()?;
            if force_scan == 0 {
                if let Some(summary) = metadata_summary(&catalog, field, typ)? {
                    check()?;
                    out.write(summary);
                    return Ok(());
                }
            }
            let mut options = ArchiveSearchOptions::default();
            let current = options.packed_stream();
            options = options.with_packed_stream(PackedStreamLimits::new(
                if max_compressed == 0 {
                    current.max_compressed_size()
                } else {
                    max_compressed
                },
                if max_decompressed == 0 {
                    current.max_decompressed_size()
                } else {
                    max_decompressed
                },
            ));
            let parsed = parse_kql("*", KqlLimits::default()).map_err(failure)?;
            let mut scanner = columnar::ProjectedScanner::open_with_catalog(
                reader,
                catalog,
                parsed,
                &options,
                &[field.to_owned()],
            )
            .map_err(failure)?;
            let mut ints = IntegerSummary::default();
            let mut floats = FloatSummary::default();
            while let Some((columns, _text, rows)) =
                scanner.next_columns_with_constants(true).map_err(failure)?
            {
                check()?;
                let column = &columns[0];
                for row in 0..rows {
                    if row % 8192 == 0 {
                        check()?;
                    }
                    let mut kind = column.kind;
                    let (int, real);
                    match kind {
                        columnar::kind::INTEGER
                        | columnar::kind::FLOAT
                        | columnar::kind::CONSTANT_INTEGER
                        | columnar::kind::CONSTANT_FLOAT => {
                            let constant = kind == columnar::kind::CONSTANT_INTEGER
                                || kind == columnar::kind::CONSTANT_FLOAT;
                            let bits = column
                                .data
                                .add(if constant { 0 } else { row * 8 })
                                .cast::<u64>()
                                .read_unaligned();
                            if kind == columnar::kind::CONSTANT_INTEGER {
                                kind = columnar::kind::INTEGER;
                            }
                            if kind == columnar::kind::CONSTANT_FLOAT {
                                kind = columnar::kind::FLOAT;
                            }
                            int = bits as i64;
                            real = f64::from_bits(bits);
                        }
                        columnar::kind::ABSENT => continue,
                        _ => {
                            let cell = column
                                .data
                                .cast::<columnar::Cell>()
                                .add(row)
                                .read_unaligned();
                            kind = cell.kind;
                            int = cell.integer;
                            real = cell.real;
                        }
                    }
                    if kind == columnar::kind::TIMESTAMP {
                        out.write(ClpNumericSummary::default());
                        return Ok(());
                    }
                    if typ == 3 {
                        if kind == columnar::kind::FLOAT {
                            floats.add(real).ok_or_else(|| failure("count overflow"))?;
                        }
                    } else {
                        let v = match kind {
                            columnar::kind::INTEGER => Some(i128::from(int)),
                            columnar::kind::UNSIGNED => Some(i128::from(int as u64)),
                            _ => None,
                        };
                        let (lo, hi) = bounds(typ).unwrap();
                        if let Some(v) = v.filter(|v| *v >= lo && *v <= hi) {
                            ints.add(v)
                                .ok_or_else(|| failure("integer aggregate overflow"))?;
                        }
                    }
                }
            }
            out.write(result(ints, floats, typ, false));
            Ok(())
        })
    }
}

#[path = "numeric_program.rs"]
mod program;
