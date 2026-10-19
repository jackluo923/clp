//! Exact, bounded CLP-S search projection descriptors.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use super::ColumnNamespace;
use super::ColumnPath;
use super::PathComponent;
use crate::archive::NodeType;
use crate::archive::SchemaTree;

/// Resource bounds for parsing and resolving a search projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionLimits {
    columns: usize,
    path_components: usize,
    string_bytes: usize,
    schema_nodes: usize,
    path_states: usize,
    resolved_nodes: usize,
}

impl ProjectionLimits {
    /// Creates explicit projection bounds.
    #[must_use]
    pub const fn new(
        max_columns: usize,
        max_path_components: usize,
        max_owned_string_bytes: usize,
        max_schema_nodes: usize,
        max_path_states: usize,
        max_resolved_nodes: usize,
    ) -> Self {
        Self {
            columns: max_columns,
            path_components: max_path_components,
            string_bytes: max_owned_string_bytes,
            schema_nodes: max_schema_nodes,
            path_states: max_path_states,
            resolved_nodes: max_resolved_nodes,
        }
    }

    /// Maximum selected descriptors.
    #[must_use]
    pub const fn max_columns(self) -> usize {
        self.columns
    }

    /// Maximum components in one descriptor.
    #[must_use]
    pub const fn max_path_components(self) -> usize {
        self.path_components
    }

    /// Maximum aggregate decoded component bytes.
    #[must_use]
    pub const fn max_owned_string_bytes(self) -> usize {
        self.string_bytes
    }

    /// Maximum nodes indexed in an archive schema tree.
    #[must_use]
    pub const fn max_schema_nodes(self) -> usize {
        self.schema_nodes
    }

    /// Maximum aggregate child nodes visited while resolving descriptors.
    #[must_use]
    pub const fn max_path_states(self) -> usize {
        self.path_states
    }

    /// Maximum aggregate resolved node IDs before deduplication.
    #[must_use]
    pub const fn max_resolved_nodes(self) -> usize {
        self.resolved_nodes
    }
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self::new(
            65_536,
            256,
            4 * 1024 * 1024,
            1_048_576,
            4_194_304,
            1_048_576,
        )
    }
}

/// How a projected field is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldForm {
    /// Its value.
    Value,
    /// Spelled `shape(<descriptor>)`: a CLP string as its log-type dictionary entry holds it, its
    /// template, or the text of a string stored whole, which is not a log shape.
    Shape,
    /// Spelled `shape(<descriptor>, all)` or `shape(<descriptor>,all)`: a CLP string as its
    /// template, a string stored whole parsed for the template it would have had, as the C++
    /// `shape(...)` writes every string. A key that itself ends in `, all` is selected by writing
    /// its comma as `\u002c`.
    ParsedShape,
}

impl FieldForm {
    /// Splits a projected field into its descriptor and how it is written.
    #[must_use]
    pub fn split(field: &str) -> (&str, Self) {
        let Some(inner) = field
            .strip_prefix("shape(")
            .and_then(|inner| inner.strip_suffix(')'))
        else {
            return (field, Self::Value);
        };
        // The suffix is matched exactly, so a key's own spaces stay part of it.
        inner
            .strip_suffix(", all")
            .or_else(|| inner.strip_suffix(",all"))
            .map_or((inner, Self::Shape), |descriptor| {
                (descriptor, Self::ParsedShape)
            })
    }

    /// Whether the field is written as a shape of either kind.
    #[must_use]
    pub const fn is_shape(self) -> bool {
        !matches!(self, Self::Value)
    }
}

/// Search-result projection resolved independently of any archive.
///
/// [`Self::all`] returns every ordinary event field. [`Self::selected`] parses the same escaped
/// dot descriptors as the C++ `--projection` option and rejects wildcard or duplicate columns. A
/// descriptor wrapped as `shape(...)` or `shape(..., all)` selects its field as the C++ projection
/// function does, a CLP-encoded string written as a template; see [`FieldForm`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    columns: Option<Vec<ColumnPath>>,
    /// How each selected column is written.
    shapes: Vec<FieldForm>,
    limits: ProjectionLimits,
}

impl Projection {
    /// Returns the default projection containing every ordinary event field.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            columns: None,
            shapes: Vec::new(),
            limits: ProjectionLimits::new(0, 0, 0, 0, 0, 0),
        }
    }

    /// Parses an explicit list of selected columns.
    ///
    /// An empty slice deliberately means an empty object for every match; callers implementing a
    /// CLI should use [`Self::all`] when no projection option was supplied.
    ///
    /// # Errors
    ///
    /// Returns an indexed syntax, duplicate, wildcard, limit, overflow, or allocation error.
    pub fn selected<S: AsRef<str>>(
        columns: &[S],
        limits: ProjectionLimits,
    ) -> Result<Self, ProjectionError> {
        check_limit(
            ProjectionResource::Columns,
            columns.len(),
            limits.max_columns(),
        )?;
        let mut parsed: Vec<ColumnPath> = Vec::new();
        parsed
            .try_reserve_exact(columns.len())
            .map_err(|_| allocation(ProjectionResource::Columns, columns.len()))?;
        let mut shapes: Vec<FieldForm> = Vec::new();
        shapes
            .try_reserve_exact(columns.len())
            .map_err(|_| allocation(ProjectionResource::Columns, columns.len()))?;
        let mut total_source_bytes = 0_usize;
        let mut total_string_bytes = 0_usize;
        for (column_index, source) in columns.iter().enumerate() {
            total_source_bytes = total_source_bytes
                .checked_add(source.as_ref().len())
                .ok_or(ProjectionError::SizeOverflow)?;
            check_limit(
                ProjectionResource::Strings,
                total_source_bytes,
                limits.max_owned_string_bytes(),
            )?;
            let (descriptor, form) = FieldForm::split(source.as_ref());
            let column =
                parse_descriptor(descriptor, column_index, limits, &mut total_string_bytes)?;
            // The pinned C++ `ColumnDescriptor::operator==` deliberately/observably compares the
            // descriptor tokens and type flags but not the namespace. As in C++, a column may be
            // selected both plainly and as `shape(...)`, though only one shape of it can be
            // written.
            if let Some(first_column) =
                parsed
                    .iter()
                    .zip(&shapes)
                    .position(|(existing, existing_form)| {
                        existing.components() == column.components()
                            && existing_form.is_shape() == form.is_shape()
                    })
            {
                return Err(ProjectionError::DuplicateColumn {
                    first_column,
                    duplicate_column: column_index,
                });
            }
            parsed.push(column);
            shapes.push(form);
        }
        Ok(Self {
            columns: Some(parsed),
            shapes,
            limits,
        })
    }

    /// Returns whether any selected column is wrapped in `shape(...)`.
    #[must_use]
    pub fn has_shapes(&self) -> bool {
        self.shapes.iter().any(|form| form.is_shape())
    }

    /// Resolves the `shape(...)` columns, then the `shape(..., all)` columns, to the CLP-encoded
    /// string nodes written as templates. A node of another type at such a path is written as its
    /// value, as in C++, and so is a CLP string inside a structured array, which no descriptor
    /// addresses.
    pub(super) fn resolve_shapes(
        &self,
        schema_tree: &SchemaTree,
    ) -> Result<(Vec<u32>, Vec<u32>), ProjectionError> {
        Ok((
            self.resolve_shape_form(schema_tree, FieldForm::Shape)?,
            self.resolve_shape_form(schema_tree, FieldForm::ParsedShape)?,
        ))
    }

    fn resolve_shape_form(
        &self,
        schema_tree: &SchemaTree,
        form: FieldForm,
    ) -> Result<Vec<u32>, ProjectionError> {
        let Some(columns) = &self.columns else {
            return Ok(Vec::new());
        };
        let shaped: Vec<ColumnPath> = columns
            .iter()
            .zip(&self.shapes)
            .filter(|(_, shape)| form == **shape)
            .map(|(column, _)| column.clone())
            .collect();
        if shaped.is_empty() {
            return Ok(Vec::new());
        }
        let plain = vec![FieldForm::Value; shaped.len()];
        let resolved = Self {
            columns: Some(shaped),
            shapes: plain,
            limits: self.limits,
        }
        .resolve_with(schema_tree, false)?;
        let mut nodes = resolved.selected_node_ids().unwrap_or_default().to_vec();
        nodes.retain(|node| {
            schema_tree
                .get(*node as usize)
                .is_some_and(|node| NodeType::ClpString == node.node_type())
        });
        Ok(nodes)
    }

    /// Returns whether every ordinary event field is selected.
    #[must_use]
    pub const fn selects_all(&self) -> bool {
        self.columns.is_none()
    }

    /// Returns the selected parsed descriptors, or `None` for the all-fields projection.
    #[must_use]
    pub fn selected_columns(&self) -> Option<&[ColumnPath]> {
        self.columns.as_deref()
    }

    /// Returns the resource limits retained for archive resolution.
    #[must_use]
    pub const fn limits(&self) -> ProjectionLimits {
        self.limits
    }

    /// Resolves each path to the nodes holding its value. An object or structured-array path
    /// resolves to nothing, as in the C++ reader, which suits an aggregation over scalar leaves.
    pub(super) fn resolve(
        &self,
        schema_tree: &SchemaTree,
    ) -> Result<ResolvedProjection, ProjectionError> {
        self.resolve_with(schema_tree, false)
    }

    /// Resolves each path for writing records out: as [`Self::resolve`], except that an object or
    /// structured-array path selects everything beneath it, so the record carries that value as
    /// stored. With arrays structured, as the writer does by default, a projection could otherwise
    /// never return an array-valued field.
    pub(super) fn resolve_records(
        &self,
        schema_tree: &SchemaTree,
    ) -> Result<ResolvedProjection, ProjectionError> {
        self.resolve_with(schema_tree, true)
    }

    fn resolve_with(
        &self,
        schema_tree: &SchemaTree,
        subtrees: bool,
    ) -> Result<ResolvedProjection, ProjectionError> {
        let Some(columns) = &self.columns else {
            return Ok(ResolvedProjection::All);
        };
        check_limit(
            ProjectionResource::SchemaNodes,
            schema_tree.len(),
            self.limits.max_schema_nodes(),
        )?;
        let index = ChildIndex::build(schema_tree)?;
        let mut nodes = Vec::new();
        let mut path_states = 0_usize;
        for path in columns {
            let Some(mut current) = index.namespace_root(schema_tree, path.namespace()) else {
                continue;
            };
            for (component_index, component) in path.components().iter().enumerate() {
                let last = component_index + 1 == path.components().len();
                let mut next_object = None;
                for &child_id in index.children_of(current)? {
                    path_states = path_states
                        .checked_add(1)
                        .ok_or(ProjectionError::SizeOverflow)?;
                    check_limit(
                        ProjectionResource::PathStates,
                        path_states,
                        self.limits.max_path_states(),
                    )?;
                    let child = schema_tree
                        .get(child_id)
                        .ok_or(ProjectionError::SizeOverflow)?;
                    if child.key_bytes() != component.value().as_bytes() {
                        continue;
                    }
                    if last {
                        // An object or structured-array root holds no value of its own: its value
                        // is everything beneath it. The C++ reader's ordered-column map has no
                        // entry for such a root, so `resolve` projects nothing for it, while
                        // `resolve_records` selects the subtree.
                        let structural = matches!(
                            child.node_type(),
                            NodeType::Object | NodeType::StructuredArray
                        );
                        if structural && !subtrees {
                            // Characterized no-op.
                        } else if structural {
                            push_descendants(
                                &index,
                                schema_tree,
                                child_id,
                                &mut nodes,
                                self.limits,
                            )?;
                        } else {
                            let retained = nodes
                                .len()
                                .checked_add(1)
                                .ok_or(ProjectionError::SizeOverflow)?;
                            check_limit(
                                ProjectionResource::ResolvedNodes,
                                retained,
                                self.limits.max_resolved_nodes(),
                            )?;
                            nodes
                                .try_reserve(1)
                                .map_err(|_| allocation(ProjectionResource::ResolvedNodes, 1))?;
                            nodes.push(
                                u32::try_from(child_id)
                                    .map_err(|_| ProjectionError::SizeOverflow)?,
                            );
                        }
                    } else if NodeType::Object == child.node_type() {
                        next_object = Some(child_id);
                        break;
                    }
                }
                if last {
                    break;
                }
                let Some(next) = next_object else {
                    break;
                };
                current = next;
            }
        }
        nodes.sort_unstable();
        nodes.dedup();
        Ok(ResolvedProjection::Selected(nodes))
    }
}

impl Default for Projection {
    fn default() -> Self {
        Self::all()
    }
}

/// Selects `root` and every node beneath it.
fn push_descendants(
    index: &ChildIndex,
    schema_tree: &SchemaTree,
    root: usize,
    nodes: &mut Vec<u32>,
    limits: ProjectionLimits,
) -> Result<(), ProjectionError> {
    let mut pending = vec![root];
    while let Some(node_id) = pending.pop() {
        schema_tree
            .get(node_id)
            .ok_or(ProjectionError::SizeOverflow)?;
        let retained = nodes
            .len()
            .checked_add(1)
            .ok_or(ProjectionError::SizeOverflow)?;
        check_limit(
            ProjectionResource::ResolvedNodes,
            retained,
            limits.max_resolved_nodes(),
        )?;
        nodes
            .try_reserve(1)
            .map_err(|_| allocation(ProjectionResource::ResolvedNodes, 1))?;
        nodes.push(u32::try_from(node_id).map_err(|_| ProjectionError::SizeOverflow)?);
        pending.extend_from_slice(index.children_of(node_id)?);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ResolvedProjection {
    All,
    Selected(Vec<u32>),
}

impl ResolvedProjection {
    pub(super) fn selected_node_ids(&self) -> Option<&[u32]> {
        match self {
            Self::All => None,
            Self::Selected(nodes) => Some(nodes),
        }
    }
}

struct ChildIndex {
    offsets: Vec<usize>,
    children: Vec<usize>,
    roots: Vec<usize>,
}

impl ChildIndex {
    fn build(tree: &SchemaTree) -> Result<Self, ProjectionError> {
        let offset_count = tree
            .len()
            .checked_add(1)
            .ok_or(ProjectionError::SizeOverflow)?;
        let mut offsets = filled_vec(offset_count, 0_usize, ProjectionResource::SchemaNodes)?;
        let mut roots = Vec::new();
        roots
            .try_reserve(tree.len())
            .map_err(|_| allocation(ProjectionResource::SchemaNodes, tree.len()))?;
        for (node_id, node) in tree.nodes().iter().enumerate() {
            if let Some(parent_id) = node.parent_id() {
                let offset_index = parent_id
                    .checked_add(1)
                    .ok_or(ProjectionError::SizeOverflow)?;
                let count = offsets
                    .get_mut(offset_index)
                    .ok_or(ProjectionError::SizeOverflow)?;
                *count = count.checked_add(1).ok_or(ProjectionError::SizeOverflow)?;
            } else {
                roots.push(node_id);
            }
        }
        for index in 1..offsets.len() {
            offsets[index] = offsets[index]
                .checked_add(offsets[index - 1])
                .ok_or(ProjectionError::SizeOverflow)?;
        }
        let child_count = *offsets.last().ok_or(ProjectionError::SizeOverflow)?;
        let mut children = filled_vec(child_count, 0_usize, ProjectionResource::SchemaNodes)?;
        let mut cursors = Vec::new();
        cursors
            .try_reserve_exact(offsets.len())
            .map_err(|_| allocation(ProjectionResource::SchemaNodes, offsets.len()))?;
        cursors.extend_from_slice(&offsets);
        for (node_id, node) in tree.nodes().iter().enumerate() {
            if let Some(parent_id) = node.parent_id() {
                let cursor = cursors
                    .get_mut(parent_id)
                    .ok_or(ProjectionError::SizeOverflow)?;
                let destination = children
                    .get_mut(*cursor)
                    .ok_or(ProjectionError::SizeOverflow)?;
                *destination = node_id;
                *cursor = cursor.checked_add(1).ok_or(ProjectionError::SizeOverflow)?;
            }
        }
        Ok(Self {
            offsets,
            children,
            roots,
        })
    }

    fn namespace_root(&self, tree: &SchemaTree, namespace: ColumnNamespace) -> Option<usize> {
        let key = namespace_bytes(namespace);
        self.roots.iter().copied().find(|root_id| {
            tree.get(*root_id).is_some_and(|root| {
                NodeType::Metadata != root.node_type() && root.key_bytes() == key
            })
        })
    }

    fn children_of(&self, node_id: usize) -> Result<&[usize], ProjectionError> {
        let start = *self
            .offsets
            .get(node_id)
            .ok_or(ProjectionError::SizeOverflow)?;
        let end = *self
            .offsets
            .get(node_id + 1)
            .ok_or(ProjectionError::SizeOverflow)?;
        self.children
            .get(start..end)
            .ok_or(ProjectionError::SizeOverflow)
    }
}

fn parse_descriptor(
    source: &str,
    column_index: usize,
    limits: ProjectionLimits,
    total_string_bytes: &mut usize,
) -> Result<ColumnPath, ProjectionError> {
    let (namespace, content, content_offset) = match source.as_bytes().first() {
        Some(b'@') => (ColumnNamespace::Autogenerated, &source[1..], 1),
        Some(b'$') => (ColumnNamespace::RangeIndex, &source[1..], 1),
        Some(b'!') => (ColumnNamespace::ReservedBang, &source[1..], 1),
        Some(b'#') => (ColumnNamespace::ReservedHash, &source[1..], 1),
        _ => (ColumnNamespace::Default, source, 0),
    };
    if content.is_empty() {
        return Err(ProjectionError::EmptyPathComponent {
            column_index,
            offset: content_offset,
        });
    }

    let raw_components = split_descriptor(content, column_index, content_offset, limits)?;
    let mut components = Vec::new();
    components
        .try_reserve_exact(raw_components.len())
        .map_err(|_| allocation(ProjectionResource::PathComponents, raw_components.len()))?;
    for (component_index, raw) in raw_components.iter().enumerate() {
        let encoded = decode_kql_key(raw, column_index, content_offset)?;
        let wildcard = encoded == "*";
        let value = decode_descriptor_token(&encoded, column_index, content_offset)?;
        if wildcard {
            return Err(ProjectionError::WildcardColumn {
                column_index,
                component_index,
            });
        }
        *total_string_bytes = total_string_bytes
            .checked_add(value.len())
            .ok_or(ProjectionError::SizeOverflow)?;
        check_limit(
            ProjectionResource::Strings,
            *total_string_bytes,
            limits.max_owned_string_bytes(),
        )?;
        components.push(PathComponent {
            value,
            wildcard: false,
        });
    }
    Ok(ColumnPath {
        namespace,
        components,
    })
}

fn split_descriptor(
    source: &str,
    column_index: usize,
    base_offset: usize,
    limits: ProjectionLimits,
) -> Result<Vec<String>, ProjectionError> {
    let mut components = Vec::new();
    let mut current = String::new();
    let bytes = source.as_bytes();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => {
                let escaped = *bytes
                    .get(cursor + 1)
                    .ok_or(ProjectionError::InvalidEscape {
                        column_index,
                        offset: base_offset + cursor,
                    })?;
                if !escaped.is_ascii() {
                    return Err(ProjectionError::InvalidEscape {
                        column_index,
                        offset: base_offset + cursor,
                    });
                }
                if b'.' == escaped {
                    reserve_string(&mut current, 1)?;
                    current.push('.');
                } else {
                    reserve_string(&mut current, 2)?;
                    current.push('\\');
                    current.push(char::from(escaped));
                }
                cursor += 2;
            }
            b'.' => {
                if current.is_empty() {
                    return Err(ProjectionError::EmptyPathComponent {
                        column_index,
                        offset: base_offset + cursor,
                    });
                }
                push_component(&mut components, &mut current, limits)?;
                cursor += 1;
            }
            _ => {
                let character = source[cursor..]
                    .chars()
                    .next()
                    .ok_or(ProjectionError::SizeOverflow)?;
                reserve_string(&mut current, character.len_utf8())?;
                current.push(character);
                cursor += character.len_utf8();
            }
        }
    }
    if current.is_empty() {
        return Err(ProjectionError::EmptyPathComponent {
            column_index,
            offset: base_offset + source.len(),
        });
    }
    push_component(&mut components, &mut current, limits)?;
    Ok(components)
}

fn push_component(
    components: &mut Vec<String>,
    current: &mut String,
    limits: ProjectionLimits,
) -> Result<(), ProjectionError> {
    let count = components
        .len()
        .checked_add(1)
        .ok_or(ProjectionError::SizeOverflow)?;
    if count > limits.max_path_components() {
        return Err(ProjectionError::LimitExceeded {
            resource: ProjectionResource::PathComponents,
            actual: count,
            limit: limits.max_path_components(),
        });
    }
    components
        .try_reserve(1)
        .map_err(|_| allocation(ProjectionResource::PathComponents, 1))?;
    components.push(std::mem::take(current));
    Ok(())
}

fn reserve_string(value: &mut String, additional: usize) -> Result<(), ProjectionError> {
    value
        .try_reserve(additional)
        .map_err(|_| allocation(ProjectionResource::Strings, additional))
}

fn decode_kql_key(
    raw: &str,
    column_index: usize,
    base_offset: usize,
) -> Result<String, ProjectionError> {
    let mut decoded = String::new();
    decoded
        .try_reserve(raw.len())
        .map_err(|_| allocation(ProjectionResource::Strings, raw.len()))?;
    let bytes = raw.as_bytes();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if b'\\' != bytes[cursor] {
            let character = raw[cursor..]
                .chars()
                .next()
                .ok_or(ProjectionError::SizeOverflow)?;
            decoded.push(character);
            cursor += character.len_utf8();
            continue;
        }
        let slash = cursor;
        cursor += 1;
        let escaped = *bytes.get(cursor).ok_or(ProjectionError::InvalidEscape {
            column_index,
            offset: base_offset + slash,
        })?;
        cursor += 1;
        match escaped {
            b'\\' => decoded.push_str("\\\\"),
            b'"' => decoded.push('"'),
            b't' => decoded.push('\t'),
            b'r' => decoded.push('\r'),
            b'n' => decoded.push('\n'),
            b'b' => decoded.push('\u{0008}'),
            b'f' => decoded.push('\u{000c}'),
            b'u' => {
                let end = cursor.checked_add(4).ok_or(ProjectionError::SizeOverflow)?;
                let hex = raw.get(cursor..end).ok_or(ProjectionError::InvalidEscape {
                    column_index,
                    offset: base_offset + slash,
                })?;
                if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(ProjectionError::InvalidEscape {
                        column_index,
                        offset: base_offset + slash,
                    });
                }
                let scalar =
                    u32::from_str_radix(hex, 16).map_err(|_| ProjectionError::InvalidEscape {
                        column_index,
                        offset: base_offset + slash,
                    })?;
                let character = char::from_u32(scalar).ok_or(ProjectionError::InvalidEscape {
                    column_index,
                    offset: base_offset + slash,
                })?;
                cursor = end;
                if '\\' == character {
                    decoded.push_str("\\\\");
                } else if '*' == character {
                    decoded.push_str("\\*");
                } else {
                    decoded.push(character);
                }
            }
            b'{' => decoded.push('{'),
            b'}' => decoded.push('}'),
            b'(' => decoded.push('('),
            b')' => decoded.push(')'),
            b'<' => decoded.push('<'),
            b'>' => decoded.push('>'),
            b'*' => decoded.push_str("\\*"),
            b'?' => decoded.push('?'),
            b'@' => decoded.push('@'),
            b'$' => decoded.push('$'),
            b'!' => decoded.push('!'),
            b'#' => decoded.push('#'),
            _ => {
                return Err(ProjectionError::InvalidEscape {
                    column_index,
                    offset: base_offset + slash,
                });
            }
        }
    }
    Ok(decoded)
}

fn decode_descriptor_token(
    encoded: &str,
    column_index: usize,
    base_offset: usize,
) -> Result<String, ProjectionError> {
    let mut decoded = String::new();
    decoded
        .try_reserve(encoded.len())
        .map_err(|_| allocation(ProjectionResource::Strings, encoded.len()))?;
    let mut escaped = false;
    for character in encoded.chars() {
        if escaped {
            decoded.push(character);
            escaped = false;
        } else if '\\' == character {
            escaped = true;
        } else {
            decoded.push(character);
        }
    }
    if escaped {
        return Err(ProjectionError::InvalidEscape {
            column_index,
            offset: base_offset + encoded.len().saturating_sub(1),
        });
    }
    Ok(decoded)
}

const fn namespace_bytes(namespace: ColumnNamespace) -> &'static [u8] {
    match namespace {
        ColumnNamespace::Default => b"",
        ColumnNamespace::Autogenerated => b"@",
        ColumnNamespace::RangeIndex => b"$",
        ColumnNamespace::ReservedBang => b"!",
        ColumnNamespace::ReservedHash => b"#",
    }
}

fn filled_vec<T: Clone>(
    count: usize,
    value: T,
    resource: ProjectionResource,
) -> Result<Vec<T>, ProjectionError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| allocation(resource, count))?;
    values.resize(count, value);
    Ok(values)
}

const fn check_limit(
    resource: ProjectionResource,
    actual: usize,
    limit: usize,
) -> Result<(), ProjectionError> {
    if actual > limit {
        Err(ProjectionError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

const fn allocation(resource: ProjectionResource, requested: usize) -> ProjectionError {
    ProjectionError::AllocationFailed {
        resource,
        requested,
    }
}

/// Resource governed by [`ProjectionLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProjectionResource {
    /// Projection descriptor collection.
    Columns,
    /// Components in one descriptor.
    PathComponents,
    /// Aggregate owned decoded component bytes.
    Strings,
    /// Indexed archive schema-tree nodes.
    SchemaNodes,
    /// Child nodes visited during resolution.
    PathStates,
    /// Resolved schema node IDs.
    ResolvedNodes,
}

impl Display for ProjectionResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Columns => "projection columns",
            Self::PathComponents => "projection path components",
            Self::Strings => "projection owned string bytes",
            Self::SchemaNodes => "projection schema nodes",
            Self::PathStates => "projection path states",
            Self::ResolvedNodes => "projection resolved nodes",
        })
    }
}

/// Failure to parse or resolve a search projection.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProjectionError {
    /// A descriptor is empty, starts or ends with `.`, or contains adjacent dots.
    EmptyPathComponent {
        /// Zero-based descriptor index.
        column_index: usize,
        /// Byte offset within that descriptor.
        offset: usize,
    },
    /// A descriptor contains a truncated or unsupported C++ KQL escape.
    InvalidEscape {
        /// Zero-based descriptor index.
        column_index: usize,
        /// Byte offset within that descriptor.
        offset: usize,
    },
    /// Projection descriptors may not contain an unescaped whole-component `*`.
    WildcardColumn {
        /// Zero-based descriptor index.
        column_index: usize,
        /// Zero-based component index.
        component_index: usize,
    },
    /// The same parsed descriptor was supplied more than once.
    DuplicateColumn {
        /// First occurrence.
        first_column: usize,
        /// Duplicate occurrence.
        duplicate_column: usize,
    },
    /// A configured projection resource bound was exceeded.
    LimitExceeded {
        /// Bounded resource.
        resource: ProjectionResource,
        /// Observed amount.
        actual: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A bounded projection allocation failed.
    AllocationFailed {
        /// Allocation category.
        resource: ProjectionResource,
        /// Requested element or byte count.
        requested: usize,
    },
    /// Checked size arithmetic or a wire-node conversion overflowed.
    SizeOverflow,
}

impl Display for ProjectionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPathComponent {
                column_index,
                offset,
            } => write!(
                formatter,
                "projection column {column_index} has an empty path component at byte {offset}"
            ),
            Self::InvalidEscape {
                column_index,
                offset,
            } => write!(
                formatter,
                "projection column {column_index} has an invalid escape at byte {offset}"
            ),
            Self::WildcardColumn {
                column_index,
                component_index,
            } => write!(
                formatter,
                "projection column {column_index}, component {component_index} contains a wildcard"
            ),
            Self::DuplicateColumn {
                first_column,
                duplicate_column,
            } => write!(
                formatter,
                "projection column {duplicate_column} duplicates column {first_column}"
            ),
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(
                formatter,
                "{resource} limit exceeded: actual {actual}, limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to allocate {requested} element(s) for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("projection size arithmetic overflow"),
        }
    }
}

impl Error for ProjectionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_pin_cpp_escaping_duplicates_and_wildcards() {
        let columns = [r"a\.b", r"literal\*", r"unicode\u002a", r"slash\\key"];
        let projection =
            Projection::selected(&columns, ProjectionLimits::default()).expect("parse");
        let parsed = projection.selected_columns().expect("selected mode");
        assert_eq!("a.b", parsed[0].components()[0].value());
        assert_eq!("literal*", parsed[1].components()[0].value());
        assert_eq!("unicode*", parsed[2].components()[0].value());
        assert_eq!(r"slash\key", parsed[3].components()[0].value());

        assert!(matches!(
            Projection::selected(&["id", "id"], ProjectionLimits::default()),
            Err(ProjectionError::DuplicateColumn { .. })
        ));
        assert!(matches!(
            Projection::selected(&["id", "@id"], ProjectionLimits::default()),
            Err(ProjectionError::DuplicateColumn { .. })
        ));
        assert!(matches!(
            Projection::selected(&["a.*"], ProjectionLimits::default()),
            Err(ProjectionError::WildcardColumn { .. })
        ));
        assert!(matches!(
            Projection::selected(&["a..b"], ProjectionLimits::default()),
            Err(ProjectionError::EmptyPathComponent { .. })
        ));
    }

    #[test]
    fn shape_descriptors_select_their_column_in_their_own_mode() {
        let projection = Projection::selected(
            &["shape(a.b)", "a.b", "shape(c)"],
            ProjectionLimits::default(),
        )
        .expect("parse");
        assert!(projection.has_shapes());
        let parsed = projection.selected_columns().expect("selected mode");
        assert_eq!(
            ["a", "b"],
            [
                parsed[0].components()[0].value(),
                parsed[0].components()[1].value()
            ]
        );
        assert_eq!("c", parsed[2].components()[0].value());
        assert!(
            !Projection::selected(&["a"], ProjectionLimits::default())
                .expect("parse")
                .has_shapes()
        );
        assert!(matches!(
            Projection::selected(&["shape(id)", "shape(id)"], ProjectionLimits::default()),
            Err(ProjectionError::DuplicateColumn { .. })
        ));
        assert!(
            matches!(
                Projection::selected(
                    &["shape(id)", "shape(id, all)"],
                    ProjectionLimits::default()
                ),
                Err(ProjectionError::DuplicateColumn { .. })
            ),
            "both shapes of one field would write the same key"
        );
        assert_eq!(("a.b", FieldForm::Value), FieldForm::split("a.b"));
        assert_eq!(("a.b", FieldForm::Shape), FieldForm::split("shape(a.b)"));
        assert_eq!(
            ("a.b", FieldForm::ParsedShape),
            FieldForm::split("shape(a.b, all)")
        );
        assert_eq!(
            ("a.b", FieldForm::ParsedShape),
            FieldForm::split("shape(a.b,all)")
        );
        assert_eq!(
            ("a.b, some", FieldForm::Shape),
            FieldForm::split("shape(a.b, some)"),
            "only `all` selects the parsed shape"
        );
        assert_eq!(
            ("my key ", FieldForm::ParsedShape),
            FieldForm::split("shape(my key , all)"),
            "a key's own spaces stay part of it"
        );
        assert_eq!(
            ("x\\u002c all", FieldForm::Shape),
            FieldForm::split("shape(x\\u002c all)"),
            "an escaped comma selects a key ending in `, all`"
        );
        let literal = Projection::selected(&["shape(x\\u002c all)"], ProjectionLimits::default())
            .expect("parse");
        assert_eq!(
            "x, all",
            literal.selected_columns().expect("selected mode")[0].components()[0].value()
        );
        assert!(matches!(
            Projection::selected(&["shape(a.*)"], ProjectionLimits::default()),
            Err(ProjectionError::WildcardColumn { .. })
        ));
    }

    #[test]
    fn explicit_empty_projection_differs_from_all_columns() {
        assert!(Projection::all().selects_all());
        let empty = Projection::selected::<&str>(&[], ProjectionLimits::default()).expect("empty");
        assert!(!empty.selects_all());
        assert_eq!(Some(&[][..]), empty.selected_columns());
    }
}
