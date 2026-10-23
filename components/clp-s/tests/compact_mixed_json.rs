//! Full-record output keeps adaptive CLP strings compact even beside numeric columns.

use std::io::{self, Cursor};

use clp_s::archive::{
    ArchiveCatalog, ArchiveCatalogLimits, ColumnLimits, DecodedPackedStream, NodeType,
    PackedStreamLimits, SchemaEntry, SingleFileArchiveReader, is_value_bearing,
};
use clp_s::search::{
    ArchiveSearchOptions, KqlLimits, SearchJsonlAdapter, SearchJsonlOptions, parse_kql,
    search_archive,
};
use clp_s::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
use clp_s::{
    ExtractionPlan, ExtractionPlanLimits, JsonlRecord, JsonlRecordSink, RecordLimits, RecordProgram,
};

const ROWS: usize = 512;

fn fixture(adaptive: bool) -> (Vec<u8>, Vec<serde_json::Value>) {
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_grouped_array_tables(false)
        .with_separate_columns_min_size(1)
        .with_adaptive_numeric_columns(adaptive)
        .with_adaptive_string_columns(adaptive);
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options);
    let mut expected = Vec::new();
    for id in 0..ROWS {
        let message = format!("job {id} completed on worker-{:02}\n", id % 7);
        let detail = if id % 2 == 0 {
            "state is \"ready\""
        } else {
            "state is \\waiting"
        };
        let ratio = id as f64 * 0.125;
        let fields = [
            FieldRef::new(b"id", ValueRef::I64(id as i64)),
            FieldRef::new(b"message", ValueRef::String(message.as_bytes())),
            FieldRef::new(b"ratio", ValueRef::F64(ratio)),
            FieldRef::new(b"status", ValueRef::String(b"ok")),
            FieldRef::new(b"detail", ValueRef::String(detail.as_bytes())),
            FieldRef::new(b"active", ValueRef::Bool(id % 2 == 0)),
        ];
        writer.append_record(RecordRef::new(&fields)).unwrap();
        expected.push(serde_json::json!({
            "id": id, "message": message, "ratio": ratio, "status": "ok",
            "detail": detail, "active": id % 2 == 0,
        }));
    }
    (writer.finish().unwrap().into_inner().into_inner(), expected)
}

fn load(bytes: &[u8]) -> (ArchiveCatalog, DecodedPackedStream, Vec<NodeType>) {
    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
    let catalog = reader
        .read_catalog(ArchiveCatalogLimits::default())
        .unwrap();
    assert_eq!(catalog.table_metadata().packed_streams().len(), 1);
    let stream = reader
        .read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            0,
            PackedStreamLimits::default(),
        )
        .unwrap();
    let table = &catalog.table_metadata().schema_tables()[0];
    let schema = catalog.schema_map().get(table.schema_id()).unwrap();
    let types = schema
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SchemaEntry::Node(id) => {
                let kind = catalog.schema_tree().get(*id as usize).unwrap().node_type();
                is_value_bearing(kind).then_some(kind)
            }
            _ => None,
        })
        .collect();
    (catalog, stream, types)
}

fn pieces(stream: &DecodedPackedStream) -> (Vec<Vec<u8>>, Vec<usize>) {
    let mut offset = 0;
    let mut parts = Vec::new();
    let mut sizes = Vec::new();
    for slot in stream.column_layout().unwrap() {
        parts.push(stream.as_bytes()[offset..offset + slot.stored].to_vec());
        sizes.push(slot.size);
        offset += slot.stored;
    }
    (parts, sizes)
}

fn render(catalog: &ArchiveCatalog, stream: &DecodedPackedStream) -> Vec<serde_json::Value> {
    let table = catalog
        .schema_tables(0, stream, ColumnLimits::default())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let plan = ExtractionPlan::compile(
        table.schema(),
        catalog.schema_tree(),
        ExtractionPlanLimits::default(),
    )
    .unwrap();
    let program =
        RecordProgram::compile(&plan, catalog.schema_tree(), RecordLimits::default()).unwrap();
    let mut writer = program
        .writer(table.table(), catalog.timestamp_patterns())
        .unwrap();
    let mut output = Vec::new();
    let mut rows = Vec::new();
    while writer.append_next_record(&mut output).unwrap() {
        rows.push(serde_json::from_slice(&output).unwrap());
        output.clear();
    }
    rows
}

#[test]
fn full_mixed_table_retains_encoded_strings_and_exact_json() {
    let (bytes, expected) = fixture(true);
    let (catalog, raw, types) = load(&bytes);
    let (stored, _) = pieces(&raw);
    let prepared = catalog
        .materialize_adaptive_numerics(0, raw, ColumnLimits::default())
        .unwrap();
    let mut offset = 0;
    let mut strings = 0;
    for (index, slot) in prepared.column_layout().unwrap().iter().enumerate() {
        let data = &prepared.as_bytes()[offset..offset + slot.stored];
        offset += slot.stored;
        if types[index] == NodeType::ClpString {
            assert!(
                slot.clp_layout.is_some(),
                "mixed table must retain compact CLP strings"
            );
            assert_eq!(
                data, stored[index],
                "retain the encoded frame without expanding its rows"
            );
            assert!(
                slot.size > slot.stored,
                "fixture exercises compact descriptors/variables"
            );
            strings += 1;
        }
    }
    assert_eq!(strings, 2);
    assert_eq!(render(&catalog, &prepared), expected);
}

#[test]
fn disjoint_prepared_projections_merge_compact_and_numeric_columns() {
    let (bytes, expected) = fixture(true);
    let (catalog, raw, _) = load(&bytes);
    let (parts, sizes) = pieces(&raw);
    // Exercise both the string-only fast path and mixed preparation, followed by a merge.
    for selected in [vec![1], vec![0, 1, 3], vec![1, 4]] {
        let left = DecodedPackedStream::from_columns(
            parts
                .iter()
                .enumerate()
                .map(|(i, part)| selected.contains(&i).then(|| part.clone()))
                .collect(),
            &sizes,
        );
        let right = DecodedPackedStream::from_columns(
            parts
                .iter()
                .enumerate()
                .map(|(i, part)| (!selected.contains(&i)).then(|| part.clone()))
                .collect(),
            &sizes,
        );
        let left = catalog
            .materialize_adaptive_numerics(0, left, ColumnLimits::default())
            .unwrap();
        let right = catalog
            .materialize_adaptive_numerics(0, right, ColumnLimits::default())
            .unwrap();
        let merged = left.merge_disjoint_columns(right).unwrap();
        assert_eq!(render(&catalog, &merged), expected);
        assert_eq!(
            merged
                .column_layout()
                .unwrap()
                .iter()
                .filter(|s| s.clp_layout.is_some())
                .count(),
            2
        );
    }
}

#[derive(Default)]
struct Sink(Vec<u8>);
impl JsonlRecordSink for Sink {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.0.extend_from_slice(record.jsonl_bytes());
        Ok(())
    }
}

#[test]
fn sparse_and_dense_full_json_searches_match_with_and_without_adaptive_strings() {
    for adaptive in [false, true] {
        let (bytes, expected) = fixture(adaptive);
        for (query, selected) in [
            ("*", expected.clone()),
            ("id:37", vec![expected[37].clone()]),
            (
                "message:\"*worker-03*\"",
                expected
                    .iter()
                    .enumerate()
                    .filter(|(id, _)| id % 7 == 3)
                    .map(|(_, row)| row.clone())
                    .collect(),
            ),
        ] {
            let query = parse_kql(query, KqlLimits::default()).unwrap();
            let mut reader = SingleFileArchiveReader::open(Cursor::new(&bytes)).unwrap();
            let mut sink = Sink::default();
            let options = SearchJsonlOptions::default();
            let mut adapter = SearchJsonlAdapter::new(&mut sink, &options);
            let stats = search_archive(
                &mut reader,
                &query,
                &mut adapter,
                &ArchiveSearchOptions::default(),
            )
            .unwrap();
            let actual: Vec<serde_json::Value> = sink
                .0
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            assert_eq!(stats.matching_rows() as usize, selected.len());
            assert_eq!(actual, selected);
        }
    }
}

#[test]
fn mixed_compact_preparation_rejects_corrupt_lengths_and_variable_limits() {
    let (bytes, _) = fixture(true);
    let (catalog, raw, types) = load(&bytes);
    let (parts, sizes) = pieces(&raw);
    let string = types
        .iter()
        .position(|ty| *ty == NodeType::ClpString)
        .unwrap();
    let mut malformed = parts.clone();
    malformed[string][..8].copy_from_slice(&u64::MAX.to_le_bytes());
    let malformed =
        DecodedPackedStream::from_columns(malformed.into_iter().map(Some).collect(), &sizes);
    assert!(
        catalog
            .materialize_adaptive_numerics(0, malformed, ColumnLimits::default())
            .is_err()
    );
    let restricted = ColumnLimits::new(u64::MAX, u64::MAX, u64::MAX, 0, u64::MAX);
    let raw = DecodedPackedStream::from_columns(parts.into_iter().map(Some).collect(), &sizes);
    assert!(
        catalog
            .materialize_adaptive_numerics(0, raw, restricted)
            .is_err()
    );
}
