use std::convert::Infallible;
use std::io;
use std::io::Read;

use super::KvIrEncodedTextError;
use super::KvIrEncodedVariable;
use super::KvIrEncoding;
use super::KvIrError;
use super::KvIrErrorKind;
use super::KvIrIntegerWidth;
use super::KvIrInvalidData;
use super::KvIrItem;
use super::KvIrItemKind;
use super::KvIrLimitResource;
use super::KvIrLimits;
use super::KvIrNamespace;
use super::KvIrNodeType;
use super::KvIrOptions;
use super::KvIrOwnedEvent;
use super::KvIrOwnedEventLimitResource;
use super::KvIrOwnedEventLimits;
use super::KvIrOwnedEventMaterializer;
use super::KvIrOwnedEventNode;
use super::KvIrOwnedValue;
use super::KvIrReadError;
use super::KvIrReader;
use super::KvIrSink;
use super::KvIrStats;
use super::KvIrTruncatedContext;
use super::KvIrValueKind;

const FOUR_BYTE_ORACLE_HEX: &str =
    include_str!("../../tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
const EIGHT_BYTE_ORACLE_HEX: &str =
    include_str!("../../tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");

#[test]
fn current_magic_probe_requires_exactly_four_bytes() {
    const EIGHT_BYTE_MAGIC_FOR_TEST: [u8; 4] = [0xfd, 0x2f, 0xb5, 0x30];

    assert_eq!(
        Some(KvIrEncoding::FourByte),
        KvIrEncoding::from_magic_number(&FOUR_BYTE_MAGIC_FOR_TEST)
    );
    assert_eq!(
        Some(KvIrEncoding::EightByte),
        KvIrEncoding::from_magic_number(&EIGHT_BYTE_MAGIC_FOR_TEST)
    );
    assert_eq!(
        None,
        KvIrEncoding::from_magic_number(&FOUR_BYTE_MAGIC_FOR_TEST[..3])
    );
    assert_eq!(
        None,
        KvIrEncoding::from_magic_number(&[
            FOUR_BYTE_MAGIC_FOR_TEST[0],
            FOUR_BYTE_MAGIC_FOR_TEST[1],
            FOUR_BYTE_MAGIC_FOR_TEST[2],
            FOUR_BYTE_MAGIC_FOR_TEST[3],
            0,
        ])
    );
    assert_eq!(None, KvIrEncoding::from_magic_number(&[0; 4]));
}

#[derive(Debug, Eq, PartialEq)]
enum OwnedValue {
    Integer(i64, KvIrIntegerWidth),
    Float(u64),
    Boolean(bool),
    String(Vec<u8>),
    EncodedText {
        encoding: KvIrEncoding,
        variables: Vec<KvIrEncodedVariable>,
        dictionaries: Vec<Vec<u8>>,
        logtype: Vec<u8>,
    },
    Null,
    EmptyObject,
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedPair {
    namespace: KvIrNamespace,
    node_id: u32,
    raw: Vec<u8>,
    value: OwnedValue,
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedSchemaNode {
    namespace: KvIrNamespace,
    node_id: u32,
    parent_id: u32,
    depth: u64,
    key: Vec<u8>,
    node_type: KvIrNodeType,
    raw: Vec<u8>,
    input_offset: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedEvent {
    encoding: KvIrEncoding,
    pairs: Vec<OwnedPair>,
    utc_offset: i64,
    input_offset: u64,
}

#[derive(Default)]
struct Capture {
    encodings: Vec<KvIrEncoding>,
    versions: Vec<String>,
    metadata: Vec<Vec<u8>>,
    preambles: Vec<Vec<u8>>,
    schemas: Vec<OwnedSchemaNode>,
    events: Vec<OwnedEvent>,
    utc_changes: Vec<(i64, i64, Vec<u8>)>,
    stream_end_bytes: Vec<u64>,
    active_encoding: Option<KvIrEncoding>,
}

impl KvIrSink for Capture {
    type Error = Infallible;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        match item {
            KvIrItem::StreamStart(header) => {
                self.active_encoding = Some(header.encoding());
                self.encodings.push(header.encoding());
                self.versions.push(header.protocol_version().to_owned());
                self.metadata.push(header.metadata_json().to_vec());
                self.preambles.push(header.raw_preamble().to_vec());
            }
            KvIrItem::SchemaNode(node) => self.schemas.push(OwnedSchemaNode {
                namespace: node.namespace(),
                node_id: node.node_id(),
                parent_id: node.parent_id(),
                depth: node.depth(),
                key: node.key().to_vec(),
                node_type: node.node_type(),
                raw: node.raw_unit().to_vec(),
                input_offset: node.input_offset(),
            }),
            KvIrItem::LogEvent(event) => {
                let pairs = event
                    .pairs()
                    .map(|pair| {
                        let value = pair.value();
                        let owned = match value.kind() {
                            KvIrValueKind::Integer(integer) => {
                                OwnedValue::Integer(integer.value(), integer.width())
                            }
                            KvIrValueKind::Float { bits } => OwnedValue::Float(bits),
                            KvIrValueKind::Boolean(value) => OwnedValue::Boolean(value),
                            KvIrValueKind::String(value) => OwnedValue::String(value.to_vec()),
                            KvIrValueKind::EncodedText(value) => OwnedValue::EncodedText {
                                encoding: value.encoding(),
                                variables: value.encoded_variables().collect(),
                                dictionaries: value
                                    .dictionary_variables()
                                    .map(<[u8]>::to_vec)
                                    .collect(),
                                logtype: value.logtype().to_vec(),
                            },
                            KvIrValueKind::Null => OwnedValue::Null,
                            KvIrValueKind::EmptyObject => OwnedValue::EmptyObject,
                        };
                        OwnedPair {
                            namespace: pair.namespace(),
                            node_id: pair.node_id(),
                            raw: value.raw_packet().to_vec(),
                            value: owned,
                        }
                    })
                    .collect();
                self.events.push(OwnedEvent {
                    encoding: self.active_encoding.expect("stream start precedes events"),
                    pairs,
                    utc_offset: event.utc_offset_millis(),
                    input_offset: event.input_offset(),
                });
            }
            KvIrItem::UtcOffsetChange(offset) => self.utc_changes.push((
                offset.old_offset_millis(),
                offset.new_offset_millis(),
                offset.raw_unit().to_vec(),
            )),
            KvIrItem::StreamEnd(end) => self.stream_end_bytes.push(end.stream_bytes()),
        }
        Ok(())
    }
}

struct Chunked<'a> {
    bytes: &'a [u8],
    position: usize,
    chunk: usize,
    interrupt_next: bool,
}

impl<'a> Chunked<'a> {
    const fn new(bytes: &'a [u8], chunk: usize) -> Self {
        Self {
            bytes,
            position: 0,
            chunk,
            interrupt_next: true,
        }
    }
}

impl Read for Chunked<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.interrupt_next {
            self.interrupt_next = false;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        self.interrupt_next = true;
        if self.position == self.bytes.len() {
            return Ok(0);
        }
        let count = self
            .chunk
            .min(output.len())
            .min(self.bytes.len() - self.position);
        output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

fn decode_hex(source: &str) -> Vec<u8> {
    let digits: Vec<u8> = source.bytes().filter(u8::is_ascii_hexdigit).collect();
    let (pairs, remainder) = digits.as_chunks::<2>();
    assert!(remainder.is_empty(), "hex fixture has an even length");
    pairs
        .iter()
        .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
        .collect()
}

const fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        b'A'..=b'F' => value - b'A' + 10,
        _ => panic!("non-hex fixture byte"),
    }
}

fn decode<R: Read>(input: R, options: KvIrOptions) -> (KvIrStats, Capture) {
    let mut reader = KvIrReader::new(input, options);
    let mut capture = Capture::default();
    let stats = reader
        .read_to_end(&mut capture)
        .expect("capture sink is infallible and fixture is valid");
    (stats, capture)
}

fn assert_cpp_oracle(encoding: KvIrEncoding, bytes: &[u8], chunk: usize) {
    let (stats, capture) = decode(Chunked::new(bytes, chunk), KvIrOptions::default());
    assert_eq!(u64::try_from(bytes.len()).unwrap(), stats.input_bytes());
    assert_eq!(1, stats.streams());
    assert_eq!(10, stats.units());
    assert_eq!(7, stats.schema_nodes());
    assert_eq!(1, stats.log_events());
    assert_eq!(1, stats.utc_offset_changes());
    assert_eq!(vec![encoding], capture.encodings);
    assert_eq!(vec!["0.1.0"], capture.versions);
    assert_eq!(216, capture.preambles[0].len());
    assert_eq!(b'{', capture.metadata[0][0]);
    assert!(
        capture.metadata[0]
            .windows(b"rust-kv-ir-reader-v1".len())
            .any(|window| window == b"rust-kv-ir-reader-v1")
    );
    assert_eq!(
        vec![(0, 3_600_000, vec![0x3f, 0, 0, 0, 0, 0, 0x36, 0xee, 0x80])],
        capture.utc_changes
    );
    assert_eq!(
        vec![u64::try_from(bytes.len()).unwrap()],
        capture.stream_end_bytes
    );

    assert_cpp_schema(&capture.schemas);
    assert_cpp_event(encoding, &capture.events[0]);
}

fn assert_cpp_schema(actual_schema: &[OwnedSchemaNode]) {
    let expected_schema = [
        (
            KvIrNamespace::AutoGenerated,
            1,
            b"level".as_slice(),
            KvIrNodeType::String,
        ),
        (
            KvIrNamespace::AutoGenerated,
            2,
            b"seq".as_slice(),
            KvIrNodeType::Integer,
        ),
        (
            KvIrNamespace::UserGenerated,
            1,
            b"empty".as_slice(),
            KvIrNodeType::Object,
        ),
        (
            KvIrNamespace::UserGenerated,
            2,
            b"message".as_slice(),
            KvIrNodeType::String,
        ),
        (
            KvIrNamespace::UserGenerated,
            3,
            b"none".as_slice(),
            KvIrNodeType::Object,
        ),
        (
            KvIrNamespace::UserGenerated,
            4,
            b"ok".as_slice(),
            KvIrNodeType::Boolean,
        ),
        (
            KvIrNamespace::UserGenerated,
            5,
            b"ratio".as_slice(),
            KvIrNodeType::Float,
        ),
    ];
    assert_eq!(expected_schema.len(), actual_schema.len());
    for (actual, (namespace, node_id, key, node_type)) in actual_schema.iter().zip(expected_schema)
    {
        assert_eq!(namespace, actual.namespace);
        assert_eq!(node_id, actual.node_id);
        assert_eq!(0, actual.parent_id);
        assert_eq!(1, actual.depth);
        assert_eq!(key, actual.key);
        assert_eq!(node_type, actual.node_type);
        assert_eq!(node_type_tag(node_type), actual.raw[0]);
    }
}

fn assert_cpp_event(encoding: KvIrEncoding, event: &OwnedEvent) {
    assert_eq!(encoding, event.encoding);
    assert_eq!(3_600_000, event.utc_offset);
    assert_eq!(7, event.pairs.len());
    assert_eq!(OwnedValue::String(b"info".to_vec()), event.pairs[0].value);
    assert_eq!(
        OwnedValue::Integer(7, KvIrIntegerWidth::One),
        event.pairs[1].value
    );
    assert_eq!(OwnedValue::EmptyObject, event.pairs[2].value);
    assert_eq!(OwnedValue::Null, event.pairs[4].value);
    assert_eq!(OwnedValue::Boolean(true), event.pairs[5].value);
    assert_eq!(OwnedValue::Float(1.25_f64.to_bits()), event.pairs[6].value);
    let expected_variable = match encoding {
        KvIrEncoding::FourByte => KvIrEncodedVariable::FourByte(42),
        KvIrEncoding::EightByte => KvIrEncodedVariable::EightByte(42),
    };
    assert_eq!(
        OwnedValue::EncodedText {
            encoding,
            variables: vec![expected_variable],
            dictionaries: vec![],
            logtype: b"task \x11 done".to_vec(),
        },
        event.pairs[3].value
    );
    assert_eq!(0x56, event.pairs[6].raw[0]);
    assert_eq!(&1.25_f64.to_bits().to_be_bytes(), &event.pairs[6].raw[1..]);
}

const fn node_type_tag(node_type: KvIrNodeType) -> u8 {
    match node_type {
        KvIrNodeType::Integer => 0x71,
        KvIrNodeType::Float => 0x72,
        KvIrNodeType::Boolean => 0x73,
        KvIrNodeType::String => 0x74,
        KvIrNodeType::UnstructuredArray => 0x75,
        KvIrNodeType::Object => 0x76,
    }
}

#[test]
fn canonical_cpp_four_byte_stream_decodes_from_one_byte_chunks() {
    let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
    assert_eq!(345, bytes.len());
    assert_cpp_oracle(KvIrEncoding::FourByte, &bytes, 1);
}

#[test]
fn canonical_cpp_eight_byte_stream_decodes_across_buffer_boundaries() {
    let bytes = decode_hex(EIGHT_BYTE_ORACLE_HEX);
    assert_eq!(349, bytes.len());
    assert_cpp_oracle(KvIrEncoding::EightByte, &bytes, 7);
}

#[test]
fn log_event_resolves_schema_without_a_consumer_owned_copy() {
    let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut saw_event = false;
    let mut owned_event = None;
    reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                saw_event = true;
                assert_eq!(3, event.schema_node_count(KvIrNamespace::AutoGenerated));
                assert_eq!(6, event.schema_node_count(KvIrNamespace::UserGenerated));

                let root = event
                    .schema_node(KvIrNamespace::UserGenerated, 0)
                    .expect("namespace root exists");
                assert_eq!(None, root.parent_id());
                assert_eq!(b"", root.key());
                assert_eq!(KvIrNodeType::Object, root.node_type());

                for pair in event.pairs() {
                    let node = event
                        .schema_node(pair.namespace(), pair.node_id())
                        .expect("every validated pair resolves to a schema node");
                    assert_eq!(pair.namespace(), node.namespace());
                    assert_eq!(pair.node_id(), node.node_id());
                    assert!(node.parent_id().is_some());
                }
                let message = event
                    .schema_node(KvIrNamespace::UserGenerated, 2)
                    .expect("message schema entry exists");
                assert_eq!(b"message", message.key());
                assert_eq!(1, message.depth());
                let encoded = match event.pair(3).expect("message pair exists").value().kind() {
                    KvIrValueKind::EncodedText(encoded) => encoded,
                    value => panic!("message pair should be encoded text, got {value:?}"),
                };
                let mut decoded = b"prefix:".to_vec();
                let decoded_range = encoded
                    .append_decoded_to(&mut decoded, 64)
                    .expect("canonical encoded text reconstructs");
                assert_eq!(b"task 42 done", &decoded[decoded_range]);
                let before_error = decoded.clone();
                assert!(matches!(
                    encoded.append_decoded_to(&mut decoded, 1),
                    Err(KvIrEncodedTextError::Limit { .. })
                ));
                assert_eq!(before_error, decoded, "failed reconstruction is atomic");
                assert!(
                    event
                        .schema_node(KvIrNamespace::UserGenerated, u32::MAX)
                        .is_none()
                );
                owned_event = Some(
                    KvIrOwnedEvent::materialize(event, KvIrOwnedEventLimits::default())
                        .expect("canonical event materializes"),
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("canonical stream is valid");
    assert!(saw_event);
    drop(reader);

    let owned = owned_event.expect("canonical stream contains one event");
    let auto = owned.nodes(KvIrNamespace::AutoGenerated);
    let user = owned.nodes(KvIrNamespace::UserGenerated);
    assert_eq!(2, auto.len());
    assert_eq!(5, user.len());
    assert_eq!(b"level", owned.resolve(auto[0].key_span()).unwrap());
    assert_eq!(b"seq", owned.resolve(auto[1].key_span()).unwrap());
    let KvIrOwnedValue::String(level) = auto[0].value() else {
        panic!("level should be an owned string")
    };
    assert_eq!(b"info", owned.resolve(level).unwrap());
    assert_eq!(KvIrOwnedValue::Integer(7), auto[1].value());

    let user_keys: Vec<&[u8]> = user
        .iter()
        .map(|node| owned.resolve(node.key_span()).unwrap())
        .collect();
    assert_eq!(
        [
            b"empty".as_slice(),
            b"message".as_slice(),
            b"none".as_slice(),
            b"ok".as_slice(),
            b"ratio".as_slice(),
        ],
        user_keys.as_slice()
    );
    assert_eq!(KvIrOwnedValue::EmptyObject, user[0].value());
    let KvIrOwnedValue::String(message) = user[1].value() else {
        panic!("message should be reconstructed once into the owned arena")
    };
    assert_eq!(b"task 42 done", owned.resolve(message).unwrap());
    assert_eq!(KvIrOwnedValue::Null, user[2].value());
    assert_eq!(KvIrOwnedValue::Boolean(true), user[3].value());
    assert_eq!(
        KvIrOwnedValue::Float {
            bits: 1.25_f64.to_bits()
        },
        user[4].value()
    );
}

#[test]
fn owned_event_arena_limit_is_exact_and_structured_near_boundary() {
    let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let materialize = |limits: KvIrOwnedEventLimits| {
        let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
        let mut materialized = None;
        reader
            .read_to_end(&mut |item: KvIrItem<'_>| {
                if let KvIrItem::LogEvent(event) = item {
                    assert!(materialized.is_none(), "fixture contains exactly one event");
                    materialized = Some(KvIrOwnedEvent::materialize(event, limits));
                }
                Ok::<(), Infallible>(())
            })
            .expect("canonical stream is valid");
        materialized.expect("canonical stream contains an event")
    };

    let expected = materialize(KvIrOwnedEventLimits::default())
        .expect("canonical event materializes with default limits");
    assert_eq!(47, expected.arena().len());
    let at_limit = materialize(KvIrOwnedEventLimits::new().with_max_arena_bytes(47))
        .expect("arena whose final byte reaches the limit materializes");
    assert_eq!(
        expected.nodes(KvIrNamespace::AutoGenerated),
        at_limit.nodes(KvIrNamespace::AutoGenerated)
    );
    assert_eq!(
        expected.nodes(KvIrNamespace::UserGenerated),
        at_limit.nodes(KvIrNamespace::UserGenerated)
    );
    assert_eq!(expected.arena(), at_limit.arena());

    let error = materialize(KvIrOwnedEventLimits::new().with_max_arena_bytes(46))
        .expect_err("one byte below the required arena size reaches the structured limit");
    assert!(matches!(
        error,
        super::KvIrOwnedEventError::Limit {
            resource: KvIrOwnedEventLimitResource::ArenaBytes,
            actual: 47,
            limit: 46
        }
    ));
}

#[test]
fn owned_event_namespace_views_preserve_an_empty_trailing_namespace() {
    let mut bytes = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    bytes.pop();
    bytes.extend_from_slice(&[
        0x71, 0x60, 0xff, 0x41, 5, b'c', b'o', b'u', b'n', b't', // auto integer
        0x65, 0xfe, 0x51, 42, 0x5e, // auto node 1 and event terminator
        0,
    ]);

    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut owned = None;
    reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                owned = Some(
                    KvIrOwnedEvent::materialize(event, KvIrOwnedEventLimits::default())
                        .expect("auto-only event materializes"),
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("auto-only stream is valid");

    let owned = owned.expect("auto-only stream contains an event");
    let auto = owned.nodes(KvIrNamespace::AutoGenerated);
    let user = owned.nodes(KvIrNamespace::UserGenerated);
    assert_eq!(1, auto.len());
    assert_eq!(0, user.len());
    assert_eq!(b"count", owned.resolve(auto[0].key_span()).unwrap());
    assert_eq!(KvIrOwnedValue::Integer(42), auto[0].value());
}

#[test]
fn owned_event_is_dfs_ordered_even_when_pair_wire_order_differs() {
    let mut bytes = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    bytes.pop();
    append_user_schema_node(&mut bytes, KvIrNodeType::Object, 0, b"a");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 0, b"z");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 1, b"c");
    bytes.extend_from_slice(&[
        0x65, 3, // user node a.c appears first in the event
        0x65, 2, // root sibling z appears second
        0x51, 30, // a.c value
        0x51, 20, // z value
        0,
    ]);

    let mut materializer =
        KvIrOwnedEventMaterializer::new().expect("materialization scratch initializes");
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut owned = None;
    reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                owned = Some(
                    materializer
                        .materialize(event, KvIrOwnedEventLimits::default())
                        .expect("nested event materializes"),
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("nested stream is valid");
    let owned = owned.expect("nested stream contains an event");
    let auto = owned.nodes(KvIrNamespace::AutoGenerated);
    let nodes = owned.nodes(KvIrNamespace::UserGenerated);
    assert_eq!(0, auto.len());
    assert_eq!(3, nodes.len());
    let keys: Vec<&[u8]> = nodes
        .iter()
        .map(|node| owned.resolve(node.key_span()).unwrap())
        .collect();
    assert_eq!(
        [b"a".as_slice(), b"c".as_slice(), b"z".as_slice()],
        keys.as_slice()
    );
    assert_eq!(
        [1, 2, 1],
        nodes
            .iter()
            .copied()
            .map(KvIrOwnedEventNode::depth)
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(KvIrOwnedValue::Object, nodes[0].value());
    assert_eq!(KvIrOwnedValue::Integer(30), nodes[1].value());
    assert_eq!(KvIrOwnedValue::Integer(20), nodes[2].value());

    let mut limited_reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut saw_limit = false;
    limited_reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                let error = materializer
                    .materialize(
                        event,
                        KvIrOwnedEventLimits::new().with_max_materialized_nodes(2),
                    )
                    .expect_err("three selected nodes exceed the limit");
                saw_limit = matches!(
                    error,
                    super::KvIrOwnedEventError::Limit {
                        resource: KvIrOwnedEventLimitResource::MaterializedNodes,
                        actual: 3,
                        limit: 2
                    }
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("materialization failure does not invalidate reader callbacks");
    assert!(saw_limit);

    let mut recovered_reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut recovered = None;
    recovered_reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                recovered = Some(
                    materializer
                        .materialize(event, KvIrOwnedEventLimits::default())
                        .expect("scratch remains reusable after an error"),
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("reader remains valid after scratch reuse");
    let recovered = recovered.expect("recovered stream contains an event");
    assert_eq!(nodes, recovered.nodes(KvIrNamespace::UserGenerated));
    assert_eq!(owned.arena(), recovered.arena());
}

#[test]
fn owned_event_preserves_nested_sibling_schema_order_for_interleaved_wire_order() {
    let mut bytes = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    bytes.pop();
    append_user_schema_node(&mut bytes, KvIrNodeType::Object, 0, b"a");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 0, b"z");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 1, b"c");
    append_user_schema_node(&mut bytes, KvIrNodeType::Object, 1, b"b");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 4, b"y");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 1, b"d");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 0, b"m");
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 4, b"x");
    bytes.extend_from_slice(&[
        0x65, 5, // a.b.y
        0x65, 7, // m
        0x65, 6, // a.d
        0x65, 8, // a.b.x
        0x65, 2, // z
        0x65, 3, // a.c
        0x51, 50, // a.b.y value
        0x51, 70, // m value
        0x51, 60, // a.d value
        0x51, 80, // a.b.x value
        0x51, 20, // z value
        0x51, 30, // a.c value
        0,
    ]);

    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut owned = None;
    reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                owned = Some(
                    KvIrOwnedEvent::materialize(event, KvIrOwnedEventLimits::default())
                        .expect("interleaved nested event materializes"),
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("interleaved nested stream is valid");

    let owned = owned.expect("interleaved nested stream contains an event");
    assert_eq!(owned.nodes(KvIrNamespace::AutoGenerated), []);
    let nodes = owned.nodes(KvIrNamespace::UserGenerated);
    let keys: Vec<&[u8]> = nodes
        .iter()
        .map(|node| owned.resolve(node.key_span()).unwrap())
        .collect();
    assert_eq!(
        [
            b"a".as_slice(),
            b"c".as_slice(),
            b"b".as_slice(),
            b"y".as_slice(),
            b"x".as_slice(),
            b"d".as_slice(),
            b"z".as_slice(),
            b"m".as_slice(),
        ],
        keys.as_slice()
    );
    assert_eq!(
        [1, 2, 2, 3, 3, 2, 1, 1],
        nodes
            .iter()
            .copied()
            .map(KvIrOwnedEventNode::depth)
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(
        [
            KvIrOwnedValue::Object,
            KvIrOwnedValue::Integer(30),
            KvIrOwnedValue::Object,
            KvIrOwnedValue::Integer(50),
            KvIrOwnedValue::Integer(80),
            KvIrOwnedValue::Integer(60),
            KvIrOwnedValue::Integer(20),
            KvIrOwnedValue::Integer(70),
        ],
        nodes
            .iter()
            .copied()
            .map(KvIrOwnedEventNode::value)
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(b"acbyxdzm", owned.arena());
}

#[test]
fn owned_event_large_index_preserves_schema_order_for_reverse_wire_order() {
    const NODE_COUNT: u8 = 40;
    let mut bytes = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    bytes.pop();
    for index in 0..NODE_COUNT {
        append_user_schema_node(
            &mut bytes,
            KvIrNodeType::Integer,
            0,
            &u32::from(index).to_be_bytes(),
        );
    }
    for node_id in (1..=NODE_COUNT).rev() {
        bytes.extend_from_slice(&[0x65, node_id]);
    }
    for node_id in (1..=NODE_COUNT).rev() {
        bytes.extend_from_slice(&[0x51, node_id]);
    }
    bytes.push(0);

    let mut materializer =
        KvIrOwnedEventMaterializer::new().expect("materialization scratch initializes");
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut owned = None;
    reader
        .read_to_end(&mut |item: KvIrItem<'_>| {
            if let KvIrItem::LogEvent(event) = item {
                owned = Some(
                    materializer
                        .materialize(event, KvIrOwnedEventLimits::default())
                        .expect("wide reverse-order event materializes"),
                );
            }
            Ok::<(), Infallible>(())
        })
        .expect("wide event stream is valid");

    let owned = owned.expect("wide stream contains an event");
    let nodes = owned.nodes(KvIrNamespace::UserGenerated);
    assert_eq!(usize::from(NODE_COUNT), nodes.len());
    for (index, node) in nodes.iter().copied().enumerate() {
        assert_eq!(
            u32::try_from(index).unwrap().to_be_bytes(),
            owned.resolve(node.key_span()).unwrap()
        );
        assert_eq!(
            KvIrOwnedValue::Integer(i64::try_from(index + 1).unwrap()),
            node.value()
        );
    }
}

#[test]
fn immediately_concatenated_cpp_streams_reset_schema_and_offsets() {
    let four = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let eight = decode_hex(EIGHT_BYTE_ORACLE_HEX);
    let mut bytes = four.clone();
    bytes.extend_from_slice(&eight);

    let (stats, capture) = decode(Chunked::new(&bytes, 3), KvIrOptions::default());
    assert_eq!(2, stats.streams());
    assert_eq!(20, stats.units());
    assert_eq!(14, stats.schema_nodes());
    assert_eq!(2, stats.log_events());
    assert_eq!(
        vec![KvIrEncoding::FourByte, KvIrEncoding::EightByte],
        capture.encodings
    );
    assert_eq!(
        u64::try_from(four.len() + 225).unwrap(),
        capture.schemas[7].input_offset
    );
    assert_eq!(vec![345, 349], capture.stream_end_bytes);
}

#[test]
fn incremental_decode_emits_exactly_one_canonical_item_per_call() {
    let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let expected = [
        KvIrItemKind::StreamStart,
        KvIrItemKind::UtcOffsetChange,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::SchemaNode,
        KvIrItemKind::LogEvent,
        KvIrItemKind::StreamEnd,
    ];

    for expected_kind in expected {
        let mut callback_count = 0_usize;
        let returned_kind = reader
            .read_next_item(&mut |item: KvIrItem<'_>| {
                callback_count += 1;
                assert_eq!(expected_kind, item.kind());
                Ok::<(), Infallible>(())
            })
            .expect("canonical item is valid");
        assert_eq!(Some(expected_kind), returned_kind);
        assert_eq!(1, callback_count);
    }

    for _ in 0..2 {
        let mut callback_count = 0_usize;
        let returned_kind = reader
            .read_next_item(&mut |_item: KvIrItem<'_>| {
                callback_count += 1;
                Ok::<(), Infallible>(())
            })
            .expect("EOF remains successful");
        assert_eq!(None, returned_kind);
        assert_eq!(0, callback_count);
    }
    assert_eq!(1, reader.stats().streams());
    assert_eq!(10, reader.stats().units());
}

#[test]
fn incremental_decode_can_stop_at_one_stream_and_continue_the_next() {
    let four = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let eight = decode_hex(EIGHT_BYTE_ORACLE_HEX);
    let mut bytes = four;
    bytes.extend_from_slice(&eight);
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());

    loop {
        let kind = reader
            .read_next_item(&mut |_item: KvIrItem<'_>| Ok::<(), Infallible>(()))
            .expect("first stream is valid")
            .expect("first stream has another item");
        if kind == KvIrItemKind::StreamEnd {
            break;
        }
    }
    assert_eq!(1, reader.stats().streams());
    assert_eq!(10, reader.stats().units());

    let next = reader
        .read_next_item(&mut |item: KvIrItem<'_>| {
            assert_eq!(KvIrItemKind::StreamStart, item.kind());
            Ok::<(), Infallible>(())
        })
        .expect("second stream preamble is valid");
    assert_eq!(Some(KvIrItemKind::StreamStart), next);
    assert_eq!(2, reader.stats().streams());

    let stats = reader
        .read_to_end(&mut |_item: KvIrItem<'_>| Ok::<(), Infallible>(()))
        .expect("second stream is valid");
    assert_eq!(2, stats.streams());
    assert_eq!(20, stats.units());
}

#[test]
fn incremental_decode_resumes_after_committed_sink_errors() {
    let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());

    let start_error = reader
        .read_next_item(&mut |item: KvIrItem<'_>| {
            assert_eq!(KvIrItemKind::StreamStart, item.kind());
            Err::<(), _>("stop at preamble")
        })
        .expect_err("sink rejects the stream start");
    assert!(matches!(
        start_error,
        KvIrReadError::Sink {
            stream_index: 0,
            unit_index: None,
            source: "stop at preamble"
        }
    ));
    assert_eq!(1, reader.stats().streams());
    assert_eq!(0, reader.stats().units());

    for expected_kind in
        std::iter::once(KvIrItemKind::UtcOffsetChange).chain([KvIrItemKind::SchemaNode; 7])
    {
        let returned = reader
            .read_next_item(&mut |item: KvIrItem<'_>| {
                assert_eq!(expected_kind, item.kind());
                Ok::<(), Infallible>(())
            })
            .expect("item after a sink error is valid");
        assert_eq!(Some(expected_kind), returned);
    }
    assert_eq!(8, reader.stats().units());

    let event_error = reader
        .read_next_item(&mut |item: KvIrItem<'_>| {
            assert_eq!(KvIrItemKind::LogEvent, item.kind());
            Err::<(), _>("stop at event")
        })
        .expect_err("sink rejects the event");
    assert!(matches!(
        event_error,
        KvIrReadError::Sink {
            stream_index: 0,
            unit_index: Some(8),
            source: "stop at event"
        }
    ));
    assert_eq!(9, reader.stats().units());
    assert_eq!(1, reader.stats().log_events());

    let next = reader
        .read_next_item(&mut |item: KvIrItem<'_>| {
            assert_eq!(KvIrItemKind::StreamEnd, item.kind());
            Ok::<(), Infallible>(())
        })
        .expect("stream end remains after rejected event");
    assert_eq!(Some(KvIrItemKind::StreamEnd), next);
    assert_eq!(10, reader.stats().units());
}

#[test]
fn every_truncated_oracle_prefix_stops_with_structured_context() {
    let bytes = decode_hex(EIGHT_BYTE_ORACLE_HEX);
    for cut in 0..bytes.len() {
        let mut reader = KvIrReader::new(&bytes[..cut], KvIrOptions::default());
        let mut capture = Capture::default();
        let error = reader
            .read_to_end(&mut capture)
            .expect_err("every strict prefix omits required bytes or the end marker");
        let error = unwrap_reader_error(error);
        assert!(matches!(error.kind(), KvIrErrorKind::Truncated { .. }));
        assert_eq!(u64::try_from(cut).unwrap(), error.input_offset());
    }
}

#[test]
fn magic_metadata_and_legacy_dialect_errors_are_distinct() {
    let invalid_magic = [0xfd, 0x2f, 0xb5, 0x31];
    assert_reader_kind(&invalid_magic, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::InvalidMagicNumber(_))
        )
    });

    let invalid_json = stream_with_metadata(br#"{"VERSION":"0.1.0",}"#);
    assert_reader_kind(&invalid_json, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::InvalidMetadataJson)
        )
    });

    let legacy = stream_with_metadata(br#"{"VERSION":"0.0.2"}"#);
    assert_reader_kind(&legacy, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::LegacyUnstructuredVersion(version))
                if version == "0.0.2"
        )
    });

    let user_metadata = stream_with_metadata(br#"{"VERSION":"0.1.0","USER_DEFINED_METADATA":[]}"#);
    assert_reader_kind(&user_metadata, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::UserDefinedMetadataMustBeObject)
        )
    });
}

#[test]
fn declared_and_semantic_limits_fail_before_unbounded_growth() {
    let bytes = decode_hex(FOUR_BYTE_ORACLE_HEX);
    let cases = [
        (
            KvIrLimits::new().with_max_metadata_bytes(208),
            KvIrLimitResource::MetadataBytes,
        ),
        (
            KvIrLimits::new().with_max_stream_bytes(344),
            KvIrLimitResource::StreamBytes,
        ),
        (
            KvIrLimits::new().with_max_unit_bytes(8),
            KvIrLimitResource::UnitBytes,
        ),
        (
            KvIrLimits::new().with_max_units_per_stream(9),
            KvIrLimitResource::UnitsPerStream,
        ),
        (
            KvIrLimits::new().with_max_schema_nodes_per_namespace(4),
            KvIrLimitResource::SchemaNodesPerNamespace,
        ),
        (
            KvIrLimits::new().with_max_nesting_depth(0),
            KvIrLimitResource::NestingDepth,
        ),
        (
            KvIrLimits::new().with_max_metadata_values(0),
            KvIrLimitResource::MetadataValues,
        ),
        (
            KvIrLimits::new().with_max_values_per_event(6),
            KvIrLimitResource::ValuesPerEvent,
        ),
        (
            KvIrLimits::new().with_max_scalar_bytes(4),
            KvIrLimitResource::ScalarBytes,
        ),
        (
            KvIrLimits::new().with_max_encoded_components_per_value(0),
            KvIrLimitResource::EncodedComponentsPerValue,
        ),
    ];
    for (limits, expected_resource) in cases {
        assert_limit(&bytes, limits, expected_resource);
    }

    let mut concatenated = bytes.clone();
    concatenated.extend_from_slice(&bytes);
    assert_limit(
        &concatenated,
        KvIrLimits::new().with_max_streams(1),
        KvIrLimitResource::Streams,
    );

    let mut nested_schema = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    nested_schema.pop();
    nested_schema.extend_from_slice(&[
        0x76, 0x60, 0, 0x41, 1, b'a', // user object at depth one
        0x71, 0x60, 1, 0x41, 1, b'b', // child at depth two
        0,
    ]);
    assert_limit(
        &nested_schema,
        KvIrLimits::new().with_max_nesting_depth(1),
        KvIrLimitResource::NestingDepth,
    );
}

fn assert_limit(input: &[u8], limits: KvIrLimits, expected_resource: KvIrLimitResource) {
    let mut reader = KvIrReader::new(input, KvIrOptions::new().with_limits(limits));
    let mut capture = Capture::default();
    let error = reader
        .read_to_end(&mut capture)
        .expect_err("limit must fail");
    let error = unwrap_reader_error(error);
    let KvIrErrorKind::Limit(limit) = error.kind() else {
        panic!("expected limit error, got {error:?}");
    };
    assert_eq!(expected_resource, limit.resource());
    assert!(limit.actual() > limit.limit());
}

#[test]
fn malformed_units_stop_without_becoming_a_following_event() {
    let mut bytes = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    bytes.pop();
    bytes.extend_from_slice(&[0x71, 0x60, 0x05, 0x41, 0x01, b'x', 0]);
    assert_reader_kind(&bytes, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::MissingParentNode {
                namespace: KvIrNamespace::UserGenerated,
                node_id: 5
            })
        )
    });

    let mut unknown = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    let last = unknown.last_mut().expect("stream has EOF");
    *last = 0xaa;
    assert_reader_kind(&unknown, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::InvalidUnitTag(0xaa))
        )
    });
}

#[test]
fn wide_schema_insertion_uses_a_near_linear_number_of_index_probes() {
    const NODE_COUNT: u32 = 20_000;
    let bytes = wide_user_integer_schema(NODE_COUNT);
    let mut reader = KvIrReader::new(bytes.as_slice(), KvIrOptions::default());
    let mut capture = Capture::default();
    let stats = reader
        .read_to_end(&mut capture)
        .expect("wide unique schema is valid");

    assert_eq!(u64::from(NODE_COUNT), stats.schema_nodes());
    assert_eq!(usize::try_from(NODE_COUNT).unwrap(), capture.schemas.len());
    assert!(
        reader.schema_index_probes() < u64::from(NODE_COUNT) * 64,
        "open-addressed lookup should stay linear under randomized hashing"
    );
}

#[test]
fn duplicate_schema_detection_remains_exact_after_a_wide_schema() {
    const NODE_COUNT: u32 = 4_096;
    let mut bytes = wide_user_integer_schema(NODE_COUNT);
    bytes.pop();
    append_user_schema_node(&mut bytes, KvIrNodeType::Integer, 0, &0_u32.to_be_bytes());
    bytes.push(0);

    assert_reader_kind(&bytes, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::DuplicateSchemaNode)
        )
    });
}

#[test]
fn event_generation_markers_preserve_duplicate_and_ancestor_errors() {
    let mut duplicate_node = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    duplicate_node.pop();
    append_user_schema_node(&mut duplicate_node, KvIrNodeType::Integer, 0, b"value");
    duplicate_node.extend_from_slice(&[0x65, 1, 0x65, 1, 0x51, 7, 0x51, 8, 0]);
    assert_reader_kind(&duplicate_node, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::DuplicateEventNode {
                namespace: KvIrNamespace::UserGenerated,
                node_id: 1
            })
        )
    });

    let mut duplicate_sibling = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    duplicate_sibling.pop();
    append_user_schema_node(&mut duplicate_sibling, KvIrNodeType::Integer, 0, b"same");
    append_user_schema_node(&mut duplicate_sibling, KvIrNodeType::String, 0, b"same");
    duplicate_sibling.extend_from_slice(&[0x65, 1, 0x65, 2, 0x51, 7, 0x41, 1, b'v', 0]);
    assert_reader_kind(&duplicate_sibling, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::DuplicateSiblingKey)
        )
    });

    let mut selected_ancestor = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    selected_ancestor.pop();
    append_user_schema_node(&mut selected_ancestor, KvIrNodeType::Object, 0, b"parent");
    append_user_schema_node(&mut selected_ancestor, KvIrNodeType::Integer, 1, b"child");
    selected_ancestor.extend_from_slice(&[0x65, 1, 0x65, 2, 0x5e, 0x51, 7, 0]);
    assert_reader_kind(&selected_ancestor, |kind| {
        matches!(
            kind,
            KvIrErrorKind::Invalid(KvIrInvalidData::ObjectValueHasDescendant)
        )
    });
}

fn wide_user_integer_schema(node_count: u32) -> Vec<u8> {
    let mut bytes = stream_with_metadata(br#"{"VERSION":"0.1.0"}"#);
    bytes.pop();
    for node_index in 0..node_count {
        append_user_schema_node(
            &mut bytes,
            KvIrNodeType::Integer,
            0,
            &node_index.to_be_bytes(),
        );
    }
    bytes.push(0);
    bytes
}

fn append_user_schema_node(
    bytes: &mut Vec<u8>,
    node_type: KvIrNodeType,
    parent_id: i8,
    key: &[u8],
) {
    let key_length = u8::try_from(key.len()).expect("test schema keys fit one-byte strings");
    bytes.extend_from_slice(&[
        node_type_tag(node_type),
        0x60,
        parent_id.to_be_bytes()[0],
        0x41,
        key_length,
    ]);
    bytes.extend_from_slice(key);
}

fn stream_with_metadata(metadata: &[u8]) -> Vec<u8> {
    assert!(u8::try_from(metadata.len()).is_ok());
    let mut bytes = FOUR_BYTE_MAGIC_FOR_TEST.to_vec();
    bytes.extend_from_slice(&[0x01, 0x11, u8::try_from(metadata.len()).unwrap()]);
    bytes.extend_from_slice(metadata);
    bytes.push(0);
    bytes
}

const FOUR_BYTE_MAGIC_FOR_TEST: [u8; 4] = [0xfd, 0x2f, 0xb5, 0x29];

fn assert_reader_kind(input: &[u8], predicate: impl FnOnce(&KvIrErrorKind) -> bool) {
    let mut reader = KvIrReader::new(input, KvIrOptions::default());
    let mut capture = Capture::default();
    let error = reader
        .read_to_end(&mut capture)
        .expect_err("input must be rejected");
    let error = unwrap_reader_error(error);
    assert!(predicate(error.kind()), "unexpected error: {error:?}");
}

fn unwrap_reader_error(error: KvIrReadError<Infallible>) -> KvIrError {
    match error {
        KvIrReadError::Reader(error) => error,
        KvIrReadError::Sink { source, .. } => match source {},
    }
}

#[test]
fn empty_and_partial_magic_are_truncation_not_clean_eof() {
    for input in [&b""[..], &b"\xfd"[..], &b"\xfd\x2f\xb5"[..]] {
        assert_reader_kind(input, |kind| {
            matches!(
                kind,
                KvIrErrorKind::Truncated {
                    context: KvIrTruncatedContext::MagicNumber,
                    ..
                }
            )
        });
    }
}
