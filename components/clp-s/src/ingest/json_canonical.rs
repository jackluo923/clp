//! Reusable bounded canonical JSON serialization over the ingest parser's flat events.

use std::str;

use super::ndjson::JsonEvent;
use super::ndjson::NdjsonInvalidRecordKind;
use super::ndjson::NdjsonLimitResource;
use super::ndjson::NdjsonLimits;
use super::ndjson::NdjsonResource;
use super::parser::Frame;
use super::parser::ParseFailure;
use super::parser::StoredEvent;
use super::parser::parse_document;
use crate::json::JsonBytePolicy;
use crate::json::JsonEscapeError;
use crate::json::JsonEscapeLimits;
use crate::json::NlohmannFloatError;
use crate::json::append_json_key_bytes;
use crate::json::append_json_string_bytes;
use crate::json::format_nlohmann_float;

const NO_NODE: usize = usize::MAX;
const NO_EVENT: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalJsonLimits {
    pub(crate) input_bytes: u64,
    pub(crate) output_bytes: usize,
    pub(crate) nesting_depth: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalJsonResource {
    Parser(NdjsonResource),
    Nodes,
    BuildStack,
    SortedChildren,
    OutputStack,
    Output,
    Destination,
}

#[derive(Debug)]
pub enum CanonicalJsonError {
    Invalid(NdjsonInvalidRecordKind),
    NumberOutOfRange,
    Escape(JsonEscapeError),
    Float(NlohmannFloatError),
    AllocationFailed {
        resource: CanonicalJsonResource,
        requested_additional: usize,
    },
    SizeOverflow,
    InvalidEventSequence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeKind {
    Object {
        sorted_start: usize,
        sorted_len: usize,
    },
    Array,
    String,
    Number,
    Boolean(bool),
    Null,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Node {
    kind: NodeKind,
    event_index: usize,
    key_event_index: usize,
    first_child: usize,
    next_sibling: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BuildFrame {
    node_id: usize,
    last_child: usize,
    pending_key_event: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputFrame {
    Value(usize),
    Array {
        next_child: usize,
        wrote_child: bool,
    },
    Object {
        next_sorted: usize,
        sorted_end: usize,
        wrote_child: bool,
    },
}

#[derive(Default)]
pub struct CanonicalJsonScratch {
    decoded: String,
    events: Vec<StoredEvent>,
    parser_stack: Vec<Frame>,
    nodes: Vec<Node>,
    build_stack: Vec<BuildFrame>,
    sorted_children: Vec<usize>,
    sort_buffer: Vec<usize>,
    output_stack: Vec<OutputFrame>,
    output: Vec<u8>,
    root_nodes: usize,
}

impl CanonicalJsonScratch {
    pub(crate) const fn new() -> Self {
        Self {
            decoded: String::new(),
            events: Vec::new(),
            parser_stack: Vec::new(),
            nodes: Vec::new(),
            build_stack: Vec::new(),
            sorted_children: Vec::new(),
            sort_buffer: Vec::new(),
            output_stack: Vec::new(),
            output: Vec::new(),
            root_nodes: 0,
        }
    }

    pub(crate) fn append_to(
        &mut self,
        source: &[u8],
        destination: &mut Vec<u8>,
        limits: CanonicalJsonLimits,
    ) -> Result<(), CanonicalJsonError> {
        let source_bytes =
            u64::try_from(source.len()).map_err(|_| CanonicalJsonError::SizeOverflow)?;
        if source_bytes > limits.input_bytes {
            return Err(CanonicalJsonError::Invalid(NdjsonInvalidRecordKind::Limit(
                super::ndjson::NdjsonLimitViolation::new(
                    NdjsonLimitResource::RecordBytes,
                    source_bytes,
                    limits.input_bytes,
                ),
            )));
        }
        let parser_limits = NdjsonLimits::new(
            limits.input_bytes,
            limits.nesting_depth,
            source_bytes,
            source_bytes,
        );
        self.decoded.clear();
        self.events.clear();
        self.parser_stack.clear();
        parse_document(
            source,
            parser_limits,
            &mut self.decoded,
            &mut self.events,
            &mut self.parser_stack,
        )
        .map_err(|source| Self::map_parse_error(&source))?;
        self.build_plan(source)?;
        self.sort_object_children(source)?;
        self.serialize(source, limits.output_bytes)?;

        destination
            .len()
            .checked_add(self.output.len())
            .ok_or(CanonicalJsonError::SizeOverflow)?;
        destination
            .try_reserve_exact(self.output.len())
            .map_err(|_| CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::Destination,
                requested_additional: self.output.len(),
            })?;
        destination.extend_from_slice(&self.output);
        Ok(())
    }

    const fn map_parse_error(source: &ParseFailure) -> CanonicalJsonError {
        match source {
            ParseFailure::Invalid(source) => Self::map_invalid(*source),
            ParseFailure::AllocationFailed {
                resource,
                requested_additional,
            } => CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::Parser(*resource),
                requested_additional: *requested_additional,
            },
            ParseFailure::SizeOverflow => CanonicalJsonError::SizeOverflow,
        }
    }

    const fn map_invalid(source: NdjsonInvalidRecordKind) -> CanonicalJsonError {
        CanonicalJsonError::Invalid(source)
    }

    fn build_plan(&mut self, source: &[u8]) -> Result<(), CanonicalJsonError> {
        self.nodes.clear();
        self.build_stack.clear();
        self.root_nodes = 0;
        for event_index in 0..self.events.len() {
            let event = self.events[event_index].resolve(source, &self.decoded);
            match event {
                JsonEvent::ObjectKey(_) => {
                    let Some(frame) = self.build_stack.last_mut() else {
                        return Err(CanonicalJsonError::InvalidEventSequence);
                    };
                    if frame.pending_key_event != NO_EVENT {
                        return Err(CanonicalJsonError::InvalidEventSequence);
                    }
                    frame.pending_key_event = event_index;
                }
                JsonEvent::ObjectStart => {
                    let node_id = self.push_node(
                        NodeKind::Object {
                            sorted_start: 0,
                            sorted_len: 0,
                        },
                        event_index,
                    )?;
                    self.push_build_frame(node_id)?;
                }
                JsonEvent::ArrayStart(_) => {
                    let node_id = self.push_node(NodeKind::Array, event_index)?;
                    self.push_build_frame(node_id)?;
                }
                JsonEvent::String(_) => {
                    self.push_node(NodeKind::String, event_index)?;
                }
                JsonEvent::Number(_) => {
                    self.push_node(NodeKind::Number, event_index)?;
                }
                JsonEvent::Boolean(value) => {
                    self.push_node(NodeKind::Boolean(value), event_index)?;
                }
                JsonEvent::Null => {
                    self.push_node(NodeKind::Null, event_index)?;
                }
                JsonEvent::ObjectEnd => self.pop_build_frame(true)?,
                JsonEvent::ArrayEnd => self.pop_build_frame(false)?,
            }
        }
        if !self.build_stack.is_empty() || self.root_nodes != 1 {
            return Err(CanonicalJsonError::InvalidEventSequence);
        }
        Ok(())
    }

    fn push_node(
        &mut self,
        kind: NodeKind,
        event_index: usize,
    ) -> Result<usize, CanonicalJsonError> {
        self.nodes
            .try_reserve(1)
            .map_err(|_| CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::Nodes,
                requested_additional: 1,
            })?;
        let node_id = self.nodes.len();
        let key_event_index = if let Some(parent) = self.build_stack.last_mut() {
            let parent_kind = self
                .nodes
                .get(parent.node_id)
                .ok_or(CanonicalJsonError::InvalidEventSequence)?
                .kind;
            if matches!(parent_kind, NodeKind::Object { .. }) {
                if parent.pending_key_event == NO_EVENT {
                    return Err(CanonicalJsonError::InvalidEventSequence);
                }
                let key = parent.pending_key_event;
                parent.pending_key_event = NO_EVENT;
                key
            } else {
                NO_EVENT
            }
        } else {
            self.root_nodes = self
                .root_nodes
                .checked_add(1)
                .ok_or(CanonicalJsonError::SizeOverflow)?;
            if self.root_nodes != 1 {
                return Err(CanonicalJsonError::InvalidEventSequence);
            }
            NO_EVENT
        };
        self.nodes.push(Node {
            kind,
            event_index,
            key_event_index,
            first_child: NO_NODE,
            next_sibling: NO_NODE,
        });
        if let Some(parent) = self.build_stack.last_mut() {
            if parent.last_child == NO_NODE {
                self.nodes
                    .get_mut(parent.node_id)
                    .ok_or(CanonicalJsonError::InvalidEventSequence)?
                    .first_child = node_id;
            } else {
                self.nodes
                    .get_mut(parent.last_child)
                    .ok_or(CanonicalJsonError::InvalidEventSequence)?
                    .next_sibling = node_id;
            }
            parent.last_child = node_id;
        }
        Ok(node_id)
    }

    fn push_build_frame(&mut self, node_id: usize) -> Result<(), CanonicalJsonError> {
        self.build_stack
            .try_reserve(1)
            .map_err(|_| CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::BuildStack,
                requested_additional: 1,
            })?;
        self.build_stack.push(BuildFrame {
            node_id,
            last_child: NO_NODE,
            pending_key_event: NO_EVENT,
        });
        Ok(())
    }

    fn pop_build_frame(&mut self, object: bool) -> Result<(), CanonicalJsonError> {
        let frame = self
            .build_stack
            .pop()
            .ok_or(CanonicalJsonError::InvalidEventSequence)?;
        let node = self
            .nodes
            .get(frame.node_id)
            .ok_or(CanonicalJsonError::InvalidEventSequence)?;
        if matches!(node.kind, NodeKind::Object { .. }) != object
            || frame.pending_key_event != NO_EVENT
        {
            return Err(CanonicalJsonError::InvalidEventSequence);
        }
        Ok(())
    }

    fn sort_object_children(&mut self, source: &[u8]) -> Result<(), CanonicalJsonError> {
        self.sorted_children.clear();
        for node_id in 0..self.nodes.len() {
            let NodeKind::Object { .. } = self.nodes[node_id].kind else {
                continue;
            };
            self.sort_buffer.clear();
            let mut child = self.nodes[node_id].first_child;
            while child != NO_NODE {
                self.sort_buffer.try_reserve(1).map_err(|_| {
                    CanonicalJsonError::AllocationFailed {
                        resource: CanonicalJsonResource::SortedChildren,
                        requested_additional: 1,
                    }
                })?;
                self.sort_buffer.push(child);
                child = self
                    .nodes
                    .get(child)
                    .ok_or(CanonicalJsonError::InvalidEventSequence)?
                    .next_sibling;
            }
            let events = &self.events;
            let decoded = &self.decoded;
            let nodes = &self.nodes;
            self.sort_buffer.sort_unstable_by(|left, right| {
                Self::key_bytes(events, decoded, nodes, source, *left)
                    .cmp(Self::key_bytes(events, decoded, nodes, source, *right))
                    .then(left.cmp(right))
            });
            let sorted_start = self.sorted_children.len();
            let mut offset = 0;
            while offset < self.sort_buffer.len() {
                let key = Self::key_bytes(events, decoded, nodes, source, self.sort_buffer[offset]);
                let mut end = offset + 1;
                while end < self.sort_buffer.len()
                    && Self::key_bytes(events, decoded, nodes, source, self.sort_buffer[end]) == key
                {
                    end += 1;
                }
                self.sorted_children.try_reserve(1).map_err(|_| {
                    CanonicalJsonError::AllocationFailed {
                        resource: CanonicalJsonResource::SortedChildren,
                        requested_additional: 1,
                    }
                })?;
                self.sorted_children.push(self.sort_buffer[end - 1]);
                offset = end;
            }
            let sorted_len = self.sorted_children.len() - sorted_start;
            self.nodes[node_id].kind = NodeKind::Object {
                sorted_start,
                sorted_len,
            };
        }
        Ok(())
    }

    fn key_bytes<'a>(
        events: &'a [StoredEvent],
        decoded: &'a str,
        nodes: &[Node],
        source: &'a [u8],
        node_id: usize,
    ) -> &'a [u8] {
        let Some(node) = nodes.get(node_id) else {
            return &[];
        };
        let Some(event) = events.get(node.key_event_index) else {
            return &[];
        };
        match event.resolve(source, decoded) {
            JsonEvent::ObjectKey(key) => key.decoded_bytes(),
            _ => &[],
        }
    }

    fn serialize(&mut self, source: &[u8], output_limit: usize) -> Result<(), CanonicalJsonError> {
        self.output.clear();
        self.output_stack.clear();
        if self.nodes.is_empty() {
            return Err(CanonicalJsonError::InvalidEventSequence);
        }
        let initial_capacity = source.len().min(output_limit);
        self.output.try_reserve(initial_capacity).map_err(|_| {
            CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::Output,
                requested_additional: initial_capacity,
            }
        })?;
        self.push_output(OutputFrame::Value(0))?;
        while let Some(frame) = self.output_stack.pop() {
            match frame {
                OutputFrame::Value(node_id) => {
                    self.serialize_value(source, node_id, output_limit)?;
                }
                OutputFrame::Array {
                    next_child,
                    wrote_child,
                } => {
                    if next_child == NO_NODE {
                        self.append_literal(b"]", output_limit)?;
                        continue;
                    }
                    let child = self
                        .nodes
                        .get(next_child)
                        .ok_or(CanonicalJsonError::InvalidEventSequence)?;
                    self.push_output(OutputFrame::Array {
                        next_child: child.next_sibling,
                        wrote_child: true,
                    })?;
                    if wrote_child {
                        self.append_literal(b",", output_limit)?;
                    }
                    self.push_output(OutputFrame::Value(next_child))?;
                }
                OutputFrame::Object {
                    next_sorted,
                    sorted_end,
                    wrote_child,
                } => {
                    if next_sorted == sorted_end {
                        self.append_literal(b"}", output_limit)?;
                        continue;
                    }
                    let node_id = *self
                        .sorted_children
                        .get(next_sorted)
                        .ok_or(CanonicalJsonError::InvalidEventSequence)?;
                    self.push_output(OutputFrame::Object {
                        next_sorted: next_sorted + 1,
                        sorted_end,
                        wrote_child: true,
                    })?;
                    if wrote_child {
                        self.append_literal(b",", output_limit)?;
                    }
                    let key =
                        Self::key_bytes(&self.events, &self.decoded, &self.nodes, source, node_id);
                    let remaining = output_limit.saturating_sub(self.output.len());
                    append_json_key_bytes(
                        key,
                        &mut self.output,
                        JsonBytePolicy::StrictUtf8,
                        JsonEscapeLimits::new(key.len(), remaining),
                    )
                    .map_err(CanonicalJsonError::Escape)?;
                    self.push_output(OutputFrame::Value(node_id))?;
                }
            }
        }
        Ok(())
    }

    fn serialize_value(
        &mut self,
        source: &[u8],
        node_id: usize,
        output_limit: usize,
    ) -> Result<(), CanonicalJsonError> {
        let node = *self
            .nodes
            .get(node_id)
            .ok_or(CanonicalJsonError::InvalidEventSequence)?;
        match node.kind {
            NodeKind::Object {
                sorted_start,
                sorted_len,
            } => {
                self.append_literal(b"{", output_limit)?;
                let sorted_end = sorted_start
                    .checked_add(sorted_len)
                    .ok_or(CanonicalJsonError::SizeOverflow)?;
                self.push_output(OutputFrame::Object {
                    next_sorted: sorted_start,
                    sorted_end,
                    wrote_child: false,
                })
            }
            NodeKind::Array => {
                self.append_literal(b"[", output_limit)?;
                self.push_output(OutputFrame::Array {
                    next_child: node.first_child,
                    wrote_child: false,
                })
            }
            NodeKind::String => {
                let JsonEvent::String(value) =
                    self.events[node.event_index].resolve(source, &self.decoded)
                else {
                    return Err(CanonicalJsonError::InvalidEventSequence);
                };
                let remaining = output_limit.saturating_sub(self.output.len());
                append_json_string_bytes(
                    value.decoded_bytes(),
                    &mut self.output,
                    JsonBytePolicy::StrictUtf8,
                    JsonEscapeLimits::new(value.decoded_bytes().len(), remaining),
                )
                .map_err(CanonicalJsonError::Escape)
            }
            NodeKind::Number => {
                let (JsonEvent::Number(value), Some(syntax)) = (
                    self.events[node.event_index].resolve(source, &self.decoded),
                    self.events[node.event_index].number_syntax(),
                ) else {
                    return Err(CanonicalJsonError::InvalidEventSequence);
                };
                Self::append_number(&mut self.output, value, syntax, output_limit)
            }
            NodeKind::Boolean(true) => self.append_literal(b"true", output_limit),
            NodeKind::Boolean(false) => self.append_literal(b"false", output_limit),
            NodeKind::Null => self.append_literal(b"null", output_limit),
        }
    }

    fn append_number(
        output: &mut Vec<u8>,
        source: &[u8],
        syntax: super::number::ValidatedJsonNumberSyntax,
        output_limit: usize,
    ) -> Result<(), CanonicalJsonError> {
        let source =
            str::from_utf8(source).map_err(|_| CanonicalJsonError::InvalidEventSequence)?;
        if syntax.is_float() {
            return Self::append_float_number(output, source, output_limit);
        }
        let mut buffer = itoa::Buffer::new();
        if source.starts_with('-') {
            match source.parse::<i64>() {
                Ok(value) => Self::append_output_literal(
                    output,
                    buffer.format(value).as_bytes(),
                    output_limit,
                ),
                Err(_) => Self::append_float_number(output, source, output_limit),
            }
        } else {
            match source.parse::<u64>() {
                Ok(value) => Self::append_output_literal(
                    output,
                    buffer.format(value).as_bytes(),
                    output_limit,
                ),
                Err(_) => Self::append_float_number(output, source, output_limit),
            }
        }
    }

    fn append_float_number(
        output: &mut Vec<u8>,
        source: &str,
        output_limit: usize,
    ) -> Result<(), CanonicalJsonError> {
        let value = source
            .parse::<f64>()
            .map_err(|_| CanonicalJsonError::NumberOutOfRange)?;
        let formatted = format_nlohmann_float(value).map_err(|source| match source {
            NlohmannFloatError::NonFinite => CanonicalJsonError::NumberOutOfRange,
            NlohmannFloatError::InvalidFormat => CanonicalJsonError::Float(source),
        })?;
        Self::append_output_literal(output, formatted.as_str().as_bytes(), output_limit)
    }

    fn push_output(&mut self, frame: OutputFrame) -> Result<(), CanonicalJsonError> {
        self.output_stack
            .try_reserve(1)
            .map_err(|_| CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::OutputStack,
                requested_additional: 1,
            })?;
        self.output_stack.push(frame);
        Ok(())
    }

    fn append_literal(
        &mut self,
        value: &[u8],
        output_limit: usize,
    ) -> Result<(), CanonicalJsonError> {
        Self::append_output_literal(&mut self.output, value, output_limit)
    }

    fn append_output_literal(
        output: &mut Vec<u8>,
        value: &[u8],
        output_limit: usize,
    ) -> Result<(), CanonicalJsonError> {
        let end = output
            .len()
            .checked_add(value.len())
            .ok_or(CanonicalJsonError::SizeOverflow)?;
        if end > output_limit {
            return Err(CanonicalJsonError::Invalid(NdjsonInvalidRecordKind::Limit(
                super::ndjson::NdjsonLimitViolation::new(
                    NdjsonLimitResource::RecordBytes,
                    u64::try_from(end).unwrap_or(u64::MAX),
                    u64::try_from(output_limit).unwrap_or(u64::MAX),
                ),
            )));
        }
        output.try_reserve_exact(value.len()).map_err(|_| {
            CanonicalJsonError::AllocationFailed {
                resource: CanonicalJsonResource::Output,
                requested_additional: value.len(),
            }
        })?;
        output.extend_from_slice(value);
        Ok(())
    }
}
