//! A projected result must never read unrelated independently compressed column frames.

use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::ops::Range;

use clp_s::archive::{ArchiveCatalogLimits, SchemaEntry, SingleFileArchiveReader, is_value_bearing};
use clp_s::search::{
    AggregationPlan, ArchiveSearchOptions, KqlLimits, Projection, ProjectionLimits,
    SearchJsonlAdapter, SearchJsonlOptions, parse_kql, search_archive,
};
use clp_s::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
use clp_s::{JsonlRecord, JsonlRecordSink};

struct GuardedInput {
    input: Cursor<Vec<u8>>,
    forbidden: Vec<Range<u64>>,
}

impl Read for GuardedInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let start = self.input.position();
        let end = start + output.len() as u64;
        if self.forbidden.iter().any(|range| start < range.end && range.start < end) {
            return Err(io::Error::other("read an unneeded payload column"));
        }
        self.input.read(output)
    }
}

impl Seek for GuardedInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.input.seek(position)
    }
}

fn fixture() -> Vec<u8> {
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_grouped_array_tables(false)
        .with_separate_columns_min_size(1)
        .with_adaptive_numeric_columns(true)
        .with_adaptive_string_columns(true);
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options);
    for id in 0..4 {
        let message = format!("message number {id}");
        let payload = format!("unrelated large payload {id} {}", "x".repeat(2048));
        let nested = [FieldRef::new(b"value", ValueRef::I64(id * 10))];
        // Repeated array nodes must bind by physical position, not just schema node ID.
        let items = [ValueRef::I64(id), ValueRef::I64(id + 100)];
        writer.append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(id)),
            FieldRef::new(b"payload", ValueRef::String(payload.as_bytes())),
            FieldRef::new(b"msg", ValueRef::String(message.as_bytes())),
            FieldRef::new(b"nested", ValueRef::Object(&nested)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ])).unwrap();
    }
    writer.finish().unwrap().into_inner().into_inner()
}

fn reader() -> SingleFileArchiveReader<GuardedInput> {
    let bytes = fixture();
    let mut reader = SingleFileArchiveReader::open(Cursor::new(&bytes)).unwrap();
    let catalog = reader.read_catalog(ArchiveCatalogLimits::default()).unwrap();
    let mut forbidden = Vec::new();
    let start = catalog.metadata().directory().get("/0").unwrap().range().start;
    for table in catalog.table_metadata().schema_tables() {
        let separate = catalog.table_metadata().separate_columns_for(table.stream_id()).unwrap();
        let stream = &catalog.table_metadata().packed_streams()[table.stream_id() as usize];
        let schema = catalog.schema_map().get(table.schema_id()).unwrap();
        let nodes: Vec<_> = schema.entries().iter().filter_map(|entry| match entry {
            SchemaEntry::Node(id) => {
                let node = catalog.schema_tree().get(*id as usize).unwrap();
                is_value_bearing(node.node_type()).then_some(node)
            }
            _ => None,
        }).collect();
        assert_eq!(nodes.len(), separate.columns().len());
        let mut offset = start + stream.file_offset();
        for (node, frame) in nodes.iter().zip(separate.columns()) {
            let end = offset + frame.compressed_size();
            if node.key_bytes() == b"payload" { forbidden.push(offset..end); }
            offset = end;
        }
    }
    assert!(!forbidden.is_empty());
    SingleFileArchiveReader::open(GuardedInput { input: Cursor::new(bytes), forbidden }).unwrap()
}

#[derive(Default)]
struct Output(Vec<u8>);
impl JsonlRecordSink for Output {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.0.extend_from_slice(record.jsonl_bytes());
        Ok(())
    }
}

fn projected(query: &str, columns: &[&str]) -> (u64, Vec<serde_json::Value>) {
    let projection = Projection::selected(columns, ProjectionLimits::default()).unwrap();
    let options = SearchJsonlOptions::new(projection);
    let mut output = Output::default();
    let mut sink = SearchJsonlAdapter::new(&mut output, &options);
    let stats = search_archive(&mut reader(), &parse_kql(query, KqlLimits::default()).unwrap(),
        &mut sink, &ArchiveSearchOptions::default()).unwrap();
    let rows = output.0.split(|b| *b == b'\n').filter(|row| !row.is_empty())
        .map(|row| serde_json::from_slice(row).unwrap()).collect();
    (stats.matching_rows(), rows)
}

#[test]
fn projection_reads_only_predicate_and_selected_columns_with_nested_values() {
    let (count, rows) = projected("id >= 2", &["msg", "nested", "items"]);
    assert_eq!(count, 2);
    assert_eq!(rows, (2..4).map(|id| serde_json::json!({
        "msg": format!("message number {id}"), "nested": {"value": id * 10},
        "items": [id, id + 100],
    })).collect::<Vec<_>>());
    let (count, rows) = projected("id:1", &["missing"]);
    assert_eq!(count, 1);
    assert_eq!(rows, [serde_json::json!({})]);
}

#[test]
fn native_aggregates_do_not_read_unrelated_columns() {
    for plan in [AggregationPlan::count(), AggregationPlan::minimum("id").unwrap(),
        AggregationPlan::maximum("id").unwrap(), AggregationPlan::unique("msg").unwrap()] {
        let mut sink = plan.start();
        let stats = search_archive(&mut reader(), &parse_kql("id >= 2", KqlLimits::default()).unwrap(),
            &mut sink, &ArchiveSearchOptions::default()).unwrap();
        assert_eq!(stats.matching_rows(), 2);
        assert!(sink.results().len() > 0);
    }
}

#[test]
fn guard_proves_full_output_would_read_payload() {
    let mut output = Output::default();
    let options = SearchJsonlOptions::default();
    let mut sink = SearchJsonlAdapter::new(&mut output, &options);
    assert!(search_archive(&mut reader(), &parse_kql("id:1", KqlLimits::default()).unwrap(),
        &mut sink, &ArchiveSearchOptions::default()).is_err());
}
