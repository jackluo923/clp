use std::ops::Range;

/// A half-open byte range in the original UTF-8 KQL input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceSpan {
    start: usize,
    end: usize,
}

impl SourceSpan {
    pub(crate) const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Returns the first byte offset covered by this span.
    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    /// Returns the byte offset immediately after this span.
    #[must_use]
    pub const fn end(self) -> usize {
        self.end
    }

    /// Returns this span as a standard half-open range.
    #[must_use]
    pub const fn range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// Stable index of an expression in a [`ParsedQuery`] node arena.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NodeId(usize);

impl NodeId {
    pub(crate) const fn new(index: usize) -> Self {
        Self(index)
    }

    /// Returns the zero-based arena index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// An owned, archive-independent parsed KQL query.
///
/// Expressions use stable node IDs rather than recursive boxes. This keeps parsing and destruction
/// stack-safe even when callers deliberately raise the default depth limit.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedQuery {
    pub(crate) nodes: Vec<ExpressionNode>,
    pub(crate) root: NodeId,
}

impl ParsedQuery {
    /// Returns the root expression ID.
    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Returns all nodes in dependency-before-consumer order.
    #[must_use]
    pub fn nodes(&self) -> &[ExpressionNode] {
        &self.nodes
    }

    /// Returns one expression, or `None` for an ID from another query.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&ExpressionNode> {
        self.nodes.get(id.0)
    }
}

/// One expression stored in a [`ParsedQuery`] arena.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionNode {
    pub(crate) kind: ExpressionKind,
    pub(crate) span: SourceSpan,
}

impl ExpressionNode {
    /// Returns this expression's kind and operands.
    #[must_use]
    pub const fn kind(&self) -> &ExpressionKind {
        &self.kind
    }

    /// Returns the source bytes that produced this expression.
    #[must_use]
    pub const fn span(&self) -> SourceSpan {
        self.span
    }
}

/// Archive-independent KQL expression kind.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExpressionKind {
    /// One field comparison.
    Predicate(Predicate),
    /// The accepted compact `key:(...)` form.
    List(ListExpression),
    /// Logical negation. `NOT` binds more tightly than either binary operator.
    Not {
        /// Negated expression.
        operand: NodeId,
    },
    /// A binary Boolean expression.
    Boolean {
        /// `AND` or `OR`.
        operator: BooleanOperator,
        /// Left operand.
        left: NodeId,
        /// Right operand.
        right: NodeId,
    },
}

/// Binary KQL Boolean operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BooleanOperator {
    /// Both operands must match.
    And,
    /// Either operand may match.
    Or,
}

/// Comparison attached to a predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ComparisonOperator {
    /// KQL `:` equality or wildcard matching.
    Equal,
    /// Numeric/timestamp `<`.
    Less,
    /// Numeric/timestamp `<=`.
    LessOrEqual,
    /// Numeric/timestamp `>`.
    Greater,
    /// Numeric/timestamp `>=`.
    GreaterOrEqual,
}

/// One field predicate.
#[derive(Clone, Debug, PartialEq)]
pub struct Predicate {
    pub(crate) path: ColumnPath,
    pub(crate) operator: ComparisonOperator,
    pub(crate) value: Literal,
}

impl Predicate {
    /// Returns the fully prefixed field path.
    #[must_use]
    pub const fn path(&self) -> &ColumnPath {
        &self.path
    }

    /// Returns the comparison operation.
    #[must_use]
    pub const fn operator(&self) -> ComparisonOperator {
        self.operator
    }

    /// Returns the typed query literal.
    #[must_use]
    pub const fn value(&self) -> &Literal {
        &self.value
    }
}

/// Combination used by the compact `key:(...)` form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ListOperator {
    /// No keyword, or leading `OR`: any value may match.
    Any,
    /// Leading `AND`: every value must match.
    All,
    /// Leading `NOT`: every individual equality is negated.
    None,
}

/// Compact list predicate retained without expansion into a Boolean tree.
#[derive(Clone, Debug, PartialEq)]
pub struct ListExpression {
    pub(crate) path: ColumnPath,
    pub(crate) operator: ListOperator,
    pub(crate) values: Vec<Literal>,
}

impl ListExpression {
    /// Returns the fully prefixed field path shared by all list values.
    #[must_use]
    pub const fn path(&self) -> &ColumnPath {
        &self.path
    }

    /// Returns how list values are combined.
    #[must_use]
    pub const fn operator(&self) -> ListOperator {
        self.operator
    }

    /// Returns the typed list values in source order.
    #[must_use]
    pub fn values(&self) -> &[Literal] {
        &self.values
    }
}

/// Namespace selected by the first unescaped path byte.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ColumnNamespace {
    /// Ordinary event fields.
    #[default]
    Default,
    /// Auto-generated fields (`@`).
    Autogenerated,
    /// File/range-index metadata (`$`).
    RangeIndex,
    /// Reserved namespace `!`.
    ReservedBang,
    /// Reserved namespace `#`.
    ReservedHash,
}

impl ColumnNamespace {
    /// Returns the current one-byte KQL prefix, or `None` for the default namespace.
    #[must_use]
    pub const fn prefix(self) -> Option<char> {
        match self {
            Self::Default => None,
            Self::Autogenerated => Some('@'),
            Self::RangeIndex => Some('$'),
            Self::ReservedBang => Some('!'),
            Self::ReservedHash => Some('#'),
        }
    }
}

/// A fully resolved dot path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnPath {
    pub(crate) namespace: ColumnNamespace,
    pub(crate) components: Vec<PathComponent>,
}

impl ColumnPath {
    /// Returns the selected namespace.
    #[must_use]
    pub const fn namespace(&self) -> ColumnNamespace {
        self.namespace
    }

    /// Returns path components in outer-to-inner order.
    #[must_use]
    pub fn components(&self) -> &[PathComponent] {
        &self.components
    }

    /// Returns whether this is exactly the default-namespace `*` descriptor.
    #[must_use]
    pub fn is_default_wildcard(&self) -> bool {
        self.namespace == ColumnNamespace::Default
            && matches!(self.components.as_slice(), [component] if component.is_wildcard())
    }
}

/// One literal or whole-component-wildcard path segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathComponent {
    pub(crate) value: String,
    pub(crate) wildcard: bool,
}

impl PathComponent {
    /// Returns the unescaped component bytes as UTF-8.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns whether this component was exactly one unescaped `*`.
    #[must_use]
    pub const fn is_wildcard(&self) -> bool {
        self.wildcard
    }
}

/// Typed KQL literal.
///
/// Quoting does not force a string: both quoted and bare values follow the same ordered conversion
/// chain of `i64`, finite `f64`, exact lowercase Boolean/null, then wildcard string.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Literal {
    /// Signed 64-bit integer plus its unescaped source spelling.
    Integer {
        /// Parsed value.
        value: i64,
        /// Original spelling after KQL unescaping.
        source: String,
    },
    /// Finite binary64 value plus its unescaped source spelling.
    Float {
        /// Parsed finite value.
        value: f64,
        /// Original spelling after KQL unescaping.
        source: String,
    },
    /// Exact lowercase `true` or `false`.
    Boolean(bool),
    /// Exact lowercase `null`.
    Null,
    /// Cleaned C++-compatible wildcard string.
    String(StringLiteral),
    /// Syntactically validated timestamp call, awaiting semantic timestamp compilation.
    Timestamp(TimestampLiteral),
}

/// A C++-compatible cleaned wildcard pattern.
///
/// Unescaped `*` and `?` remain active. Literal wildcard and backslash characters retain a leading
/// backslash, and consecutive active `*` characters are collapsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StringLiteral {
    pub(crate) wildcard_pattern: String,
}

impl StringLiteral {
    /// Returns the cleaned wildcard pattern consumed by later search compilation.
    #[must_use]
    pub fn wildcard_pattern(&self) -> &str {
        &self.wildcard_pattern
    }

    /// Returns whether the pattern contains at least one unescaped wildcard.
    #[must_use]
    pub fn has_wildcards(&self) -> bool {
        let mut escaped = false;
        for byte in self.wildcard_pattern.bytes() {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if matches!(byte, b'*' | b'?') {
                return true;
            }
        }
        false
    }
}

/// Raw quoted arguments of exact lowercase `timestamp(...)` syntax.
///
/// Contents exclude surrounding quotes but retain timestamp-pattern backslashes verbatim. Epoch
/// resolution is intentionally deferred to the archive-independent semantic compilation layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimestampLiteral {
    pub(crate) value: String,
    pub(crate) pattern: Option<String>,
}

impl TimestampLiteral {
    /// Returns the raw content of the timestamp's first quoted argument.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns the optional raw timestamp-pattern content.
    #[must_use]
    pub fn pattern(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
}
