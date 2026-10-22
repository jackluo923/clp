//! Exact source-integer timestamp extrema. Rounded pruning ranges are never SQL answers.
use super::*;
use clp_s::archive::{NodeType, SchemaEntry};
use clp_s::numeric_summary::{IntegerSummary, NumericSummaries, integer};
use numeric_aggregate::failure;

#[repr(C)]
#[derive(Default)]
pub struct TimestampSummary {
    rows: u64,
    count: u64,
    min: i64,
    max: i64,
    supported: u64,
}

fn read(archive: &ClpSArchive, field: &str) -> Result<TimestampSummary, ApiError> {
    let catalog = archive
        .open_reader()?
        .read_catalog(ArchiveCatalogLimits::default().with_skip_dictionaries(true))
        .map_err(failure)?;
    summarize(&catalog, field)
}

fn summarize(
    catalog: &clp_s::archive::ArchiveCatalog,
    field: &str,
) -> Result<TimestampSummary, ApiError> {
    let Some(summaries) = catalog
        .metadata()
        .numeric_summaries()
        .and_then(|b| NumericSummaries::decode(b).ok())
    else {
        return Ok(TimestampSummary::default());
    };
    let mut index = std::collections::HashMap::new();
    for column in &summaries.columns {
        if index.insert((column.schema, column.node), column).is_some() {
            return Ok(TimestampSummary::default());
        }
    }
    let nodes =
        columnar::resolve_path(catalog.schema_tree(), &field.split('.').collect::<Vec<_>>());
    let mut result = TimestampSummary::default();
    let mut values = IntegerSummary::default();
    for table in catalog.table_metadata().schema_tables() {
        result.rows = result
            .rows
            .checked_add(table.message_count())
            .ok_or_else(|| failure("row count overflow"))?;
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
        if matching.len() > 1 {
            return Ok(TimestampSummary::default());
        }
        for node in matching {
            let kind = catalog
                .schema_tree()
                .get(node as usize)
                .ok_or_else(|| failure("missing node"))?
                .node_type();
            if kind == NodeType::Null {
                continue;
            }
            if kind != NodeType::Timestamp {
                return Ok(TimestampSummary::default());
            }
            let Some(column) = index.get(&(table.schema_id(), node)) else {
                return Ok(TimestampSummary::default());
            };
            let mut partial = IntegerSummary::default();
            for bucket in [&column.negative, &column.nonnegative] {
                partial
                    .merge(bucket)
                    .ok_or_else(|| failure("timestamp summary overflow"))?;
            }
            if partial.count != table.message_count()
                || column.unsigned.count != 0
                || column.float.count != 0
            {
                return Ok(TimestampSummary::default());
            }
            values
                .merge(&partial)
                .ok_or_else(|| failure("timestamp summary overflow"))?;
        }
    }
    result.count = values.count;
    result.min = i64::try_from(integer(values.min)).map_err(failure)?;
    result.max = i64::try_from(integer(values.max)).map_err(failure)?;
    result.supported = 1;
    Ok(result)
}

/// Reads metadata only. `supported=0` requires the caller to scan instead.
///
/// # Safety
/// Input pointers must describe readable byte slices; output/error must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v6_timestamp_summary(
    path: *const u8,
    path_len: usize,
    field: *const u8,
    field_len: usize,
    out: *mut TimestampSummary,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    unsafe {
        ffi_entry(error, || {
            if out.is_null() {
                return Err(ApiError::InvalidArgument(
                    "null timestamp summary output".into(),
                ));
            }
            out.write(TimestampSummary::default());
            let path = str::from_utf8(borrowed_bytes(path, path_len, "path")?).map_err(failure)?;
            let field =
                str::from_utf8(borrowed_bytes(field, field_len, "field")?).map_err(failure)?;
            let archive = ClpSArchive::validate(PathBuf::from(path))?;
            out.write(read(&archive, field)?);
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
    fn exact_integer_timestamp_summaries_preserve_subunit_epochs() {
        let epochs = [
            -2_000_000_001_i64,
            -1_000_000_001,
            -1,
            0,
            1,
            1_000_000_001,
            2_000_000_001,
        ];
        for (pattern, scale) in [
            (r"\E", 1_000_000_000),
            (r"\L", 1_000_000),
            (r"\C", 1_000),
            (r"\N", 1),
        ] {
            for enabled in [false, true] {
                let options = WriterOptions::default().with_numeric_summaries(enabled);
                let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options);
                for epoch in epochs {
                    let raw = (epoch / scale).to_string();
                    writer
                        .append_record(RecordRef::new(&[FieldRef::new(
                            b"ts",
                            ValueRef::Timestamp(TimestampRef::new(epoch, &raw, pattern, "ts")),
                        )]))
                        .unwrap();
                }
                let bytes = writer.finish().unwrap().into_inner().into_inner();
                let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
                let catalog = reader
                    .read_catalog(ArchiveCatalogLimits::default())
                    .unwrap();
                let actual = summarize(&catalog, "ts").unwrap();
                assert_eq!(actual.supported != 0, enabled);
                if enabled {
                    assert_eq!(actual.rows, epochs.len() as u64);
                    assert_eq!(actual.count, actual.rows);
                    assert_eq!(actual.min, epochs[0] / scale);
                    assert_eq!(actual.max, epochs[epochs.len() - 1] / scale);
                }
            }
        }
    }
}
