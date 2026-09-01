use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use super::KvIrEncodedTextError;
use super::KvIrLogEvent;
use super::KvIrNamespace;
use super::KvIrNodeType;
use super::KvIrPair;
use super::KvIrValueKind;

const MEBIBYTE: u64 = 1024 * 1024;
const NO_INDEX: u32 = u32::MAX;
const EMPTY_INDEX_SLOT: u32 = 0;

/// Hard limits for materializing one independent KV-IR event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrOwnedEventLimits {
    materialized_nodes: u64,
    arena_bytes: u64,
    reconstructed_value_bytes: u64,
}

impl KvIrOwnedEventLimits {
    /// Creates production defaults suitable for Python/binding event ownership.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            materialized_nodes: 1_000_000,
            arena_bytes: 64 * MEBIBYTE,
            reconstructed_value_bytes: 16 * MEBIBYTE,
        }
    }

    #[must_use]
    pub const fn with_max_materialized_nodes(mut self, value: u64) -> Self {
        self.materialized_nodes = value;
        self
    }

    #[must_use]
    pub const fn with_max_arena_bytes(mut self, value: u64) -> Self {
        self.arena_bytes = value;
        self
    }

    #[must_use]
    pub const fn with_max_reconstructed_value_bytes(mut self, value: u64) -> Self {
        self.reconstructed_value_bytes = value;
        self
    }

    #[must_use]
    pub const fn max_materialized_nodes(self) -> u64 {
        self.materialized_nodes
    }

    #[must_use]
    pub const fn max_arena_bytes(self) -> u64 {
        self.arena_bytes
    }

    #[must_use]
    pub const fn max_reconstructed_value_bytes(self) -> u64 {
        self.reconstructed_value_bytes
    }
}

impl Default for KvIrOwnedEventLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// Materialization resource governed by a hard limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrOwnedEventLimitResource {
    MaterializedNodes,
    ArenaBytes,
    ReconstructedValueBytes,
}

impl Display for KvIrOwnedEventLimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MaterializedNodes => "materialized event nodes",
            Self::ArenaBytes => "owned event arena bytes",
            Self::ReconstructedValueBytes => "reconstructed encoded-text bytes",
        })
    }
}

/// Allocation whose growth failed while materializing an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrOwnedEventResource {
    TemporaryNodes,
    TemporaryIndex,
    TemporaryPath,
    TemporaryOrder,
    EventNodes,
    EventArena,
}

impl Display for KvIrOwnedEventResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TemporaryNodes => "temporary selected nodes",
            Self::TemporaryIndex => "temporary node index",
            Self::TemporaryPath => "temporary schema path",
            Self::TemporaryOrder => "temporary sibling order",
            Self::EventNodes => "owned event nodes",
            Self::EventArena => "owned event byte arena",
        })
    }
}

/// Failure while making a self-contained compact event.
#[derive(Debug)]
#[non_exhaustive]
pub enum KvIrOwnedEventError {
    MissingSchemaNode {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    InvalidSchemaPath {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    DuplicatePair {
        namespace: KvIrNamespace,
        node_id: u32,
    },
    MissingPair {
        pair_index: u32,
    },
    EncodedText {
        namespace: KvIrNamespace,
        node_id: u32,
        source: KvIrEncodedTextError,
    },
    Limit {
        resource: KvIrOwnedEventLimitResource,
        actual: u64,
        limit: u64,
    },
    AllocationFailed {
        resource: KvIrOwnedEventResource,
        requested_additional: usize,
    },
    SizeOverflow,
}

impl Display for KvIrOwnedEventError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSchemaNode { namespace, node_id } => {
                write!(formatter, "missing {namespace} schema node {node_id}")
            }
            Self::InvalidSchemaPath { namespace, node_id } => {
                write!(
                    formatter,
                    "invalid path from {namespace} schema node {node_id}"
                )
            }
            Self::DuplicatePair { namespace, node_id } => {
                write!(
                    formatter,
                    "duplicate pair for {namespace} schema node {node_id}"
                )
            }
            Self::MissingPair { pair_index } => {
                write!(formatter, "decoded event pair {pair_index} is unavailable")
            }
            Self::EncodedText {
                namespace,
                node_id,
                source,
            } => write!(
                formatter,
                "failed to reconstruct {namespace} schema node {node_id}: {source}"
            ),
            Self::Limit {
                resource,
                actual,
                limit,
            } => write!(formatter, "{resource} uses {actual}; limit is {limit}"),
            Self::AllocationFailed {
                resource,
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} additional units for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("owned KV-IR event size overflow"),
        }
    }
}

impl Error for KvIrOwnedEventError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EncodedText { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Compact byte range in an owned event's single arena.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrOwnedSpan {
    offset: u32,
    length: u32,
}

impl KvIrOwnedSpan {
    #[must_use]
    pub const fn offset(self) -> u32 {
        self.offset
    }

    #[must_use]
    pub const fn length(self) -> u32 {
        self.length
    }
}

impl KvIrOwnedSpan {
    const EMPTY: Self = Self {
        offset: 0,
        length: 0,
    };
}

/// Stable scalar/byte interpretation of one flat event node.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KvIrOwnedValueKind {
    Object = 0,
    Integer = 1,
    Float = 2,
    Boolean = 3,
    String = 4,
    ArrayJson = 5,
    Null = 6,
    EmptyObject = 7,
}

/// Value carried by one flat owned-event node.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum KvIrOwnedValue {
    /// An object selected only because it contains a selected descendant.
    Object,
    Integer(i64),
    Float {
        bits: u64,
    },
    Boolean(bool),
    String(KvIrOwnedSpan),
    ArrayJson(KvIrOwnedSpan),
    Null,
    EmptyObject,
}

/// One node in depth-first schema insertion order.
///
/// The representation is a stable 32-byte POD suitable for a zero-copy C ABI view. Integer values
/// use the two's-complement interpretation of `scalar_bits`; floats retain their exact IEEE bits;
/// booleans use zero or one. Only string and array kinds use `value_span`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvIrOwnedEventNode {
    depth: u32,
    value_kind: KvIrOwnedValueKind,
    key: KvIrOwnedSpan,
    value_span: KvIrOwnedSpan,
    scalar_bits: u64,
}

impl KvIrOwnedEventNode {
    const fn new(depth: u32, key: KvIrOwnedSpan, value: KvIrOwnedValue) -> Self {
        let (value_kind, value_span, scalar_bits) = match value {
            KvIrOwnedValue::Object => (KvIrOwnedValueKind::Object, KvIrOwnedSpan::EMPTY, 0),
            KvIrOwnedValue::Integer(value) => (
                KvIrOwnedValueKind::Integer,
                KvIrOwnedSpan::EMPTY,
                u64::from_ne_bytes(value.to_ne_bytes()),
            ),
            KvIrOwnedValue::Float { bits } => {
                (KvIrOwnedValueKind::Float, KvIrOwnedSpan::EMPTY, bits)
            }
            KvIrOwnedValue::Boolean(value) => (
                KvIrOwnedValueKind::Boolean,
                KvIrOwnedSpan::EMPTY,
                value as u64,
            ),
            KvIrOwnedValue::String(span) => (KvIrOwnedValueKind::String, span, 0),
            KvIrOwnedValue::ArrayJson(span) => (KvIrOwnedValueKind::ArrayJson, span, 0),
            KvIrOwnedValue::Null => (KvIrOwnedValueKind::Null, KvIrOwnedSpan::EMPTY, 0),
            KvIrOwnedValue::EmptyObject => {
                (KvIrOwnedValueKind::EmptyObject, KvIrOwnedSpan::EMPTY, 0)
            }
        };
        Self {
            depth,
            value_kind,
            key,
            value_span,
            scalar_bits,
        }
    }

    #[must_use]
    pub const fn depth(self) -> u32 {
        self.depth
    }

    #[must_use]
    pub const fn key_span(self) -> KvIrOwnedSpan {
        self.key
    }

    #[must_use]
    pub const fn value_kind(self) -> KvIrOwnedValueKind {
        self.value_kind
    }

    #[must_use]
    pub const fn value_span(self) -> KvIrOwnedSpan {
        self.value_span
    }

    #[must_use]
    pub const fn scalar_bits(self) -> u64 {
        self.scalar_bits
    }

    #[must_use]
    pub const fn value(self) -> KvIrOwnedValue {
        match self.value_kind {
            KvIrOwnedValueKind::Object => KvIrOwnedValue::Object,
            KvIrOwnedValueKind::Integer => {
                KvIrOwnedValue::Integer(i64::from_ne_bytes(self.scalar_bits.to_ne_bytes()))
            }
            KvIrOwnedValueKind::Float => KvIrOwnedValue::Float {
                bits: self.scalar_bits,
            },
            KvIrOwnedValueKind::Boolean => KvIrOwnedValue::Boolean(self.scalar_bits != 0),
            KvIrOwnedValueKind::String => KvIrOwnedValue::String(self.value_span),
            KvIrOwnedValueKind::ArrayJson => KvIrOwnedValue::ArrayJson(self.value_span),
            KvIrOwnedValueKind::Null => KvIrOwnedValue::Null,
            KvIrOwnedValueKind::EmptyObject => KvIrOwnedValue::EmptyObject,
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<KvIrOwnedSpan>() == 8);
    assert!(std::mem::size_of::<KvIrOwnedValueKind>() == 4);
    assert!(std::mem::offset_of!(KvIrOwnedEventNode, depth) == 0);
    assert!(std::mem::offset_of!(KvIrOwnedEventNode, value_kind) == 4);
    assert!(std::mem::offset_of!(KvIrOwnedEventNode, key) == 8);
    assert!(std::mem::offset_of!(KvIrOwnedEventNode, value_span) == 16);
    assert!(std::mem::offset_of!(KvIrOwnedEventNode, scalar_bits) == 24);
    assert!(std::mem::size_of::<KvIrOwnedEventNode>() == 32);
};

/// Self-contained event optimized for language bindings and one-pass dictionary construction.
///
/// Each namespace is a DFS-preorder flat node slice. `depth` is sufficient to open and close
/// dictionaries with a small stack; siblings retain schema insertion order. Keys and byte values
/// share one compact arena, and no whole-schema bitmap or hash map survives materialization.
#[derive(Debug)]
pub struct KvIrOwnedEvent {
    auto_nodes: Vec<KvIrOwnedEventNode>,
    user_nodes: Vec<KvIrOwnedEventNode>,
    arena: Vec<u8>,
    utc_offset_millis: i64,
    stream_index: u64,
    unit_index: u64,
    event_index: u64,
    input_offset: u64,
}

impl KvIrOwnedEvent {
    /// Copies only the event's selected schema paths and values into a compact independent form.
    ///
    /// # Errors
    ///
    /// Returns a structured error for inconsistent schema references, encoded-text reconstruction,
    /// hard limits, allocation failure, or size overflow.
    pub fn materialize(
        event: KvIrLogEvent<'_>,
        limits: KvIrOwnedEventLimits,
    ) -> Result<Self, KvIrOwnedEventError> {
        KvIrOwnedEventMaterializer::new()?.materialize(event, limits)
    }

    #[must_use]
    pub fn nodes(&self, namespace: KvIrNamespace) -> &[KvIrOwnedEventNode] {
        match namespace {
            KvIrNamespace::AutoGenerated => &self.auto_nodes,
            KvIrNamespace::UserGenerated => &self.user_nodes,
        }
    }

    #[must_use]
    pub fn arena(&self) -> &[u8] {
        &self.arena
    }

    #[must_use]
    pub fn resolve(&self, span: KvIrOwnedSpan) -> Option<&[u8]> {
        let start = usize::try_from(span.offset).ok()?;
        let length = usize::try_from(span.length).ok()?;
        let end = start.checked_add(length)?;
        self.arena.get(start..end)
    }

    #[must_use]
    pub const fn utc_offset_millis(&self) -> i64 {
        self.utc_offset_millis
    }

    #[must_use]
    pub const fn stream_index(&self) -> u64 {
        self.stream_index
    }

    #[must_use]
    pub const fn unit_index(&self) -> u64 {
        self.unit_index
    }

    #[must_use]
    pub const fn event_index(&self) -> u64 {
        self.event_index
    }

    #[must_use]
    pub const fn input_offset(&self) -> u64 {
        self.input_offset
    }
}

/// Reusable bounded scratch for making independent events without per-event tree allocations.
#[derive(Debug)]
pub struct KvIrOwnedEventMaterializer {
    auto: TempTree,
    user: TempTree,
    path: Vec<u32>,
}

impl KvIrOwnedEventMaterializer {
    /// Creates empty reusable materialization scratch.
    ///
    /// # Errors
    ///
    /// Returns an allocation error if the two namespace roots cannot be initialized.
    pub fn new() -> Result<Self, KvIrOwnedEventError> {
        Ok(Self {
            auto: TempTree::new()?,
            user: TempTree::new()?,
            path: Vec::new(),
        })
    }

    /// Materializes one event and retains only temporary index/path capacity for the next call.
    /// Returned event storage is independent of this materializer and the stream reader.
    ///
    /// # Errors
    ///
    /// Returns a structured error for inconsistent schema references, encoded-text reconstruction,
    /// hard limits, allocation failure, or size overflow. The materializer remains reusable.
    pub fn materialize(
        &mut self,
        event: KvIrLogEvent<'_>,
        limits: KvIrOwnedEventLimits,
    ) -> Result<KvIrOwnedEvent, KvIrOwnedEventError> {
        self.auto.reset();
        self.user.reset();
        self.path.clear();
        let mut materialized_nodes = 0_u64;
        for (pair_index, pair) in event.pairs().enumerate() {
            let pair_index = u32::try_from(pair_index).map_err(|_| KvIrOwnedEventError::Limit {
                resource: KvIrOwnedEventLimitResource::MaterializedNodes,
                actual: u64::from(u32::MAX) + 1,
                limit: limits.materialized_nodes,
            })?;
            let tree = match pair.namespace() {
                KvIrNamespace::AutoGenerated => &mut self.auto,
                KvIrNamespace::UserGenerated => &mut self.user,
            };
            tree.select_pair(
                &event,
                pair,
                pair_index,
                &mut self.path,
                &mut materialized_nodes,
                limits.materialized_nodes,
            )?;
        }

        self.auto.build_children()?;
        self.user.build_children()?;
        let node_capacity =
            usize::try_from(materialized_nodes).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
        let arena_capacity = event
            .raw_unit()
            .len()
            .min(usize::try_from(limits.arena_bytes).unwrap_or(usize::MAX));
        let mut owned = KvIrOwnedEvent {
            auto_nodes: Vec::new(),
            user_nodes: Vec::new(),
            arena: Vec::new(),
            utc_offset_millis: event.utc_offset_millis(),
            stream_index: event.stream_index(),
            unit_index: event.unit_index(),
            event_index: event.event_index(),
            input_offset: event.input_offset(),
        };
        owned.arena.try_reserve(arena_capacity).map_err(|_| {
            KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::EventArena,
                requested_additional: arena_capacity,
            }
        })?;
        let auto_capacity = self.auto.nodes.len().saturating_sub(1).min(node_capacity);
        owned.auto_nodes.try_reserve(auto_capacity).map_err(|_| {
            KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::EventNodes,
                requested_additional: auto_capacity,
            }
        })?;
        let user_capacity = self.user.nodes.len().saturating_sub(1).min(node_capacity);
        owned.user_nodes.try_reserve(user_capacity).map_err(|_| {
            KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::EventNodes,
                requested_additional: user_capacity,
            }
        })?;
        materialize_tree(
            &event,
            KvIrNamespace::AutoGenerated,
            &self.auto,
            &mut owned.auto_nodes,
            &mut owned.arena,
            limits,
        )?;
        materialize_tree(
            &event,
            KvIrNamespace::UserGenerated,
            &self.user,
            &mut owned.user_nodes,
            &mut owned.arena,
            limits,
        )?;
        Ok(owned)
    }
}

#[derive(Clone, Copy, Debug)]
struct TempNode {
    schema_id: u32,
    parent: u32,
    pair_index: u32,
    first_child: u32,
    next_sibling: u32,
}

impl TempNode {
    const fn root() -> Self {
        Self {
            schema_id: 0,
            parent: NO_INDEX,
            pair_index: NO_INDEX,
            first_child: NO_INDEX,
            next_sibling: NO_INDEX,
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<TempNode>() == 20);
};

#[derive(Debug)]
struct TempTree {
    nodes: Vec<TempNode>,
    slots: Vec<u32>,
    order: Vec<u32>,
}

impl TempTree {
    fn new() -> Result<Self, KvIrOwnedEventError> {
        let mut nodes = Vec::new();
        nodes
            .try_reserve(1)
            .map_err(|_| KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::TemporaryNodes,
                requested_additional: 1,
            })?;
        nodes.push(TempNode::root());
        Ok(Self {
            nodes,
            slots: Vec::new(),
            order: Vec::new(),
        })
    }

    fn reset(&mut self) {
        self.nodes.truncate(1);
        self.nodes[0] = TempNode::root();
        self.slots.fill(EMPTY_INDEX_SLOT);
        self.order.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn select_pair(
        &mut self,
        event: &KvIrLogEvent<'_>,
        pair: KvIrPair<'_>,
        pair_index: u32,
        path: &mut Vec<u32>,
        materialized_nodes: &mut u64,
        node_limit: u64,
    ) -> Result<(), KvIrOwnedEventError> {
        path.clear();
        let namespace = pair.namespace();
        let leaf_id = pair.node_id();
        let mut node_id = leaf_id;
        let parent_index = loop {
            if let Some(index) = self.find(node_id) {
                break index;
            }
            let node = event
                .schema_node(namespace, node_id)
                .ok_or(KvIrOwnedEventError::MissingSchemaNode { namespace, node_id })?;
            path.try_reserve(1)
                .map_err(|_| KvIrOwnedEventError::AllocationFailed {
                    resource: KvIrOwnedEventResource::TemporaryPath,
                    requested_additional: 1,
                })?;
            path.push(node_id);
            node_id = node
                .parent_id()
                .ok_or(KvIrOwnedEventError::InvalidSchemaPath { namespace, node_id })?;
        };

        let mut parent_index = parent_index;
        for &schema_id in path.iter().rev() {
            *materialized_nodes = materialized_nodes
                .checked_add(1)
                .ok_or(KvIrOwnedEventError::SizeOverflow)?;
            if *materialized_nodes > node_limit {
                return Err(KvIrOwnedEventError::Limit {
                    resource: KvIrOwnedEventLimitResource::MaterializedNodes,
                    actual: *materialized_nodes,
                    limit: node_limit,
                });
            }
            parent_index = self.insert(schema_id, parent_index)?;
        }
        let leaf_index =
            usize::try_from(parent_index).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
        let leaf =
            self.nodes
                .get_mut(leaf_index)
                .ok_or(KvIrOwnedEventError::InvalidSchemaPath {
                    namespace,
                    node_id: leaf_id,
                })?;
        if leaf.pair_index != NO_INDEX {
            return Err(KvIrOwnedEventError::DuplicatePair {
                namespace,
                node_id: leaf_id,
            });
        }
        leaf.pair_index = pair_index;
        Ok(())
    }

    fn find(&self, schema_id: u32) -> Option<u32> {
        if schema_id == 0 {
            return Some(0);
        }
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut slot = hash_node_id(schema_id) & mask;
        loop {
            let encoded = self.slots[slot];
            if encoded == EMPTY_INDEX_SLOT {
                return None;
            }
            let node_index = encoded;
            let index = usize::try_from(node_index).ok()?;
            if self.nodes.get(index)?.schema_id == schema_id {
                return Some(node_index);
            }
            slot = slot.wrapping_add(1) & mask;
        }
    }

    fn insert(&mut self, schema_id: u32, parent: u32) -> Result<u32, KvIrOwnedEventError> {
        self.prepare_insert()?;
        self.nodes
            .try_reserve(1)
            .map_err(|_| KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::TemporaryNodes,
                requested_additional: 1,
            })?;
        let node_index =
            u32::try_from(self.nodes.len()).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
        self.nodes.push(TempNode {
            schema_id,
            parent,
            pair_index: NO_INDEX,
            first_child: NO_INDEX,
            next_sibling: NO_INDEX,
        });
        insert_slot(&mut self.slots, &self.nodes, schema_id, node_index);
        Ok(node_index)
    }

    fn prepare_insert(&mut self) -> Result<(), KvIrOwnedEventError> {
        let next_non_root_nodes = self.nodes.len();
        let mut required = self.slots.len();
        if required == 0 {
            required = 16;
        }
        while next_non_root_nodes > required / 4 * 3 {
            required = required
                .checked_mul(2)
                .ok_or(KvIrOwnedEventError::SizeOverflow)?;
        }
        if required == self.slots.len() {
            return Ok(());
        }
        let requested_additional = required.saturating_sub(self.slots.len());
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(required)
            .map_err(|_| KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::TemporaryIndex,
                requested_additional,
            })?;
        slots.resize(required, EMPTY_INDEX_SLOT);
        for (index, node) in self.nodes.iter().enumerate().skip(1) {
            let index = u32::try_from(index).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
            insert_slot(&mut slots, &self.nodes, node.schema_id, index);
        }
        self.slots = slots;
        Ok(())
    }

    fn build_children(&mut self) -> Result<(), KvIrOwnedEventError> {
        let count = self.nodes.len().saturating_sub(1);
        self.order.clear();
        self.order
            .try_reserve(count)
            .map_err(|_| KvIrOwnedEventError::AllocationFailed {
                resource: KvIrOwnedEventResource::TemporaryOrder,
                requested_additional: count,
            })?;
        let node_count =
            u32::try_from(self.nodes.len()).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
        self.order.extend(1..node_count);
        self.order.sort_unstable_by_key(|&index| {
            self.nodes[usize::try_from(index).expect("u32 node index fits usize")].schema_id
        });
        for order_index in (0..self.order.len()).rev() {
            let child = self.order[order_index];
            let child_index =
                usize::try_from(child).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
            let parent_index = usize::try_from(self.nodes[child_index].parent)
                .map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
            let first_child = self
                .nodes
                .get(parent_index)
                .ok_or(KvIrOwnedEventError::SizeOverflow)?
                .first_child;
            self.nodes[child_index].next_sibling = first_child;
            self.nodes[parent_index].first_child = child;
        }
        Ok(())
    }
}

fn insert_slot(slots: &mut [u32], nodes: &[TempNode], schema_id: u32, node_index: u32) {
    debug_assert_ne!(0, slots.len());
    let mask = slots.len() - 1;
    let mut slot = hash_node_id(schema_id) & mask;
    while slots[slot] != EMPTY_INDEX_SLOT {
        debug_assert_ne!(
            nodes[usize::try_from(slots[slot]).expect("u32 node index fits usize")].schema_id,
            schema_id
        );
        slot = slot.wrapping_add(1) & mask;
    }
    slots[slot] = node_index;
}

fn hash_node_id(schema_id: u32) -> usize {
    let mut value = schema_id;
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    usize::try_from(value).unwrap_or_default()
}

fn materialize_tree(
    event: &KvIrLogEvent<'_>,
    namespace: KvIrNamespace,
    tree: &TempTree,
    output: &mut Vec<KvIrOwnedEventNode>,
    arena: &mut Vec<u8>,
    limits: KvIrOwnedEventLimits,
) -> Result<(), KvIrOwnedEventError> {
    let mut current = tree.nodes[0].first_child;
    while current != NO_INDEX {
        let current_index =
            usize::try_from(current).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
        let selected = tree
            .nodes
            .get(current_index)
            .ok_or(KvIrOwnedEventError::SizeOverflow)?;
        let schema = event.schema_node(namespace, selected.schema_id).ok_or(
            KvIrOwnedEventError::MissingSchemaNode {
                namespace,
                node_id: selected.schema_id,
            },
        )?;
        let depth = u32::try_from(schema.depth()).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
        let key = append_arena(arena, schema.key(), limits.arena_bytes)?;
        let value = if selected.pair_index != NO_INDEX {
            let pair_index = selected.pair_index;
            let pair = event
                .pair(usize::try_from(pair_index).map_err(|_| KvIrOwnedEventError::SizeOverflow)?)
                .ok_or(KvIrOwnedEventError::MissingPair { pair_index })?;
            materialize_value(pair, schema.node_type(), arena, limits)?
        } else if schema.node_type() == KvIrNodeType::Object {
            KvIrOwnedValue::Object
        } else {
            return Err(KvIrOwnedEventError::InvalidSchemaPath {
                namespace,
                node_id: selected.schema_id,
            });
        };
        output.push(KvIrOwnedEventNode::new(depth, key, value));

        if selected.first_child != NO_INDEX {
            current = selected.first_child;
            continue;
        }
        let mut completed = current;
        loop {
            let completed_index =
                usize::try_from(completed).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
            let completed_node = tree
                .nodes
                .get(completed_index)
                .ok_or(KvIrOwnedEventError::SizeOverflow)?;
            if completed_node.next_sibling != NO_INDEX {
                current = completed_node.next_sibling;
                break;
            }
            if completed_node.parent == 0 {
                current = NO_INDEX;
                break;
            }
            completed = completed_node.parent;
        }
    }
    Ok(())
}

fn materialize_value(
    pair: KvIrPair<'_>,
    node_type: KvIrNodeType,
    arena: &mut Vec<u8>,
    limits: KvIrOwnedEventLimits,
) -> Result<KvIrOwnedValue, KvIrOwnedEventError> {
    let namespace = pair.namespace();
    let node_id = pair.node_id();
    match pair.value().kind() {
        KvIrValueKind::Integer(value) => Ok(KvIrOwnedValue::Integer(value.value())),
        KvIrValueKind::Float { bits } => Ok(KvIrOwnedValue::Float { bits }),
        KvIrValueKind::Boolean(value) => Ok(KvIrOwnedValue::Boolean(value)),
        KvIrValueKind::String(value) => {
            append_arena(arena, value, limits.arena_bytes).map(KvIrOwnedValue::String)
        }
        KvIrValueKind::EncodedText(text) => {
            let arena_limit = usize::try_from(limits.arena_bytes).unwrap_or(usize::MAX);
            let remaining = arena_limit.saturating_sub(arena.len());
            let value_limit = usize::try_from(limits.reconstructed_value_bytes)
                .unwrap_or(usize::MAX)
                .min(remaining);
            let range =
                text.append_decoded_to(arena, value_limit)
                    .map_err(|source| match source {
                        KvIrEncodedTextError::Limit { actual, .. }
                            if remaining
                                < usize::try_from(limits.reconstructed_value_bytes)
                                    .unwrap_or(usize::MAX) =>
                        {
                            KvIrOwnedEventError::Limit {
                                resource: KvIrOwnedEventLimitResource::ArenaBytes,
                                actual: u64::try_from(arena.len().saturating_add(actual))
                                    .unwrap_or(u64::MAX),
                                limit: limits.arena_bytes,
                            }
                        }
                        KvIrEncodedTextError::Limit { actual, .. } => KvIrOwnedEventError::Limit {
                            resource: KvIrOwnedEventLimitResource::ReconstructedValueBytes,
                            actual: u64::try_from(actual).unwrap_or(u64::MAX),
                            limit: limits.reconstructed_value_bytes,
                        },
                        source => KvIrOwnedEventError::EncodedText {
                            namespace,
                            node_id,
                            source,
                        },
                    })?;
            let span = span_from_range(range.start, range.end)?;
            match node_type {
                KvIrNodeType::String => Ok(KvIrOwnedValue::String(span)),
                KvIrNodeType::UnstructuredArray => Ok(KvIrOwnedValue::ArrayJson(span)),
                _ => Err(KvIrOwnedEventError::InvalidSchemaPath { namespace, node_id }),
            }
        }
        KvIrValueKind::Null => Ok(KvIrOwnedValue::Null),
        KvIrValueKind::EmptyObject => Ok(KvIrOwnedValue::EmptyObject),
    }
}

fn append_arena(
    arena: &mut Vec<u8>,
    bytes: &[u8],
    limit: u64,
) -> Result<KvIrOwnedSpan, KvIrOwnedEventError> {
    let end = arena
        .len()
        .checked_add(bytes.len())
        .ok_or(KvIrOwnedEventError::SizeOverflow)?;
    let actual = u64::try_from(end).map_err(|_| KvIrOwnedEventError::SizeOverflow)?;
    if actual > limit {
        return Err(KvIrOwnedEventError::Limit {
            resource: KvIrOwnedEventLimitResource::ArenaBytes,
            actual,
            limit,
        });
    }
    arena
        .try_reserve(bytes.len())
        .map_err(|_| KvIrOwnedEventError::AllocationFailed {
            resource: KvIrOwnedEventResource::EventArena,
            requested_additional: bytes.len(),
        })?;
    let start = arena.len();
    arena.extend_from_slice(bytes);
    span_from_range(start, end)
}

fn span_from_range(start: usize, end: usize) -> Result<KvIrOwnedSpan, KvIrOwnedEventError> {
    let length = end
        .checked_sub(start)
        .ok_or(KvIrOwnedEventError::SizeOverflow)?;
    Ok(KvIrOwnedSpan {
        offset: u32::try_from(start).map_err(|_| KvIrOwnedEventError::SizeOverflow)?,
        length: u32::try_from(length).map_err(|_| KvIrOwnedEventError::SizeOverflow)?,
    })
}
