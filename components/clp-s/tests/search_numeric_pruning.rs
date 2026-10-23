//! Metadata pruning must skip payload reads without changing typed KQL semantics.

use clp_s::archive::{ArchiveCatalogLimits, SingleFileArchiveReader};
use clp_s::search::{
    AggregationPlan, ArchiveSearchOptions, KqlLimits, SearchJsonlAdapter, SearchJsonlOptions,
    parse_kql, search_archive,
};
use clp_s::writer::{
    FieldRef, OpenArchive, RecordRef, UnstructuredArrayRef, ValueRef, WriterOptions,
};
use clp_s::{JsonlRecord, JsonlRecordSink};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::ops::Range;

#[derive(Default)]
struct Output(Vec<u8>);
impl JsonlRecordSink for Output {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.0.extend_from_slice(record.jsonl_bytes());
        Ok(())
    }
}

fn fixture(summaries: bool) -> Vec<u8> {
    let mut writer = OpenArchive::new(
        Cursor::new(Vec::new()),
        WriterOptions::default()
            .with_log_order(false)
            .with_grouped_array_tables(false)
            .with_separate_columns_min_size(1)
            .with_numeric_summaries(summaries),
    );
    for value in [
        ValueRef::I64(i64::MIN),
        ValueRef::I64(-7),
        ValueRef::I64(0),
        ValueRef::I64(7),
        ValueRef::I64(i64::MAX),
        ValueRef::MixedInteger(clp_s::integer::IntegerValue::Unsigned(u64::MAX)),
        ValueRef::F64(7.5),
        ValueRef::String(b"text"),
        ValueRef::Null,
    ] {
        writer
            .append_record(RecordRef::new(&[
                FieldRef::new(b"id", value),
                FieldRef::new(b"marker", ValueRef::String(b"first")),
            ]))
            .unwrap();
    }
    let items = [ValueRef::I64(3), ValueRef::I64(9000)];
    writer
        .append_record(RecordRef::new(&[FieldRef::new(
            b"id",
            ValueRef::Array(&items),
        )]))
        .unwrap();
    writer
        .append_record(RecordRef::new(&[FieldRef::new(
            b"id",
            ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[4, 8000]")),
        )]))
        .unwrap();
    writer
        .append_record(RecordRef::new(&[FieldRef::new(
            b"missing",
            ValueRef::Bool(true),
        )]))
        .unwrap();
    writer.finish().unwrap().into_inner().into_inner()
}

fn search(bytes: &[u8], query: &str) -> Vec<u8> {
    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
    let mut output = Output::default();
    let options = SearchJsonlOptions::default();
    let mut sink = SearchJsonlAdapter::new(&mut output, &options);
    search_archive(
        &mut reader,
        &parse_kql(query, KqlLimits::default()).unwrap(),
        &mut sink,
        &ArchiveSearchOptions::default(),
    )
    .unwrap();
    output.0
}

#[test]
fn summaries_preserve_signed_unsigned_float_negation_lists_and_arrays() {
    let summarized = fixture(true);
    let reference = fixture(false);
    for query in [
        "id:7",
        "id:7.0",
        "id:7u64",
        "id > 7",
        "id >= 7",
        "id < 7",
        "id <= 7",
        "id < 0u64",
        "id > 9223372036854775807",
        "id:18446744073709551615",
        "id < -9223372036854775808",
        "id > 18446744073709551615",
        "id:999999",
        "id:3",
        "id:8000",
        "id:(NOT 3 7)",
        "id:(3 7)",
        "NOT id:7",
        "NOT (id > 7 OR marker:first)",
        "id > 100 AND marker:first",
        "id > 100 OR marker:first",
        "id:*",
        "id:null",
        "NOT id:null",
    ] {
        assert_eq!(
            search(&summarized, query),
            search(&reference, query),
            "{query}"
        );
    }
}

struct Guarded {
    bytes: Cursor<Vec<u8>>,
    forbidden: Vec<Range<u64>>,
}
impl Read for Guarded {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let start = self.bytes.position();
        let end = start + out.len() as u64;
        if self
            .forbidden
            .iter()
            .any(|range| start < range.end && range.start < end)
        {
            return Err(io::Error::other(
                "numeric summaries should skip this stream",
            ));
        }
        self.bytes.read(out)
    }
}
impl Seek for Guarded {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.bytes.seek(position)
    }
}

fn guarded(summaries: bool) -> SingleFileArchiveReader<Guarded> {
    let mut writer = OpenArchive::new(
        Cursor::new(Vec::new()),
        WriterOptions::default()
            .with_log_order(false)
            .with_grouped_array_tables(false)
            .with_separate_columns_min_size(1)
            .with_numeric_summaries(summaries),
    );
    for (tag, start) in [(b"low".as_slice(), 0), (b"target", 42), (b"high", 100)] {
        for value in start..start + 4 {
            writer
                .append_record(RecordRef::new(&[
                    FieldRef::new(b"id", ValueRef::I64(value)),
                    FieldRef::new(tag, ValueRef::Bool(true)),
                ]))
                .unwrap();
        }
    }
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    let mut reader = SingleFileArchiveReader::open(Cursor::new(&bytes)).unwrap();
    let catalog = reader
        .read_catalog(ArchiveCatalogLimits::default())
        .unwrap();
    let data = catalog
        .metadata()
        .directory()
        .get("/0")
        .unwrap()
        .range()
        .start;
    let mut forbidden = Vec::new();
    for (index, stream) in catalog.table_metadata().packed_streams().iter().enumerate() {
        // Writer schema order is insertion order; the middle table contains 42.
        if index != 1 {
            let start = data + stream.file_offset();
            forbidden.push(start..start + stream.compressed_size());
        }
    }
    assert_eq!(forbidden.len(), 2);
    SingleFileArchiveReader::open(Guarded {
        bytes: Cursor::new(bytes),
        forbidden,
    })
    .unwrap()
}

#[test]
fn impossible_tables_are_never_read_for_count_or_json() {
    let query = parse_kql("id:42", KqlLimits::default()).unwrap();
    let plan = AggregationPlan::count();
    let mut sink = plan.start();
    let stats = search_archive(
        &mut guarded(true),
        &query,
        &mut sink,
        &ArchiveSearchOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.matching_rows(), 1);
    assert!(
        search_archive(
            &mut guarded(false),
            &query,
            &mut plan.start(),
            &ArchiveSearchOptions::default()
        )
        .is_err()
    );
    let mut output = Output::default();
    let options = SearchJsonlOptions::default();
    let mut sink = SearchJsonlAdapter::new(&mut output, &options);
    search_archive(
        &mut guarded(true),
        &query,
        &mut sink,
        &ArchiveSearchOptions::default(),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.0).unwrap(),
        serde_json::json!({"id":42,"target":true})
    );
}
