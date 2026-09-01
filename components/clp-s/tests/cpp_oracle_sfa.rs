use std::fs::File;
use std::ops::Range;
use std::path::PathBuf;

use clp_s::archive::ArchiveCatalogLimits;
use clp_s::archive::ArchiveCompression;
use clp_s::archive::ArchiveMetadata;
use clp_s::archive::ArchiveVersion;
use clp_s::archive::ColumnData;
use clp_s::archive::ColumnLimits;
use clp_s::archive::DictionaryLimits;
use clp_s::archive::MetadataLimits;
use clp_s::archive::NodeType;
use clp_s::archive::PackedStreamLimits;
use clp_s::archive::RangeIndexValue;
use clp_s::archive::SFA_SECTION_NAMES;
use clp_s::archive::SchemaEntry;
use clp_s::archive::SchemaMap;
use clp_s::archive::SchemaMapLimits;
use clp_s::archive::SchemaTree;
use clp_s::archive::SchemaTreeLimits;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::archive::TableMetadata;
use clp_s::archive::TableMetadataLimits;
use clp_s::archive::TimestampBounds;
use clp_s::archive::append_clp_message;

const ARCHIVE_SIZE: u64 = 654;
const FILES_OFFSET: u64 = 363;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sfa-v0.5.0-minimal-cpp.bin")
}

#[test]
fn loads_a_streaming_catalog_from_the_cpp_oracle() {
    let fixture = File::open(fixture_path()).expect("open committed C++ oracle fixture");
    let mut archive = SingleFileArchiveReader::open(fixture).expect("open C++ SFA envelope");
    let catalog = archive
        .read_catalog(ArchiveCatalogLimits::default())
        .expect("load cross-validated archive catalog");

    assert_eq!(8, catalog.schema_tree().len());
    assert_eq!(1, catalog.schema_map().len());
    assert_eq!(1, catalog.table_metadata().record_count());
    assert_eq!(1, catalog.variable_dictionary().len());
    assert_eq!(1, catalog.log_type_dictionary().len());
    assert!(catalog.array_dictionary().is_empty());
    assert_eq!(1, catalog.timestamp_patterns().len());

    let mut timestamp = String::new();
    catalog
        .timestamp_patterns()
        .append_epoch_nanoseconds(0, 1_700_000_000_123_000_000, &mut timestamp)
        .expect("format the C++ fixture's precompiled timestamp pattern");
    assert_eq!("1700000000123", timestamp);

    let stream = archive
        .read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            0,
            PackedStreamLimits::default(),
        )
        .expect("load catalog's packed stream on demand");
    assert_packed_stream(stream.as_bytes());

    let mut tables = catalog
        .schema_tables(0, &stream, ColumnLimits::default())
        .expect("select the C++ fixture's schema table lazily");
    let table = tables
        .next()
        .expect("one schema table")
        .expect("decode the C++ fixture's schema table");
    assert!(tables.next().is_none());
    assert_cpp_oracle_record(&catalog, &table);
}

fn assert_cpp_oracle_record(
    catalog: &clp_s::archive::ArchiveCatalog,
    decoded: &clp_s::archive::DecodedSchemaTable<'_, '_>,
) {
    let columns = decoded.table().columns();
    let ColumnData::Timestamp(timestamp_column) = columns[1].data() else {
        panic!("C++ fixture timestamp column has the expected type");
    };
    let timestamp = timestamp_column.get(0).expect("C++ fixture timestamp");
    let ColumnData::VarString(level_column) = columns[2].data() else {
        panic!("C++ fixture level column has the expected type");
    };
    let ColumnData::ClpString(message_column) = columns[3].data() else {
        panic!("C++ fixture message column has the expected type");
    };
    let message = message_column.record(0).expect("C++ fixture CLP message");
    let ColumnData::Integer(value_column) = columns[4].data() else {
        panic!("C++ fixture integer column has the expected type");
    };
    let ColumnData::Boolean(active_column) = columns[5].data() else {
        panic!("C++ fixture Boolean column has the expected type");
    };

    let mut timestamp_lexeme = String::new();
    catalog
        .timestamp_patterns()
        .append_epoch_nanoseconds(
            timestamp.pattern_id(),
            timestamp.epoch_nanoseconds(),
            &mut timestamp_lexeme,
        )
        .expect("reconstruct C++ fixture timestamp lexeme");

    let mut output = br#"{"ts":"#.to_vec();
    output.extend_from_slice(timestamp_lexeme.as_bytes());
    output.extend_from_slice(b",\"level\":\"");
    output.extend_from_slice(level_column.value(0).expect("C++ fixture level"));
    output.extend_from_slice(b"\",\"message\":\"");
    append_clp_message(
        message.logtype(),
        catalog.variable_dictionary(),
        &message.encoded_variables(),
        &mut output,
    )
    .expect("reconstruct C++ fixture CLP message");
    output.extend_from_slice(b"\",\"value\":");
    output.extend_from_slice(
        value_column
            .get(0)
            .expect("C++ fixture value")
            .to_string()
            .as_bytes(),
    );
    output.extend_from_slice(b",\"active\":");
    output.extend_from_slice(if active_column.get(0).expect("C++ fixture active") {
        b"true"
    } else {
        b"false"
    });
    output.extend_from_slice(b"}\n");

    assert_eq!(
        include_bytes!("fixtures/sfa-v0.5.0-minimal-cpp-input.jsonl").as_slice(),
        output
    );
}

#[test]
fn reads_metadata_from_cpp_oracle_archive() {
    let fixture = File::open(fixture_path()).expect("open committed C++ oracle fixture");
    let mut archive = SingleFileArchiveReader::open(fixture).expect("open C++ SFA envelope");

    assert_eq!(ArchiveVersion::CURRENT, archive.header().version());
    assert_eq!(88, archive.header().uncompressed_size());
    assert_eq!(ARCHIVE_SIZE, archive.header().compressed_size());
    assert_eq!(299, archive.header().metadata_section_size());
    assert_eq!(FILES_OFFSET, archive.header().files_section_offset());
    assert_eq!(ArchiveCompression::Zstd, archive.header().compression());
    assert_eq!(64..FILES_OFFSET, archive.layout().metadata_range());
    assert_eq!(FILES_OFFSET..ARCHIVE_SIZE, archive.layout().files_range());

    let metadata = archive
        .read_metadata(MetadataLimits::default())
        .expect("decode metadata written by C++ clp-s");
    assert_section_directory(&metadata);
    assert_timestamp_dictionary(&metadata);
    assert_range_index(&metadata);

    let schema_tree = archive
        .read_schema_tree(&metadata, SchemaTreeLimits::default())
        .expect("decode schema tree written by C++ clp-s");
    assert_schema_tree(&schema_tree);
    let schema_map = archive
        .read_schema_map(&metadata, &schema_tree, SchemaMapLimits::default())
        .expect("decode schema map written by C++ clp-s");
    assert_schema_map(&schema_map);
    let table_metadata = archive
        .read_table_metadata(&metadata, &schema_map, TableMetadataLimits::default())
        .expect("decode table metadata written by C++ clp-s");
    assert_table_metadata(&table_metadata);

    let packed_stream = archive
        .read_packed_stream(&metadata, &table_metadata, 0, PackedStreamLimits::default())
        .expect("decode packed stream written by C++ clp-s");
    assert_packed_stream(packed_stream.as_bytes());
    metadata
        .range_index()
        .expect("C++ fixture has a range index")
        .validate_record_domain(table_metadata.record_count())
        .expect("C++ fixture range fits the table-metadata record domain");

    let variable_dictionary = archive
        .read_variable_dictionary(&metadata, DictionaryLimits::default())
        .expect("decode variable dictionary written by C++ clp-s");
    assert_eq!(1, variable_dictionary.len());
    assert_eq!(
        b"INFO",
        variable_dictionary
            .entry(0)
            .expect("C++ fixture variable entry zero")
            .value()
    );

    let logtype_dictionary = archive
        .read_log_type_dictionary(&metadata, DictionaryLimits::default())
        .expect("decode logtype dictionary written by C++ clp-s");
    assert_eq!(1, logtype_dictionary.len());
    let logtype = logtype_dictionary
        .entry(0)
        .expect("C++ fixture logtype entry zero");
    assert_eq!(b"oracle fixture", logtype.escaped_value());
    assert_eq!(0, logtype.placeholder_counts().encoded_variables());
    assert_eq!(0, logtype.placeholder_counts().escape_sequences());

    let array_dictionary = archive
        .read_array_dictionary(&metadata, DictionaryLimits::default())
        .expect("decode canonical empty array dictionary written by C++ clp-s");
    assert!(array_dictionary.is_empty());
}

fn assert_section_directory(metadata: &ArchiveMetadata) {
    let expected_ranges: [Range<u64>; 7] = [
        363..471,
        471..510,
        510..541,
        541..570,
        570..609,
        609..617,
        617..654,
    ];
    let sections = metadata.directory().sections();
    assert_eq!(SFA_SECTION_NAMES.len(), sections.len());
    for ((section, expected_name), expected_range) in
        sections.iter().zip(SFA_SECTION_NAMES).zip(expected_ranges)
    {
        assert_eq!(expected_name, section.name());
        assert_eq!(expected_range, section.range());
        assert_eq!(
            expected_range.end - expected_range.start,
            section.compressed_size()
        );
        assert_eq!(Some(section), metadata.directory().get(expected_name));
    }
}

fn assert_timestamp_dictionary(metadata: &ArchiveMetadata) {
    let timestamp_dictionary = metadata.timestamp_dictionary();
    assert_eq!(1, timestamp_dictionary.ranges().len());
    let authoritative = timestamp_dictionary
        .authoritative_range()
        .expect("C++ fixture has an authoritative timestamp");
    assert_eq!("ts", authoritative.key());
    assert_eq!(&[3], authoritative.column_ids());
    assert_eq!(
        TimestampBounds::Epoch {
            start: 1_700_000_000_123,
            end: 1_700_000_000_123,
        },
        authoritative.bounds()
    );
    assert_eq!(1, timestamp_dictionary.patterns().len());
    let pattern = timestamp_dictionary
        .pattern(0)
        .expect("C++ fixture has explicit pattern ID zero");
    assert_eq!(0, pattern.id());
    assert_eq!(r"\L", pattern.raw());

    let timestamp_dictionary = timestamp_dictionary.encoded_bytes();
    assert_eq!(80, timestamp_dictionary.len());
    assert_eq!(&1_u64.to_le_bytes(), &timestamp_dictionary[0..8]);
    assert_eq!(&2_u64.to_le_bytes(), &timestamp_dictionary[8..16]);
    assert_eq!(b"ts", &timestamp_dictionary[16..18]);
    assert_eq!(
        &1_700_000_000_123_i64.to_le_bytes(),
        &timestamp_dictionary[38..46]
    );
    assert_eq!(
        &1_700_000_000_123_i64.to_le_bytes(),
        &timestamp_dictionary[46..54]
    );
}

fn assert_range_index(metadata: &ArchiveMetadata) {
    let range_index_bytes = metadata
        .range_index_bytes()
        .expect("default C++ compression records a range index");
    assert_eq!(136, range_index_bytes.len());
    assert_eq!(&[0x91, 0x83, 0xa1, b'e', 0x01], &range_index_bytes[..5]);
    assert!(contains(
        range_index_bytes,
        b"764d6dd1-aab6-49ca-86e1-b41e19cffb16"
    ));
    assert!(contains(
        range_index_bytes,
        b"/sfa-v0.5.0-minimal-cpp-input.jsonl"
    ));

    let range_index = metadata
        .range_index()
        .expect("C++ range-index packet is structurally valid");
    assert_eq!(range_index_bytes, range_index.encoded_bytes());
    assert_eq!(1, range_index.entries().len());
    let entry = &range_index.entries()[0];
    assert_eq!(0..1, entry.range());
    assert_eq!(
        Some("764d6dd1-aab6-49ca-86e1-b41e19cffb16"),
        entry
            .field("_archive_creator_id")
            .and_then(RangeIndexValue::as_str)
    );
    assert_eq!(
        Some(0),
        entry
            .field("_file_split_number")
            .and_then(RangeIndexValue::as_u64)
    );
    assert_eq!(
        Some("/sfa-v0.5.0-minimal-cpp-input.jsonl"),
        entry.field("_filename").and_then(RangeIndexValue::as_str)
    );
    range_index
        .validate_record_domain(1)
        .expect("C++ fixture range fits its one-record domain");
    assert_eq!(0, metadata.unknown_packet_count());
}

fn assert_schema_tree(schema_tree: &SchemaTree) {
    let expected_nodes: [(Option<usize>, &[u8], NodeType); 8] = [
        (None, b"", NodeType::Metadata),
        (Some(0), b"log_event_idx", NodeType::DeltaInteger),
        (None, b"", NodeType::Object),
        (Some(2), b"ts", NodeType::Timestamp),
        (Some(2), b"level", NodeType::VarString),
        (Some(2), b"message", NodeType::ClpString),
        (Some(2), b"value", NodeType::Integer),
        (Some(2), b"active", NodeType::Boolean),
    ];
    assert_eq!(expected_nodes.len(), schema_tree.len());
    assert!(!schema_tree.is_empty());
    for (node_id, (node, expected)) in schema_tree.nodes().iter().zip(expected_nodes).enumerate() {
        let (expected_parent, expected_key, expected_type) = expected;
        assert_eq!(expected_parent, node.parent_id(), "node {node_id} parent");
        assert_eq!(expected_key, node.key_bytes(), "node {node_id} key");
        assert_eq!(
            std::str::from_utf8(expected_key).expect("fixture key is UTF-8"),
            node.key_str().expect("C++ JSON key is UTF-8"),
            "node {node_id} UTF-8 key"
        );
        assert_eq!(expected_type, node.node_type(), "node {node_id} type");
        assert_eq!(Some(node), schema_tree.get(node_id));
    }
}

fn assert_schema_map(schema_map: &SchemaMap) {
    assert_eq!(1, schema_map.len());
    assert!(!schema_map.is_empty());
    let schema = schema_map.get(0).expect("fixture has schema ID zero");
    let expected = [
        SchemaEntry::Node(1),
        SchemaEntry::Node(3),
        SchemaEntry::Node(4),
        SchemaEntry::Node(5),
        SchemaEntry::Node(6),
        SchemaEntry::Node(7),
    ];
    assert_eq!(expected, schema.entries());
    assert_eq!(expected, schema.ordered_entries());
    assert_eq!(&[] as &[SchemaEntry], schema.unordered_entries());
    assert_eq!(Some(schema), schema_map.schemas().first());
}

fn assert_table_metadata(metadata: &TableMetadata) {
    assert_eq!(57, metadata.total_uncompressed_stream_size());
    assert_eq!(1, metadata.record_count());
    assert_eq!(1, metadata.packed_streams().len());
    let stream = metadata
        .packed_stream(0)
        .expect("fixture packed stream zero");
    assert_eq!(0, stream.file_offset());
    assert_eq!(37, stream.compressed_size());
    assert_eq!(0..37, stream.compressed_range());
    assert_eq!(57, stream.uncompressed_size());

    assert_eq!(1, metadata.schema_tables().len());
    let table = metadata.schema_table(0).expect("fixture schema table zero");
    assert_eq!(0, table.stream_id());
    assert_eq!(0, table.stream_offset());
    assert_eq!(57, table.uncompressed_size());
    assert_eq!(0, table.schema_id());
    assert_eq!(1, table.message_count());
    assert_eq!(Some(table), metadata.schema_tables().first());
}

fn assert_packed_stream(bytes: &[u8]) {
    let mut expected = Vec::with_capacity(57);
    expected.extend_from_slice(&0_i64.to_le_bytes());
    expected.extend_from_slice(&1_700_000_000_123_000_000_i64.to_le_bytes());
    expected.extend_from_slice(&0_u64.to_le_bytes());
    expected.extend_from_slice(&0_u64.to_le_bytes());
    expected.extend_from_slice(&0_u64.to_le_bytes());
    expected.extend_from_slice(&0_u64.to_le_bytes());
    expected.extend_from_slice(&42_i64.to_le_bytes());
    expected.push(1);

    assert_eq!(57, bytes.len());
    assert_eq!(expected, bytes);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
