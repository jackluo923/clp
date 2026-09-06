//! Adaptive per-column integer encoding.
//!
//! clp-s otherwise stores every integer column as raw 8-byte little-endian values and leans on
//! zstd alone. On integer telemetry (scaled GPS, kinematics, counters, a monotonic epoch) that
//! leaves a large gap versus a Parquet-based store that delta- and dictionary-encodes per column.
//! This module supplies a small menu of lightweight integer codecs and a decoder that inverts any
//! of them. The writer tries the applicable schemes, zstd-compresses each, and keeps whichever is
//! smallest (see [`candidates`]); the choice rides in a self-describing one-byte scheme tag at the
//! front of the column stream, so the reader needs no external metadata to invert it
//! ([`decode`]).
//!
//! Schemes:
//! - `PLAIN` — raw 8-byte little-endian (today's behaviour; always a safe fallback).
//! - `FOR` — frame-of-reference: subtract the column minimum, bit-pack the offsets.
//! - `DELTA` — store the first value, then frame-of-reference + bit-pack the first differences.
//! - `DICT` — a table of the distinct values plus bit-packed indices into it.
//! - `RLE` — `(value, run-length)` pairs.
//! - `CONSTANT` — a single value (the whole column is one number).
//!
//! Every scheme is self-delimiting given the column's row count, so the decoder consumes exactly
//! the bytes the encoder produced.

use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};

const PLAIN: u8 = 0;
const FOR: u8 = 1;
const DELTA: u8 = 2;
const DICT: u8 = 3;
const RLE: u8 = 4;
const CONSTANT: u8 = 5;

/// Above this distinct-value count a column is treated as high-cardinality: the dictionary scheme
/// is not attempted and exact cardinality is no longer tracked. Keeps the dictionary small (indices
/// stay <= 17 bits) and bounds the stat scan's memory.
const MAX_DICT_CARDINALITY: usize = 1 << 16;

/// Error returned when an adaptive column stream cannot be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdaptiveDecodeError {
    /// The scheme tag byte is not one this crate understands.
    UnknownScheme(u8),
    /// The stream ended before all declared values/headers were read.
    Truncated,
    /// A header field is inconsistent with the layout (e.g. a bit width above 64, or run lengths
    /// that do not sum to the row count).
    Malformed,
}

impl Display for AdaptiveDecodeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownScheme(scheme) => write!(f, "unknown adaptive numeric scheme {scheme}"),
            Self::Truncated => write!(f, "adaptive numeric column stream is truncated"),
            Self::Malformed => write!(f, "adaptive numeric column stream is malformed"),
        }
    }
}

impl std::error::Error for AdaptiveDecodeError {}

/// Number of bits needed to represent every value in `0..=max` (0 when `max` is 0).
pub(crate) fn bit_width_for(max: u64) -> u8 {
    if max == 0 {
        0
    } else {
        (64 - max.leading_zeros()) as u8
    }
}

/// LSB-first bit-packing of `values`, each occupying `width` bits, appended to `out`.
pub(crate) fn pack_bits(values: &[u64], width: u8, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let w = u32::from(width);
    let mut acc: u128 = 0;
    let mut nbits: u32 = 0;
    for &v in values {
        let v = if width < 64 {
            v & ((1u64 << width) - 1)
        } else {
            v
        };
        acc |= u128::from(v) << nbits;
        nbits += w;
        while nbits >= 8 {
            out.push((acc & 0xff) as u8);
            acc >>= 8;
            nbits -= 8;
        }
    }
    if nbits > 0 {
        out.push((acc & 0xff) as u8);
    }
}

/// Byte length [`pack_bits`] produces for `count` values of `width` bits.
pub(crate) fn packed_len(count: usize, width: u8) -> usize {
    (count * usize::from(width)).div_ceil(8)
}

/// Inverse of [`pack_bits`]: unpack `count` values of `width` bits from the front of `bytes`.
pub(crate) fn unpack_bits(bytes: &[u8], width: u8, count: usize) -> Result<Vec<u64>, AdaptiveDecodeError> {
    // A value cannot span more than 64 bits; a larger width byte is corruption, and passing it to
    // the shifts below would overflow (panic in debug, mask silently in release).
    if width > 64 {
        return Err(AdaptiveDecodeError::Malformed);
    }
    let mut out = Vec::with_capacity(count);
    if width == 0 {
        out.resize(count, 0);
        return Ok(out);
    }
    if bytes.len() < packed_len(count, width) {
        return Err(AdaptiveDecodeError::Truncated);
    }
    let w = u32::from(width);
    let mask: u128 = if width < 64 {
        (1u128 << width) - 1
    } else {
        u128::from(u64::MAX)
    };
    let mut acc: u128 = 0;
    let mut nbits: u32 = 0;
    let mut iter = bytes.iter();
    for _ in 0..count {
        while nbits < w {
            let byte = *iter.next().ok_or(AdaptiveDecodeError::Truncated)?;
            acc |= u128::from(byte) << nbits;
            nbits += 8;
        }
        out.push((acc & mask) as u64);
        acc >>= w;
        nbits -= w;
    }
    Ok(out)
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, AdaptiveDecodeError> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(AdaptiveDecodeError::Truncated)?;
    Ok(i64::from_le_bytes(slice.try_into().expect("8-byte slice")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AdaptiveDecodeError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(AdaptiveDecodeError::Truncated)?;
    Ok(u32::from_le_bytes(slice.try_into().expect("4-byte slice")))
}

/// One-pass column statistics used to pick which schemes to try.
struct Stats {
    min: i64,
    max: i64,
    /// `Some(k)` when the column has `k <= MAX_DICT_CARDINALITY` distinct values.
    small_cardinality: Option<usize>,
    /// Number of maximal runs of equal consecutive values.
    runs: usize,
    /// `Some((min_delta, max_delta))` when every first difference fit in `i64`; else `None`.
    delta_range: Option<(i64, i64)>,
}

fn scan(values: &[i64]) -> Stats {
    let mut min = values[0];
    let mut max = values[0];
    let mut runs = 1usize;
    let mut distinct: HashMap<i64, ()> = HashMap::new();
    let mut overflow = false;
    let mut min_delta = i64::MAX;
    let mut max_delta = i64::MIN;
    distinct.insert(values[0], ());
    for window in values.windows(2) {
        let (prev, cur) = (window[0], window[1]);
        if cur < min {
            min = cur;
        }
        if cur > max {
            max = cur;
        }
        if cur != prev {
            runs += 1;
        }
        if distinct.len() <= MAX_DICT_CARDINALITY {
            distinct.insert(cur, ());
        }
        match cur.checked_sub(prev) {
            Some(delta) => {
                if delta < min_delta {
                    min_delta = delta;
                }
                if delta > max_delta {
                    max_delta = delta;
                }
            }
            None => overflow = true,
        }
    }
    let small_cardinality = (distinct.len() <= MAX_DICT_CARDINALITY).then_some(distinct.len());
    let delta_range = (!overflow && values.len() >= 2).then_some((min_delta, max_delta));
    Stats {
        min,
        max,
        small_cardinality,
        runs,
        delta_range,
    }
}

/// Unsigned span `max - min`, or `None` when it does not fit in `u64` (only possible across the
/// full `i64` range, in which case bit-packing cannot beat plain anyway).
fn span(min: i64, max: i64) -> Option<u64> {
    u64::try_from(i128::from(max) - i128::from(min)).ok()
}

fn encode_plain(values: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + values.len() * 8);
    out.push(PLAIN);
    for &v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn encode_constant(value: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(CONSTANT);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

fn encode_for(values: &[i64], min: i64, span: u64) -> Vec<u8> {
    let width = bit_width_for(span);
    let mut out = Vec::with_capacity(1 + 8 + 1 + packed_len(values.len(), width));
    out.push(FOR);
    out.extend_from_slice(&min.to_le_bytes());
    out.push(width);
    let offsets: Vec<u64> = values
        .iter()
        .map(|&v| (v as u64).wrapping_sub(min as u64))
        .collect();
    pack_bits(&offsets, width, &mut out);
    out
}

fn encode_delta(values: &[i64], min_delta: i64, max_delta: i64) -> Option<Vec<u8>> {
    let span = span(min_delta, max_delta)?;
    let width = bit_width_for(span);
    let mut out = Vec::with_capacity(1 + 8 + 8 + 1 + packed_len(values.len() - 1, width));
    out.push(DELTA);
    out.extend_from_slice(&values[0].to_le_bytes());
    out.extend_from_slice(&min_delta.to_le_bytes());
    out.push(width);
    let offsets: Vec<u64> = values
        .windows(2)
        .map(|w| {
            let delta = w[1] - w[0];
            (delta as u64).wrapping_sub(min_delta as u64)
        })
        .collect();
    pack_bits(&offsets, width, &mut out);
    Some(out)
}

fn encode_dict(values: &[i64], cardinality: usize) -> Vec<u8> {
    let mut table: Vec<i64> = Vec::with_capacity(cardinality);
    let mut index_of: HashMap<i64, u64> = HashMap::with_capacity(cardinality);
    let mut indices: Vec<u64> = Vec::with_capacity(values.len());
    for &v in values {
        let idx = *index_of.entry(v).or_insert_with(|| {
            table.push(v);
            (table.len() - 1) as u64
        });
        indices.push(idx);
    }
    let idx_width = bit_width_for((table.len() as u64).saturating_sub(1));
    let mut out = Vec::with_capacity(1 + 4 + table.len() * 8 + 1 + packed_len(values.len(), idx_width));
    out.push(DICT);
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for &v in &table {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(idx_width);
    pack_bits(&indices, idx_width, &mut out);
    out
}

fn encode_rle(values: &[i64], runs: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + runs * 12);
    out.push(RLE);
    out.extend_from_slice(&(runs as u32).to_le_bytes());
    let mut i = 0;
    while i < values.len() {
        let value = values[i];
        let mut count = 1u32;
        while i + (count as usize) < values.len() && values[i + count as usize] == value {
            count += 1;
        }
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        i += count as usize;
    }
    out
}

/// Encoded candidate buffers (each a full scheme byte + payload) worth zstd-measuring for this
/// column. The caller compresses each and keeps the smallest. `PLAIN` is always present as a floor,
/// except for a constant column, where `CONSTANT` alone is unbeatable.
#[must_use]
pub fn candidates(values: &[i64]) -> Vec<Vec<u8>> {
    if values.is_empty() {
        return vec![vec![PLAIN]];
    }
    let stats = scan(values);
    if stats.small_cardinality == Some(1) {
        return vec![encode_constant(values[0])];
    }

    let mut out = vec![encode_plain(values)];
    if let Some(span) = span(stats.min, stats.max) {
        out.push(encode_for(values, stats.min, span));
    }
    if let Some((min_delta, max_delta)) = stats.delta_range {
        if let Some(delta) = encode_delta(values, min_delta, max_delta) {
            out.push(delta);
        }
    }
    if let Some(cardinality) = stats.small_cardinality {
        // Only worth it when the dictionary is meaningfully smaller than storing the values.
        if cardinality < values.len() {
            out.push(encode_dict(values, cardinality));
        }
    }
    // Worth trying only when runs actually compress the column.
    if stats.runs * 2 < values.len() {
        out.push(encode_rle(values, stats.runs));
    }
    out
}

/// Decodes a column stream produced by any of the [`candidates`] schemes back into `row_count`
/// values.
///
/// # Errors
///
/// Returns [`AdaptiveDecodeError`] if the scheme tag is unknown or the stream is shorter than the
/// declared layout requires.
pub fn decode(bytes: &[u8], row_count: usize) -> Result<Vec<i64>, AdaptiveDecodeError> {
    let (&scheme, rest) = bytes.split_first().ok_or(AdaptiveDecodeError::Truncated)?;
    match scheme {
        PLAIN => {
            if rest.len() < row_count * 8 {
                return Err(AdaptiveDecodeError::Truncated);
            }
            Ok(rest
                .chunks_exact(8)
                .take(row_count)
                .map(|c| i64::from_le_bytes(c.try_into().expect("8-byte chunk")))
                .collect())
        }
        CONSTANT => {
            let value = read_i64(rest, 0)?;
            Ok(vec![value; row_count])
        }
        FOR => {
            let min = read_i64(rest, 0)?;
            let width = *rest.get(8).ok_or(AdaptiveDecodeError::Truncated)?;
            let offsets = unpack_bits(&rest[9..], width, row_count)?;
            Ok(offsets
                .into_iter()
                .map(|off| (min as u64).wrapping_add(off) as i64)
                .collect())
        }
        DELTA => {
            if row_count == 0 {
                return Ok(Vec::new());
            }
            let first = read_i64(rest, 0)?;
            let min_delta = read_i64(rest, 8)?;
            let width = *rest.get(16).ok_or(AdaptiveDecodeError::Truncated)?;
            let offsets = unpack_bits(&rest[17..], width, row_count - 1)?;
            let mut out = Vec::with_capacity(row_count);
            let mut running = first;
            out.push(running);
            for off in offsets {
                let delta = (min_delta as u64).wrapping_add(off) as i64;
                running = running.wrapping_add(delta);
                out.push(running);
            }
            Ok(out)
        }
        DICT => {
            let table_len = read_u32(rest, 0)? as usize;
            let dict_end = 4 + table_len * 8;
            let table_bytes = rest.get(4..dict_end).ok_or(AdaptiveDecodeError::Truncated)?;
            let table: Vec<i64> = table_bytes
                .chunks_exact(8)
                .map(|c| i64::from_le_bytes(c.try_into().expect("8-byte chunk")))
                .collect();
            let idx_width = *rest.get(dict_end).ok_or(AdaptiveDecodeError::Truncated)?;
            let indices = unpack_bits(&rest[dict_end + 1..], idx_width, row_count)?;
            indices
                .into_iter()
                .map(|idx| {
                    table
                        .get(idx as usize)
                        .copied()
                        .ok_or(AdaptiveDecodeError::Truncated)
                })
                .collect()
        }
        RLE => {
            let run_count = read_u32(rest, 0)? as usize;
            let mut out = Vec::with_capacity(row_count);
            let mut remaining = row_count;
            let mut offset = 4;
            for _ in 0..run_count {
                let value = read_i64(rest, offset)?;
                let count = read_u32(rest, offset + 8)? as usize;
                // Bound each run by the rows left, so a corrupt count can neither over-allocate nor
                // produce a column of the wrong length.
                if count > remaining {
                    return Err(AdaptiveDecodeError::Malformed);
                }
                remaining -= count;
                out.resize(out.len() + count, value);
                offset += 12;
            }
            if remaining != 0 {
                return Err(AdaptiveDecodeError::Malformed);
            }
            Ok(out)
        }
        other => Err(AdaptiveDecodeError::UnknownScheme(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[i64]) {
        for candidate in candidates(values) {
            let decoded = decode(&candidate, values.len())
                .unwrap_or_else(|e| panic!("decode failed ({e}) for scheme {}", candidate[0]));
            assert_eq!(decoded, values, "scheme {} mismatch", candidate[0]);
        }
    }

    #[test]
    fn empty_column() {
        round_trip(&[]);
        assert_eq!(decode(&[PLAIN], 0).unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn single_value() {
        round_trip(&[42]);
        round_trip(&[i64::MIN]);
        round_trip(&[i64::MAX]);
    }

    #[test]
    fn constant_column_picks_constant() {
        let values = vec![7i64; 1000];
        let cands = candidates(&values);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0][0], CONSTANT);
        round_trip(&values);
    }

    #[test]
    fn monotonic_timestamp() {
        let values: Vec<i64> = (0..5000).map(|i| 1_700_000_000_000 + i * 100).collect();
        round_trip(&values);
    }

    #[test]
    fn bounded_random_walk() {
        // Deterministic pseudo-walk within a bounded range (no rng dependency).
        let mut v = 500_000i64;
        let mut values = Vec::new();
        let mut state = 0x2545F4914F6CDD1Du64;
        for _ in 0..5000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let step = (state >> 33) as i64 % 7 - 3;
            v = (v + step).clamp(0, 1_000_000);
            values.push(v);
        }
        round_trip(&values);
    }

    #[test]
    fn low_cardinality_enum() {
        let values: Vec<i64> = (0..4000).map(|i| i64::from(i % 7)).collect();
        round_trip(&values);
    }

    #[test]
    fn negatives_and_extremes() {
        round_trip(&[i64::MIN, i64::MAX, 0, -1, 1, i64::MIN, i64::MAX]);
        round_trip(&[-100, -50, -50, -50, 0, 50, 100]);
    }

    #[test]
    fn high_cardinality_distinct() {
        let values: Vec<i64> = (0i64..5000).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        round_trip(&values);
    }

    #[test]
    fn bit_pack_all_widths() {
        for width in 0u8..=64 {
            let count = 300usize;
            let max = if width == 0 {
                0u64
            } else if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            let values: Vec<u64> = (0..count as u64).map(|i| i.wrapping_mul(2_654_435_761) & max).collect();
            let mut packed = Vec::new();
            pack_bits(&values, width, &mut packed);
            assert_eq!(packed.len(), packed_len(count, width), "width {width}");
            let unpacked = unpack_bits(&packed, width, count).unwrap();
            assert_eq!(unpacked, values, "width {width}");
        }
    }

    #[test]
    fn truncated_stream_errors() {
        assert_eq!(decode(&[], 0), Err(AdaptiveDecodeError::Truncated));
        assert_eq!(decode(&[FOR, 0, 0], 10), Err(AdaptiveDecodeError::Truncated));
        assert_eq!(decode(&[99], 0), Err(AdaptiveDecodeError::UnknownScheme(99)));
    }

    #[test]
    fn corrupt_headers_are_rejected_not_panicked() {
        // A FOR stream with an out-of-range bit width (128): min(8) + width(1), width > 64.
        let mut foredit = vec![FOR];
        foredit.extend_from_slice(&0i64.to_le_bytes());
        foredit.push(128);
        assert_eq!(decode(&foredit, 4), Err(AdaptiveDecodeError::Malformed));
        // width > 64 caught directly in unpack_bits.
        assert_eq!(unpack_bits(&[0; 64], 65, 4), Err(AdaptiveDecodeError::Malformed));
        // An RLE run longer than the row count must error, not over-allocate.
        let mut rle = vec![RLE];
        rle.extend_from_slice(&1u32.to_le_bytes()); // one run
        rle.extend_from_slice(&7i64.to_le_bytes()); // value
        rle.extend_from_slice(&100u32.to_le_bytes()); // count 100 > row_count 3
        assert_eq!(decode(&rle, 3), Err(AdaptiveDecodeError::Malformed));
    }
}
