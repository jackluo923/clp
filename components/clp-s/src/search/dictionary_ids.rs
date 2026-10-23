//! Discover dictionary prefixes from prepared column IDs before loading output values.

use crate::archive::{
    ArchiveCatalog, ColumnError, ColumnLimits, DecodedPackedStream, SchemaEntry, TableStreamError,
    is_value_bearing, maximum_variable_dictionary_id,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VariableDictionaryRequirement {
    /// Highest referenced ID, or no dictionary values for this stream.
    Prefix(Option<u64>),
    /// A shared/grouped or unknown layout requires the established complete-dictionary path.
    FullFallback,
}

/// Discovers references in every loaded column, including rows outside the result bitmap.
///
/// Existing table validation can inspect entire loaded columns, so limiting this to matching
/// rows would change correctness when dictionary validation is enabled. Physical columns are
/// enumerated in schema-entry order: repeated structured-array nodes are distinct columns.
/// Call only after `ArchiveCatalog::materialize_adaptive_numerics` and after loading the logtype
/// and array dictionaries needed by these columns. This function never accesses variable values.
pub(super) fn variable_dictionary_prefix(
    catalog: &ArchiveCatalog,
    stream_id: u64,
    stream: &DecodedPackedStream,
    limits: ColumnLimits,
) -> Result<VariableDictionaryRequirement, TableStreamError> {
    use VariableDictionaryRequirement::{FullFallback, Prefix};
    if catalog.table_metadata().grouped_stream_for(stream_id).is_some() {
        return Ok(FullFallback);
    }
    let Some(layout) = stream.column_layout() else { return Ok(FullFallback); };
    let mut tables = catalog.table_metadata().schema_tables().iter().enumerate()
        .filter(|(_, table)| table.stream_id() == stream_id);
    let (table_index, table) = tables.next().ok_or(TableStreamError::StreamHasNoTables { stream_id })?;
    if tables.next().is_some() { return Ok(FullFallback); }
    let schema_id = table.schema_id();
    let schema = catalog.schema_map().get(schema_id)
        .ok_or(TableStreamError::UnknownSchemaId { table_index, schema_id })?;
    let fail = |source| TableStreamError::Column { table_index, schema_id, source };
    if table.message_count() > limits.max_messages() {
        return Err(fail(ColumnError::MessageCountTooLarge {
            actual: table.message_count(), limit: limits.max_messages(),
        }));
    }
    let rows = usize::try_from(table.message_count()).map_err(|_| TableStreamError::SizeOverflow)?;
    let mut column_index = 0;
    let mut offset = 0usize;
    let mut maximum = None;
    for entry in schema.entries() {
        let SchemaEntry::Node(node_id) = *entry else { continue; };
        let node = catalog.schema_tree().get(node_id as usize)
            .ok_or_else(|| fail(ColumnError::UnknownSchemaNode { node_id }))?;
        if !is_value_bearing(node.node_type()) { continue; }
        let Some(slot) = layout.get(column_index) else { return Ok(FullFallback); };
        let physical_index = column_index;
        column_index += 1;
        let end = offset.checked_add(slot.stored).ok_or(TableStreamError::SizeOverflow)?;
        let bytes = stream.as_bytes().get(offset..end).ok_or(TableStreamError::SizeOverflow)?;
        offset = end;
        if !slot.loaded {
            if slot.stored != 0 { return Ok(FullFallback); }
            continue;
        }
        if let Some(id) = maximum_variable_dictionary_id(bytes, slot, node.node_type(), node_id,
            physical_index, rows, catalog.log_type_dictionary(), catalog.array_dictionary(), limits)
            .map_err(fail)? {
            maximum = Some(maximum.map_or(id, |previous: u64| previous.max(id)));
        }
    }
    if column_index != layout.len() || offset != stream.len() { return Ok(FullFallback); }
    Ok(Prefix(maximum))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use crate::archive::{ArchiveCatalogLimits, PackedStreamLimits, SingleFileArchiveReader};
    use crate::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};

    fn fixture(separate: bool) -> (ArchiveCatalog, DecodedPackedStream) {
        let options = WriterOptions::default().with_log_order(false)
            .with_grouped_array_tables(false)
            .with_separate_columns_min_size(if separate { 1 } else { u64::MAX });
        let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options);
        let items = [ValueRef::String(b"first"), ValueRef::String(b"last")];
        writer.append_record(RecordRef::new(&[
            FieldRef::new(b"number", ValueRef::I64(3)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ])).unwrap();
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
        let catalog = reader.read_catalog(ArchiveCatalogLimits::default()).unwrap();
        assert_eq!(catalog.table_metadata().packed_streams().len(), 1);
        let stream = reader.read_packed_stream(catalog.metadata(), catalog.table_metadata(), 0,
            PackedStreamLimits::default()).unwrap();
        let stream = catalog.materialize_adaptive_numerics(0, stream, ColumnLimits::default()).unwrap();
        (catalog, stream)
    }

    #[test]
    fn dictionary_prefix_discovery_preserves_repeated_array_column_positions() {
        let (catalog, stream) = fixture(true);
        let schema = catalog.schema_map().get(catalog.table_metadata().schema_tables()[0].schema_id()).unwrap();
        let nodes: Vec<_> = schema.entries().iter().filter_map(|entry| match entry {
            SchemaEntry::Node(id) if is_value_bearing(catalog.schema_tree().get(*id as usize).unwrap().node_type()) => Some(*id),
            _ => None,
        }).collect();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[1], nodes[2], "fixture must repeat the structured-array node");
        assert_eq!(variable_dictionary_prefix(&catalog, 0, &stream, ColumnLimits::default()).unwrap(),
            VariableDictionaryRequirement::Prefix(Some(1)));
        let layout = stream.column_layout().unwrap();
        let mut selected = layout.to_vec();
        let offset: usize = layout[..2].iter().map(|slot| slot.stored).sum();
        let bytes = stream.as_bytes()[offset..].to_vec();
        for slot in &mut selected[..2] { slot.loaded = false; slot.stored = 0; }
        let last_only = DecodedPackedStream::from_column_buffer(bytes, selected);
        assert_eq!(variable_dictionary_prefix(&catalog, 0, &last_only, ColumnLimits::default()).unwrap(),
            VariableDictionaryRequirement::Prefix(Some(1)));
    }

    #[test]
    fn dictionary_prefix_discovery_falls_back_for_shared_layouts_and_skips_omitted_columns() {
        let (catalog, stream) = fixture(false);
        assert_eq!(variable_dictionary_prefix(&catalog, 0, &stream, ColumnLimits::default()).unwrap(),
            VariableDictionaryRequirement::FullFallback);
        let (catalog, stream) = fixture(true);
        let mut layout = stream.column_layout().unwrap().to_vec();
        for slot in &mut layout { slot.loaded = false; slot.stored = 0; }
        let empty = DecodedPackedStream::from_column_buffer(Vec::new(), layout);
        assert_eq!(variable_dictionary_prefix(&catalog, 0, &empty, ColumnLimits::default()).unwrap(),
            VariableDictionaryRequirement::Prefix(None));
    }
}
