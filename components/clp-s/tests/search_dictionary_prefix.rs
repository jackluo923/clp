//! Output-only dictionary prefixes must preserve search semantics and exact JSON values.

use std::io::{self, Cursor};

use clp_s::archive::{
    ArchiveCatalog, ArchiveCatalogError, ArchiveCatalogLimits, ArchiveMetadata, ArrayDictionary,
    ColumnLimits, ColumnTruncation, DecodedPackedStream, DictionaryError, DictionaryLimits,
    LogTypeDictionary, PackedStreamError, PackedStreamLimits, SingleFileArchiveReader,
    TableMetadata, VariableDictionary,
};
use clp_s::search::{
    AggregationPlan, ArchiveSearchOptions, KqlLimits, Projection, ProjectionLimits,
    SearchJsonlAdapter, SearchJsonlOptions, parse_kql, search_archive,
};
use clp_s::writer::{
    FieldRef, OpenArchive, RecordRef, RetainedFloatRef, UnstructuredArrayRef, ValueRef,
    WriterOptions,
};
use clp_s::{ArchiveReader, JsonlRecord, JsonlRecordSink};

struct TrackingReader {
    inner: SingleFileArchiveReader<Cursor<Vec<u8>>>,
    full_reads: usize,
    validate_references: bool,
    return_unmarked_empty: bool,
    // (requested minimum, returned entries, complete)
    prefix_reads: Vec<(u64, usize, bool)>,
}

impl TrackingReader {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap(),
            full_reads: 0,
            validate_references: false,
            return_unmarked_empty: false,
            prefix_reads: Vec::new(),
        }
    }
}

impl ArchiveReader for TrackingReader {
    fn read_catalog(
        &mut self,
        limits: ArchiveCatalogLimits,
    ) -> Result<ArchiveCatalog, ArchiveCatalogError> {
        self.inner.read_catalog(limits)
    }

    fn read_variable_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<VariableDictionary, DictionaryError> {
        self.full_reads += 1;
        let dictionary = self.inner.read_variable_dictionary(metadata, limits)?;
        if self.return_unmarked_empty {
            assert_eq!(self.full_reads, 1, "a complete empty dictionary must not trigger a read loop");
            assert!(dictionary.is_empty());
            Ok(VariableDictionary::empty())
        } else {
            Ok(dictionary)
        }
    }

    fn read_variable_dictionary_prefix(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
        minimum_entries: u64,
    ) -> Result<VariableDictionary, DictionaryError> {
        let dictionary =
            self.inner
                .read_variable_dictionary_prefix(metadata, limits, minimum_entries)?;
        self.prefix_reads
            .push((minimum_entries, dictionary.len(), dictionary.is_complete()));
        Ok(dictionary)
    }

    fn read_log_type_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<LogTypeDictionary, DictionaryError> {
        self.inner.read_log_type_dictionary(metadata, limits)
    }

    fn read_log_type_dictionary_fields(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
        fields: &[u32],
    ) -> Result<LogTypeDictionary, DictionaryError> {
        self.inner
            .read_log_type_dictionary_fields(metadata, limits, fields)
    }

    fn read_array_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<ArrayDictionary, DictionaryError> {
        self.inner.read_array_dictionary(metadata, limits)
    }

    fn read_packed_stream(
        &mut self,
        metadata: &ArchiveMetadata,
        tables: &TableMetadata,
        stream_id: usize,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        self.inner
            .read_packed_stream(metadata, tables, stream_id, limits)
    }

    fn read_packed_stream_projected(
        &mut self,
        metadata: &ArchiveMetadata,
        tables: &TableMetadata,
        stream_id: usize,
        wanted: &[bool],
        truncate: Option<ColumnTruncation<'_>>,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        ArchiveReader::read_packed_stream_projected(
            &mut self.inner,
            metadata,
            tables,
            stream_id,
            wanted,
            truncate,
            limits,
        )
    }
}

#[derive(Default)]
struct Output(Vec<u8>);

impl JsonlRecordSink for Output {
    fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
        self.0.extend_from_slice(record.jsonl_bytes());
        Ok(())
    }
}

fn options(separate: bool) -> WriterOptions {
    WriterOptions::default()
        .with_log_order(false)
        .with_grouped_array_tables(false)
        .with_separate_columns_min_size(if separate { 1 } else { u64::MAX })
        .with_adaptive_numeric_columns(separate)
        .with_adaptive_string_columns(separate)
}

fn append_tail(writer: &mut OpenArchive<Cursor<Vec<u8>>>, first: usize, last: usize) {
    for index in first..last {
        let value = format!(
            "unused-{index:08x}-{:016x}",
            (index as u64).wrapping_mul(0x9e3779b97f4a7c15)
        );
        writer
            .append_record(RecordRef::new(&[
                FieldRef::new(b"id", ValueRef::I64(-1)),
                FieldRef::new(b"unused", ValueRef::String(value.as_bytes())),
            ]))
            .unwrap();
    }
}

fn fixture(separate: bool, unselected_tail: bool) -> Vec<u8> {
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options(separate));
    writer
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(1)),
            FieldRef::new(b"value", ValueRef::String(b"early")),
        ]))
        .unwrap();
    // Table streams are ordered by size. Keep this early-ID table ahead of the later one,
    // while only its first row matches the numeric predicate.
    for _ in 0..32 {
        writer
            .append_record(RecordRef::new(&[
                FieldRef::new(b"id", ValueRef::I64(0)),
                FieldRef::new(b"value", ValueRef::String(b"early")),
            ]))
            .unwrap();
    }
    append_tail(&mut writer, 0, 32);
    // This later schema requires extending the prefix after the first match has been emitted.
    writer
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(2)),
            FieldRef::new(b"value", ValueRef::String(b"later")),
            FieldRef::new(b"second", ValueRef::Bool(true)),
        ]))
        .unwrap();
    append_tail(&mut writer, 32, 4096);
    if unselected_tail {
        // Whole-column validation also needs IDs from nonmatching rows in a loaded column.
        writer
            .append_record(RecordRef::new(&[
                FieldRef::new(b"id", ValueRef::I64(-2)),
                FieldRef::new(b"value", ValueRef::String(b"unselected-tail-value")),
            ]))
            .unwrap();
    }
    writer.finish().unwrap().into_inner().into_inner()
}

fn search_bytes(reader: &mut TrackingReader, query: &str, selected: Option<&[&str]>) -> Vec<u8> {
    let projection = selected.map_or_else(Projection::all, |fields| {
        Projection::selected(fields, ProjectionLimits::default()).unwrap()
    });
    let options = SearchJsonlOptions::new(projection);
    let mut output = Output::default();
    let mut sink = SearchJsonlAdapter::new(&mut output, &options);
    let search_options = ArchiveSearchOptions::default().with_columns(
        ColumnLimits::default().with_dictionary_validation(reader.validate_references),
    );
    let stats = search_archive(
        reader,
        &parse_kql(query, KqlLimits::default()).unwrap(),
        &mut sink,
        &search_options,
    )
    .unwrap();
    assert_eq!(
        stats.matching_rows(),
        output
            .0
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty())
            .count() as u64
    );
    output.0
}

fn search(
    reader: &mut TrackingReader,
    query: &str,
    selected: Option<&[&str]>,
) -> Vec<serde_json::Value> {
    search_bytes(reader, query, selected)
        .split(|byte| *byte == b'\n')
        .filter(|row| !row.is_empty())
        .map(|row| serde_json::from_slice(row).unwrap())
        .collect()
}

#[test]
fn numeric_filter_reads_output_dictionary_prefix_and_extends_for_later_schemas() {
    for (projection, validate) in [
        (None, false),
        (Some(&["id", "value"][..]), false),
        (None, true),
        (Some(&["id", "value"][..]), true),
    ] {
        let mut reader = TrackingReader::new(fixture(true, false));
        reader.validate_references = validate;
        let rows = search(&mut reader, "id >= 1", projection);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], serde_json::json!({"id": 1, "value": "early"}));
        assert_eq!(rows[1]["id"], 2);
        assert_eq!(rows[1]["value"], "later");
        assert_eq!(
            reader.full_reads, 0,
            "output-only variable dictionaries should stay lazy"
        );
        assert!(!reader.prefix_reads.is_empty());
        assert!(
            reader
                .prefix_reads
                .iter()
                .all(|(_, entries, complete)| *entries < 128 && !complete),
            "unused tail must stay unloaded: {:?}",
            reader.prefix_reads
        );
        assert!(
            reader
                .prefix_reads
                .windows(2)
                .any(|reads| reads[0].0 < reads[1].0),
            "later schema should extend the retained prefix: {:?}",
            reader.prefix_reads
        );
    }
}

#[test]
fn string_filters_never_treat_unloaded_entries_as_absent() {
    for (query, expected) in [
        ("value:unselected-tail-value", vec![-2]),
        ("NOT value:early", vec![-2, 2]),
        ("NOT (value:early OR value:later)", vec![-2]),
    ] {
        let mut reader = TrackingReader::new(fixture(true, true));
        let mut ids: Vec<_> = search(&mut reader, query, Some(&["id"]))
            .into_iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, expected, "{query}");
        assert_eq!(
            reader.full_reads, 1,
            "{query}: filter dictionaries must be complete"
        );
        assert!(reader.prefix_reads.is_empty(), "{query}");
    }
}

#[test]
fn count_needs_no_output_dictionary_and_shared_frames_remain_correct() {
    let bytes = fixture(true, false);
    let mut reader = TrackingReader::new(bytes);
    let plan = AggregationPlan::count();
    let mut sink = plan.start();
    let stats = search_archive(
        &mut reader,
        &parse_kql("id >= 1", KqlLimits::default()).unwrap(),
        &mut sink,
        &ArchiveSearchOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.matching_rows(), 2);
    assert_eq!(reader.full_reads, 0);
    assert!(reader.prefix_reads.is_empty());

    let mut reader = TrackingReader::new(fixture(true, false));
    let plan = AggregationPlan::unique("value").unwrap();
    let mut sink = plan.start();
    let stats = search_archive(
        &mut reader,
        &parse_kql("id >= 1", KqlLimits::default()).unwrap(),
        &mut sink,
        &ArchiveSearchOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.matching_rows(), 2);
    assert_eq!(sink.results().len(), 2);
    assert_eq!(reader.full_reads, 0);
    assert!(
        reader
            .prefix_reads
            .iter()
            .all(|(_, entries, complete)| *entries < 128 && !complete)
    );

    let mut reader = TrackingReader::new(fixture(false, false));
    assert_eq!(search(&mut reader, "id >= 1", None).len(), 2);
    assert_eq!(reader.full_reads, 1);
}

#[test]
fn shared_zero_match_skips_output_dictionary_unless_strict_validation_needs_it() {
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options(false));
    for id in [1, 3] {
        writer.append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(id)),
            FieldRef::new(b"value", ValueRef::String(b"required-on-output")),
        ])).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    for selected in [None, Some(&["id", "value"][..])] {
        for validate in [false, true] {
            let mut reader = TrackingReader::new(bytes.clone());
            reader.validate_references = validate;
            // The absent value lies inside [1, 3], so metadata cannot reject the table.
            assert!(search(&mut reader, "id:2", selected).is_empty());
            assert_eq!(reader.full_reads, usize::from(validate));
            assert!(reader.prefix_reads.is_empty());
        }
    }
}

#[test]
fn full_reader_empty_dictionary_is_complete_even_without_prefix_marker() {
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options(false));
    writer.append_record(RecordRef::new(&[
        FieldRef::new(b"id", ValueRef::I64(1)),
        // A CLP template with no placeholders needs the log dictionary but no variable entries.
        FieldRef::new(b"message", ValueRef::String(b"hello world")),
    ])).unwrap();
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    let mut reader = TrackingReader::new(bytes);
    reader.return_unmarked_empty = true;
    assert_eq!(search(&mut reader, "id:1", None),
        [serde_json::json!({"id": 1, "message": "hello world"})]);
    assert_eq!(reader.full_reads, 1);
    assert!(reader.prefix_reads.is_empty());
}

#[test]
fn nonmatching_high_id_in_a_loaded_column_keeps_whole_column_validation() {
    for validate in [false, true] {
        let mut reader = TrackingReader::new(fixture(true, true));
        reader.validate_references = validate;
        let rows = search(&mut reader, "id >= 1", None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["value"], "early");
        assert_eq!(rows[1]["value"], "later");
        assert_eq!(reader.full_reads, 0);
        assert!(
            reader
                .prefix_reads
                .iter()
                .any(|(_, entries, complete)| *entries > 4096 && *complete),
            "a high ID in the same loaded column must remain validated"
        );
    }
}

#[test]
fn prefix_output_preserves_clp_variables_float_lexemes_and_array_elements() {
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options(true));
    let items = [
        ValueRef::String(b"array-alpha"),
        ValueRef::String(b"array-beta"),
    ];
    let token = b"123456789.123456789";
    writer
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(1)),
            FieldRef::new(b"status", ValueRef::String(b"ready")),
            FieldRef::new(
                b"message",
                ValueRef::String(b"connected to worker-ab12:9876"),
            ),
            FieldRef::new(
                b"number",
                ValueRef::RetainedFloat(RetainedFloatRef::new(123456789.123456789, token)),
            ),
            FieldRef::new(b"items", ValueRef::Array(&items)),
            FieldRef::new(
                b"raw",
                ValueRef::UnstructuredArray(UnstructuredArrayRef::new(b"[\"raw-ab12\", 3]")),
            ),
        ]))
        .unwrap();
    append_tail(&mut writer, 0, 4096);
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    let mut reader = TrackingReader::new(bytes);
    let output = search_bytes(&mut reader, "id:1", None);
    assert!(
        output.windows(token.len()).any(|part| part == token),
        "DictionaryFloat must retain its original number token"
    );
    let rows: Vec<serde_json::Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|row| !row.is_empty())
        .map(|row| serde_json::from_slice(row).unwrap())
        .collect();
    assert_eq!(
        rows,
        [serde_json::json!({
            "id": 1, "status": "ready", "message": "connected to worker-ab12:9876",
            "number": 123456789.123456789, "items": ["array-alpha", "array-beta"],
            "raw": ["raw-ab12", 3],
        })]
    );
    assert_eq!(reader.full_reads, 0);
    assert!(!reader.prefix_reads.is_empty());
    assert!(
        reader
            .prefix_reads
            .iter()
            .all(|(_, entries, complete)| *entries < 64 && !complete)
    );
}
