//! Profiles incremental KV-IR decoding plus compact owned-event materialization.

use std::env;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs::File;
use std::path::PathBuf;

use clp_s::ingest::KvIrEncoding;
use clp_s::ingest::KvIrItem;
use clp_s::ingest::KvIrNamespace;
use clp_s::ingest::KvIrNodeType;
use clp_s::ingest::KvIrOptions;
use clp_s::ingest::KvIrOwnedEvent;
use clp_s::ingest::KvIrOwnedEventError;
use clp_s::ingest::KvIrOwnedEventLimits;
use clp_s::ingest::KvIrOwnedEventMaterializer;
use clp_s::ingest::KvIrOwnedSpan;
use clp_s::ingest::KvIrOwnedValue;
use clp_s::ingest::KvIrReader;
use clp_s::ingest::KvIrSink;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let input_path = PathBuf::from(arguments.next().ok_or("missing KV-IR stream path")?);
    if arguments.next().is_some() {
        return Err("usage: profile_kv_ir_owned_event STREAM".into());
    }

    let file_bytes = input_path.metadata()?.len();
    let mut reader = KvIrReader::new(File::open(input_path)?, KvIrOptions::default());
    let mut sink = OwnedEventSink::new()?;
    while reader.read_next_item(&mut sink)?.is_some() {}
    let stats = reader.stats();
    if stats.input_bytes() != file_bytes {
        return Err(format!(
            "decoder accounted for {} of {file_bytes} input bytes",
            stats.input_bytes()
        )
        .into());
    }
    eprintln!(
        "events={} owned_events={} schema_nodes={} streams={} units={} input_bytes={} \
         flat_nodes={} objects={} integers={} floats={} booleans={} strings={} arrays={} nulls={} \
         empty_objects={} key_bytes={} string_bytes={} array_bytes={} value_bytes={} \
         arena_bytes={} metadata_bytes={} digest={}",
        stats.log_events(),
        sink.counts.events,
        stats.schema_nodes(),
        stats.streams(),
        stats.units(),
        stats.input_bytes(),
        sink.counts.flat_nodes,
        sink.counts.objects,
        sink.counts.integers,
        sink.counts.floats,
        sink.counts.booleans,
        sink.counts.strings,
        sink.counts.arrays,
        sink.counts.nulls,
        sink.counts.empty_objects,
        sink.counts.key_bytes,
        sink.counts.string_bytes,
        sink.counts.array_bytes,
        sink.counts.value_bytes,
        sink.counts.arena_bytes,
        sink.counts.metadata_bytes,
        sink.digest,
    );
    Ok(())
}

#[derive(Debug)]
enum ProfileError {
    Materialize(KvIrOwnedEventError),
    InvalidSpan(KvIrOwnedSpan),
    ArenaCoverage { covered: usize, arena: usize },
    CounterOverflow,
}

impl Display for ProfileError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Materialize(source) => Display::fmt(source, formatter),
            Self::InvalidSpan(span) => write!(
                formatter,
                "invalid owned span offset={} length={}",
                span.offset(),
                span.length()
            ),
            Self::ArenaCoverage { covered, arena } => {
                write!(
                    formatter,
                    "owned spans cover {covered} of {arena} arena bytes"
                )
            }
            Self::CounterOverflow => formatter.write_str("profile counter overflow"),
        }
    }
}

impl Error for ProfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Materialize(source) => Some(source),
            _ => None,
        }
    }
}

impl From<KvIrOwnedEventError> for ProfileError {
    fn from(source: KvIrOwnedEventError) -> Self {
        Self::Materialize(source)
    }
}

#[derive(Default)]
struct OwnedCounts {
    events: u64,
    flat_nodes: u64,
    objects: u64,
    integers: u64,
    floats: u64,
    booleans: u64,
    strings: u64,
    arrays: u64,
    nulls: u64,
    empty_objects: u64,
    key_bytes: u64,
    string_bytes: u64,
    array_bytes: u64,
    value_bytes: u64,
    arena_bytes: u64,
    metadata_bytes: u64,
}

struct OwnedEventSink {
    digest: Digest128,
    counts: OwnedCounts,
    materializer: KvIrOwnedEventMaterializer,
}

impl OwnedEventSink {
    fn new() -> Result<Self, KvIrOwnedEventError> {
        Ok(Self {
            digest: Digest128::new(),
            counts: OwnedCounts::default(),
            materializer: KvIrOwnedEventMaterializer::new()?,
        })
    }

    fn consume_event(&mut self, event: &KvIrOwnedEvent) -> Result<(), ProfileError> {
        self.digest.byte(2);
        self.digest.i64(event.utc_offset_millis());
        self.digest.u64(event.stream_index());
        self.digest.u64(event.unit_index());
        self.digest.u64(event.event_index());
        self.digest.u64(event.input_offset());
        self.digest
            .u64(u64::try_from(event.arena().len()).map_err(|_| ProfileError::CounterOverflow)?);
        add(&mut self.counts.events, 1)?;
        add(
            &mut self.counts.arena_bytes,
            u64::try_from(event.arena().len()).map_err(|_| ProfileError::CounterOverflow)?,
        )?;

        let mut covered = 0_usize;
        for namespace in [KvIrNamespace::AutoGenerated, KvIrNamespace::UserGenerated] {
            let nodes = event.nodes(namespace);
            self.digest.byte(namespace_tag(namespace));
            self.digest
                .u64(u64::try_from(nodes.len()).map_err(|_| ProfileError::CounterOverflow)?);
            for (node_index, node) in nodes.iter().copied().enumerate() {
                add(&mut self.counts.flat_nodes, 1)?;
                self.digest
                    .u64(u64::try_from(node_index).map_err(|_| ProfileError::CounterOverflow)?);
                self.digest.u32(node.depth());
                let key = self.consume_span(event, node.key_span(), &mut covered)?;
                add(
                    &mut self.counts.key_bytes,
                    u64::try_from(key.len()).map_err(|_| ProfileError::CounterOverflow)?,
                )?;
                match node.value() {
                    KvIrOwnedValue::Object => {
                        add(&mut self.counts.objects, 1)?;
                        self.digest.byte(0);
                    }
                    KvIrOwnedValue::Integer(value) => {
                        add(&mut self.counts.integers, 1)?;
                        self.digest.byte(1);
                        self.digest.i64(value);
                    }
                    KvIrOwnedValue::Float { bits } => {
                        add(&mut self.counts.floats, 1)?;
                        self.digest.byte(2);
                        self.digest.u64(bits);
                    }
                    KvIrOwnedValue::Boolean(value) => {
                        add(&mut self.counts.booleans, 1)?;
                        self.digest.byte(3);
                        self.digest.byte(u8::from(value));
                    }
                    KvIrOwnedValue::String(span) => {
                        add(&mut self.counts.strings, 1)?;
                        self.digest.byte(4);
                        let value = self.consume_span(event, span, &mut covered)?;
                        let byte_count = u64::try_from(value.len())
                            .map_err(|_| ProfileError::CounterOverflow)?;
                        add(&mut self.counts.string_bytes, byte_count)?;
                        add(&mut self.counts.value_bytes, byte_count)?;
                    }
                    KvIrOwnedValue::ArrayJson(span) => {
                        add(&mut self.counts.arrays, 1)?;
                        self.digest.byte(5);
                        let value = self.consume_span(event, span, &mut covered)?;
                        let byte_count = u64::try_from(value.len())
                            .map_err(|_| ProfileError::CounterOverflow)?;
                        add(&mut self.counts.array_bytes, byte_count)?;
                        add(&mut self.counts.value_bytes, byte_count)?;
                    }
                    KvIrOwnedValue::Null => {
                        add(&mut self.counts.nulls, 1)?;
                        self.digest.byte(6);
                    }
                    KvIrOwnedValue::EmptyObject => {
                        add(&mut self.counts.empty_objects, 1)?;
                        self.digest.byte(7);
                    }
                    _ => self.digest.byte(u8::MAX),
                }
            }
        }
        if covered != event.arena().len() {
            return Err(ProfileError::ArenaCoverage {
                covered,
                arena: event.arena().len(),
            });
        }
        Ok(())
    }

    fn consume_span<'event>(
        &mut self,
        event: &'event KvIrOwnedEvent,
        span: KvIrOwnedSpan,
        covered: &mut usize,
    ) -> Result<&'event [u8], ProfileError> {
        self.digest.u32(span.offset());
        self.digest.u32(span.length());
        let bytes = event.resolve(span).ok_or(ProfileError::InvalidSpan(span))?;
        self.digest.bytes(bytes);
        *covered = covered
            .checked_add(bytes.len())
            .ok_or(ProfileError::CounterOverflow)?;
        Ok(bytes)
    }
}

impl KvIrSink for OwnedEventSink {
    type Error = ProfileError;

    fn write_item(&mut self, item: KvIrItem<'_>) -> Result<(), Self::Error> {
        match item {
            KvIrItem::StreamStart(header) => {
                self.digest.byte(0);
                self.digest.byte(encoding_tag(header.encoding()));
                self.digest
                    .bytes_with_length(header.protocol_version().as_bytes());
                self.digest.bytes_with_length(header.metadata_json());
                self.digest.bytes_with_length(header.raw_preamble());
                self.digest.u64(header.stream_index());
                self.digest.u64(header.input_offset());
                add(
                    &mut self.counts.metadata_bytes,
                    u64::try_from(header.metadata_json().len())
                        .map_err(|_| ProfileError::CounterOverflow)?,
                )?;
            }
            KvIrItem::SchemaNode(node) => {
                self.digest.byte(1);
                self.digest.byte(namespace_tag(node.namespace()));
                self.digest.u32(node.node_id());
                self.digest.u32(node.parent_id());
                self.digest.u64(node.depth());
                self.digest.bytes_with_length(node.key());
                self.digest.byte(node_type_tag(node.node_type()));
            }
            KvIrItem::LogEvent(event) => {
                let owned = self
                    .materializer
                    .materialize(event, KvIrOwnedEventLimits::default())?;
                self.consume_event(&owned)?;
            }
            KvIrItem::UtcOffsetChange(offset) => {
                self.digest.byte(3);
                self.digest.i64(offset.old_offset_millis());
                self.digest.i64(offset.new_offset_millis());
            }
            KvIrItem::StreamEnd(end) => {
                self.digest.byte(4);
                self.digest.u64(end.stream_index());
                self.digest.u64(end.unit_index());
                self.digest.u64(end.input_offset());
                self.digest.u64(end.stream_bytes());
            }
            _ => self.digest.byte(u8::MAX),
        }
        Ok(())
    }
}

fn add(counter: &mut u64, value: u64) -> Result<(), ProfileError> {
    *counter = counter
        .checked_add(value)
        .ok_or(ProfileError::CounterOverflow)?;
    Ok(())
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

impl Display for Digest128 {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
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
