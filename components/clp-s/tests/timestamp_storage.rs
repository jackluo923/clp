//! Storage ticks and quantization must agree with extraction, summaries and pruning.
use clp_s::archive::{
    ArchiveCatalogLimits, ColumnData, ColumnLimits, PackedStreamLimits, SingleFileArchiveReader,
};
use clp_s::numeric_summary::{NumericSummaries, integer};
use clp_s::timestamp_chunks::TimestampChunks;
use clp_s::timestamp_precision::{TimestampPrecision as Precision, TimestampRounding as Rounding};
use clp_s::writer::{FieldRef, OpenArchive, RecordRef, TimestampRef, ValueRef, WriterOptions};
use clp_s::{ExtractionMode, ExtractionOptions, extract_jsonl};
use std::io::Cursor;

fn archive(values: &[i64], precision: Precision, rounding: Rounding, split: bool) -> Vec<u8> {
    let options = WriterOptions::default()
        .with_timestamp_precision(precision)
        .with_timestamp_rounding(rounding);
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), options);
    for (i, &ns) in values.iter().enumerate() {
        let lexeme = ns.to_string();
        let mut fields = vec![
            FieldRef::new(
                b"ts",
                ValueRef::Timestamp(TimestampRef::new(ns, &lexeme, r"\N", "ts")),
            ),
            FieldRef::new(b"unindexed", ValueRef::I64(ns)),
        ];
        if split && i % 2 == 1 {
            fields.push(FieldRef::new(b"other", ValueRef::Bool(true)));
        }
        writer.append_record(RecordRef::new(&fields)).unwrap();
    }
    writer.finish().unwrap().into_inner().into_inner()
}

fn check(values: &[i64], expected: &[i64], precision: Precision, rounding: Rounding, split: bool) {
    let bytes = archive(values, precision, rounding, split);
    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone())).unwrap();
    let catalog = reader
        .read_catalog(ArchiveCatalogLimits::default())
        .unwrap();
    let dict = catalog.metadata().timestamp_dictionary();
    let range = &dict.ranges()[0];
    assert_eq!(range.storage_precision(), precision);
    assert_eq!(range.rounding(), rounding);
    let node = range.column_ids()[0] as u32;
    let summaries =
        NumericSummaries::decode(catalog.metadata().numeric_summaries().unwrap()).unwrap();
    let summary_sum: i128 = summaries
        .columns
        .iter()
        .filter(|s| s.node == node)
        .map(|s| integer(s.negative.sum) + integer(s.nonnegative.sum))
        .sum();
    assert_eq!(
        summary_sum,
        expected.iter().map(|&v| i128::from(v)).sum::<i128>()
    );
    let chunks = TimestampChunks::decode(catalog.metadata().timestamp_chunks().unwrap()).unwrap();
    let bounds: Vec<_> = chunks
        .columns
        .iter()
        .filter(|c| c.node == node)
        .flat_map(|c| &c.bounds)
        .collect();
    assert_eq!(
        bounds.iter().map(|v| v[0]).min(),
        expected.iter().copied().min()
    );
    assert_eq!(
        bounds.iter().map(|v| v[1]).max(),
        expected.iter().copied().max()
    );
    let mut normalized = Vec::new();
    for stream_id in 0..catalog.table_metadata().packed_streams().len() {
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                stream_id,
                PackedStreamLimits::default(),
            )
            .unwrap();
        let stream = catalog
            .materialize_adaptive_numerics(stream_id as u64, stream, ColumnLimits::default())
            .unwrap();
        for table in catalog
            .schema_tables(stream_id as u64, &stream, ColumnLimits::default())
            .unwrap()
        {
            let table = table.unwrap();
            for column in table.table().columns() {
                if let ColumnData::Timestamp(ts) = column.data() {
                    let ns: Vec<_> = ts.epochs().values().collect();
                    // Prove the on-disk integers are quantized ticks, not nanoseconds.
                    let ticks: Vec<_> = ns
                        .iter()
                        .map(|v| {
                            i64::try_from(
                                i128::from(*v) / i128::from(precision.nanoseconds_per_tick()),
                            )
                            .unwrap()
                        })
                        .collect();
                    let deltas: Vec<_> = ticks
                        .iter()
                        .enumerate()
                        .map(|(i, v)| v - if i % clp_s::timestamp_chunks::CHUNK_ROWS as usize == 0 { 0 } else { ticks[i - 1] })
                        .collect();
                    assert_eq!(ts.epochs().deltas().iter().collect::<Vec<_>>(), deltas);
                    normalized.extend(ns);
                }
            }
        }
    }
    normalized.sort_unstable();
    let mut sorted = expected.to_vec();
    sorted.sort_unstable();
    assert_eq!(normalized, sorted);
    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
    let mut json = Vec::new();
    extract_jsonl(
        &mut reader,
        &mut json,
        ExtractionOptions::new(ExtractionMode::LogOrder),
    )
    .unwrap();
    for (i, line) in json
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let row: serde_json::Value = serde_json::from_slice(line).unwrap();
        assert_eq!(row["ts"].as_i64(), Some(expected[i]));
        assert_eq!(row["unindexed"].as_i64(), Some(values[i]));
    }
}

#[test]
fn all_units_quantize_values_indexes_and_summaries_consistently() {
    for duration in [
        "ns", "1ns", "10ns", "100ns", "us", "1us", "10us", "100us", "250us", "ms", "1ms", "10ms",
        "100ms", "s", "1s", "10s", "100s",
    ] {
        let precision: Precision = duration.parse().unwrap();
        let scale = precision.nanoseconds_per_tick();
        for rounding in [Rounding::Truncate, Rounding::Nearest] {
            let input = [
                -1250 * scale - scale / 2,
                -scale / 2,
                0,
                scale / 2,
                1250 * scale + scale / 2,
            ];
            let expected: Vec<_> = input
                .iter()
                .map(|&ns| precision.quantize(ns, rounding).unwrap() * scale)
                .collect();
            check(&input, &expected, precision, rounding, false);
        }
    }
}

#[test]
fn quantization_handles_irregular_ticks_across_schemas_and_negative_epochs() {
    let values = [-150_999_999, -50_000_001, 49_900_000, 149_999_999];
    check(
        &values,
        &[-151_000_000, -50_000_000, 50_000_000, 150_000_000],
        Precision::Milliseconds,
        Rounding::Nearest,
        true,
    );
    let values = [1_050_000_000, 1_151_000_000, 1_250_000_000, 1_351_000_000];
    check(
        &values,
        &values,
        Precision::Milliseconds,
        Rounding::Truncate,
        true,
    );
}

#[test]
fn quantization_overflow_rejects_the_record_without_polluting_the_writer() {
    let mut writer = OpenArchive::new(
        Cursor::new(Vec::new()),
        WriterOptions::default().with_timestamp_precision(Precision::Seconds),
    );
    let lexeme = i64::MIN.to_string();
    let fields = [FieldRef::new(
        b"ts",
        ValueRef::Timestamp(TimestampRef::new(i64::MIN, &lexeme, r"\N", "ts")),
    )];
    assert!(matches!(
        writer.append_record(RecordRef::new(&fields)),
        Err(clp_s::writer::AppendError::Timestamp {
            reason: clp_s::writer::TimestampError::QuantizationOverflow,
            ..
        })
    ));
    let fields = [FieldRef::new(
        b"ts",
        ValueRef::Timestamp(TimestampRef::new(1, "1", r"\N", "ts")),
    )];
    writer.append_record(RecordRef::new(&fields)).unwrap();
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
    let mut output = Vec::new();
    extract_jsonl(&mut reader, &mut output, ExtractionOptions::default()).unwrap();
    assert_eq!(output, b"{\"ts\":0}\n");
}

#[test]
fn nanosecond_precision_preserves_extreme_ticks() {
    for values in [[i64::MIN, i64::MIN + 100], [i64::MAX - 100, i64::MAX]] {
        check(
            &values,
            &values,
            Precision::Nanoseconds,
            Rounding::Truncate,
            false,
        );
    }
}

#[test]
fn conflicting_per_field_storage_is_rejected_atomically() {
    use clp_s::timestamp_precision::TimestampStorage;
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
    let value = TimestampRef::new(1_050_000_000, "1050000000", r"\N", "ts");
    let fine = TimestampStorage::default();
    let coarse = TimestampStorage {
        precision: Precision::Milliseconds,
        ..fine
    };
    writer
        .append_record(RecordRef::new(&[FieldRef::new(
            b"ts",
            ValueRef::Timestamp(value.with_storage(fine)),
        )]))
        .unwrap();
    assert!(matches!(
        writer.append_record(RecordRef::new(&[FieldRef::new(
            b"ts",
            ValueRef::Timestamp(value.with_storage(coarse))
        )])),
        Err(clp_s::writer::AppendError::Timestamp {
            reason: clp_s::writer::TimestampError::ConflictingStorage,
            ..
        })
    ));
    writer
        .append_record(RecordRef::new(&[FieldRef::new(
            b"ts",
            ValueRef::Timestamp(value.with_storage(fine)),
        )]))
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
    let mut output = Vec::new();
    extract_jsonl(&mut reader, &mut output, ExtractionOptions::default()).unwrap();
    assert_eq!(output, b"{\"ts\":1050000000}\n{\"ts\":1050000000}\n");
}

#[test]
fn chunk_restart_roundtrip_and_disjoint_window_selection() {
    use clp_s::search::{parse_kql, KqlLimits, SearchOptions};
    let block = clp_s::timestamp_chunks::CHUNK_ROWS as usize;
    // First and third chunks overlap; the intervening chunk must be skipped even though
    // the bounding first-to-last row span covers it. A partial final chunk must also work.
    let values: Vec<_> = (0..3 * block + 11).map(|i| {
        let base = if i / block == 1 { 1_800_000_000_000_000_000 } else { 1_700_000_000_000_000_000 };
        base + (i % block) as i64 * 1_000_000
    }).collect();
    check(&values, &values, Precision::Milliseconds, Rounding::Truncate, false);
    let mut reader = SingleFileArchiveReader::open(Cursor::new(archive(&values, Precision::Milliseconds, Rounding::Truncate, false))).unwrap();
    let catalog = reader.read_catalog(ArchiveCatalogLimits::default()).unwrap();
    let parsed = parse_kql("ts >= 1700000000000000000 AND ts < 1700000000003000000", KqlLimits::default()).unwrap();
    let compiled = parsed.compile_for_archive(&catalog, SearchOptions::default()).unwrap();
    let mut matched = 0;
    for stream_id in 0..catalog.table_metadata().packed_streams().len() {
        let selections = compiled.stream_row_selections(stream_id as u64);
        let stream = reader.read_packed_stream(catalog.metadata(), catalog.table_metadata(), stream_id, PackedStreamLimits::default()).unwrap();
        let stream = catalog.materialize_adaptive_numerics(stream_id as u64, stream, ColumnLimits::default()).unwrap();
        for table in catalog.schema_tables(stream_id as u64, &stream, ColumnLimits::default()).unwrap().with_row_selections(selections) {
            let table = table.unwrap();
            assert_eq!(compiled.table_row_ranges(table.schema().id(), values.len()), Some(vec![(0, block), (block * 2, values.len())]));
            let bitmap = compiled.match_table(&table).unwrap();
            let expected: Vec<_> = values.iter().enumerate().filter(|(_, v)| **v >= 1_700_000_000_000_000_000 && **v < 1_700_000_000_003_000_000).map(|(i, _)| i).collect();
            assert_eq!(bitmap.matching_rows().collect::<Vec<_>>(), expected);
            matched += bitmap.match_count();
        }
    }
    assert_eq!(matched, 9);
}
