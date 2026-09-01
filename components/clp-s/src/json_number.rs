//! Allocation-conscious scalar JSON number emission for CLP-S extraction.
//!
//! Current C++ CLP-S extraction formats integer columns with `std::to_string(int64_t)` and
//! ordinary float columns with `std::to_string(double)`. Under the C numeric locale used by the
//! extractor, the latter is fixed notation with exactly six fractional digits. This module
//! reproduces that finite-value spelling, including negative zero. Formatted-float columns retain
//! their separate archive spelling path and do not use this module.
//!
//! Each function formats into a fixed stack buffer, validates the exact destination size, and
//! reserves once before appending. No temporary heap allocation, unsafe code, locale, or
//! process-global state is used. Non-finite floats are rejected because `nan` and `inf` are not
//! JSON numbers. The finite formatting path was differentially verified against the mandated
//! GCC 8.5/libstdc++ environment over boundary values and a deterministic 65,536-bit-pattern
//! sample with no mismatches.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write as _;

/// Maximum bytes emitted for one signed 64-bit integer.
pub const MAX_I64_JSON_BYTES: usize = 20;

/// Maximum bytes emitted for one finite binary64 value in fixed notation with six fraction digits.
///
/// A negative finite binary64 value has at most 309 integer digits, one sign, one decimal point,
/// and six fractional digits.
pub const MAX_F64_JSON_BYTES: usize = 317;

/// Appends the C++ CLP-S JSON spelling of a signed 64-bit integer.
///
/// The integer is encoded directly into a fixed stack buffer. The destination contents remain
/// unchanged on every error.
///
/// # Errors
///
/// Returns [`JsonNumberError`] if an internal formatting bound is violated, destination-size
/// arithmetic overflows, or the exact bounded reservation fails.
#[inline]
pub fn append_json_i64(value: i64, destination: &mut Vec<u8>) -> Result<(), JsonNumberError> {
    let mut scratch = [0_u8; MAX_I64_JSON_BYTES];
    let formatted = format_i64(value, &mut scratch)?;
    append_preformatted(formatted, destination)
}

/// Appends the C++ CLP-S JSON spelling of a finite binary64 value.
///
/// Output uses fixed notation with exactly six fractional digits, matching
/// `std::to_string(double)` under the extractor's C numeric locale. Negative zero and negative
/// finite values that round to zero retain their sign. The destination contents remain unchanged
/// on every error.
///
/// # Errors
///
/// Returns [`JsonNumberError::NonFiniteFloat`] for NaN or either infinity. It otherwise returns an
/// error if the fixed stack bound is violated, destination-size arithmetic overflows, or the exact
/// bounded reservation fails.
#[inline]
pub fn append_json_f64(value: f64, destination: &mut Vec<u8>) -> Result<(), JsonNumberError> {
    if !value.is_finite() {
        return Err(JsonNumberError::NonFiniteFloat(classify_non_finite(value)));
    }

    let mut scratch = StackBuffer::<MAX_F64_JSON_BYTES>::new();
    write!(&mut scratch, "{value:.6}").map_err(|_| JsonNumberError::FormattingBoundExceeded {
        bound: MAX_F64_JSON_BYTES,
    })?;
    let formatted = scratch
        .as_bytes()
        .ok_or(JsonNumberError::FormattingBoundExceeded {
            bound: MAX_F64_JSON_BYTES,
        })?;
    append_preformatted(formatted, destination)
}

fn format_i64(
    value: i64,
    scratch: &mut [u8; MAX_I64_JSON_BYTES],
) -> Result<&[u8], JsonNumberError> {
    let mut cursor = scratch.len();
    let mut magnitude = value.unsigned_abs();
    loop {
        cursor = cursor
            .checked_sub(1)
            .ok_or(JsonNumberError::FormattingBoundExceeded {
                bound: MAX_I64_JSON_BYTES,
            })?;
        let digit =
            u8::try_from(magnitude % 10).map_err(|_| JsonNumberError::FormattingBoundExceeded {
                bound: MAX_I64_JSON_BYTES,
            })?;
        let slot = scratch
            .get_mut(cursor)
            .ok_or(JsonNumberError::FormattingBoundExceeded {
                bound: MAX_I64_JSON_BYTES,
            })?;
        *slot = b'0' + digit;
        magnitude /= 10;
        if 0 == magnitude {
            break;
        }
    }

    if value.is_negative() {
        cursor = cursor
            .checked_sub(1)
            .ok_or(JsonNumberError::FormattingBoundExceeded {
                bound: MAX_I64_JSON_BYTES,
            })?;
        let slot = scratch
            .get_mut(cursor)
            .ok_or(JsonNumberError::FormattingBoundExceeded {
                bound: MAX_I64_JSON_BYTES,
            })?;
        *slot = b'-';
    }

    scratch
        .get(cursor..)
        .ok_or(JsonNumberError::FormattingBoundExceeded {
            bound: MAX_I64_JSON_BYTES,
        })
}

fn append_preformatted(formatted: &[u8], destination: &mut Vec<u8>) -> Result<(), JsonNumberError> {
    destination
        .len()
        .checked_add(formatted.len())
        .ok_or(JsonNumberError::OutputSizeOverflow)?;
    destination
        .try_reserve_exact(formatted.len())
        .map_err(|_| JsonNumberError::AllocationFailed {
            requested_additional: formatted.len(),
        })?;
    destination.extend_from_slice(formatted);
    Ok(())
}

const fn classify_non_finite(value: f64) -> NonFiniteFloat {
    if value.is_nan() {
        NonFiniteFloat::NaN
    } else if value.is_sign_negative() {
        NonFiniteFloat::NegativeInfinity
    } else {
        NonFiniteFloat::PositiveInfinity
    }
}

struct StackBuffer<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> StackBuffer<CAPACITY> {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            len: 0,
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        self.bytes.get(..self.len)
    }
}

impl<const CAPACITY: usize> fmt::Write for StackBuffer<CAPACITY> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        destination.copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Classification of a binary64 value that cannot be emitted as a JSON number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NonFiniteFloat {
    /// Any quiet or signaling NaN payload.
    NaN,
    /// Positive infinity.
    PositiveInfinity,
    /// Negative infinity.
    NegativeInfinity,
}

impl Display for NonFiniteFloat {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NaN => "NaN",
            Self::PositiveInfinity => "positive infinity",
            Self::NegativeInfinity => "negative infinity",
        })
    }
}

/// Error returned while formatting or reserving one scalar JSON number.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum JsonNumberError {
    /// A non-finite binary64 value was rejected instead of emitting invalid JSON.
    NonFiniteFloat(NonFiniteFloat),
    /// A standard-library formatting result exceeded its proved fixed stack bound.
    FormattingBoundExceeded {
        /// Stack-buffer size available to the formatter.
        bound: usize,
    },
    /// Destination-size arithmetic overflowed `usize`.
    OutputSizeOverflow,
    /// The destination could not reserve the exact additional capacity.
    AllocationFailed {
        /// Exact bytes the operation attempted to reserve.
        requested_additional: usize,
    },
}

impl Display for JsonNumberError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteFloat(value) => {
                write!(formatter, "cannot emit non-finite {value} as a JSON number")
            }
            Self::FormattingBoundExceeded { bound } => {
                write!(
                    formatter,
                    "JSON number exceeded its {bound}-byte formatting bound"
                )
            }
            Self::OutputSizeOverflow => formatter.write_str("JSON number output size overflow"),
            Self::AllocationFailed {
                requested_additional,
            } => write!(
                formatter,
                "failed to reserve {requested_additional} bytes for JSON number output"
            ),
        }
    }
}

impl Error for JsonNumberError {}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::JsonNumberError;
    use super::MAX_F64_JSON_BYTES;
    use super::MAX_I64_JSON_BYTES;
    use super::NonFiniteFloat;
    use super::StackBuffer;
    use super::append_json_f64;
    use super::append_json_i64;

    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;

    #[test]
    fn matches_cpp_integer_boundaries_and_appends() {
        let cases = [
            (i64::MIN, "-9223372036854775808"),
            (-9_007_199_254_740_992, "-9007199254740992"),
            (-1, "-1"),
            (0, "0"),
            (1, "1"),
            (9_007_199_254_740_992, "9007199254740992"),
            (i64::MAX, "9223372036854775807"),
        ];

        let mut output = b"prefix=".to_vec();
        for (index, &(value, expected)) in cases.iter().enumerate() {
            if 0 != index {
                output.push(b',');
            }
            append_json_i64(value, &mut output).expect("i64 has a fixed formatting bound");
            assert!(output.ends_with(expected.as_bytes()));
        }
        assert_eq!(MAX_I64_JSON_BYTES, cases[0].1.len());

        let (hash, bytes) = hash_i64_values(cases.iter().map(|&(value, _)| value));
        assert_eq!(0xf922_55c5_2f5b_e8c9, hash);
        assert_eq!(83, bytes);
    }

    #[test]
    fn matches_readable_cpp_float_rounding_vectors() {
        let cases = [
            (0.0, "0.000000"),
            (-0.0, "-0.000000"),
            (1.0, "1.000000"),
            (-1.0, "-1.000000"),
            (1.234_567_4, "1.234567"),
            (1.234_567_5, "1.234568"),
            (1.234_567_6, "1.234568"),
            (-1.234_567_5, "-1.234568"),
            (0.000_000_4, "0.000000"),
            (0.000_000_5, "0.000000"),
            (0.000_000_6, "0.000001"),
            (-0.000_000_4, "-0.000000"),
            (-0.000_000_5, "-0.000000"),
            (-0.000_000_6, "-0.000001"),
            (f64::from_bits(0x3ea0_c6f7_a0b5_ed8c), "0.000000"),
            (f64::from_bits(0x3ea0_c6f7_a0b5_ed8e), "0.000001"),
            (f64::from_bits(1), "0.000000"),
            (f64::from_bits(0x8000_0000_0000_0001), "-0.000000"),
            (f64::MIN_POSITIVE, "0.000000"),
            (-f64::MIN_POSITIVE, "-0.000000"),
            (9_007_199_254_740_991.0, "9007199254740991.000000"),
            (9_007_199_254_740_992.0, "9007199254740992.000000"),
            (1e20, "100000000000000000000.000000"),
            (-1e20, "-100000000000000000000.000000"),
        ];

        for &(value, expected) in &cases {
            let mut output = Vec::new();
            append_json_f64(value, &mut output).expect("finite f64 has a fixed formatting bound");
            assert_eq!(expected.as_bytes(), output, "bits {:016x}", value.to_bits());
        }
    }

    #[test]
    fn longest_finite_value_reaches_the_documented_stack_bound() {
        let mut positive = Vec::new();
        append_json_f64(f64::MAX, &mut positive).expect("maximum finite f64 is bounded");
        assert_eq!(MAX_F64_JSON_BYTES - 1, positive.len());
        assert!(positive.ends_with(b".000000"));

        let mut negative = Vec::new();
        append_json_f64(-f64::MAX, &mut negative).expect("minimum finite f64 is bounded");
        assert_eq!(MAX_F64_JSON_BYTES, negative.len());
        assert_eq!(b'-', negative[0]);
        assert_eq!(positive, &negative[1..]);
    }

    #[test]
    fn rejects_non_finite_json_numbers_without_modifying_output() {
        let cases = [
            (f64::NAN, NonFiniteFloat::NaN),
            (-f64::NAN, NonFiniteFloat::NaN),
            (f64::INFINITY, NonFiniteFloat::PositiveInfinity),
            (f64::NEG_INFINITY, NonFiniteFloat::NegativeInfinity),
        ];
        for (value, classification) in cases {
            let mut output = b"unchanged".to_vec();
            assert_eq!(
                Err(JsonNumberError::NonFiniteFloat(classification)),
                append_json_f64(value, &mut output)
            );
            assert_eq!(b"unchanged", output.as_slice());
        }
    }

    #[test]
    fn checked_stack_writer_fails_without_partial_destination_output() {
        let mut scratch = StackBuffer::<3>::new();
        assert!(write!(&mut scratch, "123").is_ok());
        assert!(write!(&mut scratch, "4").is_err());
        assert_eq!(Some(b"123".as_slice()), scratch.as_bytes());
    }

    #[test]
    fn matches_cpp_boundary_corpus_hash() {
        // GCC 8.5/libstdc++ oracle: std::to_string(double), one newline-delimited result per value.
        let values = cpp_boundary_values();
        let (hash, bytes, finite_count) = hash_f64_values(values);
        assert_eq!(35, finite_count);
        assert_eq!(1848, bytes);
        assert_eq!(0x15a4_c4ac_07c6_ef49, hash);
    }

    #[test]
    fn matches_large_deterministic_cpp_finite_bit_sample() {
        // The same oracle formats every finite SplitMix64 bit pattern and hashes exact bytes.
        let mut state = 0x243f_6a88_85a3_08d3;
        let values = (0..65_536).map(|_| f64::from_bits(splitmix64(&mut state)));
        let (hash, bytes, finite_count) = hash_f64_values(values);
        assert_eq!(65_504, finite_count);
        assert_eq!(5_637_941, bytes);
        assert_eq!(0x6287_8fd4_daee_e6d1, hash);
    }

    fn cpp_boundary_values() -> impl Iterator<Item = f64> {
        [
            0.0,
            -0.0,
            1.0,
            -1.0,
            1.234_567_4,
            1.234_567_5,
            1.234_567_6,
            -1.234_567_4,
            -1.234_567_5,
            -1.234_567_6,
            0.000_000_4,
            0.000_000_5,
            0.000_000_6,
            -0.000_000_4,
            -0.000_000_5,
            -0.000_000_6,
            f64::from_bits(0x3ea0_c6f7_a0b5_ed8c),
            f64::from_bits(0x3ea0_c6f7_a0b5_ed8e),
            f64::from_bits(0xbea0_c6f7_a0b5_ed8c),
            f64::from_bits(0xbea0_c6f7_a0b5_ed8e),
            f64::from_bits(1),
            f64::from_bits(0x8000_0000_0000_0001),
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            9_007_199_254_740_991.0,
            9_007_199_254_740_992.0,
            9_007_199_254_740_994.0,
            1e20,
            -1e20,
            1e100,
            -1e100,
            1e308,
            -1e308,
            f64::MAX,
            -f64::MAX,
        ]
        .into_iter()
    }

    fn hash_i64_values(values: impl IntoIterator<Item = i64>) -> (u64, usize) {
        let mut hash = FNV_OFFSET;
        let mut bytes = 0;
        let mut output = Vec::with_capacity(MAX_I64_JSON_BYTES);
        for value in values {
            output.clear();
            append_json_i64(value, &mut output).expect("i64 has a fixed formatting bound");
            update_hash(&mut hash, &mut bytes, &output);
        }
        (hash, bytes)
    }

    fn hash_f64_values(values: impl IntoIterator<Item = f64>) -> (u64, usize, usize) {
        let mut hash = FNV_OFFSET;
        let mut bytes = 0;
        let mut finite_count = 0;
        let mut output = Vec::with_capacity(MAX_F64_JSON_BYTES);
        for value in values {
            if !value.is_finite() {
                continue;
            }
            output.clear();
            append_json_f64(value, &mut output).expect("finite f64 has a fixed formatting bound");
            update_hash(&mut hash, &mut bytes, &output);
            finite_count += 1;
        }
        (hash, bytes, finite_count)
    }

    fn update_hash(hash: &mut u64, byte_count: &mut usize, bytes: &[u8]) {
        for &byte in bytes.iter().chain(std::iter::once(&b'\n')) {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(FNV_PRIME);
            *byte_count += 1;
        }
    }

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = *state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}
