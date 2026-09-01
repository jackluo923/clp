use std::io::Cursor;

use clp_s::ExtractionMode;
use clp_s::ExtractionOptions;
use clp_s::archive::ArchiveCatalogLimits;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::extract_jsonl;
use clp_s::writer::AppendError;
use clp_s::writer::AppendResource;
use clp_s::writer::FieldRef;
use clp_s::writer::OpenArchive;
use clp_s::writer::RecordRef;
use clp_s::writer::ValueRef;
use clp_s::writer::WriterLimits;
use clp_s::writer::WriterOptions;

const CPP_ARCHIVE_HEX: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");
const CPP_PHYSICAL_JSONL: &[u8] =
    include_bytes!("fixtures/sfa-v0.5.0-structured-arrays-cpp-search.jsonl");
const SOURCE_JSONL: &[u8] = include_bytes!("fixtures/sfa-v0.5.0-structured-arrays-cpp-input.jsonl");

fn decode_hex(source: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(source.len() / 2);
    let mut high = None;
    for byte in source
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => panic!("fixture contains a non-hex byte"),
        };
        if let Some(high) = high.take() {
            decoded.push((high << 4) | nibble);
        } else {
            high = Some(nibble);
        }
    }
    assert!(high.is_none(), "fixture contains an odd number of nibbles");
    decoded
}

#[allow(clippy::too_many_lines)]
fn write_cpp_oracle_records(options: WriterOptions) -> Vec<u8> {
    let mut archive = OpenArchive::new(Cursor::new(Vec::new()), options);

    let empty_values = [];
    let object_x = [FieldRef::new(b"x", ValueRef::I64(1))];
    let object_y = [FieldRef::new(b"y", ValueRef::I64(2))];
    let items = [
        ValueRef::I64(1),
        ValueRef::I64(2),
        ValueRef::Null,
        ValueRef::Object(&object_x),
        ValueRef::Object(&object_y),
        ValueRef::Array(&empty_values),
    ];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(0)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append mixed scalar/object array");

    let first_xy = [
        FieldRef::new(b"x", ValueRef::I64(1)),
        FieldRef::new(b"y", ValueRef::I64(0)),
    ];
    let second_xy = [
        FieldRef::new(b"x", ValueRef::I64(0)),
        FieldRef::new(b"y", ValueRef::I64(2)),
    ];
    let items = [ValueRef::Object(&first_xy), ValueRef::Object(&second_xy)];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(1)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append repeated heterogeneous object elements");

    let x_fields = [FieldRef::new(b"x", ValueRef::I64(3))];
    let y_fields = [FieldRef::new(b"y", ValueRef::I64(4))];
    let first_nested_array = [ValueRef::Object(&x_fields)];
    let second_nested_array = [ValueRef::Object(&y_fields)];
    let items = [
        ValueRef::Array(&first_nested_array),
        ValueRef::Array(&second_nested_array),
    ];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(2)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append nested arrays");

    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(3)),
            FieldRef::new(b"items", ValueRef::Array(&[])),
        ]))
        .expect("append empty array");

    let items = [ValueRef::Null];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(4)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append null array element");

    let empty_object = [];
    let null_x = [FieldRef::new(b"x", ValueRef::Null)];
    let items = [ValueRef::Object(&empty_object), ValueRef::Object(&null_x)];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(5)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append empty and null-only objects");

    let nested_object_x = [FieldRef::new(b"x", ValueRef::I64(5))];
    let nested_items = [ValueRef::Object(&nested_object_x)];
    let object = [FieldRef::new(b"items", ValueRef::Array(&nested_items))];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(6)),
            FieldRef::new(b"obj", ValueRef::Object(&object)),
        ]))
        .expect("append array below an ordinary object");

    let yes = [FieldRef::new(b"z", ValueRef::String(b"yes"))];
    let no = [FieldRef::new(b"z", ValueRef::String(b"no"))];
    let first_nested = [FieldRef::new(b"nested", ValueRef::Object(&yes))];
    let second_nested = [FieldRef::new(b"nested", ValueRef::Object(&no))];
    let items = [
        ValueRef::Object(&first_nested),
        ValueRef::Object(&second_nested),
    ];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(7)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append named nested objects");

    let deep_z = [FieldRef::new(b"z", ValueRef::String(b"deep"))];
    let deep_object = [ValueRef::Object(&deep_z)];
    let nested_array = [FieldRef::new(b"nested", ValueRef::Array(&deep_object))];
    let items = [ValueRef::Object(&nested_array)];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(8)),
            FieldRef::new(b"items", ValueRef::Array(&items)),
        ]))
        .expect("append array nested in an object element");

    archive
        .finish()
        .expect("finish structured-array archive")
        .into_inner()
        .into_inner()
}

#[test]
fn rust_writer_is_byte_exact_with_cpp_and_reconstructs_every_record() {
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_uncompressed_size(u64::try_from(SOURCE_JSONL.len()).expect("source size fits u64"));
    let actual = write_cpp_oracle_records(options);
    let mut reader = SingleFileArchiveReader::open(Cursor::new(actual.clone()))
        .expect("open Rust structured-array SFA");
    let catalog = reader
        .read_catalog(ArchiveCatalogLimits::default())
        .expect("read Rust structured-array catalog");
    assert_eq!(
        22,
        catalog.schema_tree().len(),
        "actual schema nodes: {:?}",
        catalog
            .schema_tree()
            .nodes()
            .iter()
            .map(|node| (node.parent_id(), node.key_bytes(), node.node_type()))
            .collect::<Vec<_>>()
    );
    assert_eq!(9, catalog.schema_map().len());
    assert_eq!(9, catalog.table_metadata().record_count());

    let expected = decode_hex(CPP_ARCHIVE_HEX);
    assert_eq!(expected, actual, "Rust SFA must be byte-identical to C++");

    let mut extracted = Vec::new();
    let stats = extract_jsonl(
        &mut reader,
        &mut extracted,
        ExtractionOptions::new(ExtractionMode::Unordered),
    )
    .expect("extract Rust structured arrays");
    assert_eq!(CPP_PHYSICAL_JSONL, extracted);
    assert_eq!(9, stats.records());
}

#[test]
fn structured_array_limit_failure_is_transactional() {
    let options = WriterOptions::default()
        .with_log_order(false)
        .with_limits(WriterLimits::DEFAULT.with_structured_array_schema_entry_limit(2));
    let baseline = [FieldRef::new(b"accepted", ValueRef::I64(1))];
    let mut expected = OpenArchive::new(Cursor::new(Vec::new()), options);
    expected
        .append_record(RecordRef::new(&baseline))
        .expect("append expected baseline");
    let expected = expected
        .finish()
        .expect("finish expected baseline")
        .into_inner()
        .into_inner();

    let mut actual = OpenArchive::new(Cursor::new(Vec::new()), options);
    actual
        .append_record(RecordRef::new(&baseline))
        .expect("append actual baseline");
    let values = [ValueRef::I64(1), ValueRef::I64(2)];
    let rejected = [FieldRef::new(b"items", ValueRef::Array(&values))];
    assert!(matches!(
        actual.append_record(RecordRef::new(&rejected)),
        Err(AppendError::LimitExceeded {
            resource: AppendResource::StructuredArraySchemaEntries,
            actual: 3,
            limit: 2,
        })
    ));
    assert_eq!(1, actual.record_count());
    assert_eq!(1, actual.schema_count());
    assert_eq!(
        expected,
        actual
            .finish()
            .expect("finish after rejected structured array")
            .into_inner()
            .into_inner()
    );
}

#[test]
fn log_order_merge_preserves_structured_array_records_across_schemas() {
    let mut archive = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
    let scalar_items = [ValueRef::I64(1)];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(0)),
            FieldRef::new(b"items", ValueRef::Array(&scalar_items)),
        ]))
        .expect("append scalar array schema");
    let object_fields = [FieldRef::new(b"x", ValueRef::I64(2))];
    let object_items = [ValueRef::Object(&object_fields)];
    archive
        .append_record(RecordRef::new(&[
            FieldRef::new(b"id", ValueRef::I64(1)),
            FieldRef::new(b"items", ValueRef::Array(&object_items)),
        ]))
        .expect("append object array schema");
    let bytes = archive
        .finish()
        .expect("finish log-order structured arrays")
        .into_inner()
        .into_inner();

    let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes))
        .expect("open log-order structured arrays");
    let mut extracted = Vec::new();
    extract_jsonl(
        &mut reader,
        &mut extracted,
        ExtractionOptions::new(ExtractionMode::LogOrder),
    )
    .expect("ordered extraction of structured arrays");
    assert_eq!(
        b"{\"id\":0,\"items\":[1]}\n{\"id\":1,\"items\":[{\"x\":2}]}\n",
        extracted.as_slice()
    );
}
