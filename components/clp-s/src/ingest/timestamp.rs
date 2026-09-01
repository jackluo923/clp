//! Authoritative timestamp-path configuration and exact JSON scalar recognition.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::str;

use super::ClassifiedJsonNumber;
use super::JsonNumberClassificationError;
use super::JsonString;
use super::KvIrNamespace;
use super::classify_json_number;
use super::number::ValidatedJsonNumberSyntax;
use super::number::classify_validated_json_number_text;
use crate::writer::TimestampRef;

const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const NANOSECONDS_PER_MICROSECOND: i64 = 1_000;
const EPOCH_MILLISECONDS_1971: u64 = 31_536_000_000;
const EPOCH_MICROSECONDS_1971: u64 = 31_536_000_000_000;
const EPOCH_NANOSECONDS_1971: u64 = 31_536_000_000_000_000;

/// Limits applied while compiling one authoritative JSON field path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonTimestampPathLimits {
    descriptor_bytes: u64,
    components: u64,
    component_bytes: u64,
}

impl JsonTimestampPathLimits {
    /// Conservative defaults matching the JSON reader's default nesting domain.
    pub const DEFAULT: Self = Self::new(64 * 1024, 256, 16 * 1024);

    /// Creates a complete set of path-compilation limits.
    #[must_use]
    pub const fn new(
        max_descriptor_bytes: u64,
        max_components: u64,
        max_component_bytes: u64,
    ) -> Self {
        Self {
            descriptor_bytes: max_descriptor_bytes,
            components: max_components,
            component_bytes: max_component_bytes,
        }
    }

    /// Maximum bytes in the original dotted descriptor.
    #[must_use]
    pub const fn max_descriptor_bytes(self) -> u64 {
        self.descriptor_bytes
    }

    /// Maximum decoded path components.
    #[must_use]
    pub const fn max_components(self) -> u64 {
        self.components
    }

    /// Maximum UTF-8 bytes in one decoded component.
    #[must_use]
    pub const fn max_component_bytes(self) -> u64 {
        self.component_bytes
    }
}

impl Default for JsonTimestampPathLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Path resource rejected by [`JsonTimestampPathLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonTimestampPathResource {
    /// Original descriptor bytes.
    DescriptorBytes,
    /// Decoded dotted components.
    Components,
    /// Bytes in one decoded component.
    ComponentBytes,
}

impl Display for JsonTimestampPathResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DescriptorBytes => "timestamp descriptor bytes",
            Self::Components => "timestamp path components",
            Self::ComponentBytes => "timestamp component bytes",
        })
    }
}

/// Failure to compile a C++-compatible dotted authoritative timestamp path.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonTimestampPathError {
    /// The descriptor or one dotted component is empty.
    EmptyComponent {
        /// Byte offset at which an empty component begins.
        byte_offset: usize,
    },
    /// A reverse solidus was not followed by a supported KQL key escape.
    InvalidEscape {
        /// Byte offset of the reverse solidus.
        byte_offset: usize,
    },
    /// A `\u` escape was malformed or contained an unpaired surrogate.
    InvalidUnicodeEscape {
        /// Byte offset of the reverse solidus introducing the escape.
        byte_offset: usize,
    },
    /// An unescaped `*` component would be a wildcard, which C++ rejects for timestamp keys.
    Wildcard {
        /// Zero-based component index.
        component_index: usize,
    },
    /// A non-default CLP-S namespace cannot identify a field in the direct JSON adapter.
    UnsupportedNamespace {
        /// Namespace prefix byte (`@`, `$`, `!`, or `#`).
        namespace: u8,
    },
    /// A configured bound was exceeded.
    LimitExceeded {
        /// Limited resource.
        resource: JsonTimestampPathResource,
        /// Observed amount.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
}

impl Display for JsonTimestampPathError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyComponent { byte_offset } => {
                write!(
                    formatter,
                    "empty timestamp path component at byte {byte_offset}"
                )
            }
            Self::InvalidEscape { byte_offset } => {
                write!(
                    formatter,
                    "invalid timestamp path escape at byte {byte_offset}"
                )
            }
            Self::InvalidUnicodeEscape { byte_offset } => write!(
                formatter,
                "invalid timestamp path Unicode escape at byte {byte_offset}"
            ),
            Self::Wildcard { component_index } => write!(
                formatter,
                "timestamp path component {component_index} is an unescaped wildcard"
            ),
            Self::UnsupportedNamespace { namespace } => write!(
                formatter,
                "timestamp namespace '{}' is not available in direct JSON input",
                char::from(*namespace)
            ),
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(formatter, "{actual} {resource} exceeds limit {limit}"),
        }
    }
}

impl Error for JsonTimestampPathError {}

/// A validated, decoded dotted path to one authoritative JSON scalar.
///
/// Parsing follows the current C++ column-descriptor convention: unescaped dots delimit object
/// levels and `\.` denotes a literal dot. Other supported KQL key escapes are decoded once.
/// Wildcards and non-default namespaces are rejected because they cannot denote one direct JSON
/// field. The original descriptor is retained as the archive timestamp range key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonTimestampPath {
    descriptor: String,
    components: Box<[String]>,
}

impl JsonTimestampPath {
    /// Compiles a descriptor with conservative defaults.
    ///
    /// # Errors
    ///
    /// Returns a located grammar, namespace, wildcard, or resource-limit error.
    pub fn parse(descriptor: &str) -> Result<Self, JsonTimestampPathError> {
        Self::parse_with_limits(descriptor, JsonTimestampPathLimits::DEFAULT)
    }

    /// Compiles a descriptor with caller-supplied resource limits.
    ///
    /// # Errors
    ///
    /// Returns a located grammar, namespace, wildcard, or resource-limit error.
    pub fn parse_with_limits(
        descriptor: &str,
        limits: JsonTimestampPathLimits,
    ) -> Result<Self, JsonTimestampPathError> {
        check_path_limit(
            JsonTimestampPathResource::DescriptorBytes,
            descriptor.len(),
            limits.max_descriptor_bytes(),
        )?;
        if let Some(namespace @ (b'@' | b'$' | b'!' | b'#')) = descriptor.as_bytes().first() {
            return Err(JsonTimestampPathError::UnsupportedNamespace {
                namespace: *namespace,
            });
        }

        Self::parse_components(descriptor, 0, limits)
    }

    fn parse_components(
        descriptor: &str,
        start: usize,
        limits: JsonTimestampPathLimits,
    ) -> Result<Self, JsonTimestampPathError> {
        debug_assert!(start <= descriptor.len());

        let mut components = Vec::new();
        let mut component_start = start;
        let mut cursor = start;
        while cursor <= descriptor.len() {
            if cursor == descriptor.len() || descriptor.as_bytes()[cursor] == b'.' {
                push_component(descriptor, component_start, cursor, limits, &mut components)?;
                if cursor == descriptor.len() {
                    break;
                }
                cursor += 1;
                component_start = cursor;
                continue;
            }
            if descriptor.as_bytes()[cursor] == b'\\' {
                cursor = skip_path_escape(descriptor, cursor)?;
            } else {
                let character = descriptor[cursor..].chars().next().ok_or(
                    JsonTimestampPathError::InvalidEscape {
                        byte_offset: cursor,
                    },
                )?;
                cursor += character.len_utf8();
            }
        }
        Ok(Self {
            descriptor: descriptor.to_owned(),
            components: components.into_boxed_slice(),
        })
    }

    /// Returns the original descriptor stored as the archive timestamp range key.
    #[must_use]
    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    /// Returns decoded JSON object-key components from root to leaf.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    pub(super) fn matches(&self, component_index: usize, key: &[u8]) -> bool {
        self.components
            .get(component_index)
            .is_some_and(|component| component.as_bytes() == key)
    }
}

/// Kind of exact JSON scalar rejected during authoritative timestamp recognition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonTimestampScalarKind {
    /// A JSON number token.
    Number,
    /// A quoted JSON string token.
    String,
}

impl Display for JsonTimestampScalarKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Number => "number",
            Self::String => "string",
        })
    }
}

/// Failure to recognize one matching authoritative timestamp scalar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonTimestampError {
    /// Number grammar or the C++ JSON numeric domain was invalid.
    Number {
        /// Exact classifier failure.
        source: JsonNumberClassificationError,
    },
    /// A parser-supplied exact token was unexpectedly not UTF-8.
    InvalidUtf8,
    /// The token is valid JSON but outside the currently supported authoritative timestamp
    /// patterns.
    UnsupportedLexeme {
        /// Scalar kind that failed recognition.
        kind: JsonTimestampScalarKind,
    },
    /// Epoch scaling or calendar conversion exceeded signed epoch nanoseconds.
    EpochNanosecondsOutOfRange,
}

impl Display for JsonTimestampError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number { source } => write!(formatter, "invalid timestamp number: {source}"),
            Self::InvalidUtf8 => formatter.write_str("timestamp token is not UTF-8"),
            Self::UnsupportedLexeme { kind } => {
                write!(
                    formatter,
                    "unsupported authoritative timestamp {kind} lexeme"
                )
            }
            Self::EpochNanosecondsOutOfRange => {
                formatter.write_str("timestamp does not fit signed epoch nanoseconds")
            }
        }
    }
}

impl Error for JsonTimestampError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Number { source } => Some(source),
            _ => None,
        }
    }
}

/// Precompiled authoritative timestamp configuration for the JSON archive adapter.
///
/// Recognition covers the C++ numeric defaults (integer epoch values with inferred precision and
/// fractional epoch seconds) plus quoted canonical `YYYY-MM-DD[T ]HH:MM:SS` date-times, optional
/// `.`/`,` fractions, and optional literal `Z`. Fractions resolve to `\3`, `\6`, `\9`, or `\T`
/// exactly as C++. Other valid JSON strings/numbers at the configured path fail explicitly; they
/// are never silently stored with a different type. Values of all other JSON kinds retain their
/// ordinary behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonTimestampResolver {
    path: JsonTimestampPath,
}

impl JsonTimestampResolver {
    /// Creates a resolver for an already-compiled path.
    #[must_use]
    pub const fn new(path: JsonTimestampPath) -> Self {
        Self { path }
    }

    /// Compiles a resolver from one dotted JSON field descriptor.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`JsonTimestampPath::parse`].
    pub fn parse(descriptor: &str) -> Result<Self, JsonTimestampPathError> {
        JsonTimestampPath::parse(descriptor).map(Self::new)
    }

    /// Compiles a resolver with caller-supplied path resource limits.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`JsonTimestampPath::parse_with_limits`].
    pub fn parse_with_limits(
        descriptor: &str,
        limits: JsonTimestampPathLimits,
    ) -> Result<Self, JsonTimestampPathError> {
        JsonTimestampPath::parse_with_limits(descriptor, limits).map(Self::new)
    }

    /// Returns the configured path.
    #[must_use]
    pub const fn path(&self) -> &JsonTimestampPath {
        &self.path
    }

    /// Resolves an exact numeric JSON token.
    ///
    /// # Errors
    ///
    /// Returns a number-domain, unsupported-pattern, or epoch-range error.
    pub fn resolve_number<'a>(
        &'a self,
        source: &'a [u8],
    ) -> Result<TimestampRef<'a>, JsonTimestampError> {
        let source_str = str::from_utf8(source).map_err(|_| JsonTimestampError::InvalidUtf8)?;
        let classified =
            classify_json_number(source).map_err(|source| JsonTimestampError::Number { source })?;
        self.resolve_classified_number(source_str, classified)
            .map(|(timestamp, _)| timestamp)
    }

    pub(super) fn resolve_validated_number<'a>(
        &'a self,
        source: &'a [u8],
        syntax: ValidatedJsonNumberSyntax,
    ) -> Result<(TimestampRef<'a>, bool), JsonTimestampError> {
        let source_str = str::from_utf8(source).map_err(|_| JsonTimestampError::InvalidUtf8)?;
        let classified = classify_validated_json_number_text(source_str, syntax)
            .map_err(|source| JsonTimestampError::Number { source })?;
        self.resolve_classified_number(source_str, classified)
    }

    fn resolve_classified_number<'a>(
        &'a self,
        source: &'a str,
        classified: ClassifiedJsonNumber<'a>,
    ) -> Result<(TimestampRef<'a>, bool), JsonTimestampError> {
        let (epoch, pattern) = match classified {
            ClassifiedJsonNumber::Integer(value) => resolve_integer(value, false)?,
            ClassifiedJsonNumber::Float { .. } => resolve_fractional_epoch(source, false)?,
        };
        Ok((
            TimestampRef::new(epoch, source, pattern, self.path.descriptor()),
            is_exact_canonical_integer(source.as_bytes(), classified),
        ))
    }

    /// Resolves an exact quoted JSON string token.
    ///
    /// # Errors
    ///
    /// Returns an unsupported-pattern or epoch-range error. The parser-provided decoded value is
    /// intentionally not substituted for the exact raw token because timestamp patterns must
    /// reproduce quotes and JSON escapes byte-for-byte.
    pub fn resolve_string<'a>(
        &'a self,
        value: JsonString<'a>,
    ) -> Result<TimestampRef<'a>, JsonTimestampError> {
        let lexeme =
            str::from_utf8(value.raw_json()).map_err(|_| JsonTimestampError::InvalidUtf8)?;
        let inner = lexeme
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            })?;
        let resolved = match inner.parse::<i64>() {
            Ok(integer) => resolve_integer(integer, true),
            Err(_) if inner.as_bytes().contains(&b'.') && looks_numeric(inner) => {
                resolve_fractional_epoch(inner, true)
            }
            Err(_) => resolve_iso_date_time(inner),
        };
        let (epoch, pattern) = resolved.map_err(|error| match error {
            JsonTimestampError::UnsupportedLexeme { .. } => JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            },
            other => other,
        })?;
        Ok(TimestampRef::new(
            epoch,
            lexeme,
            pattern,
            self.path.descriptor(),
        ))
    }
}

fn is_exact_canonical_integer(source: &[u8], classified: ClassifiedJsonNumber<'_>) -> bool {
    match classified {
        ClassifiedJsonNumber::Integer(value) => {
            source != b"-0" && (source.first() == Some(&b'-') || value >= 0)
        }
        ClassifiedJsonNumber::Float { .. } => false,
    }
}

/// Namespace-aware authoritative timestamp configuration for validated KV-IR streams.
///
/// An unprefixed descriptor selects the user-generated schema tree and a leading `@` selects the
/// auto-generated tree. The C++ descriptor grammar also accepts range-index and reserved namespace
/// prefixes (`$`, `!`, and `#`); KV-IR has no corresponding schema tree, so those
/// descriptors compile successfully but never match a value. The original descriptor is retained
/// as the archive timestamp range key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KvIrTimestampResolver {
    path: JsonTimestampPath,
    namespace: Option<KvIrNamespace>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct KvIrResolvedTimestamp {
    pub(super) epoch_nanoseconds: i64,
    pub(super) pattern: &'static str,
}

impl KvIrTimestampResolver {
    /// Compiles a namespace-aware KV-IR timestamp descriptor with conservative limits.
    ///
    /// # Errors
    ///
    /// Returns a located grammar, wildcard, or resource-limit error.
    pub fn parse(descriptor: &str) -> Result<Self, JsonTimestampPathError> {
        Self::parse_with_limits(descriptor, JsonTimestampPathLimits::DEFAULT)
    }

    /// Compiles a namespace-aware descriptor with caller-supplied path limits.
    ///
    /// # Errors
    ///
    /// Returns a located grammar, wildcard, or resource-limit error.
    pub fn parse_with_limits(
        descriptor: &str,
        limits: JsonTimestampPathLimits,
    ) -> Result<Self, JsonTimestampPathError> {
        check_path_limit(
            JsonTimestampPathResource::DescriptorBytes,
            descriptor.len(),
            limits.max_descriptor_bytes(),
        )?;
        let (namespace, start) = match descriptor.as_bytes().first() {
            Some(b'@') => (Some(KvIrNamespace::AutoGenerated), 1),
            Some(b'$' | b'!' | b'#') => (None, 1),
            _ => (Some(KvIrNamespace::UserGenerated), 0),
        };
        Ok(Self {
            path: JsonTimestampPath::parse_components(descriptor, start, limits)?,
            namespace,
        })
    }

    /// Returns the compiled path and its original descriptor.
    #[must_use]
    pub const fn path(&self) -> &JsonTimestampPath {
        &self.path
    }

    /// Returns the selected KV-IR schema tree.
    ///
    /// `None` denotes a syntactically valid `$`, `!`, or `#` namespace that KV-IR cannot supply.
    #[must_use]
    pub const fn namespace(&self) -> Option<KvIrNamespace> {
        self.namespace
    }

    pub(super) fn resolve_integer(value: i64) -> Result<KvIrResolvedTimestamp, JsonTimestampError> {
        let (epoch, pattern) = resolve_integer(value, false)?;
        Ok(KvIrResolvedTimestamp {
            epoch_nanoseconds: epoch,
            pattern,
        })
    }

    pub(super) fn resolve_fixed_nine_float(
        lexeme: &str,
    ) -> Result<KvIrResolvedTimestamp, JsonTimestampError> {
        let (epoch, pattern) = resolve_fixed_nine_fractional_epoch(lexeme)?;
        Ok(KvIrResolvedTimestamp {
            epoch_nanoseconds: epoch,
            pattern,
        })
    }

    pub(super) fn resolve_string(
        value: &[u8],
    ) -> Result<KvIrResolvedTimestamp, JsonTimestampError> {
        let value = str::from_utf8(value).map_err(|_| JsonTimestampError::InvalidUtf8)?;
        let resolved = match value.parse::<i64>() {
            Ok(integer) => resolve_integer(integer, true),
            Err(_) if value.as_bytes().contains(&b'.') && looks_numeric(value) => {
                resolve_fractional_epoch(value, true)
            }
            Err(_) => resolve_iso_date_time(value),
        };
        let (epoch, pattern) = resolved.map_err(|error| match error {
            JsonTimestampError::UnsupportedLexeme { .. } => JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            },
            other => other,
        })?;
        Ok(KvIrResolvedTimestamp {
            epoch_nanoseconds: epoch,
            pattern,
        })
    }
}

fn check_path_limit(
    resource: JsonTimestampPathResource,
    actual: usize,
    limit: u64,
) -> Result<(), JsonTimestampPathError> {
    let actual = u64::try_from(actual).unwrap_or(u64::MAX);
    if actual > limit {
        Err(JsonTimestampPathError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn skip_path_escape(descriptor: &str, start: usize) -> Result<usize, JsonTimestampPathError> {
    let escaped = start
        .checked_add(1)
        .and_then(|index| descriptor.as_bytes().get(index).copied())
        .ok_or(JsonTimestampPathError::InvalidEscape { byte_offset: start })?;
    if b'u' == escaped {
        let first_end = start
            .checked_add(6)
            .ok_or(JsonTimestampPathError::InvalidUnicodeEscape { byte_offset: start })?;
        let first = parse_hex_quad(descriptor, start + 2, start)?;
        if (0xd800..=0xdbff).contains(&first) {
            if descriptor.as_bytes().get(first_end..first_end + 2) != Some(br"\u") {
                return Err(JsonTimestampPathError::InvalidUnicodeEscape { byte_offset: start });
            }
            let second = parse_hex_quad(descriptor, first_end + 2, start)?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(JsonTimestampPathError::InvalidUnicodeEscape { byte_offset: start });
            }
            return first_end
                .checked_add(6)
                .ok_or(JsonTimestampPathError::InvalidUnicodeEscape { byte_offset: start });
        }
        if (0xdc00..=0xdfff).contains(&first) {
            return Err(JsonTimestampPathError::InvalidUnicodeEscape { byte_offset: start });
        }
        return Ok(first_end);
    }
    if matches!(
        escaped,
        b'.' | b'\\'
            | b'"'
            | b't'
            | b'r'
            | b'n'
            | b'b'
            | b'f'
            | b'{'
            | b'}'
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'*'
            | b'?'
            | b'@'
            | b'$'
            | b'!'
            | b'#'
    ) {
        Ok(start + 2)
    } else {
        Err(JsonTimestampPathError::InvalidEscape { byte_offset: start })
    }
}

fn push_component(
    descriptor: &str,
    start: usize,
    end: usize,
    limits: JsonTimestampPathLimits,
    components: &mut Vec<String>,
) -> Result<(), JsonTimestampPathError> {
    if start == end {
        return Err(JsonTimestampPathError::EmptyComponent { byte_offset: start });
    }
    if descriptor.as_bytes().get(start..end) == Some(b"*") {
        return Err(JsonTimestampPathError::Wildcard {
            component_index: components.len(),
        });
    }
    check_path_limit(
        JsonTimestampPathResource::Components,
        components.len().saturating_add(1),
        limits.max_components(),
    )?;
    let component = decode_component(descriptor, start, end)?;
    check_path_limit(
        JsonTimestampPathResource::ComponentBytes,
        component.len(),
        limits.max_component_bytes(),
    )?;
    components.push(component);
    Ok(())
}

fn decode_component(
    descriptor: &str,
    start: usize,
    end: usize,
) -> Result<String, JsonTimestampPathError> {
    let mut output = String::with_capacity(end - start);
    let mut cursor = start;
    while cursor < end {
        if descriptor.as_bytes()[cursor] != b'\\' {
            let character = descriptor[cursor..end].chars().next().ok_or(
                JsonTimestampPathError::InvalidEscape {
                    byte_offset: cursor,
                },
            )?;
            output.push(character);
            cursor += character.len_utf8();
            continue;
        }
        let escaped = descriptor.as_bytes()[cursor + 1];
        if b'u' == escaped {
            let first = parse_hex_quad(descriptor, cursor + 2, cursor)?;
            cursor += 6;
            let scalar = if (0xd800..=0xdbff).contains(&first) {
                let second = parse_hex_quad(descriptor, cursor + 2, cursor - 6)?;
                cursor += 6;
                0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
            } else {
                u32::from(first)
            };
            output.push(char::from_u32(scalar).ok_or_else(|| {
                JsonTimestampPathError::InvalidUnicodeEscape {
                    byte_offset: cursor.saturating_sub(6),
                }
            })?);
            continue;
        }
        output.push(match escaped {
            b't' => '\t',
            b'r' => '\r',
            b'n' => '\n',
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            other => char::from(other),
        });
        cursor += 2;
    }
    Ok(output)
}

fn parse_hex_quad(
    descriptor: &str,
    start: usize,
    escape_start: usize,
) -> Result<u16, JsonTimestampPathError> {
    let bytes = descriptor.as_bytes().get(start..start + 4).ok_or(
        JsonTimestampPathError::InvalidUnicodeEscape {
            byte_offset: escape_start,
        },
    )?;
    let mut value = 0_u16;
    for byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => u16::from(*byte - b'0'),
            b'a'..=b'f' => u16::from(*byte - b'a' + 10),
            b'A'..=b'F' => u16::from(*byte - b'A' + 10),
            _ => {
                return Err(JsonTimestampPathError::InvalidUnicodeEscape {
                    byte_offset: escape_start,
                });
            }
        };
        value = value * 16 + digit;
    }
    Ok(value)
}

fn resolve_integer(value: i64, quoted: bool) -> Result<(i64, &'static str), JsonTimestampError> {
    // C++ negates negative inputs while estimating precision and multiplies without checked
    // arithmetic. `i64::MIN` and overflowing scaled epochs therefore have no defined portable
    // result; reject them instead of assigning archive bytes to undefined behavior.
    if i64::MIN == value {
        return Err(JsonTimestampError::EpochNanosecondsOutOfRange);
    }
    let magnitude = value.unsigned_abs();
    let (factor, pattern, quoted_pattern) = if magnitude > EPOCH_NANOSECONDS_1971 {
        (1_i64, r"\N", r#""\N""#)
    } else if magnitude > EPOCH_MICROSECONDS_1971 {
        (NANOSECONDS_PER_MICROSECOND, r"\C", r#""\C""#)
    } else if magnitude > EPOCH_MILLISECONDS_1971 {
        (NANOSECONDS_PER_MILLISECOND, r"\L", r#""\L""#)
    } else {
        (NANOSECONDS_PER_SECOND, r"\E", r#""\E""#)
    };
    let epoch = value
        .checked_mul(factor)
        .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    Ok((epoch, if quoted { quoted_pattern } else { pattern }))
}

fn looks_numeric(value: &str) -> bool {
    value
        .as_bytes()
        .iter()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'.'))
}

fn resolve_fractional_epoch(
    source: &str,
    quoted: bool,
) -> Result<(i64, &'static str), JsonTimestampError> {
    resolve_fractional_epoch_with_precision(source, quoted, None)
}

fn resolve_fixed_nine_fractional_epoch(
    source: &str,
) -> Result<(i64, &'static str), JsonTimestampError> {
    resolve_fractional_epoch_with_precision(source, false, Some(FractionPrecision::Nanoseconds))
}

fn resolve_fractional_epoch_with_precision(
    source: &str,
    quoted: bool,
    fixed_precision: Option<FractionPrecision>,
) -> Result<(i64, &'static str), JsonTimestampError> {
    let (seconds_source, fraction) =
        source
            .split_once('.')
            .ok_or(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::Number,
            })?;
    if fraction.is_empty()
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || seconds_source.is_empty()
        || (fixed_precision.is_none() && seconds_source == "-0")
        || (fixed_precision.is_some() && fraction.len() != 9)
    {
        return Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::Number,
        });
    }
    let seconds = seconds_source
        .parse::<i64>()
        .map_err(|_| JsonTimestampError::EpochNanosecondsOutOfRange)?;
    let fraction_value =
        fraction
            .parse::<i64>()
            .map_err(|_| JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::Number,
            })?;
    let scale = 10_i64.pow(
        u32::try_from(9_usize.saturating_sub(fraction.len()))
            .map_err(|_| JsonTimestampError::EpochNanosecondsOutOfRange)?,
    );
    let fractional_nanoseconds = fraction_value
        .checked_mul(scale)
        .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    let whole = seconds
        .checked_mul(NANOSECONDS_PER_SECOND)
        .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    let epoch = if seconds_source.starts_with('-') {
        whole.checked_sub(fractional_nanoseconds)
    } else {
        whole.checked_add(fractional_nanoseconds)
    }
    .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    let precision = fixed_precision.map_or_else(|| fraction_precision(fraction), Ok)?;
    Ok((epoch, numeric_fraction_pattern(precision, quoted)))
}

#[derive(Clone, Copy)]
enum FractionPrecision {
    Milliseconds,
    Microseconds,
    Nanoseconds,
    Variable,
}

fn fraction_precision(fraction: &str) -> Result<FractionPrecision, JsonTimestampError> {
    match fraction.len() {
        3 => Ok(FractionPrecision::Milliseconds),
        6 => Ok(FractionPrecision::Microseconds),
        9 => Ok(FractionPrecision::Nanoseconds),
        _ if fraction.ends_with('0') => Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::Number,
        }),
        _ => Ok(FractionPrecision::Variable),
    }
}

const fn numeric_fraction_pattern(precision: FractionPrecision, quoted: bool) -> &'static str {
    match (precision, quoted) {
        (FractionPrecision::Milliseconds, false) => r"\E.\3",
        (FractionPrecision::Microseconds, false) => r"\E.\6",
        (FractionPrecision::Nanoseconds, false) => r"\E.\9",
        (FractionPrecision::Variable, false) => r"\E.\T",
        (FractionPrecision::Milliseconds, true) => r#""\E.\3""#,
        (FractionPrecision::Microseconds, true) => r#""\E.\6""#,
        (FractionPrecision::Nanoseconds, true) => r#""\E.\9""#,
        (FractionPrecision::Variable, true) => r#""\E.\T""#,
    }
}

fn resolve_iso_date_time(source: &str) -> Result<(i64, &'static str), JsonTimestampError> {
    let bytes = source.as_bytes();
    if bytes.len() < 19
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::String,
        });
    }
    let year = parse_date_digits(bytes, 0, 4)?;
    let month = parse_date_digits(bytes, 5, 2)?;
    let day = parse_date_digits(bytes, 8, 2)?;
    let hour = parse_date_digits(bytes, 11, 2)?;
    let minute = parse_date_digits(bytes, 14, 2)?;
    let source_second = parse_date_digits(bytes, 17, 2)?;
    if hour > 23 || minute > 59 || source_second > 59 {
        return Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::String,
        });
    }

    let mut cursor = 19_usize;
    let fraction = if let Some(separator @ (b'.' | b',')) = bytes.get(cursor).copied() {
        let start = cursor + 1;
        cursor = start;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        let digits = source
            .get(start..cursor)
            .ok_or(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            })?;
        if digits.is_empty() || digits.len() > 9 {
            return Err(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            });
        }
        Some((separator, digits, fraction_precision(digits)?))
    } else {
        None
    };
    let zulu = if bytes.get(cursor) == Some(&b'Z') {
        cursor += 1;
        true
    } else {
        false
    };
    if cursor != bytes.len() {
        return Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::String,
        });
    }

    let days = days_from_civil(i64::from(year), month, day)?;
    let seconds = i128::from(days)
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(i128::from(hour) * 3_600))
        .and_then(|value| value.checked_add(i128::from(minute) * 60))
        .and_then(|value| value.checked_add(i128::from(source_second.min(59))))
        .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    let mut epoch = seconds
        .checked_mul(i128::from(NANOSECONDS_PER_SECOND))
        .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    if let Some((_, digits, _)) = fraction {
        let value = digits
            .parse::<i128>()
            .map_err(|_| JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            })?;
        epoch = epoch
            .checked_add(value * 10_i128.pow(u32::try_from(9 - digits.len()).unwrap_or(0)))
            .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)?;
    }
    let epoch = i64::try_from(epoch).map_err(|_| JsonTimestampError::EpochNanosecondsOutOfRange)?;
    let pattern = iso_date_time_pattern(bytes[10], fraction, zulu)?;
    Ok((epoch, pattern))
}

fn parse_date_digits(bytes: &[u8], start: usize, length: usize) -> Result<u32, JsonTimestampError> {
    let digits = bytes
        .get(start..start + length)
        .ok_or(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::String,
        })?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::String,
        });
    }
    Ok(digits
        .iter()
        .fold(0_u32, |value, digit| value * 10 + u32::from(*digit - b'0')))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Result<i64, JsonTimestampError> {
    if !(1..=12).contains(&month) || 0 == day || day > days_in_month(year, month) {
        return Err(JsonTimestampError::UnsupportedLexeme {
            kind: JsonTimestampScalarKind::String,
        });
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)
        .and_then(|value| value.checked_add(day_of_era))
        .and_then(|value| value.checked_sub(719_468))
        .ok_or(JsonTimestampError::EpochNanosecondsOutOfRange)
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if 0 == year.rem_euclid(4)
            && (0 != year.rem_euclid(100) || 0 == year.rem_euclid(400)) =>
        {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn iso_date_time_pattern(
    separator: u8,
    fraction: Option<(u8, &str, FractionPrecision)>,
    zulu: bool,
) -> Result<&'static str, JsonTimestampError> {
    let suffix = fraction.map(|(punctuation, _, precision)| (punctuation, precision));
    let pattern = match (separator, suffix, zulu) {
        (b'T', None, false) => r#""\Y-\m-\dT\H:\M:\S""#,
        (b'T', None, true) => r#""\Y-\m-\dT\H:\M:\SZ""#,
        (b' ', None, false) => r#""\Y-\m-\d \H:\M:\S""#,
        (b' ', None, true) => r#""\Y-\m-\d \H:\M:\SZ""#,
        (b'T', Some((b'.', FractionPrecision::Milliseconds)), false) => r#""\Y-\m-\dT\H:\M:\S.\3""#,
        (b'T', Some((b'.', FractionPrecision::Microseconds)), false) => r#""\Y-\m-\dT\H:\M:\S.\6""#,
        (b'T', Some((b'.', FractionPrecision::Nanoseconds)), false) => r#""\Y-\m-\dT\H:\M:\S.\9""#,
        (b'T', Some((b'.', FractionPrecision::Variable)), false) => r#""\Y-\m-\dT\H:\M:\S.\T""#,
        (b'T', Some((b',', FractionPrecision::Milliseconds)), false) => r#""\Y-\m-\dT\H:\M:\S,\3""#,
        (b'T', Some((b',', FractionPrecision::Microseconds)), false) => r#""\Y-\m-\dT\H:\M:\S,\6""#,
        (b'T', Some((b',', FractionPrecision::Nanoseconds)), false) => r#""\Y-\m-\dT\H:\M:\S,\9""#,
        (b'T', Some((b',', FractionPrecision::Variable)), false) => r#""\Y-\m-\dT\H:\M:\S,\T""#,
        (b' ', Some((b'.', FractionPrecision::Milliseconds)), false) => r#""\Y-\m-\d \H:\M:\S.\3""#,
        (b' ', Some((b'.', FractionPrecision::Microseconds)), false) => r#""\Y-\m-\d \H:\M:\S.\6""#,
        (b' ', Some((b'.', FractionPrecision::Nanoseconds)), false) => r#""\Y-\m-\d \H:\M:\S.\9""#,
        (b' ', Some((b'.', FractionPrecision::Variable)), false) => r#""\Y-\m-\d \H:\M:\S.\T""#,
        (b' ', Some((b',', FractionPrecision::Milliseconds)), false) => r#""\Y-\m-\d \H:\M:\S,\3""#,
        (b' ', Some((b',', FractionPrecision::Microseconds)), false) => r#""\Y-\m-\d \H:\M:\S,\6""#,
        (b' ', Some((b',', FractionPrecision::Nanoseconds)), false) => r#""\Y-\m-\d \H:\M:\S,\9""#,
        (b' ', Some((b',', FractionPrecision::Variable)), false) => r#""\Y-\m-\d \H:\M:\S,\T""#,
        // A literal trailing Z does not change the epoch; it remains part of the exact pattern.
        (b'T', Some((b'.', FractionPrecision::Milliseconds)), true) => r#""\Y-\m-\dT\H:\M:\S.\3Z""#,
        (b'T', Some((b'.', FractionPrecision::Microseconds)), true) => r#""\Y-\m-\dT\H:\M:\S.\6Z""#,
        (b'T', Some((b'.', FractionPrecision::Nanoseconds)), true) => r#""\Y-\m-\dT\H:\M:\S.\9Z""#,
        (b'T', Some((b'.', FractionPrecision::Variable)), true) => r#""\Y-\m-\dT\H:\M:\S.\TZ""#,
        (b'T', Some((b',', FractionPrecision::Milliseconds)), true) => r#""\Y-\m-\dT\H:\M:\S,\3Z""#,
        (b'T', Some((b',', FractionPrecision::Microseconds)), true) => r#""\Y-\m-\dT\H:\M:\S,\6Z""#,
        (b'T', Some((b',', FractionPrecision::Nanoseconds)), true) => r#""\Y-\m-\dT\H:\M:\S,\9Z""#,
        (b'T', Some((b',', FractionPrecision::Variable)), true) => r#""\Y-\m-\dT\H:\M:\S,\TZ""#,
        (b' ', Some((b'.', FractionPrecision::Milliseconds)), true) => r#""\Y-\m-\d \H:\M:\S.\3Z""#,
        (b' ', Some((b'.', FractionPrecision::Microseconds)), true) => r#""\Y-\m-\d \H:\M:\S.\6Z""#,
        (b' ', Some((b'.', FractionPrecision::Nanoseconds)), true) => r#""\Y-\m-\d \H:\M:\S.\9Z""#,
        (b' ', Some((b'.', FractionPrecision::Variable)), true) => r#""\Y-\m-\d \H:\M:\S.\TZ""#,
        (b' ', Some((b',', FractionPrecision::Milliseconds)), true) => r#""\Y-\m-\d \H:\M:\S,\3Z""#,
        (b' ', Some((b',', FractionPrecision::Microseconds)), true) => r#""\Y-\m-\d \H:\M:\S,\6Z""#,
        (b' ', Some((b',', FractionPrecision::Nanoseconds)), true) => r#""\Y-\m-\d \H:\M:\S,\9Z""#,
        (b' ', Some((b',', FractionPrecision::Variable)), true) => r#""\Y-\m-\d \H:\M:\S,\TZ""#,
        _ => {
            return Err(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            });
        }
    };
    Ok(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_paths_decode_escaped_components_once() {
        let path = JsonTimestampPath::parse(r"outer.event\.time.\*literal.emoji\u002evalue")
            .expect("compile escaped path");
        assert_eq!(
            ["outer", "event.time", "*literal", "emoji.value"],
            path.components()
        );
        assert_eq!(
            Err(JsonTimestampPathError::Wildcard { component_index: 1 }),
            JsonTimestampPath::parse("outer.*")
        );
        assert!(matches!(
            JsonTimestampPath::parse("outer."),
            Err(JsonTimestampPathError::EmptyComponent { byte_offset: 6 })
        ));
    }

    #[test]
    fn numeric_defaults_match_cpp_precision_and_fraction_rules() {
        let resolver = JsonTimestampResolver::parse("ts").expect("compile timestamp path");
        let integer = resolver
            .resolve_number(b"1700000000123")
            .expect("epoch milliseconds");
        assert_eq!(1_700_000_000_123_000_000, integer.epoch_nanoseconds());
        assert_eq!(r"\L", integer.pattern());

        let fraction = resolver
            .resolve_number(b"1762445893.00100201")
            .expect("variable fraction");
        assert_eq!(1_762_445_893_001_002_010, fraction.epoch_nanoseconds());
        assert_eq!(r"\E.\T", fraction.pattern());
        assert!(matches!(
            resolver.resolve_number(b"1762445893.10"),
            Err(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::Number,
            })
        ));
    }

    #[test]
    fn cpp_end_to_end_numeric_timestamp_forms_resolve_exactly() {
        let resolver = JsonTimestampResolver::parse("timestamp").expect("compile timestamp path");
        let quoted = JsonString::new(br#""2015678901234""#, "2015678901234");
        let quoted = resolver
            .resolve_string(quoted)
            .expect("quoted epoch milliseconds");
        assert_eq!(2_015_678_901_234_000_000, quoted.epoch_nanoseconds());
        assert_eq!(r#""\L""#, quoted.pattern());

        let integer = resolver
            .resolve_number(b"2015678901234999")
            .expect("epoch microseconds");
        assert_eq!(2_015_678_901_234_999_000, integer.epoch_nanoseconds());
        assert_eq!(r"\C", integer.pattern());

        let fraction = resolver
            .resolve_number(b"2015678901.234999123")
            .expect("fractional epoch seconds");
        assert_eq!(2_015_678_901_234_999_123, fraction.epoch_nanoseconds());
        assert_eq!(r"\E.\9", fraction.pattern());
    }

    #[test]
    fn quoted_cpp_fixture_date_time_resolves_to_exact_pattern() {
        let resolver = JsonTimestampResolver::parse("ts").expect("compile timestamp path");
        let source = JsonString::new(br#""2015-02-01T01:02:03.004""#, "2015-02-01T01:02:03.004");
        let timestamp = resolver
            .resolve_string(source)
            .expect("resolve canonical date-time");
        assert_eq!(1_422_752_523_004_000_000, timestamp.epoch_nanoseconds());
        assert_eq!(r#""\Y-\m-\dT\H:\M:\S.\3""#, timestamp.pattern());
        assert_eq!("ts", timestamp.range_key());

        let timezone = JsonString::new(
            br#""2026-01-01T12:34:56.789 UTC-05""#,
            "2026-01-01T12:34:56.789 UTC-05",
        );
        assert!(matches!(
            resolver.resolve_string(timezone),
            Err(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::String,
            })
        ));
    }

    #[test]
    fn path_compilation_limits_are_enforced_before_record_ingestion() {
        assert!(matches!(
            JsonTimestampResolver::parse_with_limits(
                "outer.ts",
                JsonTimestampPathLimits::new(7, 8, 8),
            ),
            Err(JsonTimestampPathError::LimitExceeded {
                resource: JsonTimestampPathResource::DescriptorBytes,
                actual: 8,
                limit: 7,
            })
        ));
        assert!(matches!(
            JsonTimestampResolver::parse_with_limits(
                "outer.ts",
                JsonTimestampPathLimits::new(8, 1, 8),
            ),
            Err(JsonTimestampPathError::LimitExceeded {
                resource: JsonTimestampPathResource::Components,
                actual: 2,
                limit: 1,
            })
        ));
    }

    #[test]
    fn kv_paths_select_protocol_namespaces_and_preserve_the_range_key() {
        let user = KvIrTimestampResolver::parse(r"outer.event\.time")
            .expect("compile user-generated path");
        assert_eq!(Some(KvIrNamespace::UserGenerated), user.namespace());
        assert_eq!(["outer", "event.time"], user.path().components());
        assert_eq!(r"outer.event\.time", user.path().descriptor());

        let auto = KvIrTimestampResolver::parse("@outer.ts").expect("compile auto-generated path");
        assert_eq!(Some(KvIrNamespace::AutoGenerated), auto.namespace());
        assert_eq!(["outer", "ts"], auto.path().components());
        assert_eq!("@outer.ts", auto.path().descriptor());

        for descriptor in ["$range.ts", "!reserved.ts", "#reserved.ts"] {
            let reserved =
                KvIrTimestampResolver::parse(descriptor).expect("compile reserved namespace");
            assert_eq!(None, reserved.namespace());
            assert_eq!(descriptor, reserved.path().descriptor());
        }
    }

    #[test]
    fn kv_float_recognition_pins_cpp_fixed_nine_formatting() {
        let timestamp = KvIrTimestampResolver::resolve_fixed_nine_float("1700000000.124999046")
            .expect("resolve the fixed-nine C++ rendering");
        assert_eq!(1_700_000_000_124_999_046, timestamp.epoch_nanoseconds);
        assert_eq!(r"\E.\9", timestamp.pattern);
        assert!(matches!(
            KvIrTimestampResolver::resolve_fixed_nine_float("1700000000.124999"),
            Err(JsonTimestampError::UnsupportedLexeme {
                kind: JsonTimestampScalarKind::Number,
            })
        ));
    }
}
