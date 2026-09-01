//! Allocation-conscious JSON escaping and exact numeric formatting.
//!
//! Current C++ CLP-S extraction escapes only ASCII controls, quotation marks, and reverse
//! soliduses. Valid non-ASCII UTF-8 (including U+2028 and U+2029) is copied unchanged. The
//! [`JsonBytePolicy::PreserveInvalidUtf8`] mode deliberately reproduces the same operation on
//! arbitrary bytes; its output is valid JSON only when the input bytes are valid UTF-8.
//!
//! Prefer the `&str` APIs, or [`JsonBytePolicy::StrictUtf8`], when emitting standards-compliant
//! JSON. All append operations validate and reserve before modifying the destination, preserve
//! its contents on every error, reserve only the exact appended size, and copy safe byte runs in
//! bulk. The write pass performs no temporary allocation.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

const MEBIBYTE: usize = 1024 * 1024;

/// Resource limits applied to one escaped JSON string fragment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonEscapeLimits {
    input_bytes: usize,
    output_bytes: usize,
}

impl JsonEscapeLimits {
    /// Default limits for one dictionary value or schema key.
    pub const DEFAULT: Self = Self::new(64 * MEBIBYTE, 384 * MEBIBYTE + 3);

    /// Creates explicit input and appended-output limits.
    ///
    /// `max_output_bytes` includes any quotes and the colon emitted by a key operation. It does
    /// not include bytes already present in the caller-owned destination.
    #[must_use]
    pub const fn new(max_input_bytes: usize, max_output_bytes: usize) -> Self {
        Self {
            input_bytes: max_input_bytes,
            output_bytes: max_output_bytes,
        }
    }

    /// Maximum source bytes accepted by one operation.
    #[must_use]
    pub const fn max_input_bytes(self) -> usize {
        self.input_bytes
    }

    /// Maximum bytes one operation may append, including requested JSON framing.
    #[must_use]
    pub const fn max_output_bytes(self) -> usize {
        self.output_bytes
    }
}

impl Default for JsonEscapeLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Validation policy for an arbitrary byte slice.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonBytePolicy {
    /// Reject invalid UTF-8 before modifying the destination.
    #[default]
    StrictUtf8,
    /// Reproduce C++ CLP-S byte-preserving extraction, even for invalid UTF-8.
    ///
    /// This escapes JSON-significant ASCII bytes but copies all bytes at or above `0x20` other
    /// than `"` and `\` verbatim. The result is not standards-compliant JSON if `source` is not
    /// valid UTF-8.
    PreserveInvalidUtf8,
}

/// Appends escaped JSON string contents without surrounding quotes.
///
/// This is the allocation-safe Rust equivalent of C++ `StringUtils::escape_json_string` for a
/// known UTF-8 source. It does not escape `/`, U+2028, or U+2029.
///
/// # Errors
///
/// Returns [`JsonEscapeError`] for a resource-limit violation, size overflow, or failed bounded
/// reservation. The destination contents remain unchanged on error.
#[inline]
pub fn append_escaped_str(
    source: &str,
    destination: &mut String,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    append_str(source, destination, JsonFraming::Contents, limits)
}

/// Appends a complete quoted JSON string.
///
/// # Errors
///
/// Returns [`JsonEscapeError`] for a resource-limit violation, size overflow, or failed bounded
/// reservation. The destination contents remain unchanged on error.
#[inline]
pub fn append_json_string(
    source: &str,
    destination: &mut String,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    append_str(source, destination, JsonFraming::String, limits)
}

/// Appends a quoted and escaped JSON object key followed by `:`.
///
/// # Errors
///
/// Returns [`JsonEscapeError`] for a resource-limit violation, size overflow, or failed bounded
/// reservation. The destination contents remain unchanged on error.
#[inline]
pub fn append_json_key(
    source: &str,
    destination: &mut String,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    append_str(source, destination, JsonFraming::Key, limits)
}

/// Appends escaped JSON string contents from arbitrary bytes without surrounding quotes.
///
/// [`JsonBytePolicy::StrictUtf8`] validates the entire source before writing.
/// [`JsonBytePolicy::PreserveInvalidUtf8`] exactly matches C++ CLP-S byte-string escaping, but can
/// produce output that is not valid JSON. The explicit policy prevents accidentally treating
/// arbitrary archive bytes as standards-compliant JSON.
///
/// # Errors
///
/// Returns [`JsonEscapeError`] for invalid UTF-8 under the strict policy, a resource-limit
/// violation, size overflow, or failed bounded reservation. The destination contents remain
/// unchanged on error.
#[inline]
pub fn append_escaped_bytes(
    source: &[u8],
    destination: &mut Vec<u8>,
    policy: JsonBytePolicy,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    append_bytes(source, destination, policy, JsonFraming::Contents, limits)
}

/// Appends a complete quoted JSON byte string.
///
/// Under [`JsonBytePolicy::PreserveInvalidUtf8`], the returned bytes match C++ CLP-S extraction
/// but are not valid JSON when `source` is invalid UTF-8.
///
/// # Errors
///
/// Returns [`JsonEscapeError`] for invalid UTF-8 under the strict policy, a resource-limit
/// violation, size overflow, or failed bounded reservation. The destination contents remain
/// unchanged on error.
#[inline]
pub fn append_json_string_bytes(
    source: &[u8],
    destination: &mut Vec<u8>,
    policy: JsonBytePolicy,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    append_bytes(source, destination, policy, JsonFraming::String, limits)
}

/// Appends a quoted and escaped JSON object key followed by `:`.
///
/// Under [`JsonBytePolicy::PreserveInvalidUtf8`], the returned bytes match C++ CLP-S extraction
/// but are not valid JSON when `source` is invalid UTF-8.
///
/// # Errors
///
/// Returns [`JsonEscapeError`] for invalid UTF-8 under the strict policy, a resource-limit
/// violation, size overflow, or failed bounded reservation. The destination contents remain
/// unchanged on error.
#[inline]
pub fn append_json_key_bytes(
    source: &[u8],
    destination: &mut Vec<u8>,
    policy: JsonBytePolicy,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    append_bytes(source, destination, policy, JsonFraming::Key, limits)
}

#[derive(Clone, Copy)]
enum JsonFraming {
    Contents,
    String,
    Key,
}

impl JsonFraming {
    const fn byte_len(self) -> usize {
        match self {
            Self::Contents => 0,
            Self::String => 2,
            Self::Key => 3,
        }
    }

    fn append_prefix_to_string(self, destination: &mut String) {
        if !matches!(self, Self::Contents) {
            destination.push('"');
        }
    }

    fn append_suffix_to_string(self, destination: &mut String) {
        match self {
            Self::Contents => {}
            Self::String => destination.push('"'),
            Self::Key => destination.push_str("\":"),
        }
    }

    fn append_prefix_to_bytes(self, destination: &mut Vec<u8>) {
        if !matches!(self, Self::Contents) {
            destination.push(b'"');
        }
    }

    fn append_suffix_to_bytes(self, destination: &mut Vec<u8>) {
        match self {
            Self::Contents => {}
            Self::String => destination.push(b'"'),
            Self::Key => destination.extend_from_slice(b"\":"),
        }
    }
}

fn append_str(
    source: &str,
    destination: &mut String,
    framing: JsonFraming,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    let analysis = analyze(source.as_bytes(), framing, limits)?;
    reserve_string(destination, analysis.appended_size)?;

    framing.append_prefix_to_string(destination);
    if analysis.needs_escaping {
        append_escaped_str_validated(source, destination);
    } else {
        destination.push_str(source);
    }
    framing.append_suffix_to_string(destination);
    Ok(())
}

fn append_bytes(
    source: &[u8],
    destination: &mut Vec<u8>,
    policy: JsonBytePolicy,
    framing: JsonFraming,
    limits: JsonEscapeLimits,
) -> Result<(), JsonEscapeError> {
    check_input_size(source.len(), limits)?;
    if matches!(policy, JsonBytePolicy::StrictUtf8) {
        std::str::from_utf8(source).map_err(|error| JsonEscapeError::InvalidUtf8 {
            valid_up_to: error.valid_up_to(),
            error_len: error.error_len(),
        })?;
    }
    let analysis = analyze_checked_input(source, framing, limits)?;
    reserve_bytes(destination, analysis.appended_size)?;

    framing.append_prefix_to_bytes(destination);
    if analysis.needs_escaping {
        append_escaped_bytes_validated(source, destination);
    } else {
        destination.extend_from_slice(source);
    }
    framing.append_suffix_to_bytes(destination);
    Ok(())
}

fn analyze(
    source: &[u8],
    framing: JsonFraming,
    limits: JsonEscapeLimits,
) -> Result<EscapeAnalysis, JsonEscapeError> {
    check_input_size(source.len(), limits)?;
    analyze_checked_input(source, framing, limits)
}

const fn check_input_size(actual: usize, limits: JsonEscapeLimits) -> Result<(), JsonEscapeError> {
    if actual > limits.input_bytes {
        return Err(JsonEscapeError::InputLimitExceeded {
            actual,
            limit: limits.input_bytes,
        });
    }
    Ok(())
}

fn analyze_checked_input(
    source: &[u8],
    framing: JsonFraming,
    limits: JsonEscapeLimits,
) -> Result<EscapeAnalysis, JsonEscapeError> {
    let mut escaped_size = source.len();
    let mut needs_escaping = false;
    for &byte in source {
        let extra_bytes = escape_extra_bytes(byte);
        if 0 != extra_bytes {
            needs_escaping = true;
            escaped_size = escaped_size
                .checked_add(extra_bytes)
                .ok_or(JsonEscapeError::OutputSizeOverflow)?;
        }
    }
    let appended_size = escaped_size
        .checked_add(framing.byte_len())
        .ok_or(JsonEscapeError::OutputSizeOverflow)?;
    if appended_size > limits.output_bytes {
        return Err(JsonEscapeError::OutputLimitExceeded {
            required: appended_size,
            limit: limits.output_bytes,
        });
    }
    Ok(EscapeAnalysis {
        appended_size,
        needs_escaping,
    })
}

#[derive(Clone, Copy)]
struct EscapeAnalysis {
    appended_size: usize,
    needs_escaping: bool,
}

fn reserve_string(destination: &mut String, appended_size: usize) -> Result<(), JsonEscapeError> {
    destination
        .len()
        .checked_add(appended_size)
        .ok_or(JsonEscapeError::OutputSizeOverflow)?;
    destination
        .try_reserve_exact(appended_size)
        .map_err(|_| JsonEscapeError::AllocationFailed {
            requested_additional: appended_size,
        })
}

fn reserve_bytes(destination: &mut Vec<u8>, appended_size: usize) -> Result<(), JsonEscapeError> {
    destination
        .len()
        .checked_add(appended_size)
        .ok_or(JsonEscapeError::OutputSizeOverflow)?;
    destination
        .try_reserve_exact(appended_size)
        .map_err(|_| JsonEscapeError::AllocationFailed {
            requested_additional: appended_size,
        })
}

fn append_escaped_str_validated(source: &str, destination: &mut String) {
    let bytes = source.as_bytes();
    let mut run_start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let Some(sequence) = escape_sequence(byte) else {
            continue;
        };
        destination.push_str(&source[run_start..index]);
        destination.push_str(sequence);
        run_start = index + 1;
    }
    destination.push_str(&source[run_start..]);
}

fn append_escaped_bytes_validated(source: &[u8], destination: &mut Vec<u8>) {
    let mut run_start = 0;
    for (index, &byte) in source.iter().enumerate() {
        let Some(sequence) = escape_sequence(byte) else {
            continue;
        };
        destination.extend_from_slice(&source[run_start..index]);
        destination.extend_from_slice(sequence.as_bytes());
        run_start = index + 1;
    }
    destination.extend_from_slice(&source[run_start..]);
}

#[inline]
const fn escape_extra_bytes(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 1,
        0x00..=0x1f => 5,
        _ => 0,
    }
}

#[inline]
const fn escape_sequence(byte: u8) -> Option<&'static str> {
    match byte {
        b'"' => Some("\\\""),
        b'\\' => Some("\\\\"),
        b'\x08' => Some("\\b"),
        b'\t' => Some("\\t"),
        b'\n' => Some("\\n"),
        b'\x0c' => Some("\\f"),
        b'\r' => Some("\\r"),
        0x00..=0x1f => Some(CONTROL_ESCAPES[byte as usize]),
        _ => None,
    }
}

const CONTROL_ESCAPES: [&str; 32] = [
    "\\u0000", "\\u0001", "\\u0002", "\\u0003", "\\u0004", "\\u0005", "\\u0006", "\\u0007", "\\b",
    "\\t", "\\n", "\\u000b", "\\f", "\\r", "\\u000e", "\\u000f", "\\u0010", "\\u0011", "\\u0012",
    "\\u0013", "\\u0014", "\\u0015", "\\u0016", "\\u0017", "\\u0018", "\\u0019", "\\u001a",
    "\\u001b", "\\u001c", "\\u001d", "\\u001e", "\\u001f",
];

/// Error returned while validating, sizing, or reserving an escaped JSON fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonEscapeError {
    /// The source exceeds the configured per-operation input limit.
    InputLimitExceeded {
        /// Source byte count.
        actual: usize,
        /// Configured byte limit.
        limit: usize,
    },
    /// The exact escaped fragment exceeds the configured per-operation output limit.
    OutputLimitExceeded {
        /// Exact bytes required, including requested framing.
        required: usize,
        /// Configured byte limit.
        limit: usize,
    },
    /// Escaped or destination size arithmetic overflowed `usize`.
    OutputSizeOverflow,
    /// A strict byte-source operation encountered malformed UTF-8.
    InvalidUtf8 {
        /// Bytes that were valid UTF-8 before the malformed sequence.
        valid_up_to: usize,
        /// Length of the malformed sequence, or `None` if it was truncated.
        error_len: Option<usize>,
    },
    /// The destination could not reserve the exact additional capacity.
    AllocationFailed {
        /// Exact bytes the operation attempted to reserve.
        requested_additional: usize,
    },
}

impl Display for JsonEscapeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "JSON string input has {actual} bytes, exceeding limit {limit}"
                )
            }
            Self::OutputLimitExceeded { required, limit } => {
                write!(
                    formatter,
                    "escaped JSON fragment needs {required} bytes, exceeding limit {limit}"
                )
            }
            Self::OutputSizeOverflow => formatter.write_str("escaped JSON output size overflow"),
            Self::InvalidUtf8 {
                valid_up_to,
                error_len,
            } => match error_len {
                Some(error_len) => write!(
                    formatter,
                    "JSON byte string contains invalid UTF-8 at byte {valid_up_to} (sequence \
                     length {error_len})"
                ),
                None => write!(
                    formatter,
                    "JSON byte string contains truncated UTF-8 at byte {valid_up_to}"
                ),
            },
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} bytes for escaped JSON output"
            ),
        }
    }
}

impl Error for JsonEscapeError {}

/// Failure to format a binary64 value with `nlohmann::json::dump()` spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NlohmannFloatError {
    /// JSON has no representation for NaN or infinity as a numeric literal.
    NonFinite,
    /// The shortest finite representation could not be normalized within the fixed buffer.
    InvalidFormat,
}

impl Display for NlohmannFloatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonFinite => "cannot format a non-finite JSON number",
            Self::InvalidFormat => "failed to normalize a finite JSON number",
        })
    }
}

impl Error for NlohmannFloatError {}

/// Fixed-capacity, allocation-free `nlohmann::json::dump()` binary64 representation.
#[derive(Clone, Copy)]
pub struct NlohmannFloat {
    bytes: [u8; 32],
    len: usize,
}

impl NlohmannFloat {
    const fn new() -> Self {
        Self {
            bytes: [0; 32],
            len: 0,
        }
    }

    /// Returns the formatted ASCII representation.
    ///
    /// # Panics
    ///
    /// Panics only if the formatter's private invariant that it writes ASCII bytes is violated.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len])
            .expect("the float formatter writes only ASCII bytes")
    }

    fn push(&mut self, byte: u8) -> Result<(), NlohmannFloatError> {
        let destination = self
            .bytes
            .get_mut(self.len)
            .ok_or(NlohmannFloatError::InvalidFormat)?;
        *destination = byte;
        self.len += 1;
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), NlohmannFloatError> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(NlohmannFloatError::InvalidFormat)?;
        let destination = self
            .bytes
            .get_mut(self.len..end)
            .ok_or(NlohmannFloatError::InvalidFormat)?;
        destination.copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// Formats a finite binary64 value exactly like `nlohmann::json::dump()`.
///
/// This includes its fixed/scientific thresholds, at least one fractional digit for finite
/// integral floats, a two-digit signed exponent, and preservation of negative zero.
///
/// # Errors
///
/// Returns [`NlohmannFloatError::NonFinite`] for NaN or infinity. An internal normalization or
/// fixed-buffer failure is reported as [`NlohmannFloatError::InvalidFormat`].
pub fn format_nlohmann_float(value: f64) -> Result<NlohmannFloat, NlohmannFloatError> {
    if !value.is_finite() {
        return Err(NlohmannFloatError::NonFinite);
    }
    let mut output = NlohmannFloat::new();
    let negative = value.is_sign_negative();
    let magnitude = value.abs();
    if negative {
        output.push(b'-')?;
    }
    if 0.0 == magnitude {
        output.extend(b"0.0")?;
        return Ok(output);
    }

    let mut ryu_buffer = ryu::Buffer::new();
    let shortest = ryu_buffer.format_finite(magnitude);
    let (mantissa, explicit_exponent) = split_shortest_float(shortest)?;
    let mut digits = [0_u8; 24];
    let mut digit_count = 0_usize;
    let mut fraction_digits = 0_i32;
    let mut after_decimal = false;
    for byte in mantissa.bytes() {
        if b'.' == byte {
            if after_decimal {
                return Err(NlohmannFloatError::InvalidFormat);
            }
            after_decimal = true;
            continue;
        }
        if !byte.is_ascii_digit() || digit_count >= digits.len() {
            return Err(NlohmannFloatError::InvalidFormat);
        }
        digits[digit_count] = byte;
        digit_count += 1;
        fraction_digits += i32::from(after_decimal);
    }
    if 0 == digit_count {
        return Err(NlohmannFloatError::InvalidFormat);
    }
    let first_nonzero = digits[..digit_count]
        .iter()
        .position(|digit| b'0' != *digit)
        .ok_or(NlohmannFloatError::InvalidFormat)?;
    if 0 != first_nonzero {
        digits.copy_within(first_nonzero..digit_count, 0);
        digit_count -= first_nonzero;
    }
    let mut decimal_exponent = explicit_exponent
        .checked_sub(fraction_digits)
        .ok_or(NlohmannFloatError::InvalidFormat)?;
    while 1 < digit_count && b'0' == digits[digit_count - 1] {
        digit_count -= 1;
        decimal_exponent = decimal_exponent
            .checked_add(1)
            .ok_or(NlohmannFloatError::InvalidFormat)?;
    }
    format_float_digits(&mut output, &digits[..digit_count], decimal_exponent)?;
    Ok(output)
}

fn split_shortest_float(source: &str) -> Result<(&str, i32), NlohmannFloatError> {
    let Some(exponent_offset) = source.find(['e', 'E']) else {
        return Ok((source, 0));
    };
    let mantissa = source
        .get(..exponent_offset)
        .ok_or(NlohmannFloatError::InvalidFormat)?;
    let exponent = source
        .get(exponent_offset + 1..)
        .ok_or(NlohmannFloatError::InvalidFormat)?
        .parse::<i32>()
        .map_err(|_| NlohmannFloatError::InvalidFormat)?;
    Ok((mantissa, exponent))
}

fn format_float_digits(
    output: &mut NlohmannFloat,
    digits: &[u8],
    decimal_exponent: i32,
) -> Result<(), NlohmannFloatError> {
    let length = i32::try_from(digits.len()).map_err(|_| NlohmannFloatError::InvalidFormat)?;
    let decimal_position = length
        .checked_add(decimal_exponent)
        .ok_or(NlohmannFloatError::InvalidFormat)?;
    if length <= decimal_position && decimal_position <= 15 {
        output.extend(digits)?;
        for _ in length..decimal_position {
            output.push(b'0')?;
        }
        output.extend(b".0")?;
        return Ok(());
    }
    if 0 < decimal_position && decimal_position <= 15 {
        let split =
            usize::try_from(decimal_position).map_err(|_| NlohmannFloatError::InvalidFormat)?;
        output.extend(
            digits
                .get(..split)
                .ok_or(NlohmannFloatError::InvalidFormat)?,
        )?;
        output.push(b'.')?;
        output.extend(
            digits
                .get(split..)
                .ok_or(NlohmannFloatError::InvalidFormat)?,
        )?;
        return Ok(());
    }
    if -4 < decimal_position && decimal_position <= 0 {
        output.extend(b"0.")?;
        let zero_count =
            usize::try_from(-decimal_position).map_err(|_| NlohmannFloatError::InvalidFormat)?;
        for _ in 0..zero_count {
            output.push(b'0')?;
        }
        output.extend(digits)?;
        return Ok(());
    }

    output.push(*digits.first().ok_or(NlohmannFloatError::InvalidFormat)?)?;
    if 1 < digits.len() {
        output.push(b'.')?;
        output.extend(digits.get(1..).ok_or(NlohmannFloatError::InvalidFormat)?)?;
    }
    output.push(b'e')?;
    let exponent = decimal_position
        .checked_sub(1)
        .ok_or(NlohmannFloatError::InvalidFormat)?;
    if exponent < 0 {
        output.push(b'-')?;
    } else {
        output.push(b'+')?;
    }
    let mut exponent_buffer = itoa::Buffer::new();
    let magnitude = exponent.unsigned_abs();
    let exponent_digits = exponent_buffer.format(magnitude);
    if exponent_digits.len() < 2 {
        output.push(b'0')?;
    }
    output.extend(exponent_digits.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::JsonBytePolicy;
    use super::JsonEscapeError;
    use super::JsonEscapeLimits;
    use super::append_escaped_bytes;
    use super::append_escaped_str;
    use super::append_json_key;
    use super::append_json_key_bytes;
    use super::append_json_string;
    use super::append_json_string_bytes;
    use super::format_nlohmann_float;

    const LIMITS: JsonEscapeLimits = JsonEscapeLimits::new(1024 * 1024, 6 * 1024 * 1024 + 3);

    #[test]
    fn matches_cpp_string_utils_for_every_ascii_control() {
        let cases: &[(u8, &str)] = &[
            (0x00, "\\u0000"),
            (0x01, "\\u0001"),
            (0x02, "\\u0002"),
            (0x03, "\\u0003"),
            (0x04, "\\u0004"),
            (0x05, "\\u0005"),
            (0x06, "\\u0006"),
            (0x07, "\\u0007"),
            (0x08, "\\b"),
            (0x09, "\\t"),
            (0x0a, "\\n"),
            (0x0b, "\\u000b"),
            (0x0c, "\\f"),
            (0x0d, "\\r"),
            (0x0e, "\\u000e"),
            (0x0f, "\\u000f"),
            (0x10, "\\u0010"),
            (0x11, "\\u0011"),
            (0x12, "\\u0012"),
            (0x13, "\\u0013"),
            (0x14, "\\u0014"),
            (0x15, "\\u0015"),
            (0x16, "\\u0016"),
            (0x17, "\\u0017"),
            (0x18, "\\u0018"),
            (0x19, "\\u0019"),
            (0x1a, "\\u001a"),
            (0x1b, "\\u001b"),
            (0x1c, "\\u001c"),
            (0x1d, "\\u001d"),
            (0x1e, "\\u001e"),
            (0x1f, "\\u001f"),
        ];
        for &(input, expected) in cases {
            let mut actual = String::new();
            let mut input_buffer = [0; 4];
            let source = char::from(input).encode_utf8(&mut input_buffer);
            append_escaped_str(source, &mut actual, LIMITS)
                .expect("one-byte ASCII input is bounded");
            assert_eq!(expected, actual, "input byte 0x{input:02x}");
        }
    }

    #[test]
    fn matches_cpp_simdjson_builder_vector_and_json_framing() {
        let source = "Hello, \"world\"!";

        let mut contents = String::from("prefix=");
        append_escaped_str(source, &mut contents, LIMITS).expect("bounded C++ vector");
        assert_eq!("prefix=Hello, \\\"world\\\"!", contents);

        let mut string = String::new();
        append_json_string(source, &mut string, LIMITS).expect("bounded C++ vector");
        assert_eq!(r#""Hello, \"world\"!""#, string);

        let mut key = String::from("{");
        append_json_key(source, &mut key, LIMITS).expect("bounded C++ vector");
        assert_eq!(r#"{"Hello, \"world\"!":"#, key);
    }

    #[test]
    fn matches_cpp_valid_utf8_differential_vector() {
        let source = "\n𠀏a中\u{001f}¢\\";
        let expected = "\\n𠀏a中\\u001f¢\\\\";

        let mut string_output = String::new();
        append_escaped_str(source, &mut string_output, LIMITS).expect("bounded UTF-8 vector");
        assert_eq!(expected, string_output);

        let mut byte_output = Vec::new();
        append_escaped_bytes(
            source.as_bytes(),
            &mut byte_output,
            JsonBytePolicy::StrictUtf8,
            LIMITS,
        )
        .expect("bounded UTF-8 vector");
        assert_eq!(expected.as_bytes(), byte_output);
    }

    #[test]
    fn leaves_slash_non_ascii_and_javascript_line_separators_unescaped() {
        let source = "</script>/é中\u{2028}\u{2029}";
        let mut output = String::new();
        append_json_string(source, &mut output, LIMITS).expect("bounded UTF-8 vector");
        assert_eq!(format!("\"{source}\""), output);
    }

    #[test]
    fn byte_preserving_policy_matches_cpp_on_non_utf8() {
        let source = [0xff, b'"', 0x80, b'\\', b'\n', 0xc0, 0xaf];
        let expected = [
            b'"', 0xff, b'\\', b'"', 0x80, b'\\', b'\\', b'\\', b'n', 0xc0, 0xaf, b'"',
        ];
        let mut output = Vec::new();
        append_json_string_bytes(
            &source,
            &mut output,
            JsonBytePolicy::PreserveInvalidUtf8,
            LIMITS,
        )
        .expect("bounded byte-preserving vector");
        assert_eq!(expected, output.as_slice());
        assert!(std::str::from_utf8(&output).is_err());
    }

    #[test]
    fn strict_byte_policy_rejects_malformed_and_truncated_utf8_transactionally() {
        let mut output = b"unchanged".to_vec();
        assert_eq!(
            Err(JsonEscapeError::InvalidUtf8 {
                valid_up_to: 1,
                error_len: Some(1),
            }),
            append_escaped_bytes(b"a\xffb", &mut output, JsonBytePolicy::StrictUtf8, LIMITS,)
        );
        assert_eq!(b"unchanged", output.as_slice());

        assert_eq!(
            Err(JsonEscapeError::InvalidUtf8 {
                valid_up_to: 1,
                error_len: None,
            }),
            append_json_key_bytes(
                b"a\xe2\x82",
                &mut output,
                JsonBytePolicy::StrictUtf8,
                LIMITS,
            )
        );
        assert_eq!(b"unchanged", output.as_slice());
    }

    #[test]
    fn all_byte_values_follow_cpp_byte_rules() {
        let source: Vec<u8> = (0..=u8::MAX).collect();
        let mut output = Vec::new();
        append_escaped_bytes(
            &source,
            &mut output,
            JsonBytePolicy::PreserveInvalidUtf8,
            LIMITS,
        )
        .expect("all-byte vector is bounded");

        let escaped_controls = concat!(
            "\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007",
            "\\b\\t\\n\\u000b\\f\\r\\u000e\\u000f",
            "\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015\\u0016\\u0017",
            "\\u0018\\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f"
        );
        let mut expected = escaped_controls.as_bytes().to_vec();
        expected.extend_from_slice(
            concat!(
                " !\\\"#$%&'()*+,-./0123456789:;<=>?@",
                "ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\\\]^_`",
                "abcdefghijklmnopqrstuvwxyz{|}~\x7f"
            )
            .as_bytes(),
        );
        expected.extend(0x80..=u8::MAX);
        assert_eq!(expected, output);
    }

    #[test]
    fn string_and_key_byte_framing_are_exact() {
        let mut output = b"{".to_vec();
        append_json_key_bytes(b"a\tb", &mut output, JsonBytePolicy::StrictUtf8, LIMITS)
            .expect("bounded key");
        append_json_string_bytes(b"c\nd", &mut output, JsonBytePolicy::StrictUtf8, LIMITS)
            .expect("bounded value");
        output.push(b'}');
        assert_eq!(br#"{"a\tb":"c\nd"}"#, output.as_slice());
    }

    #[test]
    fn safe_runs_are_copied_without_changing_utf8() {
        let source = "safe é 中 / 0123456789 ".repeat(2048);
        let mut output = String::from("prefix:");
        append_escaped_str(&source, &mut output, LIMITS).expect("large safe vector is bounded");
        assert_eq!("prefix:", &output[..7]);
        assert_eq!(source, output[7..]);
    }

    #[test]
    fn limits_include_exact_expansion_and_framing_and_roll_back() {
        let mut output = String::from("unchanged");
        assert_eq!(
            Err(JsonEscapeError::InputLimitExceeded {
                actual: 3,
                limit: 2,
            }),
            append_escaped_str("abc", &mut output, JsonEscapeLimits::new(2, usize::MAX))
        );
        assert_eq!("unchanged", output);

        assert_eq!(
            Err(JsonEscapeError::OutputLimitExceeded {
                required: 8,
                limit: 7,
            }),
            append_json_string("\0", &mut output, JsonEscapeLimits::new(1, 7))
        );
        assert_eq!("unchanged", output);

        append_json_string("\0", &mut output, JsonEscapeLimits::new(1, 8))
            .expect("exact output limit is inclusive");
        assert_eq!(r#"unchanged"\u0000""#, output);
    }

    #[test]
    fn empty_fragments_and_default_policy_are_well_defined() {
        assert_eq!(JsonBytePolicy::StrictUtf8, JsonBytePolicy::default());
        let defaults = JsonEscapeLimits::default();
        assert_eq!(64 * 1024 * 1024, defaults.max_input_bytes());
        assert_eq!(384 * 1024 * 1024 + 3, defaults.max_output_bytes());

        let mut contents = String::from("x");
        append_escaped_str("", &mut contents, JsonEscapeLimits::new(0, 0))
            .expect("empty contents append nothing");
        assert_eq!("x", contents);

        let mut string = String::new();
        append_json_string("", &mut string, JsonEscapeLimits::new(0, 2))
            .expect("empty JSON string needs only quotes");
        assert_eq!(r#""""#, string);
    }

    #[test]
    fn nlohmann_float_spelling_is_allocation_free_and_exact() {
        let cases = [
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (1.0, "1.0"),
            (123_456_789.0, "123456789.0"),
            (1e-6, "1e-06"),
            (1e-5, "1e-05"),
            (1e15, "1e+15"),
            (1e16, "1e+16"),
            (1e308, "1e+308"),
            (f64::MAX, "1.7976931348623157e+308"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                expected,
                format_nlohmann_float(value).expect("finite").as_str()
            );
        }
        assert!(format_nlohmann_float(f64::INFINITY).is_err());
        assert!(format_nlohmann_float(f64::NAN).is_err());
    }
}
