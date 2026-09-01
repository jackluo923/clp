//! Transactional, library-first serializer for CLP's current key-value IR protocol.
//!
//! The event API consumes two borrowed `MessagePack` maps directly. It deliberately does not build
//! a Serde value tree: map keys and scalar payloads remain borrowed while the two reusable schema
//! trees and staging buffers are updated. An event is published to
//! [`KvIrSerializer::pending_output`] only after both maps have been validated and serialized
//! successfully.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Write;

use super::KvIrEncoding;
use super::KvIrNodeType;
use super::json_canonical::CanonicalJsonLimits;
use super::json_canonical::CanonicalJsonScratch;

const FOUR_BYTE_MAGIC: [u8; 4] = [0xfd, 0x2f, 0xb5, 0x29];
const EIGHT_BYTE_MAGIC: [u8; 4] = [0xfd, 0x2f, 0xb5, 0x30];
const METADATA_SUFFIX: &[u8] = b"\"VARIABLES_SCHEMA_ID\":\"com.yscope.clp.VariablesSchemaV2\",\
    \"VARIABLE_ENCODING_METHODS_ID\":\"com.yscope.clp.VariableEncodingMethodsV1\",\
    \"VERSION\":\"0.1.0\"}";
const MEBIBYTE: u64 = 1024 * 1024;

const INTEGER_PLACEHOLDER: u8 = 0x11;
const DICTIONARY_PLACEHOLDER: u8 = 0x12;
const FLOAT_PLACEHOLDER: u8 = 0x13;
const ESCAPE_MARKER: u8 = b'\\';
const EMPTY_SCHEMA_INDEX_SLOT: u32 = 0;
const LINEAR_SCHEMA_SCAN_LIMIT: usize = 32;
const CPP_FLOAT_BUFFER_BYTES: usize = 32;

/// Hard limits for untrusted `MessagePack` and bounded serializer-owned memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrSerializerLimits {
    input_bytes_per_map: u64,
    pending_output_bytes: u64,
    event_output_bytes: u64,
    metadata_bytes: u64,
    schema_nodes_per_namespace: u64,
    nesting_depth: u64,
    values_per_map: u64,
    scalar_bytes: u64,
}

impl KvIrSerializerLimits {
    pub const DEFAULT: Self = Self {
        input_bytes_per_map: 16 * MEBIBYTE,
        pending_output_bytes: 16 * MEBIBYTE,
        event_output_bytes: 16 * MEBIBYTE,
        metadata_bytes: 64 * 1024,
        schema_nodes_per_namespace: 1_000_000,
        nesting_depth: 256,
        values_per_map: 1_000_000,
        scalar_bytes: 8 * MEBIBYTE,
    };

    #[must_use]
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    #[must_use]
    pub const fn with_max_input_bytes_per_map(mut self, value: u64) -> Self {
        self.input_bytes_per_map = value;
        self
    }

    #[must_use]
    pub const fn with_max_pending_output_bytes(mut self, value: u64) -> Self {
        self.pending_output_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_event_output_bytes(mut self, value: u64) -> Self {
        self.event_output_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_metadata_bytes(mut self, value: u64) -> Self {
        self.metadata_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_schema_nodes_per_namespace(mut self, value: u64) -> Self {
        self.schema_nodes_per_namespace = value;
        self
    }

    #[must_use]
    pub const fn with_max_nesting_depth(mut self, value: u64) -> Self {
        self.nesting_depth = value;
        self
    }

    #[must_use]
    pub const fn with_max_values_per_map(mut self, value: u64) -> Self {
        self.values_per_map = value;
        self
    }

    #[must_use]
    pub const fn with_max_scalar_bytes(mut self, value: u64) -> Self {
        self.scalar_bytes = value;
        self
    }

    #[must_use]
    pub const fn max_input_bytes_per_map(self) -> u64 {
        self.input_bytes_per_map
    }

    #[must_use]
    pub const fn max_pending_output_bytes(self) -> u64 {
        self.pending_output_bytes
    }

    #[must_use]
    pub const fn max_event_output_bytes(self) -> u64 {
        self.event_output_bytes
    }

    #[must_use]
    pub const fn max_metadata_bytes(self) -> u64 {
        self.metadata_bytes
    }

    #[must_use]
    pub const fn max_schema_nodes_per_namespace(self) -> u64 {
        self.schema_nodes_per_namespace
    }

    #[must_use]
    pub const fn max_nesting_depth(self) -> u64 {
        self.nesting_depth
    }

    #[must_use]
    pub const fn max_values_per_map(self) -> u64 {
        self.values_per_map
    }

    #[must_use]
    pub const fn max_scalar_bytes(self) -> u64 {
        self.scalar_bytes
    }
}

impl Default for KvIrSerializerLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Serializer construction options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrSerializerOptions {
    encoding: KvIrEncoding,
    limits: KvIrSerializerLimits,
}

impl KvIrSerializerOptions {
    #[must_use]
    pub const fn new(encoding: KvIrEncoding) -> Self {
        Self {
            encoding,
            limits: KvIrSerializerLimits::DEFAULT,
        }
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: KvIrSerializerLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub const fn encoding(self) -> KvIrEncoding {
        self.encoding
    }

    #[must_use]
    pub const fn limits(self) -> KvIrSerializerLimits {
        self.limits
    }
}

impl Default for KvIrSerializerOptions {
    fn default() -> Self {
        Self::new(KvIrEncoding::FourByte)
    }
}

/// Resource protected by a serializer limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrSerializerLimitResource {
    InputBytesPerMap,
    PendingOutputBytes,
    EventOutputBytes,
    MetadataBytes,
    SchemaNodesPerNamespace,
    NestingDepth,
    ValuesPerMap,
    ScalarBytes,
}

impl Display for KvIrSerializerLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InputBytesPerMap => "MessagePack input bytes per map",
            Self::PendingOutputBytes => "pending KV-IR output bytes",
            Self::EventOutputBytes => "KV-IR event output bytes",
            Self::MetadataBytes => "KV-IR metadata bytes",
            Self::SchemaNodesPerNamespace => "schema nodes per namespace",
            Self::NestingDepth => "MessagePack nesting depth",
            Self::ValuesPerMap => "MessagePack values per map",
            Self::ScalarBytes => "MessagePack scalar bytes",
        })
    }
}

/// Exact serializer limit violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrSerializerLimitViolation {
    resource: KvIrSerializerLimitResource,
    actual: u64,
    limit: u64,
}

impl KvIrSerializerLimitViolation {
    const fn new(resource: KvIrSerializerLimitResource, actual: u64, limit: u64) -> Self {
        Self {
            resource,
            actual,
            limit,
        }
    }

    #[must_use]
    pub const fn resource(self) -> KvIrSerializerLimitResource {
        self.resource
    }

    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

/// Which input map contains an invalid `MessagePack` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KvIrSerializerInput {
    AutoGenerated,
    UserGenerated,
}

impl Display for KvIrSerializerInput {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AutoGenerated => "auto-generated",
            Self::UserGenerated => "user-generated",
        })
    }
}

/// Why borrowed `MessagePack` could not be represented by the current KV-IR protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrMessagePackErrorKind {
    Truncated,
    ReservedMarker,
    RootMustBeMap,
    MapKeyMustBeString,
    UnsupportedBinary,
    UnsupportedExtension,
    IntegerOutOfRange,
    TrailingBytes,
    LengthOutOfRange,
}

impl Display for KvIrMessagePackErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "truncated MessagePack",
            Self::ReservedMarker => "reserved MessagePack marker",
            Self::RootMustBeMap => "root MessagePack value is not a map",
            Self::MapKeyMustBeString => "MessagePack map key is not a string",
            Self::UnsupportedBinary => "MessagePack binary value is unsupported",
            Self::UnsupportedExtension => "MessagePack extension value is unsupported",
            Self::IntegerOutOfRange => "MessagePack integer exceeds signed 64-bit range",
            Self::TrailingBytes => "bytes follow the root MessagePack map",
            Self::LengthOutOfRange => "MessagePack length is not representable on this platform",
        })
    }
}

/// Construction or event serialization failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrSerializerError {
    InvalidUserDefinedMetadata,
    MetadataTooLarge(KvIrSerializerLimitViolation),
    MessagePack {
        input: KvIrSerializerInput,
        offset: usize,
        kind: KvIrMessagePackErrorKind,
    },
    Limit(KvIrSerializerLimitViolation),
    AllocationFailed {
        resource: &'static str,
        requested_additional: usize,
    },
    Finished,
    SizeOverflow,
}

impl Display for KvIrSerializerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUserDefinedMetadata => {
                formatter.write_str("user-defined metadata must be one valid JSON object")
            }
            Self::MetadataTooLarge(source) | Self::Limit(source) => write!(
                formatter,
                "{} is {}, exceeding limit {}",
                source.resource, source.actual, source.limit
            ),
            Self::MessagePack {
                input,
                offset,
                kind,
            } => write!(
                formatter,
                "invalid {input} MessagePack at byte {offset}: {kind}"
            ),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to allocate {requested_additional} additional byte(s) for {resource}"
            ),
            Self::Finished => formatter.write_str("KV-IR serializer is already finished"),
            Self::SizeOverflow => formatter.write_str("KV-IR serializer size overflow"),
        }
    }
}

impl Error for KvIrSerializerError {}

/// Cumulative committed serializer statistics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvIrSerializerStats {
    log_events: u64,
    schema_nodes: u64,
    utc_offset_changes: u64,
    serialized_bytes: u64,
}

impl KvIrSerializerStats {
    #[must_use]
    pub const fn log_events(self) -> u64 {
        self.log_events
    }

    #[must_use]
    pub const fn schema_nodes(self) -> u64 {
        self.schema_nodes
    }

    #[must_use]
    pub const fn utc_offset_changes(self) -> u64 {
        self.utc_offset_changes
    }

    #[must_use]
    pub const fn serialized_bytes(self) -> u64 {
        self.serialized_bytes
    }
}

#[derive(Debug)]
struct SchemaNode {
    parent_id: u32,
    node_type: KvIrNodeType,
    key_end: usize,
}

impl SchemaNode {
    const fn root() -> Self {
        Self {
            parent_id: 0,
            node_type: KvIrNodeType::Object,
            key_end: 0,
        }
    }
}

/// Compact exact index into `SchemaTree::nodes`.
///
/// Slots store node IDs directly (zero is both the root ID and the empty-slot marker). Randomized
/// hashing protects the linear-probing table from attacker-selected keys, while every hit is still
/// checked against the complete parent/type/key identity. Keys themselves stay in encounter order
/// in one packed arena, avoiding a second owned key in the index.
#[derive(Debug)]
struct SchemaIndex {
    hash_builder: Option<ahash::RandomState>,
    slots: Vec<u32>,
    #[cfg(test)]
    forced_hash: Option<u64>,
}

impl SchemaIndex {
    const fn new() -> Self {
        Self {
            // Small schemas are faster to scan directly and never need hashing. Defer both the
            // randomized state and its entropy/runtime footprint until the index is actually
            // allocated.
            hash_builder: None,
            slots: Vec::new(),
            #[cfg(test)]
            forced_hash: None,
        }
    }

    fn locate(
        &self,
        nodes: &[SchemaNode],
        keys: &[u8],
        parent_id: u32,
        key: &[u8],
        node_type: KvIrNodeType,
    ) -> (u64, Option<u32>) {
        if self.slots.is_empty() {
            return (0, None);
        }
        let hash = self.hash_key(parent_id, key, node_type);

        let mask = self.slots.len() - 1;
        let mut slot_index = Self::starting_slot(hash, mask);
        loop {
            let node_id = self.slots[slot_index];
            if node_id == EMPTY_SCHEMA_INDEX_SLOT {
                return (hash, None);
            }
            if Self::matches(nodes, keys, node_id, parent_id, key, node_type) {
                return (hash, Some(node_id));
            }
            slot_index = slot_index.wrapping_add(1) & mask;
        }
    }

    fn try_prepare_insert(
        &mut self,
        nodes: &[SchemaNode],
        keys: &[u8],
    ) -> Result<(), KvIrSerializerError> {
        // `nodes` includes the root, so its current length is the number of indexed entries after
        // the pending insertion.
        let next_entry_count = nodes.len();
        if self.slots.is_empty() && next_entry_count <= LINEAR_SCHEMA_SCAN_LIMIT {
            return Ok(());
        }
        let mut required_slots = self.slots.len();
        if required_slots == 0 {
            required_slots = 16;
        }
        while next_entry_count > required_slots / 4 * 3 {
            required_slots = required_slots
                .checked_mul(2)
                .ok_or(KvIrSerializerError::SizeOverflow)?;
        }
        if required_slots == self.slots.len() {
            return Ok(());
        }

        if self.hash_builder.is_none() {
            self.hash_builder = Some(ahash::RandomState::new());
        }

        let requested_additional = required_slots.saturating_sub(self.slots.len());
        let mut new_slots = Vec::new();
        new_slots
            .try_reserve_exact(required_slots)
            .map_err(|_| allocation("schema index slots", requested_additional))?;
        new_slots.resize(required_slots, EMPTY_SCHEMA_INDEX_SLOT);
        for node_index in 1..nodes.len() {
            let node_id =
                u32::try_from(node_index).map_err(|_| KvIrSerializerError::SizeOverflow)?;
            let node = nodes
                .get(node_index)
                .ok_or(KvIrSerializerError::SizeOverflow)?;
            let key =
                Self::node_key(nodes, keys, node_index).ok_or(KvIrSerializerError::SizeOverflow)?;
            let hash = self.hash_key(node.parent_id, key, node.node_type);
            Self::insert_slot(&mut new_slots, hash, node_id);
        }
        self.slots = new_slots;
        Ok(())
    }

    fn insert(&mut self, hash: u64, node_id: u32) {
        if !self.slots.is_empty() {
            Self::insert_slot(&mut self.slots, hash, node_id);
        }
    }

    fn remove(&mut self, nodes: &[SchemaNode], keys: &[u8], node_id: u32) {
        if self.slots.is_empty() {
            return;
        }
        let Some(node_index) = usize::try_from(node_id).ok() else {
            return;
        };
        let Some(node) = nodes.get(node_index) else {
            return;
        };
        let Some(key) = Self::node_key(nodes, keys, node_index) else {
            return;
        };
        let mask = self.slots.len() - 1;
        let hash = self.hash_key(node.parent_id, key, node.node_type);
        let mut hole = Self::starting_slot(hash, mask);
        loop {
            let indexed_node_id = self.slots[hole];
            if indexed_node_id == EMPTY_SCHEMA_INDEX_SLOT {
                return;
            }
            if indexed_node_id == node_id {
                break;
            }
            hole = hole.wrapping_add(1) & mask;
        }

        // Backshift deletion keeps lookup chains intact without accumulating tombstones after
        // failed transactions. This matters when a long-lived serializer sees repeated failures.
        let mut candidate_slot = hole.wrapping_add(1) & mask;
        loop {
            let candidate_id = self.slots[candidate_slot];
            if candidate_id == EMPTY_SCHEMA_INDEX_SLOT {
                self.slots[hole] = EMPTY_SCHEMA_INDEX_SLOT;
                return;
            }
            let Some(candidate_index) = usize::try_from(candidate_id).ok() else {
                self.slots[hole] = EMPTY_SCHEMA_INDEX_SLOT;
                return;
            };
            let Some(candidate) = nodes.get(candidate_index) else {
                self.slots[hole] = EMPTY_SCHEMA_INDEX_SLOT;
                return;
            };
            let Some(candidate_key) = Self::node_key(nodes, keys, candidate_index) else {
                self.slots[hole] = EMPTY_SCHEMA_INDEX_SLOT;
                return;
            };
            let candidate_hash =
                self.hash_key(candidate.parent_id, candidate_key, candidate.node_type);
            let ideal_slot = Self::starting_slot(candidate_hash, mask);
            let candidate_distance = candidate_slot.wrapping_sub(ideal_slot) & mask;
            let hole_distance = hole.wrapping_sub(ideal_slot) & mask;
            if candidate_distance > hole_distance {
                self.slots[hole] = candidate_id;
                hole = candidate_slot;
            }
            candidate_slot = candidate_slot.wrapping_add(1) & mask;
        }
    }

    fn insert_slot(slots: &mut [u32], hash: u64, node_id: u32) {
        let mask = slots.len() - 1;
        let mut slot_index = Self::starting_slot(hash, mask);
        while slots[slot_index] != EMPTY_SCHEMA_INDEX_SLOT {
            slot_index = slot_index.wrapping_add(1) & mask;
        }
        slots[slot_index] = node_id;
    }

    fn matches(
        nodes: &[SchemaNode],
        keys: &[u8],
        node_id: u32,
        parent_id: u32,
        key: &[u8],
        node_type: KvIrNodeType,
    ) -> bool {
        let Some(node_index) = usize::try_from(node_id).ok() else {
            return false;
        };
        nodes.get(node_index).is_some_and(|node| {
            node.parent_id == parent_id
                && node.node_type == node_type
                && Self::node_key(nodes, keys, node_index).is_some_and(|candidate| candidate == key)
        })
    }

    fn node_key<'a>(nodes: &[SchemaNode], keys: &'a [u8], node_index: usize) -> Option<&'a [u8]> {
        if node_index == 0 {
            return Some(&[]);
        }
        let key_start = nodes.get(node_index.checked_sub(1)?)?.key_end;
        let key_end = nodes.get(node_index)?.key_end;
        keys.get(key_start..key_end)
    }

    fn hash_key(&self, parent_id: u32, key: &[u8], node_type: KvIrNodeType) -> u64 {
        #[cfg(test)]
        if let Some(hash) = self.forced_hash {
            return hash;
        }
        self.hash_builder
            .as_ref()
            .expect("schema hash state exists whenever the index is allocated")
            .hash_one((parent_id, node_type_hash_tag(node_type), key))
    }

    fn starting_slot(hash: u64, mask: usize) -> usize {
        let mask_u64 = u64::try_from(mask).unwrap_or(u64::MAX);
        usize::try_from(hash & mask_u64).unwrap_or_default()
    }
}

#[derive(Debug)]
struct SchemaTree {
    nodes: Vec<SchemaNode>,
    keys: Vec<u8>,
    index: SchemaIndex,
}

impl SchemaTree {
    fn new() -> Result<Self, KvIrSerializerError> {
        let mut nodes = Vec::new();
        nodes
            .try_reserve(1)
            .map_err(|_| allocation("schema nodes", 1))?;
        nodes.push(SchemaNode::root());
        Ok(Self {
            nodes,
            keys: Vec::new(),
            index: SchemaIndex::new(),
        })
    }

    fn find(&self, parent_id: u32, key: &[u8], node_type: KvIrNodeType) -> (u64, Option<u32>) {
        if self.index.slots.is_empty() {
            for node_index in 1..self.nodes.len() {
                let Some(node) = self.nodes.get(node_index) else {
                    break;
                };
                if node.parent_id == parent_id
                    && node.node_type == node_type
                    && SchemaIndex::node_key(&self.nodes, &self.keys, node_index)
                        .is_some_and(|candidate| candidate == key)
                {
                    return (0, u32::try_from(node_index).ok());
                }
            }
            return (0, None);
        }
        self.index
            .locate(&self.nodes, &self.keys, parent_id, key, node_type)
    }

    fn insert(
        &mut self,
        hash: u64,
        parent_id: u32,
        key: &[u8],
        node_type: KvIrNodeType,
        limit: u64,
    ) -> Result<u32, KvIrSerializerError> {
        let node_id = u32::try_from(self.nodes.len()).map_err(|_| {
            KvIrSerializerError::Limit(KvIrSerializerLimitViolation::new(
                KvIrSerializerLimitResource::SchemaNodesPerNamespace,
                u64::from(u32::MAX) + 1,
                limit,
            ))
        })?;
        if u64::from(node_id) > limit {
            return Err(KvIrSerializerError::Limit(
                KvIrSerializerLimitViolation::new(
                    KvIrSerializerLimitResource::SchemaNodesPerNamespace,
                    u64::from(node_id),
                    limit,
                ),
            ));
        }
        self.nodes
            .try_reserve(1)
            .map_err(|_| allocation("schema nodes", 1))?;
        let key_end = self
            .keys
            .len()
            .checked_add(key.len())
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        self.keys
            .try_reserve(key.len())
            .map_err(|_| allocation("schema keys", key.len()))?;
        let was_indexed = !self.index.slots.is_empty();
        self.index.try_prepare_insert(&self.nodes, &self.keys)?;
        // `find` deliberately skips hashing while the tree is in linear-scan mode. The insertion
        // that crosses the threshold creates the index, so compute its hash only in that case.
        let hash = if was_indexed || self.index.slots.is_empty() {
            hash
        } else {
            self.index.hash_key(parent_id, key, node_type)
        };
        self.keys.extend_from_slice(key);
        self.nodes.push(SchemaNode {
            parent_id,
            node_type,
            key_end,
        });
        self.index.insert(hash, node_id);
        Ok(node_id)
    }

    fn rollback(&mut self, snapshot: usize) {
        let key_end = self
            .nodes
            .get(snapshot.saturating_sub(1))
            .map_or(0, |node| node.key_end);
        if snapshot.saturating_sub(1) <= LINEAR_SCHEMA_SCAN_LIMIT {
            self.nodes.truncate(snapshot);
            self.keys.truncate(key_end);
            self.index.slots.clear();
            return;
        }
        while self.nodes.len() > snapshot {
            let node_id = u32::try_from(self.nodes.len() - 1).unwrap_or(u32::MAX);
            self.index.remove(&self.nodes, &self.keys, node_id);
            self.nodes.pop();
        }
        self.keys.truncate(key_end);
    }
}

const fn node_type_hash_tag(node_type: KvIrNodeType) -> u8 {
    match node_type {
        KvIrNodeType::Integer => 0,
        KvIrNodeType::Float => 1,
        KvIrNodeType::Boolean => 2,
        KvIrNodeType::String => 3,
        KvIrNodeType::UnstructuredArray => 4,
        KvIrNodeType::Object => 5,
    }
}

/// Reusable current-protocol KV-IR serializer.
#[derive(Debug)]
pub struct KvIrSerializer {
    options: KvIrSerializerOptions,
    auto_schema: SchemaTree,
    user_schema: SchemaTree,
    pending: Vec<u8>,
    pending_start: usize,
    schema_stage: Vec<u8>,
    sequential_stage: Vec<u8>,
    user_values_stage: Vec<u8>,
    logtype: Vec<u8>,
    array_json: Vec<u8>,
    current_utc_offset: i64,
    stats: KvIrSerializerStats,
    finished: bool,
}

impl KvIrSerializer {
    /// Creates a serializer and stages its preamble.
    ///
    /// `user_defined_metadata_json`, when present, is parsed, bounded, canonicalized like
    /// `nlohmann::json`, and required to have an object root.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrSerializerError`] if metadata is invalid or exceeds a configured limit, or if
    /// initial serializer storage cannot be allocated.
    pub fn new(
        options: KvIrSerializerOptions,
        user_defined_metadata_json: Option<&[u8]>,
    ) -> Result<Self, KvIrSerializerError> {
        let mut pending = Vec::new();
        let metadata = Self::metadata_json(options.limits, user_defined_metadata_json)?;
        let magic = match options.encoding {
            KvIrEncoding::FourByte => FOUR_BYTE_MAGIC,
            KvIrEncoding::EightByte => EIGHT_BYTE_MAGIC,
        };
        pending
            .try_reserve_exact(magic.len() + 3 + metadata.len())
            .map_err(|_| allocation("pending output", magic.len() + 3 + metadata.len()))?;
        pending.extend_from_slice(&magic);
        pending.push(0x01);
        if u8::try_from(metadata.len()).is_ok() {
            pending.push(0x11);
            pending
                .push(u8::try_from(metadata.len()).map_err(|_| KvIrSerializerError::SizeOverflow)?);
        } else {
            pending.push(0x12);
            pending.extend_from_slice(
                &u16::try_from(metadata.len())
                    .map_err(|_| {
                        KvIrSerializerError::MetadataTooLarge(KvIrSerializerLimitViolation::new(
                            KvIrSerializerLimitResource::MetadataBytes,
                            u64::try_from(metadata.len()).unwrap_or(u64::MAX),
                            u64::from(u16::MAX),
                        ))
                    })?
                    .to_be_bytes(),
            );
        }
        pending.extend_from_slice(&metadata);
        let pending_bytes =
            u64::try_from(pending.len()).map_err(|_| KvIrSerializerError::SizeOverflow)?;
        if pending_bytes > options.limits.pending_output_bytes {
            return Err(KvIrSerializerError::Limit(
                KvIrSerializerLimitViolation::new(
                    KvIrSerializerLimitResource::PendingOutputBytes,
                    pending_bytes,
                    options.limits.pending_output_bytes,
                ),
            ));
        }
        Ok(Self {
            options,
            auto_schema: SchemaTree::new()?,
            user_schema: SchemaTree::new()?,
            pending,
            pending_start: 0,
            schema_stage: Vec::new(),
            sequential_stage: Vec::new(),
            user_values_stage: Vec::new(),
            logtype: Vec::new(),
            array_json: Vec::new(),
            current_utc_offset: 0,
            stats: KvIrSerializerStats {
                serialized_bytes: pending_bytes,
                ..KvIrSerializerStats::default()
            },
            finished: false,
        })
    }

    /// Creates a four-byte serializer, matching `clp_ffi_py.ir.Serializer`.
    ///
    /// # Errors
    ///
    /// Returns the construction errors documented by [`Self::new`].
    pub fn new_four_byte(
        user_defined_metadata_json: Option<&[u8]>,
    ) -> Result<Self, KvIrSerializerError> {
        Self::new(KvIrSerializerOptions::default(), user_defined_metadata_json)
    }

    #[must_use]
    pub const fn encoding(&self) -> KvIrEncoding {
        self.options.encoding
    }

    #[must_use]
    pub const fn current_utc_offset_millis(&self) -> i64 {
        self.current_utc_offset
    }

    #[must_use]
    pub const fn stats(&self) -> KvIrSerializerStats {
        self.stats
    }

    /// Bytes ready for a binding or caller-owned output adapter.
    #[must_use]
    pub fn pending_output(&self) -> &[u8] {
        &self.pending[self.pending_start..]
    }

    /// Marks a prefix of [`Self::pending_output`] as consumed without reallocating.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrSerializerError::SizeOverflow`] if `bytes` exceeds the pending length.
    pub fn consume_pending(&mut self, bytes: usize) -> Result<(), KvIrSerializerError> {
        if bytes > self.pending_output().len() {
            return Err(KvIrSerializerError::SizeOverflow);
        }
        self.pending_start = self
            .pending_start
            .checked_add(bytes)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        if self.pending_start == self.pending.len() {
            self.pending.clear();
            self.pending_start = 0;
        }
        Ok(())
    }

    /// Performs one output-stream write and consumes exactly the reported prefix.
    ///
    /// # Errors
    ///
    /// Returns an I/O error from `output`, or if its reported byte count violates the [`Write`]
    /// contract.
    pub fn write_pending<W: Write + ?Sized>(&mut self, output: &mut W) -> io::Result<usize> {
        let written = output.write(self.pending_output())?;
        self.consume_pending(written).map_err(io::Error::other)?;
        Ok(written)
    }

    /// Writes all currently pending bytes. Newly serialized bytes are not generated implicitly.
    ///
    /// # Errors
    ///
    /// Returns an I/O error from `output`, including [`io::ErrorKind::WriteZero`] if it makes no
    /// progress while bytes remain.
    pub fn write_all_pending<W: Write + ?Sized>(&mut self, output: &mut W) -> io::Result<()> {
        while !self.pending_output().is_empty() {
            let written = self.write_pending(output)?;
            if written == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
        }
        Ok(())
    }

    /// Stages a UTC-offset packet. As in C++, a packet is emitted even when the value is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrSerializerError`] if the serializer is finished, output limits are exceeded,
    /// counters overflow, or staging allocation fails.
    pub fn change_utc_offset(
        &mut self,
        utc_offset_millis: i64,
    ) -> Result<usize, KvIrSerializerError> {
        self.ensure_open()?;
        let mut packet = [0_u8; 9];
        packet[0] = 0x3f;
        packet[1..].copy_from_slice(&utc_offset_millis.to_be_bytes());
        let utc_offset_changes = self
            .stats
            .utc_offset_changes
            .checked_add(1)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        self.append_pending(&packet)?;
        self.current_utc_offset = utc_offset_millis;
        self.stats.utc_offset_changes = utc_offset_changes;
        Ok(packet.len())
    }

    /// Serializes one pair of `MessagePack` maps transactionally.
    ///
    /// The return value is the number of newly pending bytes, including newly declared schema
    /// nodes. Each input slice must begin with one complete `MessagePack` map. Bytes after the
    /// first map are ignored for compatibility with the C++ serializer.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrSerializerError`] for invalid or unsupported `MessagePack`, configured limit
    /// violations, allocation failure, counter overflow, or use after [`Self::finish`]. No schema
    /// or output is committed when an event fails.
    pub fn serialize_log_event_from_msgpack_maps(
        &mut self,
        auto_generated: &[u8],
        user_generated: &[u8],
    ) -> Result<usize, KvIrSerializerError> {
        self.ensure_open()?;
        self.check_input_size(auto_generated)?;
        self.check_input_size(user_generated)?;
        let auto_snapshot = self.auto_schema.nodes.len();
        let user_snapshot = self.user_schema.nodes.len();
        self.schema_stage.clear();
        self.sequential_stage.clear();
        self.user_values_stage.clear();

        let result =
            self.serialize_one_map(KvIrSerializerInput::AutoGenerated, auto_generated, true);
        let result = result.and_then(|()| {
            self.serialize_one_map(KvIrSerializerInput::UserGenerated, user_generated, false)
        });
        if let Err(source) = result {
            self.auto_schema.rollback(auto_snapshot);
            self.user_schema.rollback(user_snapshot);
            self.schema_stage.clear();
            self.sequential_stage.clear();
            self.user_values_stage.clear();
            return Err(source);
        }

        match self.commit_staged_event(auto_snapshot, user_snapshot) {
            Ok(event_bytes) => Ok(event_bytes),
            Err(source) => {
                self.rollback_event(auto_snapshot, user_snapshot);
                Err(source)
            }
        }
    }

    /// Appends the required end-of-stream byte once.
    ///
    /// # Errors
    ///
    /// Returns [`KvIrSerializerError`] on a repeated finish, output limit, allocation failure, or
    /// counter overflow.
    pub fn finish(&mut self) -> Result<usize, KvIrSerializerError> {
        self.ensure_open()?;
        self.append_pending(&[0])?;
        self.finished = true;
        Ok(1)
    }

    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    fn metadata_json(
        limits: KvIrSerializerLimits,
        user_metadata: Option<&[u8]>,
    ) -> Result<Vec<u8>, KvIrSerializerError> {
        let mut metadata = Vec::new();
        if let Some(source) = user_metadata {
            let mut canonical = Vec::new();
            let mut scratch = CanonicalJsonScratch::new();
            scratch
                .append_to(
                    source,
                    &mut canonical,
                    CanonicalJsonLimits {
                        input_bytes: limits.metadata_bytes,
                        output_bytes: usize::try_from(limits.metadata_bytes).unwrap_or(usize::MAX),
                        nesting_depth: limits.nesting_depth,
                    },
                )
                .map_err(|_| KvIrSerializerError::InvalidUserDefinedMetadata)?;
            if canonical.first() != Some(&b'{') || canonical.last() != Some(&b'}') {
                return Err(KvIrSerializerError::InvalidUserDefinedMetadata);
            }
            checked_extend(
                &mut metadata,
                b"{\"USER_DEFINED_METADATA\":",
                limits.metadata_bytes,
                KvIrSerializerLimitResource::MetadataBytes,
                "metadata",
            )?;
            checked_extend(
                &mut metadata,
                &canonical,
                limits.metadata_bytes,
                KvIrSerializerLimitResource::MetadataBytes,
                "metadata",
            )?;
            checked_extend(
                &mut metadata,
                b",",
                limits.metadata_bytes,
                KvIrSerializerLimitResource::MetadataBytes,
                "metadata",
            )?;
        } else {
            metadata.push(b'{');
        }
        checked_extend(
            &mut metadata,
            METADATA_SUFFIX,
            limits.metadata_bytes.min(u64::from(u16::MAX)),
            KvIrSerializerLimitResource::MetadataBytes,
            "metadata",
        )?;
        Ok(metadata)
    }

    fn serialize_one_map(
        &mut self,
        input_kind: KvIrSerializerInput,
        bytes: &[u8],
        auto_generated: bool,
    ) -> Result<(), KvIrSerializerError> {
        let mut reader = MessagePackReader::new(bytes, input_kind, self.options.limits);
        let pairs = reader.read_root_map()?;
        if !auto_generated && pairs == 0 {
            checked_push(
                &mut self.sequential_stage,
                0x5e,
                self.options.limits.event_output_bytes,
                "event output",
            )?;
        } else {
            self.serialize_map_entries(&mut reader, pairs, 0, auto_generated, 1)?;
        }
        Ok(())
    }

    fn serialize_map_entries(
        &mut self,
        reader: &mut MessagePackReader<'_>,
        pairs: u32,
        parent_id: u32,
        auto_generated: bool,
        depth: u64,
    ) -> Result<(), KvIrSerializerError> {
        reader.check_depth(depth)?;
        for _ in 0..pairs {
            let key = reader.read_map_key()?;
            let value_offset = reader.position;
            let value = reader.read_header()?;
            if let Header::Unsupported(kind) = value {
                return Err(reader.error_at(value_offset, kind));
            }
            let node_type = node_type(&value).ok_or(KvIrSerializerError::SizeOverflow)?;
            let (node_id, is_new) =
                self.find_or_insert_node(auto_generated, parent_id, key, node_type)?;
            if is_new {
                self.serialize_schema_node(auto_generated, parent_id, key, node_type)?;
            }
            match value {
                Header::Map(0) => {
                    self.serialize_leaf(auto_generated, node_id, Primitive::EmptyObject)?;
                }
                Header::Map(children) => self.serialize_map_entries(
                    reader,
                    children,
                    node_id,
                    auto_generated,
                    depth
                        .checked_add(1)
                        .ok_or(KvIrSerializerError::SizeOverflow)?,
                )?,
                Header::Array(elements) => {
                    self.array_json.clear();
                    append_limited(&mut self.array_json, b"[", self.options.limits.scalar_bytes)?;
                    self.stringify_array(reader, elements, depth)?;
                    append_limited(&mut self.array_json, b"]", self.options.limits.scalar_bytes)?;
                    let array = std::mem::take(&mut self.array_json);
                    let result = self.serialize_leaf(
                        auto_generated,
                        node_id,
                        Primitive::EncodedText(&array),
                    );
                    self.array_json = array;
                    result?;
                }
                Header::Unsupported(kind) => {
                    return Err(reader.error_at(value_offset, kind));
                }
                primitive => {
                    let primitive = Primitive::try_from(primitive)
                        .map_err(|kind| reader.error_at(value_offset, kind))?;
                    self.serialize_leaf(auto_generated, node_id, primitive)?;
                }
            }
        }
        Ok(())
    }

    fn stringify_array(
        &mut self,
        reader: &mut MessagePackReader<'_>,
        elements: u32,
        depth: u64,
    ) -> Result<(), KvIrSerializerError> {
        let next_depth = depth
            .checked_add(1)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        reader.check_depth(next_depth)?;
        for index in 0..elements {
            if index != 0 {
                append_limited(&mut self.array_json, b",", self.options.limits.scalar_bytes)?;
            }
            let offset = reader.position;
            match reader.read_header()? {
                Header::Nil => append_limited(
                    &mut self.array_json,
                    b"null",
                    self.options.limits.scalar_bytes,
                )?,
                Header::Boolean(value) => append_limited(
                    &mut self.array_json,
                    if value { b"true" } else { b"false" },
                    self.options.limits.scalar_bytes,
                )?,
                Header::Integer(value) => append_i64(
                    &mut self.array_json,
                    value,
                    self.options.limits.scalar_bytes,
                )?,
                Header::Float(value) => append_cpp_float(
                    &mut self.array_json,
                    value,
                    self.options.limits.scalar_bytes,
                )?,
                Header::String(value) => append_msgpack_string(
                    &mut self.array_json,
                    value,
                    self.options.limits.scalar_bytes,
                )?,
                Header::Array(children) => {
                    append_limited(&mut self.array_json, b"[", self.options.limits.scalar_bytes)?;
                    self.stringify_array(reader, children, next_depth)?;
                    append_limited(&mut self.array_json, b"]", self.options.limits.scalar_bytes)?;
                }
                Header::Map(children) => {
                    append_limited(&mut self.array_json, b"{", self.options.limits.scalar_bytes)?;
                    self.stringify_array_map(reader, children, next_depth)?;
                    append_limited(&mut self.array_json, b"}", self.options.limits.scalar_bytes)?;
                }
                Header::Unsupported(kind) => return Err(reader.error_at(offset, kind)),
            }
        }
        Ok(())
    }

    fn stringify_array_map(
        &mut self,
        reader: &mut MessagePackReader<'_>,
        pairs: u32,
        depth: u64,
    ) -> Result<(), KvIrSerializerError> {
        reader.check_depth(depth)?;
        for index in 0..pairs {
            if index != 0 {
                append_limited(&mut self.array_json, b",", self.options.limits.scalar_bytes)?;
            }
            let key = reader.read_map_key()?;
            append_msgpack_string(&mut self.array_json, key, self.options.limits.scalar_bytes)?;
            append_limited(&mut self.array_json, b":", self.options.limits.scalar_bytes)?;
            let value_offset = reader.position;
            match reader.read_header()? {
                Header::Nil => append_limited(
                    &mut self.array_json,
                    b"null",
                    self.options.limits.scalar_bytes,
                )?,
                Header::Boolean(value) => append_limited(
                    &mut self.array_json,
                    if value { b"true" } else { b"false" },
                    self.options.limits.scalar_bytes,
                )?,
                Header::Integer(value) => append_i64(
                    &mut self.array_json,
                    value,
                    self.options.limits.scalar_bytes,
                )?,
                Header::Float(value) => append_cpp_float(
                    &mut self.array_json,
                    value,
                    self.options.limits.scalar_bytes,
                )?,
                Header::String(value) => append_msgpack_string(
                    &mut self.array_json,
                    value,
                    self.options.limits.scalar_bytes,
                )?,
                Header::Array(children) => {
                    append_limited(&mut self.array_json, b"[", self.options.limits.scalar_bytes)?;
                    self.stringify_array(reader, children, depth)?;
                    append_limited(&mut self.array_json, b"]", self.options.limits.scalar_bytes)?;
                }
                Header::Map(children) => {
                    append_limited(&mut self.array_json, b"{", self.options.limits.scalar_bytes)?;
                    self.stringify_array_map(reader, children, depth + 1)?;
                    append_limited(&mut self.array_json, b"}", self.options.limits.scalar_bytes)?;
                }
                Header::Unsupported(kind) => return Err(reader.error_at(value_offset, kind)),
            }
        }
        Ok(())
    }

    fn find_or_insert_node(
        &mut self,
        auto_generated: bool,
        parent_id: u32,
        key: &[u8],
        node_type: KvIrNodeType,
    ) -> Result<(u32, bool), KvIrSerializerError> {
        let tree = if auto_generated {
            &mut self.auto_schema
        } else {
            &mut self.user_schema
        };
        let (hash, existing_node_id) = tree.find(parent_id, key, node_type);
        if let Some(node_id) = existing_node_id {
            return Ok((node_id, false));
        }
        let node_id = tree.insert(
            hash,
            parent_id,
            key,
            node_type,
            self.options.limits.schema_nodes_per_namespace,
        )?;
        Ok((node_id, true))
    }

    fn serialize_schema_node(
        &mut self,
        auto_generated: bool,
        parent_id: u32,
        key: &[u8],
        node_type: KvIrNodeType,
    ) -> Result<(), KvIrSerializerError> {
        let tag = match node_type {
            KvIrNodeType::Integer => 0x71,
            KvIrNodeType::Float => 0x72,
            KvIrNodeType::Boolean => 0x73,
            KvIrNodeType::String => 0x74,
            KvIrNodeType::UnstructuredArray => 0x75,
            KvIrNodeType::Object => 0x76,
        };
        checked_push(
            &mut self.schema_stage,
            tag,
            self.options.limits.event_output_bytes,
            "event schema staging",
        )?;
        encode_node_id(
            &mut self.schema_stage,
            parent_id,
            auto_generated,
            [0x60, 0x61, 0x62],
            self.options.limits.event_output_bytes,
        )?;
        serialize_string(
            &mut self.schema_stage,
            key,
            [0x41, 0x42, 0x43],
            self.options.limits.event_output_bytes,
        )
    }

    fn serialize_leaf(
        &mut self,
        auto_generated: bool,
        node_id: u32,
        value: Primitive<'_>,
    ) -> Result<(), KvIrSerializerError> {
        encode_node_id(
            &mut self.sequential_stage,
            node_id,
            auto_generated,
            [0x65, 0x66, 0x67],
            self.options.limits.event_output_bytes,
        )?;
        if auto_generated {
            serialize_primitive(
                self.options.encoding,
                value,
                &mut self.sequential_stage,
                &mut self.logtype,
                self.options.limits.event_output_bytes,
            )
        } else {
            serialize_primitive(
                self.options.encoding,
                value,
                &mut self.user_values_stage,
                &mut self.logtype,
                self.options.limits.event_output_bytes,
            )
        }
    }

    fn append_pending(&mut self, bytes: &[u8]) -> Result<(), KvIrSerializerError> {
        let serialized_bytes = self
            .stats
            .serialized_bytes
            .checked_add(u64::try_from(bytes.len()).map_err(|_| KvIrSerializerError::SizeOverflow)?)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        self.compact_pending();
        let resulting = self
            .pending
            .len()
            .checked_add(bytes.len())
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        self.check_pending_output(resulting)?;
        self.pending
            .try_reserve(bytes.len())
            .map_err(|_| allocation("pending output", bytes.len()))?;
        self.pending.extend_from_slice(bytes);
        self.stats.serialized_bytes = serialized_bytes;
        Ok(())
    }

    fn commit_staged_event(
        &mut self,
        auto_snapshot: usize,
        user_snapshot: usize,
    ) -> Result<usize, KvIrSerializerError> {
        let event_bytes = self
            .schema_stage
            .len()
            .checked_add(self.sequential_stage.len())
            .and_then(|value| value.checked_add(self.user_values_stage.len()))
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        self.check_event_output(event_bytes)?;
        self.compact_pending();
        let resulting = self
            .pending
            .len()
            .checked_add(event_bytes)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        self.check_pending_output(resulting)?;

        let new_schema_nodes = self
            .auto_schema
            .nodes
            .len()
            .checked_sub(auto_snapshot)
            .and_then(|value| {
                value.checked_add(self.user_schema.nodes.len().checked_sub(user_snapshot)?)
            })
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        let mut stats = self.stats;
        stats.schema_nodes = stats
            .schema_nodes
            .checked_add(
                u64::try_from(new_schema_nodes).map_err(|_| KvIrSerializerError::SizeOverflow)?,
            )
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        stats.log_events = stats
            .log_events
            .checked_add(1)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        stats.serialized_bytes = stats
            .serialized_bytes
            .checked_add(u64::try_from(event_bytes).map_err(|_| KvIrSerializerError::SizeOverflow)?)
            .ok_or(KvIrSerializerError::SizeOverflow)?;

        self.pending
            .try_reserve(event_bytes)
            .map_err(|_| allocation("pending output", event_bytes))?;
        self.pending.extend_from_slice(&self.schema_stage);
        self.pending.extend_from_slice(&self.sequential_stage);
        self.pending.extend_from_slice(&self.user_values_stage);
        self.stats = stats;
        Ok(event_bytes)
    }

    fn rollback_event(&mut self, auto_snapshot: usize, user_snapshot: usize) {
        self.auto_schema.rollback(auto_snapshot);
        self.user_schema.rollback(user_snapshot);
        self.schema_stage.clear();
        self.sequential_stage.clear();
        self.user_values_stage.clear();
    }

    fn compact_pending(&mut self) {
        if self.pending_start == 0 {
            return;
        }
        self.pending.copy_within(self.pending_start.., 0);
        self.pending
            .truncate(self.pending.len() - self.pending_start);
        self.pending_start = 0;
    }

    const fn ensure_open(&self) -> Result<(), KvIrSerializerError> {
        if self.finished {
            Err(KvIrSerializerError::Finished)
        } else {
            Ok(())
        }
    }

    fn check_input_size(&self, bytes: &[u8]) -> Result<(), KvIrSerializerError> {
        let actual = u64::try_from(bytes.len()).map_err(|_| KvIrSerializerError::SizeOverflow)?;
        if actual > self.options.limits.input_bytes_per_map {
            Err(KvIrSerializerError::Limit(
                KvIrSerializerLimitViolation::new(
                    KvIrSerializerLimitResource::InputBytesPerMap,
                    actual,
                    self.options.limits.input_bytes_per_map,
                ),
            ))
        } else {
            Ok(())
        }
    }

    fn check_event_output(&self, actual: usize) -> Result<(), KvIrSerializerError> {
        check_limit(
            actual,
            self.options.limits.event_output_bytes,
            KvIrSerializerLimitResource::EventOutputBytes,
        )
    }

    fn check_pending_output(&self, actual: usize) -> Result<(), KvIrSerializerError> {
        check_limit(
            actual,
            self.options.limits.pending_output_bytes,
            KvIrSerializerLimitResource::PendingOutputBytes,
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum Header<'a> {
    Nil,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(&'a [u8]),
    Array(u32),
    Map(u32),
    Unsupported(KvIrMessagePackErrorKind),
}

const fn node_type(value: &Header<'_>) -> Option<KvIrNodeType> {
    match value {
        Header::Integer(_) => Some(KvIrNodeType::Integer),
        Header::Float(_) => Some(KvIrNodeType::Float),
        Header::Boolean(_) => Some(KvIrNodeType::Boolean),
        Header::String(_) => Some(KvIrNodeType::String),
        Header::Array(_) => Some(KvIrNodeType::UnstructuredArray),
        Header::Nil | Header::Map(_) => Some(KvIrNodeType::Object),
        Header::Unsupported(_) => None,
    }
}

#[derive(Clone, Copy, Debug)]
enum Primitive<'a> {
    Nil,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(&'a [u8]),
    EncodedText(&'a [u8]),
    EmptyObject,
}

impl<'a> TryFrom<Header<'a>> for Primitive<'a> {
    type Error = KvIrMessagePackErrorKind;

    fn try_from(value: Header<'a>) -> Result<Self, Self::Error> {
        match value {
            Header::Nil => Ok(Self::Nil),
            Header::Boolean(value) => Ok(Self::Boolean(value)),
            Header::Integer(value) => Ok(Self::Integer(value)),
            Header::Float(value) => Ok(Self::Float(value)),
            Header::String(value) => Ok(Self::String(value)),
            Header::Unsupported(kind) => Err(kind),
            Header::Array(_) | Header::Map(_) => Err(KvIrMessagePackErrorKind::ReservedMarker),
        }
    }
}

#[derive(Debug)]
struct MessagePackReader<'a> {
    bytes: &'a [u8],
    position: usize,
    input: KvIrSerializerInput,
    limits: KvIrSerializerLimits,
    values: u64,
}

impl<'a> MessagePackReader<'a> {
    const fn new(
        bytes: &'a [u8],
        input: KvIrSerializerInput,
        limits: KvIrSerializerLimits,
    ) -> Self {
        Self {
            bytes,
            position: 0,
            input,
            limits,
            values: 0,
        }
    }

    /// Reads the root map while keeping the overwhelmingly common fixed-map marker on a small
    /// expected-type path. All other markers are decoded by `read_header` so their consumption and
    /// error behavior remain unchanged.
    #[inline]
    fn read_root_map(&mut self) -> Result<u32, KvIrSerializerError> {
        let offset = self.position;
        if let Some(marker @ 0x80..=0x8f) = self.bytes.get(self.position).copied() {
            self.consume_peeked_value()?;
            return Ok(u32::from(marker & 0x0f));
        }
        let Header::Map(pairs) = self.read_header()? else {
            return Err(self.error_at(offset, KvIrMessagePackErrorKind::RootMustBeMap));
        };
        Ok(pairs)
    }

    /// Reads a map key while keeping the overwhelmingly common fixed-string marker on a small
    /// expected-type path. All other markers are decoded by `read_header` so their consumption and
    /// error behavior remain unchanged.
    #[inline]
    fn read_map_key(&mut self) -> Result<&'a [u8], KvIrSerializerError> {
        let offset = self.position;
        if let Some(marker @ 0xa0..=0xbf) = self.bytes.get(self.position).copied() {
            self.consume_peeked_value()?;
            return self.read_string(usize::from(marker & 0x1f));
        }
        let Header::String(key) = self.read_header()? else {
            return Err(self.error_at(offset, KvIrMessagePackErrorKind::MapKeyMustBeString));
        };
        Ok(key)
    }

    /// Accounts for a marker that an expected-type reader has already verified at `position`.
    /// This deliberately mirrors `read_header`'s marker-consumption and value-limit ordering.
    #[inline]
    fn consume_peeked_value(&mut self) -> Result<(), KvIrSerializerError> {
        debug_assert!(self.bytes.get(self.position).is_some());
        self.position += 1;
        self.values = self
            .values
            .checked_add(1)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        if self.values > self.limits.values_per_map {
            return Err(KvIrSerializerError::Limit(
                KvIrSerializerLimitViolation::new(
                    KvIrSerializerLimitResource::ValuesPerMap,
                    self.values,
                    self.limits.values_per_map,
                ),
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn read_header(&mut self) -> Result<Header<'a>, KvIrSerializerError> {
        let offset = self.position;
        let marker = self.read_byte()?;
        self.values = self
            .values
            .checked_add(1)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        if self.values > self.limits.values_per_map {
            return Err(KvIrSerializerError::Limit(
                KvIrSerializerLimitViolation::new(
                    KvIrSerializerLimitResource::ValuesPerMap,
                    self.values,
                    self.limits.values_per_map,
                ),
            ));
        }
        let value = match marker {
            0x00..=0x7f => Header::Integer(i64::from(marker)),
            0x80..=0x8f => Header::Map(u32::from(marker & 0x0f)),
            0x90..=0x9f => Header::Array(u32::from(marker & 0x0f)),
            0xa0..=0xbf => Header::String(self.read_string(usize::from(marker & 0x1f))?),
            0xc0 => Header::Nil,
            0xc1 => return Err(self.error_at(offset, KvIrMessagePackErrorKind::ReservedMarker)),
            0xc2 => Header::Boolean(false),
            0xc3 => Header::Boolean(true),
            0xc4 => {
                let length = usize::from(self.read_byte()?);
                self.skip(length)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedBinary)
            }
            0xc5 => {
                let length = usize::from(self.read_u16()?);
                self.skip(length)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedBinary)
            }
            0xc6 => {
                let encoded_length = self.read_u32()?;
                let length = self.usize_from_u32(encoded_length)?;
                self.skip(length)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedBinary)
            }
            0xc7 => {
                let length = usize::from(self.read_byte()?);
                self.skip(
                    length
                        .checked_add(1)
                        .ok_or(KvIrSerializerError::SizeOverflow)?,
                )?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xc8 => {
                let length = usize::from(self.read_u16()?);
                self.skip(
                    length
                        .checked_add(1)
                        .ok_or(KvIrSerializerError::SizeOverflow)?,
                )?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xc9 => {
                let encoded_length = self.read_u32()?;
                let length = self.usize_from_u32(encoded_length)?;
                self.skip(
                    length
                        .checked_add(1)
                        .ok_or(KvIrSerializerError::SizeOverflow)?,
                )?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xca => Header::Float(f64::from(f32::from_bits(self.read_u32()?))),
            0xcb => Header::Float(f64::from_bits(self.read_u64()?)),
            0xcc => Header::Integer(i64::from(self.read_byte()?)),
            0xcd => Header::Integer(i64::from(self.read_u16()?)),
            0xce => Header::Integer(i64::from(self.read_u32()?)),
            0xcf => {
                let value = self.read_u64()?;
                Header::Integer(i64::try_from(value).map_err(|_| {
                    self.error_at(offset, KvIrMessagePackErrorKind::IntegerOutOfRange)
                })?)
            }
            0xd0 => Header::Integer(i64::from(i8::from_be_bytes([self.read_byte()?]))),
            0xd1 => Header::Integer(i64::from(i16::from_be_bytes(self.read_array()?))),
            0xd2 => Header::Integer(i64::from(i32::from_be_bytes(self.read_array()?))),
            0xd3 => Header::Integer(i64::from_be_bytes(self.read_array()?)),
            0xd4 => {
                self.skip(2)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xd5 => {
                self.skip(3)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xd6 => {
                self.skip(5)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xd7 => {
                self.skip(9)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xd8 => {
                self.skip(17)?;
                Header::Unsupported(KvIrMessagePackErrorKind::UnsupportedExtension)
            }
            0xd9 => {
                let length = usize::from(self.read_byte()?);
                Header::String(self.read_string(length)?)
            }
            0xda => {
                let length = usize::from(self.read_u16()?);
                Header::String(self.read_string(length)?)
            }
            0xdb => {
                let encoded_length = self.read_u32()?;
                let length = self.usize_from_u32(encoded_length)?;
                Header::String(self.read_string(length)?)
            }
            0xdc => Header::Array(u32::from(self.read_u16()?)),
            0xdd => Header::Array(self.read_u32()?),
            0xde => Header::Map(u32::from(self.read_u16()?)),
            0xdf => Header::Map(self.read_u32()?),
            0xe0..=0xff => Header::Integer(i64::from(i8::from_be_bytes([marker]))),
        };
        Ok(value)
    }

    fn read_string(&mut self, length: usize) -> Result<&'a [u8], KvIrSerializerError> {
        check_limit(
            length,
            self.limits.scalar_bytes,
            KvIrSerializerLimitResource::ScalarBytes,
        )?;
        self.read_bytes(length)
    }

    fn read_byte(&mut self) -> Result<u8, KvIrSerializerError> {
        let Some(value) = self.bytes.get(self.position).copied() else {
            return Err(self.error(KvIrMessagePackErrorKind::Truncated));
        };
        self.position += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, KvIrSerializerError> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    fn read_u32(&mut self) -> Result<u32, KvIrSerializerError> {
        Ok(u32::from_be_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, KvIrSerializerError> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], KvIrSerializerError> {
        let bytes = self.read_bytes(N)?;
        bytes
            .try_into()
            .map_err(|_| self.error(KvIrMessagePackErrorKind::Truncated))
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], KvIrSerializerError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        let Some(bytes) = self.bytes.get(self.position..end) else {
            self.position = self.bytes.len();
            return Err(self.error(KvIrMessagePackErrorKind::Truncated));
        };
        self.position = end;
        Ok(bytes)
    }

    fn skip(&mut self, length: usize) -> Result<(), KvIrSerializerError> {
        self.read_bytes(length).map(|_| ())
    }

    fn usize_from_u32(&self, value: u32) -> Result<usize, KvIrSerializerError> {
        usize::try_from(value).map_err(|_| self.error(KvIrMessagePackErrorKind::LengthOutOfRange))
    }

    const fn check_depth(&self, actual: u64) -> Result<(), KvIrSerializerError> {
        if actual > self.limits.nesting_depth {
            Err(KvIrSerializerError::Limit(
                KvIrSerializerLimitViolation::new(
                    KvIrSerializerLimitResource::NestingDepth,
                    actual,
                    self.limits.nesting_depth,
                ),
            ))
        } else {
            Ok(())
        }
    }

    const fn error(&self, kind: KvIrMessagePackErrorKind) -> KvIrSerializerError {
        self.error_at(self.position, kind)
    }

    const fn error_at(&self, offset: usize, kind: KvIrMessagePackErrorKind) -> KvIrSerializerError {
        KvIrSerializerError::MessagePack {
            input: self.input,
            offset,
            kind,
        }
    }
}

fn serialize_primitive(
    encoding: KvIrEncoding,
    value: Primitive<'_>,
    output: &mut Vec<u8>,
    logtype: &mut Vec<u8>,
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    match value {
        Primitive::Nil => checked_push(output, 0x5f, limit, "event value staging"),
        Primitive::Boolean(value) => checked_push(
            output,
            if value { 0x57 } else { 0x58 },
            limit,
            "event value staging",
        ),
        Primitive::Integer(value) => serialize_integer(output, value, limit),
        Primitive::Float(value) => {
            checked_push(output, 0x56, limit, "event value staging")?;
            checked_extend(
                output,
                &value.to_bits().to_be_bytes(),
                limit,
                KvIrSerializerLimitResource::EventOutputBytes,
                "event value staging",
            )
        }
        Primitive::String(value) => {
            if value.contains(&b' ') {
                serialize_clp_text(encoding, value, output, logtype, limit)
            } else {
                serialize_string(output, value, [0x41, 0x42, 0x43], limit)
            }
        }
        Primitive::EncodedText(value) => {
            serialize_clp_text(encoding, value, output, logtype, limit)
        }
        Primitive::EmptyObject => checked_push(output, 0x5e, limit, "event value staging"),
    }
}

fn serialize_integer(
    output: &mut Vec<u8>,
    value: i64,
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    if let Ok(value) = i8::try_from(value) {
        checked_extend(
            output,
            &[0x51, value.to_be_bytes()[0]],
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "event value staging",
        )
    } else if let Ok(value) = i16::try_from(value) {
        checked_push(output, 0x52, limit, "event value staging")?;
        checked_extend(
            output,
            &value.to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "event value staging",
        )
    } else if let Ok(value) = i32::try_from(value) {
        checked_push(output, 0x53, limit, "event value staging")?;
        checked_extend(
            output,
            &value.to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "event value staging",
        )
    } else {
        checked_push(output, 0x54, limit, "event value staging")?;
        checked_extend(
            output,
            &value.to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "event value staging",
        )
    }
}

fn serialize_clp_text(
    encoding: KvIrEncoding,
    message: &[u8],
    output: &mut Vec<u8>,
    logtype: &mut Vec<u8>,
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    checked_push(
        output,
        match encoding {
            KvIrEncoding::FourByte => 0x59,
            KvIrEncoding::EightByte => 0x5a,
        },
        limit,
        "CLP value staging",
    )?;
    logtype.clear();
    let mut previous_end = 0_usize;
    let mut scan_end = 0_usize;
    let mut constant_escapes = 0_usize;
    while let Some(variable) = next_variable(message, scan_end, &mut constant_escapes) {
        append_escaped_constant(
            &message[previous_end..variable.begin],
            constant_escapes,
            logtype,
            limit,
        )?;
        let token = &message[variable.begin..variable.end];
        if let Some(encoded) = encode_float(token, encoding) {
            checked_push(logtype, FLOAT_PLACEHOLDER, limit, "CLP logtype")?;
            serialize_encoded_variable(output, encoded, limit)?;
        } else if let Some(encoded) = encode_integer_token(token, encoding) {
            checked_push(logtype, INTEGER_PLACEHOLDER, limit, "CLP logtype")?;
            serialize_encoded_variable(output, encoded, limit)?;
        } else {
            checked_push(logtype, DICTIONARY_PLACEHOLDER, limit, "CLP logtype")?;
            serialize_string(output, token, [0x11, 0x12, 0x13], limit)?;
        }
        previous_end = variable.end;
        scan_end = variable.end;
        constant_escapes = 0;
    }
    append_escaped_constant(&message[previous_end..], constant_escapes, logtype, limit)?;
    serialize_string(output, logtype, [0x21, 0x22, 0x23], limit)
}

#[derive(Clone, Copy)]
enum EncodedVariable {
    Four(i32),
    Eight(i64),
}

fn serialize_encoded_variable(
    output: &mut Vec<u8>,
    value: EncodedVariable,
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    match value {
        EncodedVariable::Four(value) => {
            checked_push(output, 0x18, limit, "CLP value staging")?;
            checked_extend(
                output,
                &value.to_be_bytes(),
                limit,
                KvIrSerializerLimitResource::EventOutputBytes,
                "CLP value staging",
            )
        }
        EncodedVariable::Eight(value) => {
            checked_push(output, 0x19, limit, "CLP value staging")?;
            checked_extend(
                output,
                &value.to_be_bytes(),
                limit,
                KvIrSerializerLimitResource::EventOutputBytes,
                "CLP value staging",
            )
        }
    }
}

fn encode_integer_token(value: &[u8], encoding: KvIrEncoding) -> Option<EncodedVariable> {
    let (negative, start) = if value.first() == Some(&b'-') {
        if !value
            .get(1)
            .is_some_and(|byte| (b'1'..=b'9').contains(byte))
        {
            return None;
        }
        (true, 1)
    } else {
        let first = *value.first()?;
        if !first.is_ascii_digit() || (value.len() > 1 && first == b'0') {
            return None;
        }
        (false, 0)
    };
    let mut magnitude = 0_u64;
    for digit in &value[start..] {
        if !digit.is_ascii_digit() {
            return None;
        }
        magnitude = magnitude
            .checked_mul(10)?
            .checked_add(u64::from(*digit - b'0'))?;
    }
    let signed = if negative {
        if magnitude == 1_u64 << 63 {
            i64::MIN
        } else {
            -i64::try_from(magnitude).ok()?
        }
    } else {
        i64::try_from(magnitude).ok()?
    };
    match encoding {
        KvIrEncoding::FourByte => i32::try_from(signed).ok().map(EncodedVariable::Four),
        KvIrEncoding::EightByte => Some(EncodedVariable::Eight(signed)),
    }
}

fn encode_float(value: &[u8], encoding: KvIrEncoding) -> Option<EncodedVariable> {
    let negative = value.first() == Some(&b'-');
    let start = usize::from(negative);
    let max_digits = match encoding {
        KvIrEncoding::FourByte => 8,
        KvIrEncoding::EightByte => 16,
    };
    if value.is_empty() || value.len() > max_digits + 1 + usize::from(negative) {
        return None;
    }
    let mut digits = 0_u64;
    let mut digit_count = 0_usize;
    let mut decimal_position = None;
    for (position, byte) in value.iter().copied().enumerate().skip(start) {
        if byte.is_ascii_digit() {
            digits = digits
                .checked_mul(10)?
                .checked_add(u64::from(byte - b'0'))?;
            digit_count += 1;
        } else if byte == b'.' && decimal_position.is_none() {
            decimal_position = value.len().checked_sub(position + 1);
        } else {
            return None;
        }
    }
    let decimal_position = decimal_position?;
    if decimal_position == 0 || digit_count == 0 || digit_count > max_digits {
        return None;
    }
    match encoding {
        KvIrEncoding::FourByte => {
            if digits > (1_u64 << 25) - 1 {
                return None;
            }
            let mut encoded = u32::from(negative);
            encoded = (encoded << 25) | u32::try_from(digits).ok()?;
            encoded = (encoded << 3) | u32::try_from(digit_count - 1).ok()? & 0x07;
            encoded = (encoded << 3) | u32::try_from(decimal_position - 1).ok()? & 0x07;
            Some(EncodedVariable::Four(i32::from_be_bytes(
                encoded.to_be_bytes(),
            )))
        }
        KvIrEncoding::EightByte => {
            let mut encoded = u64::from(negative);
            encoded = (encoded << 55) | digits & ((1_u64 << 54) - 1);
            encoded = (encoded << 4) | u64::try_from(digit_count - 1).ok()? & 0x0f;
            encoded = (encoded << 4) | u64::try_from(decimal_position - 1).ok()? & 0x0f;
            Some(EncodedVariable::Eight(i64::from_be_bytes(
                encoded.to_be_bytes(),
            )))
        }
    }
}

#[derive(Clone, Copy)]
struct VariableSpan {
    begin: usize,
    end: usize,
}

fn next_variable(
    message: &[u8],
    mut end: usize,
    constant_escapes: &mut usize,
) -> Option<VariableSpan> {
    while end < message.len() {
        let mut begin = end;
        while begin < message.len() && is_delimiter(message[begin]) {
            *constant_escapes += usize::from(is_marker(message[begin]));
            begin += 1;
        }
        if begin == message.len() {
            return None;
        }
        let mut contains_decimal_digit = false;
        let mut contains_alphabet = false;
        let mut all_hexadecimal = true;
        let mut token_escapes = 0_usize;
        end = begin;
        while end < message.len() {
            let byte = message[end];
            if byte.is_ascii_digit() {
                contains_decimal_digit = true;
            } else if byte.is_ascii_alphabetic() {
                contains_alphabet = true;
            } else if is_delimiter(byte) {
                break;
            }
            all_hexadecimal &= byte.is_ascii_hexdigit();
            token_escapes += usize::from(is_marker(byte));
            end += 1;
        }
        if contains_decimal_digit
            || (begin > 0 && message[begin - 1] == b'=' && contains_alphabet)
            || (end - begin >= 2 && all_hexadecimal)
        {
            return Some(VariableSpan { begin, end });
        }
        *constant_escapes += token_escapes;
    }
    None
}

const fn is_delimiter(byte: u8) -> bool {
    !matches!(
        byte,
        b'+' | b'-' | b'.' | b'0'..=b'9' | b'A'..=b'Z' | b'\\' | b'_' | b'a'..=b'z'
    )
}

const fn is_marker(byte: u8) -> bool {
    matches!(
        byte,
        ESCAPE_MARKER | INTEGER_PLACEHOLDER | DICTIONARY_PLACEHOLDER | FLOAT_PLACEHOLDER
    )
}

fn append_escaped_constant(
    constant: &[u8],
    escapes: usize,
    output: &mut Vec<u8>,
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    if escapes == 0 {
        return append_limited(output, constant, limit);
    }
    for byte in constant {
        if is_marker(*byte) {
            checked_push(output, ESCAPE_MARKER, limit, "CLP logtype")?;
        }
        checked_push(output, *byte, limit, "CLP logtype")?;
    }
    Ok(())
}

fn encode_node_id(
    output: &mut Vec<u8>,
    node_id: u32,
    auto_generated: bool,
    tags: [u8; 3],
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    if let Ok(mut value) = i8::try_from(node_id) {
        if auto_generated {
            value = !value;
        }
        checked_extend(
            output,
            &[tags[0], value.to_be_bytes()[0]],
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "node ID staging",
        )
    } else if let Ok(mut value) = i16::try_from(node_id) {
        if auto_generated {
            value = !value;
        }
        checked_push(output, tags[1], limit, "node ID staging")?;
        checked_extend(
            output,
            &value.to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "node ID staging",
        )
    } else if let Ok(mut value) = i32::try_from(node_id) {
        if auto_generated {
            value = !value;
        }
        checked_push(output, tags[2], limit, "node ID staging")?;
        checked_extend(
            output,
            &value.to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "node ID staging",
        )
    } else {
        Err(KvIrSerializerError::Limit(
            KvIrSerializerLimitViolation::new(
                KvIrSerializerLimitResource::SchemaNodesPerNamespace,
                u64::from(node_id),
                2_147_483_647,
            ),
        ))
    }
}

fn serialize_string(
    output: &mut Vec<u8>,
    value: &[u8],
    tags: [u8; 3],
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    if u8::try_from(value.len()).is_ok() {
        checked_extend(
            output,
            &[
                tags[0],
                u8::try_from(value.len()).map_err(|_| KvIrSerializerError::SizeOverflow)?,
            ],
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "string staging",
        )?;
    } else if u16::try_from(value.len()).is_ok() {
        checked_push(output, tags[1], limit, "string staging")?;
        checked_extend(
            output,
            &u16::try_from(value.len())
                .map_err(|_| KvIrSerializerError::SizeOverflow)?
                .to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "string staging",
        )?;
    } else {
        let length = u32::try_from(value.len()).map_err(|_| {
            KvIrSerializerError::Limit(KvIrSerializerLimitViolation::new(
                KvIrSerializerLimitResource::ScalarBytes,
                u64::try_from(value.len()).unwrap_or(u64::MAX),
                u64::from(u32::MAX),
            ))
        })?;
        checked_push(output, tags[2], limit, "string staging")?;
        checked_extend(
            output,
            &length.to_be_bytes(),
            limit,
            KvIrSerializerLimitResource::EventOutputBytes,
            "string staging",
        )?;
    }
    checked_extend(
        output,
        value,
        limit,
        KvIrSerializerLimitResource::EventOutputBytes,
        "string staging",
    )
}

fn append_msgpack_string(
    output: &mut Vec<u8>,
    value: &[u8],
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    checked_push(output, b'"', limit, "array JSON")?;
    let mut position = 0;
    while position < value.len() {
        let next_escape = value[position..]
            .iter()
            .position(|byte| needs_json_escape(*byte))
            .map_or(value.len(), |offset| position + offset);
        if next_escape != position {
            append_limited(output, &value[position..next_escape], limit)?;
        }
        if next_escape == value.len() {
            break;
        }

        let byte = value[next_escape];
        match byte {
            b'\\' => append_limited(output, br"\\", limit)?,
            b'"' => append_limited(output, br#"\""#, limit)?,
            b'/' => append_limited(output, br"\/", limit)?,
            0x08 => append_limited(output, br"\b", limit)?,
            0x0c => append_limited(output, br"\f", limit)?,
            b'\n' => append_limited(output, br"\n", limit)?,
            b'\r' => append_limited(output, br"\r", limit)?,
            b'\t' => append_limited(output, br"\t", limit)?,
            0x00..=0x1f | 0x7f => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let escaped = [
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[usize::from(byte >> 4)],
                    HEX[usize::from(byte & 0x0f)],
                ];
                append_limited(output, &escaped, limit)?;
            }
            _ => unreachable!("the scan stops only at bytes requiring JSON escaping"),
        }
        position = next_escape + 1;
    }
    checked_push(output, b'"', limit, "array JSON")
}

const fn needs_json_escape(byte: u8) -> bool {
    matches!(byte, b'\\' | b'"' | b'/' | 0x00..=0x1f | 0x7f)
}

fn append_i64(output: &mut Vec<u8>, value: i64, limit: u64) -> Result<(), KvIrSerializerError> {
    let mut buffer = itoa::Buffer::new();
    append_limited(output, buffer.format(value).as_bytes(), limit)
}

// `msgpack::object` uses an ordinary default-configured C++ ostream: six significant digits and
// `%g`-style fixed/scientific selection. Keep the conversion in a fixed-capacity writer so arrays
// containing floats do not allocate a temporary `String` for every value.
fn append_cpp_float(
    output: &mut Vec<u8>,
    value: f64,
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    if value.is_nan() {
        return append_limited(
            output,
            if value.is_sign_negative() {
                b"-nan"
            } else {
                b"nan"
            },
            limit,
        );
    }
    if value == f64::INFINITY {
        return append_limited(output, b"inf", limit);
    }
    if value == f64::NEG_INFINITY {
        return append_limited(output, b"-inf", limit);
    }
    if value == 0.0 {
        return append_limited(
            output,
            if value.is_sign_negative() {
                b"-0"
            } else {
                b"0"
            },
            limit,
        );
    }

    let mut formatted = CppFloatBuffer::new();
    // Every finite binary64 exponent is far inside `i32`; the cast only selects `%g` layout.
    #[allow(clippy::cast_possible_truncation)]
    let exponent = value.abs().log10().floor() as i32;
    if (-4..6).contains(&exponent) {
        let places = usize::try_from((5 - exponent).max(0)).unwrap_or(0);
        fmt::write(&mut formatted, format_args!("{value:.places$}"))
            .map_err(|_| KvIrSerializerError::SizeOverflow)?;
        formatted.trim_fraction();
    } else {
        fmt::write(&mut formatted, format_args!("{value:.5e}"))
            .map_err(|_| KvIrSerializerError::SizeOverflow)?;
        formatted.normalize_scientific()?;
    }
    append_limited(output, formatted.as_bytes(), limit)
}

struct CppFloatBuffer {
    bytes: [u8; CPP_FLOAT_BUFFER_BYTES],
    len: usize,
}

impl CppFloatBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; CPP_FLOAT_BUFFER_BYTES],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn trim_fraction(&mut self) {
        if !self.as_bytes().contains(&b'.') {
            return;
        }
        while self.as_bytes().last() == Some(&b'0') {
            self.len -= 1;
        }
        if self.as_bytes().last() == Some(&b'.') {
            self.len -= 1;
        }
    }

    fn normalize_scientific(&mut self) -> Result<(), KvIrSerializerError> {
        let exponent_index = self
            .as_bytes()
            .iter()
            .position(|byte| *byte == b'e')
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        let mut mantissa_end = exponent_index;
        while mantissa_end > 0 && self.bytes[mantissa_end - 1] == b'0' {
            mantissa_end -= 1;
        }
        if mantissa_end > 0 && self.bytes[mantissa_end - 1] == b'.' {
            mantissa_end -= 1;
        }

        let mut exponent_start = exponent_index + 1;
        let exponent_sign = if self.bytes.get(exponent_start) == Some(&b'-') {
            exponent_start += 1;
            b'-'
        } else {
            if self.bytes.get(exponent_start) == Some(&b'+') {
                exponent_start += 1;
            }
            b'+'
        };
        while self.bytes.get(exponent_start) == Some(&b'0') {
            exponent_start += 1;
        }
        let exponent_digits = self
            .bytes
            .get(exponent_start..self.len)
            .ok_or(KvIrSerializerError::SizeOverflow)?;
        if exponent_digits.is_empty() || exponent_digits.len() > 3 {
            return Err(KvIrSerializerError::SizeOverflow);
        }
        let mut digits = [0_u8; 3];
        digits[..exponent_digits.len()].copy_from_slice(exponent_digits);
        let digit_count = exponent_digits.len();

        self.len = mantissa_end;
        self.write_bytes(b"e")
            .map_err(|_| KvIrSerializerError::SizeOverflow)?;
        self.write_bytes(&[exponent_sign])
            .map_err(|_| KvIrSerializerError::SizeOverflow)?;
        if digit_count == 1 {
            self.write_bytes(b"0")
                .map_err(|_| KvIrSerializerError::SizeOverflow)?;
        }
        self.write_bytes(&digits[..digit_count])
            .map_err(|_| KvIrSerializerError::SizeOverflow)
    }

    fn write_bytes(&mut self, value: &[u8]) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        destination.copy_from_slice(value);
        self.len = end;
        Ok(())
    }
}

impl fmt::Write for CppFloatBuffer {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.write_bytes(value.as_bytes())
    }
}

fn append_limited(
    output: &mut Vec<u8>,
    bytes: &[u8],
    limit: u64,
) -> Result<(), KvIrSerializerError> {
    checked_extend(
        output,
        bytes,
        limit,
        KvIrSerializerLimitResource::ScalarBytes,
        "array JSON or CLP logtype",
    )
}

fn checked_push(
    output: &mut Vec<u8>,
    byte: u8,
    limit: u64,
    resource: &'static str,
) -> Result<(), KvIrSerializerError> {
    checked_extend(
        output,
        &[byte],
        limit,
        KvIrSerializerLimitResource::EventOutputBytes,
        resource,
    )
}

fn checked_extend(
    output: &mut Vec<u8>,
    bytes: &[u8],
    limit: u64,
    limit_resource: KvIrSerializerLimitResource,
    allocation_resource: &'static str,
) -> Result<(), KvIrSerializerError> {
    let resulting = output
        .len()
        .checked_add(bytes.len())
        .ok_or(KvIrSerializerError::SizeOverflow)?;
    check_limit(resulting, limit, limit_resource)?;
    output
        .try_reserve(bytes.len())
        .map_err(|_| allocation(allocation_resource, bytes.len()))?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn check_limit(
    actual: usize,
    limit: u64,
    resource: KvIrSerializerLimitResource,
) -> Result<(), KvIrSerializerError> {
    let actual = u64::try_from(actual).map_err(|_| KvIrSerializerError::SizeOverflow)?;
    if actual > limit {
        Err(KvIrSerializerError::Limit(
            KvIrSerializerLimitViolation::new(resource, actual, limit),
        ))
    } else {
        Ok(())
    }
}

const fn allocation(resource: &'static str, requested_additional: usize) -> KvIrSerializerError {
    KvIrSerializerError::AllocationFailed {
        resource,
        requested_additional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUR_BYTE_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
    const EIGHT_BYTE_ORACLE_HEX: &str =
        include_str!("../../tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");

    #[test]
    fn reproduces_current_cpp_oracles_for_both_variable_widths() {
        for (encoding, oracle) in [
            (KvIrEncoding::FourByte, FOUR_BYTE_ORACLE_HEX),
            (KvIrEncoding::EightByte, EIGHT_BYTE_ORACLE_HEX),
        ] {
            let mut serializer = KvIrSerializer::new(
                KvIrSerializerOptions::new(encoding),
                Some(br#"{"fixture":"rust-kv-ir-reader-v1"}"#),
            )
            .expect("create serializer");
            serializer
                .change_utc_offset(3_600_000)
                .expect("change offset");
            serializer
                .serialize_log_event_from_msgpack_maps(&auto_map(), &user_map())
                .expect("serialize event");
            serializer.finish().expect("finish stream");
            assert_eq!(
                decode_hex(oracle),
                serializer.pending_output(),
                "{encoding:?}"
            );
            assert_eq!(1, serializer.stats().log_events());
            assert_eq!(7, serializer.stats().schema_nodes());
        }
    }

    #[test]
    fn stack_float_formatter_matches_cpp_stream_spellings() {
        let cases = [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.234_567_89, "1.23457"),
            (999_999.0, "999999"),
            (1_000_000.0, "1e+06"),
            (0.000_1, "0.0001"),
            (0.000_099_999_9, "9.99999e-05"),
            (f64::MAX, "1.79769e+308"),
            (f64::from_bits(1), "4.94066e-324"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (f64::from_bits(f64::NAN.to_bits() | (1_u64 << 63)), "-nan"),
        ];
        for (value, expected) in cases {
            let mut formatted = Vec::new();
            append_cpp_float(&mut formatted, value, u64::MAX).expect("format array float");
            assert_eq!(expected.as_bytes(), formatted, "{value:?}");
        }
    }

    #[test]
    fn stack_float_formatter_normalizes_scientific_exponents() {
        for (source, expected) in [
            ("1.00000e6", "1e+06"),
            ("-1.25000e-5", "-1.25e-05"),
            ("9.99999e100", "9.99999e+100"),
        ] {
            let mut buffer = CppFloatBuffer::new();
            buffer.write_bytes(source.as_bytes()).expect("source fits");
            buffer.normalize_scientific().expect("normalize exponent");
            assert_eq!(expected.as_bytes(), buffer.as_bytes(), "{source}");
        }
    }

    #[test]
    fn msgpack_string_run_copy_matches_bytewise_escaping() {
        let all_bytes = (u8::MIN..=u8::MAX).collect::<Vec<_>>();
        for value in [
            b"ordinary UTF-8: caf\xc3\xa9".as_slice(),
            all_bytes.as_slice(),
        ] {
            let mut expected = Vec::new();
            checked_push(&mut expected, b'"', u64::MAX, "array JSON").unwrap();
            for byte in value {
                match *byte {
                    b'\\' => append_limited(&mut expected, br"\\", u64::MAX).unwrap(),
                    b'"' => append_limited(&mut expected, br#"\""#, u64::MAX).unwrap(),
                    b'/' => append_limited(&mut expected, br"\/", u64::MAX).unwrap(),
                    0x08 => append_limited(&mut expected, br"\b", u64::MAX).unwrap(),
                    0x0c => append_limited(&mut expected, br"\f", u64::MAX).unwrap(),
                    b'\n' => append_limited(&mut expected, br"\n", u64::MAX).unwrap(),
                    b'\r' => append_limited(&mut expected, br"\r", u64::MAX).unwrap(),
                    b'\t' => append_limited(&mut expected, br"\t", u64::MAX).unwrap(),
                    0x00..=0x1f | 0x7f => {
                        const HEX: &[u8; 16] = b"0123456789abcdef";
                        append_limited(
                            &mut expected,
                            &[
                                b'\\',
                                b'u',
                                b'0',
                                b'0',
                                HEX[usize::from(*byte >> 4)],
                                HEX[usize::from(*byte & 0x0f)],
                            ],
                            u64::MAX,
                        )
                        .unwrap();
                    }
                    byte => checked_push(&mut expected, byte, u64::MAX, "array JSON").unwrap(),
                }
            }
            checked_push(&mut expected, b'"', u64::MAX, "array JSON").unwrap();

            let mut actual = Vec::new();
            append_msgpack_string(&mut actual, value, u64::MAX).unwrap();
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn expected_type_reader_fast_paths_cover_every_fixed_header() {
        for pairs in 0..=15_u8 {
            let bytes = [0x80 | pairs];
            assert_root_reader_parity(
                &bytes,
                KvIrSerializerLimits::DEFAULT,
                &format!("fixed map with {pairs} pair(s)"),
            );
        }

        for length in 0..=31_usize {
            let mut bytes = Vec::with_capacity(length + 1);
            bytes.push(0xa0 | u8::try_from(length).expect("fixed-string length fits u8"));
            bytes.extend(
                (0..length)
                    .map(|index| u8::try_from(index).expect("fixed-string payload index fits u8")),
            );
            assert_map_key_reader_parity(
                &bytes,
                KvIrSerializerLimits::DEFAULT,
                &format!("fixed string with {length} byte(s)"),
            );
        }
    }

    #[test]
    fn expected_type_reader_fallback_matches_generic_reader_for_every_marker() {
        for marker in u8::MIN..=u8::MAX {
            // Thirty-two zero bytes complete every fixed-width marker and the largest fixed string
            // or extension. Variable-length markers decode a zero length from the prefix.
            let mut bytes = vec![marker];
            bytes.resize(33, 0);
            let context = format!("MessagePack marker {marker:#04x}");
            assert_root_reader_parity(&bytes, KvIrSerializerLimits::DEFAULT, &context);
            assert_map_key_reader_parity(&bytes, KvIrSerializerLimits::DEFAULT, &context);
        }
    }

    #[test]
    fn expected_type_reader_fast_paths_preserve_truncation_and_limit_ordering() {
        for encoded_length in 1..=31_usize {
            let marker = 0xa0 | u8::try_from(encoded_length).expect("fixed-string length fits u8");
            for available_bytes in 0..encoded_length {
                let mut bytes = vec![marker];
                bytes.resize(available_bytes + 1, b'x');
                assert_map_key_reader_parity(
                    &bytes,
                    KvIrSerializerLimits::DEFAULT,
                    &format!(
                        "fixed string length {encoded_length} truncated to {available_bytes} \
                         byte(s)"
                    ),
                );
            }

            let scalar_limit = u64::try_from(encoded_length - 1).expect("length fits u64");
            let limits = KvIrSerializerLimits::new().with_max_scalar_bytes(scalar_limit);
            let mut bytes = vec![marker];
            bytes.resize(encoded_length + 1, b'x');
            assert_map_key_reader_parity(
                &bytes,
                limits,
                &format!("fixed string length {encoded_length} exceeds scalar limit"),
            );
        }

        let truncated_fallbacks: &[&[u8]] = &[
            &[],
            &[0xc4],
            &[0xc4, 2, 0],
            &[0xc7],
            &[0xd9],
            &[0xd9, 3, b'x'],
            &[0xda, 0],
            &[0xdb, 0, 0, 0],
            &[0xde, 0],
            &[0xdf, 0, 0, 0],
        ];
        for bytes in truncated_fallbacks {
            let context = format!("truncated fallback {bytes:?}");
            assert_root_reader_parity(bytes, KvIrSerializerLimits::DEFAULT, &context);
            assert_map_key_reader_parity(bytes, KvIrSerializerLimits::DEFAULT, &context);
        }

        let no_values = KvIrSerializerLimits::new().with_max_values_per_map(0);
        assert_root_reader_parity(&[0x80], no_values, "fixed map exceeds value limit");
        assert_map_key_reader_parity(&[0xa0], no_values, "fixed string exceeds value limit");
    }

    #[test]
    fn ignores_trailing_nil_and_garbage_after_each_root_map() {
        let trailers: &[(&[u8], &[u8])] = &[(&[0xc0], b"garbage"), (b"garbage", &[0xc0])];
        for (encoding, oracle) in [
            (KvIrEncoding::FourByte, FOUR_BYTE_ORACLE_HEX),
            (KvIrEncoding::EightByte, EIGHT_BYTE_ORACLE_HEX),
        ] {
            for &(auto_trailer, user_trailer) in trailers {
                let mut auto = auto_map();
                auto.extend_from_slice(auto_trailer);
                let mut user = user_map();
                user.extend_from_slice(user_trailer);

                let mut serializer = KvIrSerializer::new(
                    KvIrSerializerOptions::new(encoding),
                    Some(br#"{"fixture":"rust-kv-ir-reader-v1"}"#),
                )
                .expect("create serializer");
                serializer
                    .change_utc_offset(3_600_000)
                    .expect("change offset");
                serializer
                    .serialize_log_event_from_msgpack_maps(&auto, &user)
                    .expect("serialize the first root maps");
                serializer.finish().expect("finish stream");

                assert_eq!(
                    decode_hex(oracle),
                    serializer.pending_output(),
                    "{encoding:?}: auto trailer {auto_trailer:?}, user trailer {user_trailer:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_empty_truncated_or_non_map_first_root_values() {
        let invalid_inputs: &[(&[u8], usize, KvIrMessagePackErrorKind)] = &[
            (&[], 0, KvIrMessagePackErrorKind::Truncated),
            (&[0x81], 1, KvIrMessagePackErrorKind::Truncated),
            (&[0xc0, 0x80], 0, KvIrMessagePackErrorKind::RootMustBeMap),
        ];
        for &(invalid, expected_offset, expected_kind) in invalid_inputs {
            for invalid_input in [
                KvIrSerializerInput::AutoGenerated,
                KvIrSerializerInput::UserGenerated,
            ] {
                let mut serializer = KvIrSerializer::new_four_byte(None).expect("serializer");
                let before = serializer.pending_output().to_vec();
                let empty_map = [0x80];
                let (auto, user) = match invalid_input {
                    KvIrSerializerInput::AutoGenerated => (invalid, empty_map.as_slice()),
                    KvIrSerializerInput::UserGenerated => (empty_map.as_slice(), invalid),
                };

                let error = serializer
                    .serialize_log_event_from_msgpack_maps(auto, user)
                    .expect_err("invalid first root object");
                assert!(matches!(
                    error,
                    KvIrSerializerError::MessagePack {
                        input,
                        offset,
                        kind,
                    } if input == invalid_input
                        && offset == expected_offset
                        && kind == expected_kind
                ));
                assert_eq!(before, serializer.pending_output());
            }
        }
    }

    #[test]
    fn failed_second_map_rolls_back_schema_and_pending_output() {
        let mut serializer = KvIrSerializer::new_four_byte(None).expect("serializer");
        let preamble = serializer.pending_output().to_vec();
        let invalid_user = [0x81, 0xa1, b'x', 0x81, 0x01, 0x02];
        let error = serializer
            .serialize_log_event_from_msgpack_maps(&auto_map(), &invalid_user)
            .expect_err("integer nested key is invalid");
        assert!(matches!(
            error,
            KvIrSerializerError::MessagePack {
                kind: KvIrMessagePackErrorKind::MapKeyMustBeString,
                ..
            }
        ));
        assert_eq!(preamble, serializer.pending_output());
        assert_eq!(
            KvIrSerializerStats {
                serialized_bytes: u64::try_from(preamble.len()).unwrap(),
                ..KvIrSerializerStats::default()
            },
            serializer.stats()
        );

        serializer
            .serialize_log_event_from_msgpack_maps(&auto_map(), &user_map())
            .expect("subsequent valid event");
        assert_eq!(7, serializer.stats().schema_nodes());
    }

    #[test]
    fn schema_index_is_exact_under_collisions_and_suffix_rollback() {
        let mut tree = SchemaTree::new().expect("schema tree");
        tree.index.forced_hash = Some(7);

        let integer_id = insert_tree_node(&mut tree, 0, b"same", KvIrNodeType::Integer);
        let string_id = insert_tree_node(&mut tree, 0, b"same", KvIrNodeType::String);
        let nested_id = insert_tree_node(&mut tree, integer_id, b"same", KvIrNodeType::Integer);
        assert_ne!(integer_id, string_id);
        assert_ne!(integer_id, nested_id);
        assert_eq!(
            Some(integer_id),
            tree.find(0, b"same", KvIrNodeType::Integer).1
        );
        assert_eq!(
            Some(string_id),
            tree.find(0, b"same", KvIrNodeType::String).1
        );
        assert_eq!(
            Some(nested_id),
            tree.find(integer_id, b"same", KvIrNodeType::Integer).1
        );

        for index in 0..512_u32 {
            let key = format!("collision-{index:04}");
            insert_tree_node(&mut tree, 0, key.as_bytes(), KvIrNodeType::Boolean);
        }
        let snapshot = tree.nodes.len();
        let key_snapshot = tree.keys.len();
        for index in 512..1_024_u32 {
            let key = format!("collision-{index:04}");
            insert_tree_node(&mut tree, 0, key.as_bytes(), KvIrNodeType::Boolean);
        }
        tree.rollback(snapshot);
        assert_eq!(key_snapshot, tree.keys.len());
        for index in 0..512_u32 {
            let key = format!("collision-{index:04}");
            assert!(
                tree.find(0, key.as_bytes(), KvIrNodeType::Boolean)
                    .1
                    .is_some()
            );
        }
        for index in 512..1_024_u32 {
            let key = format!("collision-{index:04}");
            assert_eq!(None, tree.find(0, key.as_bytes(), KvIrNodeType::Boolean).1);
        }

        let reinserted = insert_tree_node(&mut tree, 0, b"collision-0512", KvIrNodeType::Boolean);
        assert_eq!(u32::try_from(snapshot).unwrap(), reinserted);
    }

    #[test]
    fn schema_hash_index_is_initialized_only_after_the_linear_scan_threshold() {
        let mut tree = SchemaTree::new().expect("schema tree");
        assert!(tree.index.hash_builder.is_none());
        assert_eq!(0, tree.index.slots.len());

        for index in 0..LINEAR_SCHEMA_SCAN_LIMIT {
            let key = format!("linear-{index:04}");
            insert_tree_node(&mut tree, 0, key.as_bytes(), KvIrNodeType::String);
        }
        assert!(tree.index.hash_builder.is_none());
        assert_eq!(0, tree.index.slots.len());

        insert_tree_node(&mut tree, 0, b"indexed", KvIrNodeType::String);
        assert!(tree.index.hash_builder.is_some());
        assert_ne!(0, tree.index.slots.len());
        assert_eq!(
            Some(u32::try_from(LINEAR_SCHEMA_SCAN_LIMIT + 1).expect("node ID fits u32")),
            tree.find(0, b"indexed", KvIrNodeType::String).1
        );
    }

    #[test]
    fn large_failed_event_restores_schema_index_and_wire_output() {
        const FIELD_COUNT: u32 = 4_096;
        let auto = wide_integer_map(FIELD_COUNT);
        let invalid_user = [0x81, 0xa1, b'x', 0x81, 0x01, 0x02];
        let empty_user = [0x80];
        let mut serializer = KvIrSerializer::new_four_byte(None).expect("serializer");
        let preamble = serializer.pending_output().to_vec();

        for _ in 0..2 {
            assert!(matches!(
                serializer.serialize_log_event_from_msgpack_maps(&auto, &invalid_user),
                Err(KvIrSerializerError::MessagePack {
                    kind: KvIrMessagePackErrorKind::MapKeyMustBeString,
                    ..
                })
            ));
            assert_eq!(1, serializer.auto_schema.nodes.len());
            assert_eq!(0, serializer.auto_schema.keys.len());
            assert_eq!(preamble, serializer.pending_output());
        }

        serializer
            .serialize_log_event_from_msgpack_maps(&auto, &empty_user)
            .expect("valid event after rollback");
        let mut fresh = KvIrSerializer::new_four_byte(None).expect("fresh serializer");
        fresh
            .serialize_log_event_from_msgpack_maps(&auto, &empty_user)
            .expect("fresh valid event");
        assert_eq!(fresh.pending_output(), serializer.pending_output());
        assert_eq!(fresh.stats(), serializer.stats());
        assert_eq!(u64::from(FIELD_COUNT), serializer.stats().schema_nodes());
    }

    #[test]
    fn empty_maps_and_array_text_are_current_protocol_values() {
        let mut serializer = KvIrSerializer::new_four_byte(None).expect("serializer");
        serializer
            .consume_pending(serializer.pending_output().len())
            .unwrap();
        let auto = [0x80];
        let user = [
            0x81, 0xa1, b'a', 0x94, 0x01, 0xa3, b'a', b'/', b'b', 0x81, 0xa1, b'x', 0xc3, 0x90,
        ];
        serializer
            .serialize_log_event_from_msgpack_maps(&auto, &user)
            .expect("array event");
        let bytes = serializer.pending_output();
        assert!(bytes.windows(5).any(|window| window == br"a\\/b"));
        assert!(bytes.contains(&0x59));
    }

    #[test]
    fn input_and_pending_limits_are_enforced_without_partial_commit() {
        let limits = KvIrSerializerLimits::new().with_max_input_bytes_per_map(1);
        let mut serializer =
            KvIrSerializer::new(KvIrSerializerOptions::default().with_limits(limits), None)
                .expect("preamble fits default pending limit");
        let before = serializer.pending_output().to_vec();
        assert!(matches!(
            serializer.serialize_log_event_from_msgpack_maps(&[0x80, 0], &[0x80]),
            Err(KvIrSerializerError::Limit(KvIrSerializerLimitViolation {
                resource: KvIrSerializerLimitResource::InputBytesPerMap,
                ..
            }))
        ));
        assert_eq!(before, serializer.pending_output());
    }

    fn assert_root_reader_parity(bytes: &[u8], limits: KvIrSerializerLimits, context: &str) {
        let mut expected =
            MessagePackReader::new(bytes, KvIrSerializerInput::AutoGenerated, limits);
        let mut actual = MessagePackReader::new(bytes, KvIrSerializerInput::AutoGenerated, limits);
        let expected_result = read_root_map_via_generic_header(&mut expected);
        let actual_result = actual.read_root_map();
        assert_reader_results_equal(expected_result, actual_result, context);
        assert_eq!(expected.position, actual.position, "position: {context}");
        assert_eq!(expected.values, actual.values, "value count: {context}");
    }

    fn assert_map_key_reader_parity(bytes: &[u8], limits: KvIrSerializerLimits, context: &str) {
        let mut expected =
            MessagePackReader::new(bytes, KvIrSerializerInput::UserGenerated, limits);
        let mut actual = MessagePackReader::new(bytes, KvIrSerializerInput::UserGenerated, limits);
        let expected_result = read_map_key_via_generic_header(&mut expected).map(<[u8]>::to_vec);
        let actual_result = actual.read_map_key().map(<[u8]>::to_vec);
        assert_reader_results_equal(expected_result, actual_result, context);
        assert_eq!(expected.position, actual.position, "position: {context}");
        assert_eq!(expected.values, actual.values, "value count: {context}");
    }

    fn read_root_map_via_generic_header(
        reader: &mut MessagePackReader<'_>,
    ) -> Result<u32, KvIrSerializerError> {
        let offset = reader.position;
        let Header::Map(pairs) = reader.read_header()? else {
            return Err(reader.error_at(offset, KvIrMessagePackErrorKind::RootMustBeMap));
        };
        Ok(pairs)
    }

    fn read_map_key_via_generic_header<'a>(
        reader: &mut MessagePackReader<'a>,
    ) -> Result<&'a [u8], KvIrSerializerError> {
        let offset = reader.position;
        let Header::String(key) = reader.read_header()? else {
            return Err(reader.error_at(offset, KvIrMessagePackErrorKind::MapKeyMustBeString));
        };
        Ok(key)
    }

    fn assert_reader_results_equal<T>(
        expected: Result<T, KvIrSerializerError>,
        actual: Result<T, KvIrSerializerError>,
        context: &str,
    ) where
        T: std::fmt::Debug + Eq, {
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(expected, actual, "{context}"),
            (Err(expected), Err(actual)) => {
                assert_eq!(format!("{expected:?}"), format!("{actual:?}"), "{context}");
            }
            (expected, actual) => {
                panic!("reader outcome mismatch for {context}: {expected:?} != {actual:?}")
            }
        }
    }

    fn auto_map() -> Vec<u8> {
        vec![
            0x82, 0xa5, b'l', b'e', b'v', b'e', b'l', 0xa4, b'i', b'n', b'f', b'o', 0xa3, b's',
            b'e', b'q', 0x07,
        ]
    }

    fn user_map() -> Vec<u8> {
        let mut bytes = vec![
            0x85, 0xa5, b'e', b'm', b'p', b't', b'y', 0x80, 0xa7, b'm', b'e', b's', b's', b'a',
            b'g', b'e', 0xac, b't', b'a', b's', b'k', b' ', b'4', b'2', b' ', b'd', b'o', b'n',
            b'e', 0xa4, b'n', b'o', b'n', b'e', 0xc0, 0xa2, b'o', b'k', 0xc3, 0xa5, b'r', b'a',
            b't', b'i', b'o', 0xcb,
        ];
        bytes.extend_from_slice(&1.25_f64.to_bits().to_be_bytes());
        bytes
    }

    fn insert_tree_node(
        tree: &mut SchemaTree,
        parent_id: u32,
        key: &[u8],
        node_type: KvIrNodeType,
    ) -> u32 {
        let (hash, existing) = tree.find(parent_id, key, node_type);
        assert!(existing.is_none());
        tree.insert(
            hash,
            parent_id,
            key,
            node_type,
            KvIrSerializerLimits::DEFAULT.schema_nodes_per_namespace,
        )
        .expect("insert schema node")
    }

    fn wide_integer_map(field_count: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(usize::try_from(field_count).unwrap() * 16 + 5);
        bytes.push(0xdf);
        bytes.extend_from_slice(&field_count.to_be_bytes());
        for index in 0..field_count {
            let key = format!("field-{index:08}");
            bytes.push(0xa0 | u8::try_from(key.len()).unwrap());
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(1);
        }
        bytes
    }

    fn decode_hex(source: &str) -> Vec<u8> {
        let digits: Vec<u8> = source.bytes().filter(u8::is_ascii_hexdigit).collect();
        let (pairs, remainder) = digits.as_chunks::<2>();
        assert_eq!(0, remainder.len());
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
            _ => panic!("non-hex byte"),
        }
    }
}
