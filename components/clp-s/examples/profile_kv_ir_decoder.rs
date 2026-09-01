//! Profiles the incremental KV-IR decoder without materializing owned maps.

use std::convert::Infallible;
use std::env;
use std::fs::File;
use std::path::PathBuf;

use clp_s::ingest::KvIrEncodedVariable;
use clp_s::ingest::KvIrEncoding;
use clp_s::ingest::KvIrIntegerWidth;
use clp_s::ingest::KvIrItem;
use clp_s::ingest::KvIrItemKind;
use clp_s::ingest::KvIrNamespace;
use clp_s::ingest::KvIrNodeType;
use clp_s::ingest::KvIrOptions;
use clp_s::ingest::KvIrReader;
use clp_s::ingest::KvIrSink;
use clp_s::ingest::KvIrValueKind;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let input_path = PathBuf::from(arguments.next().ok_or("missing KV-IR stream path")?);
    let validation = match arguments.next() {
        None => false,
        Some(argument) if argument == "--validate" => true,
        Some(_) => return Err("expected --validate after the stream path".into()),
    };
    if arguments.next().is_some() {
        return Err("usage: profile_kv_ir_decoder STREAM [--validate]".into());
    }

    let file_bytes = input_path.metadata()?.len();
    let mut reader = KvIrReader::new(File::open(input_path)?, KvIrOptions::default());
    let mut sink = ProfileSink::new(validation);
    let mut item_counts = ItemCounts::default();
    while let Some(kind) = reader.read_next_item(&mut sink)? {
        item_counts.add(kind);
    }
    let stats = reader.stats();
    if stats.input_bytes() != file_bytes {
        return Err(format!(
            "decoder accounted for {} of {file_bytes} input bytes",
            stats.input_bytes()
        )
        .into());
    }
    if validation && sink.raw_bytes != file_bytes {
        return Err(format!("sink observed {} of {file_bytes} raw bytes", sink.raw_bytes).into());
    }
    eprintln!(
        "events={} schema_nodes={} streams={} units={} utc_offsets={} input_bytes={} \
         start_items={} schema_items={} event_items={} offset_items={} end_items={} validation={} \
         raw_digest={} semantic_digest={} pairs={} integers={} floats={} booleans={} strings={} \
         encoded_text={} nulls={} empty_objects={}",
        stats.log_events(),
        stats.schema_nodes(),
        stats.streams(),
        stats.units(),
        stats.utc_offset_changes(),
        stats.input_bytes(),
        item_counts.stream_starts,
        item_counts.schema_nodes,
        item_counts.log_events,
        item_counts.utc_offsets,
        item_counts.stream_ends,
        validation,
        sink.raw_digest,
        sink.semantic_digest,
        sink.value_counts.pairs,
        sink.value_counts.integers,
        sink.value_counts.floats,
        sink.value_counts.booleans,
        sink.value_counts.strings,
        sink.value_counts.encoded_text,
        sink.value_counts.nulls,
        sink.value_counts.empty_objects,
    );
    Ok(())
}

#[derive(Default)]
struct ItemCounts {
    stream_starts: u64,
    schema_nodes: u64,
    log_events: u64,
    utc_offsets: u64,
    stream_ends: u64,
}

impl ItemCounts {
    const fn add(&mut self, kind: KvIrItemKind) {
        match kind {
            KvIrItemKind::StreamStart => self.stream_starts += 1,
            KvIrItemKind::SchemaNode => self.schema_nodes += 1,
            KvIrItemKind::LogEvent => self.log_events += 1,
            KvIrItemKind::UtcOffsetChange => self.utc_offsets += 1,
            KvIrItemKind::StreamEnd => self.stream_ends += 1,
            _ => {}
        }
    }
}

struct ProfileSink {
    validation: bool,
    raw_bytes: u64,
    raw_digest: Digest128,
    semantic_digest: Digest128,
    value_counts: ValueCounts,
}

impl ProfileSink {
    fn new(validation: bool) -> Self {
        Self {
            validation,
            raw_bytes: 0,
            raw_digest: Digest128::new(),
            semantic_digest: Digest128::new(),
            value_counts: ValueCounts::default(),
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.raw_bytes = self
            .raw_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.raw_digest.bytes(bytes);
    }
}

impl KvIrSink for ProfileSink {
    type Error = Infallible;

    #[allow(clippy::too_many_lines)]
    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        if !self.validation {
            return Ok(());
        }
        match item {
            KvIrItem::StreamStart(header) => {
                self.raw(header.raw_preamble());
                self.semantic_digest.byte(0);
                self.semantic_digest.byte(encoding_tag(header.encoding()));
                self.semantic_digest
                    .bytes_with_length(header.protocol_version().as_bytes());
                self.semantic_digest
                    .bytes_with_length(header.metadata_json());
                self.semantic_digest.u64(header.stream_index());
                self.semantic_digest.u64(header.input_offset());
            }
            KvIrItem::SchemaNode(node) => {
                self.raw(node.raw_unit());
                self.semantic_digest.byte(1);
                self.semantic_digest.byte(namespace_tag(node.namespace()));
                self.semantic_digest.u32(node.node_id());
                self.semantic_digest.u32(node.parent_id());
                self.semantic_digest.u64(node.depth());
                self.semantic_digest.bytes_with_length(node.key());
                self.semantic_digest.byte(node_type_tag(node.node_type()));
                self.semantic_digest.u64(node.stream_index());
                self.semantic_digest.u64(node.unit_index());
                self.semantic_digest.u64(node.input_offset());
            }
            KvIrItem::LogEvent(event) => {
                self.raw(event.raw_unit());
                self.semantic_digest.byte(2);
                self.semantic_digest.i64(event.utc_offset_millis());
                self.semantic_digest.u64(event.stream_index());
                self.semantic_digest.u64(event.unit_index());
                self.semantic_digest.u64(event.event_index());
                self.semantic_digest.u64(event.input_offset());
                self.semantic_digest
                    .u64(u64::try_from(event.pair_count()).unwrap_or(u64::MAX));
                for pair in event.pairs() {
                    self.value_counts.pairs += 1;
                    self.semantic_digest.byte(namespace_tag(pair.namespace()));
                    self.semantic_digest.u32(pair.node_id());
                    let value = pair.value();
                    self.semantic_digest.bytes_with_length(value.raw_packet());
                    match value.kind() {
                        KvIrValueKind::Integer(integer) => {
                            self.value_counts.integers += 1;
                            self.semantic_digest.byte(0);
                            self.semantic_digest.i64(integer.value());
                            self.semantic_digest
                                .byte(integer_width_tag(integer.width()));
                        }
                        KvIrValueKind::Float { bits } => {
                            self.value_counts.floats += 1;
                            self.semantic_digest.byte(1);
                            self.semantic_digest.u64(bits);
                        }
                        KvIrValueKind::Boolean(value) => {
                            self.value_counts.booleans += 1;
                            self.semantic_digest.byte(2);
                            self.semantic_digest.byte(u8::from(value));
                        }
                        KvIrValueKind::String(value) => {
                            self.value_counts.strings += 1;
                            self.semantic_digest.byte(3);
                            self.semantic_digest.bytes_with_length(value);
                        }
                        KvIrValueKind::EncodedText(value) => {
                            self.value_counts.encoded_text += 1;
                            self.semantic_digest.byte(4);
                            self.semantic_digest.byte(encoding_tag(value.encoding()));
                            let variables = value.encoded_variables();
                            self.semantic_digest
                                .u64(u64::try_from(variables.len()).unwrap_or(u64::MAX));
                            for variable in variables {
                                match variable {
                                    KvIrEncodedVariable::FourByte(inner) => {
                                        self.semantic_digest.byte(0);
                                        self.semantic_digest.i64(i64::from(inner));
                                    }
                                    KvIrEncodedVariable::EightByte(inner) => {
                                        self.semantic_digest.byte(1);
                                        self.semantic_digest.i64(inner);
                                    }
                                    _ => self.semantic_digest.byte(u8::MAX),
                                }
                            }
                            let dictionaries = value.dictionary_variables();
                            self.semantic_digest
                                .u64(u64::try_from(dictionaries.len()).unwrap_or(u64::MAX));
                            for dictionary in dictionaries {
                                self.semantic_digest.bytes_with_length(dictionary);
                            }
                            self.semantic_digest.bytes_with_length(value.logtype());
                        }
                        KvIrValueKind::Null => {
                            self.value_counts.nulls += 1;
                            self.semantic_digest.byte(5);
                        }
                        KvIrValueKind::EmptyObject => {
                            self.value_counts.empty_objects += 1;
                            self.semantic_digest.byte(6);
                        }
                        _ => self.semantic_digest.byte(u8::MAX),
                    }
                }
            }
            KvIrItem::UtcOffsetChange(offset) => {
                self.raw(offset.raw_unit());
                self.semantic_digest.byte(3);
                self.semantic_digest.i64(offset.old_offset_millis());
                self.semantic_digest.i64(offset.new_offset_millis());
                self.semantic_digest.u64(offset.stream_index());
                self.semantic_digest.u64(offset.unit_index());
                self.semantic_digest.u64(offset.input_offset());
            }
            KvIrItem::StreamEnd(end) => {
                self.raw(end.raw_unit());
                self.semantic_digest.byte(4);
                self.semantic_digest.u64(end.stream_index());
                self.semantic_digest.u64(end.unit_index());
                self.semantic_digest.u64(end.input_offset());
                self.semantic_digest.u64(end.stream_bytes());
            }
            _ => self.semantic_digest.byte(u8::MAX),
        }
        Ok(())
    }
}

#[derive(Default)]
struct ValueCounts {
    pairs: u64,
    integers: u64,
    floats: u64,
    booleans: u64,
    strings: u64,
    encoded_text: u64,
    nulls: u64,
    empty_objects: u64,
}

#[derive(Clone, Copy)]
struct Digest128 {
    lower: u64,
    upper: u64,
}

impl Digest128 {
    const fn new() -> Self {
        Self {
            lower: 0xcbf2_9ce4_8422_2325,
            upper: 0x6c62_272e_07bb_0142,
        }
    }

    fn byte(&mut self, value: u8) {
        self.lower ^= u64::from(value);
        self.lower = self.lower.wrapping_mul(0x0000_0100_0000_01b3);
        self.upper ^= u64::from(value).wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.upper = self
            .upper
            .rotate_left(13)
            .wrapping_mul(0xbf58_476d_1ce4_e5b9);
    }

    fn bytes(&mut self, values: &[u8]) {
        for value in values {
            self.byte(*value);
        }
    }

    fn bytes_with_length(&mut self, values: &[u8]) {
        self.u64(u64::try_from(values.len()).unwrap_or(u64::MAX));
        self.bytes(values);
    }

    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes(&value.to_be_bytes());
    }
}

impl std::fmt::Display for Digest128 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:016x}{:016x}", self.lower, self.upper)
    }
}

const fn encoding_tag(value: KvIrEncoding) -> u8 {
    match value {
        KvIrEncoding::FourByte => 0,
        KvIrEncoding::EightByte => 1,
        _ => u8::MAX,
    }
}

const fn namespace_tag(value: KvIrNamespace) -> u8 {
    match value {
        KvIrNamespace::AutoGenerated => 0,
        KvIrNamespace::UserGenerated => 1,
        _ => u8::MAX,
    }
}

const fn node_type_tag(value: KvIrNodeType) -> u8 {
    match value {
        KvIrNodeType::Integer => 0,
        KvIrNodeType::Float => 1,
        KvIrNodeType::Boolean => 2,
        KvIrNodeType::String => 3,
        KvIrNodeType::UnstructuredArray => 4,
        KvIrNodeType::Object => 5,
        _ => u8::MAX,
    }
}

const fn integer_width_tag(value: KvIrIntegerWidth) -> u8 {
    match value {
        KvIrIntegerWidth::One => 0,
        KvIrIntegerWidth::Two => 1,
        KvIrIntegerWidth::Four => 2,
        KvIrIntegerWidth::Eight => 3,
        _ => u8::MAX,
    }
}
