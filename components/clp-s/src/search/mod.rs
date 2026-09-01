//! Bounded parsing and archive-backed matching for the pinned CLP-S KQL dialect.
//!
//! Parsing produces an owned archive-independent node arena. Semantic compilation then binds paths
//! and dictionary matches to one archive catalog without normalizing the query into exponential
//! DNF or serializing rows as JSON. Current-format timestamp literals and authoritative timestamp
//! bounds are resolved during semantic compilation.

mod aggregation;
mod archive;
mod array;
mod ast;
mod jsonl;
mod kv_ir;
mod lexer;
mod msgpack;
mod parser;
mod projection;
mod reducer;
mod results_cache;
mod semantic;
mod timestamp_query;
mod wildcard;

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

pub use aggregation::AggregationError;
pub use aggregation::AggregationJsonError;
pub use aggregation::AggregationKind;
pub use aggregation::AggregationLimits;
pub use aggregation::AggregationNumber;
pub use aggregation::AggregationPlan;
pub use aggregation::AggregationPlanError;
pub use aggregation::AggregationResource;
pub use aggregation::AggregationResultDocument;
pub use aggregation::AggregationResultRef;
pub use aggregation::AggregationResults;
pub use aggregation::AggregationSink;
pub use aggregation::AggregationValueRef;
pub use aggregation::TimestampStringError;
pub use archive::ArchiveMatchSink;
pub use archive::ArchiveMatchingRows;
pub use archive::ArchiveRowRef;
pub use archive::ArchiveSearchError;
pub use archive::ArchiveSearchOptions;
pub use archive::ArchiveSearchStats;
pub use archive::ArchiveTableMatches;
pub use archive::search_archive;
pub use array::ArraySearchError;
pub use array::ArraySyntaxErrorKind;
pub use ast::BooleanOperator;
pub use ast::ColumnNamespace;
pub use ast::ColumnPath;
pub use ast::ComparisonOperator;
pub use ast::ExpressionKind;
pub use ast::ExpressionNode;
pub use ast::ListExpression;
pub use ast::ListOperator;
pub use ast::Literal;
pub use ast::NodeId;
pub use ast::ParsedQuery;
pub use ast::PathComponent;
pub use ast::Predicate;
pub use ast::SourceSpan;
pub use ast::StringLiteral;
pub use ast::TimestampLiteral;
pub use jsonl::SearchJsonlAdapter;
pub use jsonl::SearchJsonlAdapterError;
pub use jsonl::SearchJsonlOptions;
pub use kv_ir::KvIrEncodedTextError;
pub use kv_ir::KvIrJsonlError;
pub use kv_ir::KvIrJsonlLimitResource;
pub use kv_ir::KvIrJsonlLimits;
pub use kv_ir::KvIrJsonlMatchSink;
pub use kv_ir::KvIrJsonlOptions;
pub use kv_ir::KvIrJsonlResource;
pub use kv_ir::KvIrMatchSink;
pub use kv_ir::KvIrMatchedEvent;
pub use kv_ir::KvIrSearchError;
pub use kv_ir::KvIrSearchFailure;
pub use kv_ir::KvIrSearchInvalidData;
pub use kv_ir::KvIrSearchLimitResource;
pub use kv_ir::KvIrSearchLimitViolation;
pub use kv_ir::KvIrSearchLimits;
pub use kv_ir::KvIrSearchOptions;
pub use kv_ir::KvIrSearchResource;
pub use kv_ir::KvIrSearchSchemaNode;
pub use kv_ir::KvIrSearchSink;
pub use kv_ir::KvIrSearchStats;
pub use kv_ir::is_cpp_tolerated_kv_ir_truncation;
pub use kv_ir::is_kv_ir_search_candidate;
pub use kv_ir::search_first_kv_ir_stream;
pub use lexer::KqlToken;
pub use lexer::KqlTokenKind;
pub use lexer::lex_kql;
pub use msgpack::SearchMsgpackAdapter;
pub use msgpack::SearchMsgpackAdapterError;
pub use msgpack::SearchMsgpackOptions;
pub use msgpack::SearchMsgpackResource;
pub use msgpack::SearchMsgpackString;
pub use parser::parse_kql;
pub use projection::Projection;
pub use projection::ProjectionError;
pub use projection::ProjectionLimits;
pub use projection::ProjectionResource;
pub use reducer::ReducerProtocol;
pub use reducer::ReducerProtocolError;
pub use results_cache::AggregationResultsCacheAdapter;
pub use results_cache::AggregationResultsCacheAdapterError;
pub use results_cache::AggregationResultsCacheBatchSink;
pub use results_cache::ResultsCacheOptionsError;
pub use results_cache::ResultsCacheResource;
pub use results_cache::ResultsCacheSearchResult;
pub use results_cache::SearchResultsCacheAdapter;
pub use results_cache::SearchResultsCacheAdapterError;
pub use results_cache::SearchResultsCacheBatchSink;
pub use results_cache::SearchResultsCacheOptions;
pub use semantic::AuthoritativeTimestampRange;
pub use semantic::CompiledQuery;
pub use semantic::MatchBitmap;
pub use semantic::MatchingRows;
pub use semantic::SearchError;
pub use semantic::SearchLimits;
pub use semantic::SearchOptions;
pub use semantic::SearchResource;
pub use semantic::UnsupportedSearchFeature;
pub use timestamp_query::TimestampQueryError;

/// Resource bounds applied during KQL lexing and parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KqlLimits {
    input_bytes: usize,
    tokens: usize,
    nodes: usize,
    depth: usize,
    path_components: usize,
    list_values: usize,
    string_bytes: usize,
}

impl KqlLimits {
    /// Creates explicit byte, token, node, depth, path, list, and owned-string limits.
    #[must_use]
    pub const fn new(
        max_input_bytes: usize,
        max_tokens: usize,
        max_nodes: usize,
        max_depth: usize,
        max_path_components: usize,
        max_list_values: usize,
        max_owned_string_bytes: usize,
    ) -> Self {
        Self {
            input_bytes: max_input_bytes,
            tokens: max_tokens,
            nodes: max_nodes,
            depth: max_depth,
            path_components: max_path_components,
            list_values: max_list_values,
            string_bytes: max_owned_string_bytes,
        }
    }

    /// Maximum UTF-8 input bytes.
    #[must_use]
    pub const fn max_input_bytes(self) -> usize {
        self.input_bytes
    }

    /// Maximum lexer tokens, excluding the implicit end of input.
    #[must_use]
    pub const fn max_tokens(self) -> usize {
        self.tokens
    }

    /// Maximum expression-arena nodes.
    #[must_use]
    pub const fn max_nodes(self) -> usize {
        self.nodes
    }

    /// Maximum syntactic grouping or resulting expression depth.
    #[must_use]
    pub const fn max_depth(self) -> usize {
        self.depth
    }

    /// Maximum components in any source or fully prefixed column path.
    #[must_use]
    pub const fn max_path_components(self) -> usize {
        self.path_components
    }

    /// Maximum values in one compact `key:(...)` list.
    #[must_use]
    pub const fn max_list_values(self) -> usize {
        self.list_values
    }

    /// Maximum aggregate owned string bytes processed while building the AST.
    #[must_use]
    pub const fn max_owned_string_bytes(self) -> usize {
        self.string_bytes
    }
}

impl Default for KqlLimits {
    fn default() -> Self {
        Self::new(
            1024 * 1024,
            262_144,
            65_536,
            256,
            256,
            65_536,
            4 * 1024 * 1024,
        )
    }
}

/// Allocation named by a bounded parser error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KqlResource {
    /// Lexer token arena.
    Tokens,
    /// Parsed expression arena.
    Nodes,
    /// Iterative parser operator stack.
    Operators,
    /// Iterative parser value stack.
    Values,
    /// Column path components and active nested prefixes.
    PathComponents,
    /// Values in one compact list expression.
    ListValues,
    /// Owned decoded or copied string storage.
    Strings,
}

impl Display for KqlResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tokens => "KQL tokens",
            Self::Nodes => "KQL expression nodes",
            Self::Operators => "KQL operator stack",
            Self::Values => "KQL value stack",
            Self::PathComponents => "KQL path components",
            Self::ListValues => "KQL list values",
            Self::Strings => "KQL owned strings",
        })
    }
}

/// Broad syntax item expected at an error offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KqlExpected {
    /// A predicate, bare value, prefix `NOT`, or grouped query.
    Expression,
    /// `AND`, `OR`, a matching closing delimiter, or end of input.
    BooleanOperator,
    /// A quoted or unquoted KQL literal.
    Literal,
    /// A value literal or exact lowercase `timestamp(...)` call.
    Value,
    /// A quoted timestamp argument.
    QuotedTimestamp,
    /// A comma or closing timestamp parenthesis.
    TimestampSeparator,
    /// A matching `)`.
    ClosingParenthesis,
    /// A matching `}`.
    ClosingBrace,
    /// End of input.
    EndOfInput,
}

impl Display for KqlExpected {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Expression => "an expression",
            Self::BooleanOperator => "AND, OR, a closing delimiter, or end of input",
            Self::Literal => "a quoted or unquoted literal",
            Self::Value => "a literal or timestamp(...) value",
            Self::QuotedTimestamp => "a quoted timestamp argument",
            Self::TimestampSeparator => "a comma or closing timestamp parenthesis",
            Self::ClosingParenthesis => "')'",
            Self::ClosingBrace => "'}'",
            Self::EndOfInput => "end of input",
        })
    }
}

/// Structured KQL lexing or parsing failure with an original-input byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KqlError {
    /// Input exceeds the configured byte bound.
    InputTooLong {
        /// Actual UTF-8 byte length.
        actual: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Token count would exceed its configured bound.
    TokenLimitExceeded {
        /// Start of the rejected token.
        offset: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Expression count would exceed its configured bound.
    NodeLimitExceeded {
        /// Expression start.
        offset: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Grouping or expression depth would exceed its configured bound.
    DepthLimitExceeded {
        /// Construct causing the excess depth.
        offset: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A source or prefixed field path has too many components.
    PathLimitExceeded {
        /// Component causing the excess.
        offset: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// One compact list has too many values.
    ListLimitExceeded {
        /// Value causing the excess.
        offset: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Aggregate owned string bytes would exceed their bound.
    StringLimitExceeded {
        /// String causing the excess.
        offset: usize,
        /// Required aggregate bytes.
        required: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A checked parser allocation failed.
    AllocationFailed {
        /// Input position associated with the allocation.
        offset: usize,
        /// State being grown.
        resource: KqlResource,
        /// Requested additional elements or bytes.
        requested: usize,
    },
    /// Checked size arithmetic overflowed.
    SizeOverflow {
        /// Input position associated with the arithmetic.
        offset: usize,
    },
    /// A quoted token has no grammar-compatible closing quote.
    UnterminatedQuotedString {
        /// Opening quote.
        offset: usize,
    },
    /// A backslash escape is not accepted by the pinned KQL grammar/value decoder.
    InvalidEscape {
        /// Backslash byte.
        offset: usize,
    },
    /// A `\\uXXXX` escape is malformed or names a surrogate.
    InvalidUnicodeEscape {
        /// Backslash byte.
        offset: usize,
    },
    /// A character cannot begin any lexer token.
    UnexpectedCharacter {
        /// Character byte offset.
        offset: usize,
        /// Rejected Unicode scalar.
        character: char,
    },
    /// A syntactically valid token appears in the wrong position.
    UnexpectedToken {
        /// Rejected token or end-of-input position.
        offset: usize,
        /// Required grammar category.
        expected: KqlExpected,
        /// Rejected token; `None` means end of input.
        found: Option<KqlTokenKind>,
    },
    /// A dot path begins, ends, or contains two dots with no component between them.
    EmptyPathComponent {
        /// Empty component position.
        offset: usize,
    },
    /// C++ nested-prefix semantics reject an explicitly namespaced inner path.
    NestedNamespace {
        /// Inner namespace prefix.
        offset: usize,
    },
    /// An internal stack relationship was inconsistent; no panic was used.
    MalformedExpression {
        /// Nearest input position.
        offset: usize,
    },
}

impl KqlError {
    /// Returns the original-input byte offset associated with this failure.
    #[must_use]
    pub const fn offset(&self) -> usize {
        match self {
            Self::InputTooLong { limit, .. } => *limit,
            Self::TokenLimitExceeded { offset, .. }
            | Self::NodeLimitExceeded { offset, .. }
            | Self::DepthLimitExceeded { offset, .. }
            | Self::PathLimitExceeded { offset, .. }
            | Self::ListLimitExceeded { offset, .. }
            | Self::StringLimitExceeded { offset, .. }
            | Self::AllocationFailed { offset, .. }
            | Self::SizeOverflow { offset }
            | Self::UnterminatedQuotedString { offset }
            | Self::InvalidEscape { offset }
            | Self::InvalidUnicodeEscape { offset }
            | Self::UnexpectedCharacter { offset, .. }
            | Self::UnexpectedToken { offset, .. }
            | Self::EmptyPathComponent { offset }
            | Self::NestedNamespace { offset }
            | Self::MalformedExpression { offset } => *offset,
        }
    }
}

impl Display for KqlError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLong { actual, limit } => {
                write!(formatter, "KQL input has {actual} bytes, limit is {limit}")
            }
            Self::TokenLimitExceeded { offset, limit } => {
                write!(
                    formatter,
                    "KQL token limit {limit} exceeded at byte {offset}"
                )
            }
            Self::NodeLimitExceeded { offset, limit } => {
                write!(
                    formatter,
                    "KQL node limit {limit} exceeded at byte {offset}"
                )
            }
            Self::DepthLimitExceeded { offset, limit } => {
                write!(
                    formatter,
                    "KQL depth limit {limit} exceeded at byte {offset}"
                )
            }
            Self::PathLimitExceeded { offset, limit } => {
                write!(
                    formatter,
                    "KQL path-component limit {limit} exceeded at byte {offset}"
                )
            }
            Self::ListLimitExceeded { offset, limit } => {
                write!(
                    formatter,
                    "KQL list-value limit {limit} exceeded at byte {offset}"
                )
            }
            Self::StringLimitExceeded {
                offset,
                required,
                limit,
            } => write!(
                formatter,
                "KQL owned strings require {required} bytes at byte {offset}, limit is {limit}"
            ),
            Self::AllocationFailed {
                offset,
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} additional {resource} at byte {offset}"
            ),
            Self::SizeOverflow { offset } => {
                write!(formatter, "KQL size arithmetic overflow at byte {offset}")
            }
            Self::UnterminatedQuotedString { offset } => {
                write!(formatter, "unterminated KQL quoted string at byte {offset}")
            }
            Self::InvalidEscape { offset } => {
                write!(formatter, "invalid KQL escape at byte {offset}")
            }
            Self::InvalidUnicodeEscape { offset } => {
                write!(formatter, "invalid KQL Unicode escape at byte {offset}")
            }
            Self::UnexpectedCharacter { offset, character } => {
                write!(
                    formatter,
                    "unexpected KQL character {character:?} at byte {offset}"
                )
            }
            Self::UnexpectedToken {
                offset,
                expected,
                found,
            } => match found {
                Some(found) => write!(
                    formatter,
                    "expected {expected} at byte {offset}, found {found}"
                ),
                None => write!(
                    formatter,
                    "expected {expected} at byte {offset}, found end of input"
                ),
            },
            Self::EmptyPathComponent { offset } => {
                write!(formatter, "empty KQL path component at byte {offset}")
            }
            Self::NestedNamespace { offset } => write!(
                formatter,
                "explicit namespace on a nested inner path at byte {offset}"
            ),
            Self::MalformedExpression { offset } => {
                write!(formatter, "malformed KQL expression near byte {offset}")
            }
        }
    }
}

impl Error for KqlError {}
