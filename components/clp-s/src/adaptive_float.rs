//! Adaptive per-column floating-point encoding.
//!
//! clp-s otherwise stores every float column as raw 8-byte IEEE-754 values and leans on zstd alone.
//! Near-unique doubles (sensor decimals, prices, coordinates) barely compress that way, because a
//! double's sign/exponent/mantissa bytes are interleaved per value and the low mantissa bytes look
//! random to a byte-oriented compressor. This module supplies a small menu of lightweight,
//! bit-exact float codecs and a decoder that inverts any of them; the writer tries the applicable
//! ones, zstd-compresses each, and keeps whichever is smallest, recording the choice in a
//! self-describing one-byte scheme tag at the front of the column stream.
//!
//! Every scheme operates on the raw 64-bit patterns (`f64::to_bits`), so `NaN` payloads, both
//! infinities, and `-0.0` all round-trip exactly.
//!
//! Schemes:
//! - `PLAIN` — raw 8-byte little-endian (today's behaviour; always a safe fallback).
//! - `BYTE_STREAM_SPLIT` — regroup the eight bytes of every value into eight planes so
//!   like-significance bytes (which are highly repetitive across nearby values) sit together and
//!   zstd can compress them; the mechanism Parquet uses for floating point.
//! - `DICT` — a table of the distinct bit patterns plus bit-packed indices (low-cardinality floats).
//! - `RLE` — `(value, run-length)` pairs (long constant runs).
//! - `CONSTANT` — a single value (the whole column is one number).

use std::collections::HashMap;

use crate::adaptive_int::{AdaptiveDecodeError, bit_width_for, pack_bits, packed_len, unpack_bits};

const PLAIN: u8 = 0;
const BYTE_STREAM_SPLIT: u8 = 1;
const DICT: u8 = 2;
const CONSTANT: u8 = 3;
const RLE: u8 = 4;

/// Above this distinct-value count the dictionary scheme is not attempted; keeps the dictionary
/// small and bounds the stat scan's memory.
const MAX_DICT_CARDINALITY: usize = 1 << 16;

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, AdaptiveDecodeError> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(AdaptiveDecodeError::Truncated)?;
    Ok(u64::from_le_bytes(slice.try_into().expect("8-byte slice")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AdaptiveDecodeError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(AdaptiveDecodeError::Truncated)?;
    Ok(u32::from_le_bytes(slice.try_into().expect("4-byte slice")))
}

/// One-pass statistics: distinct count (for dictionary/constant) and run count (for RLE).
fn scan(bits: &[u64]) -> (Option<usize>, usize) {
    let mut runs = 1usize;
    let mut distinct: HashMap<u64, ()> = HashMap::new();
    distinct.insert(bits[0], ());
    for window in bits.windows(2) {
        if window[0] != window[1] {
            runs += 1;
        }
        if distinct.len() <= MAX_DICT_CARDINALITY {
            distinct.insert(window[1], ());
        }
    }
    let small_cardinality = (distinct.len() <= MAX_DICT_CARDINALITY).then_some(distinct.len());
    (small_cardinality, runs)
}

fn encode_plain(bits: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + bits.len() * 8);
    out.push(PLAIN);
    for &v in bits {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn encode_byte_stream_split(bits: &[u64]) -> Vec<u8> {
    let n = bits.len();
    let mut out = Vec::with_capacity(1 + n * 8);
    out.push(BYTE_STREAM_SPLIT);
    for plane in 0..8u32 {
        for &v in bits {
            out.push((v >> (8 * plane)) as u8);
        }
    }
    out
}

fn encode_constant(value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(CONSTANT);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

fn encode_dict(bits: &[u64], cardinality: usize) -> Vec<u8> {
    let mut table: Vec<u64> = Vec::with_capacity(cardinality);
    let mut index_of: HashMap<u64, u64> = HashMap::with_capacity(cardinality);
    let mut indices: Vec<u64> = Vec::with_capacity(bits.len());
    for &v in bits {
        let idx = *index_of.entry(v).or_insert_with(|| {
            table.push(v);
            (table.len() - 1) as u64
        });
        indices.push(idx);
    }
    let idx_width = bit_width_for((table.len() as u64).saturating_sub(1));
    let mut out =
        Vec::with_capacity(1 + 4 + table.len() * 8 + 1 + packed_len(bits.len(), idx_width));
    out.push(DICT);
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for &v in &table {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(idx_width);
    pack_bits(&indices, idx_width, &mut out);
    out
}

fn encode_rle(bits: &[u64], runs: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + runs * 12);
    out.push(RLE);
    out.extend_from_slice(&(runs as u32).to_le_bytes());
    let mut i = 0;
    while i < bits.len() {
        let value = bits[i];
        let mut count = 1u32;
        while i + (count as usize) < bits.len() && bits[i + count as usize] == value {
            count += 1;
        }
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        i += count as usize;
    }
    out
}

/// Encoded candidate buffers worth zstd-measuring for this float column, each a full scheme byte +
/// payload. `bits` are the raw `f64::to_bits` patterns. The caller compresses each and keeps the
/// smallest.
#[must_use]
pub fn candidates(bits: &[u64]) -> Vec<Vec<u8>> {
    if bits.is_empty() {
        return vec![vec![PLAIN]];
    }
    let (small_cardinality, runs) = scan(bits);
    if small_cardinality == Some(1) {
        return vec![encode_constant(bits[0])];
    }
    let mut out = vec![encode_plain(bits), encode_byte_stream_split(bits)];
    if let Some(cardinality) = small_cardinality {
        if cardinality < bits.len() {
            out.push(encode_dict(bits, cardinality));
        }
    }
    if runs * 2 < bits.len() {
        out.push(encode_rle(bits, runs));
    }
    out
}

/// Decodes a column stream produced by any of the [`candidates`] schemes into `row_count` raw
/// 64-bit float patterns.
///
/// # Errors
///
/// Returns [`AdaptiveDecodeError`] if the scheme tag is unknown or the stream is shorter than the
/// declared layout requires.
pub fn decode(bytes: &[u8], row_count: usize) -> Result<Vec<u64>, AdaptiveDecodeError> {
    let (&scheme, rest) = bytes.split_first().ok_or(AdaptiveDecodeError::Truncated)?;
    match scheme {
        PLAIN => {
            if rest.len() < row_count * 8 {
                return Err(AdaptiveDecodeError::Truncated);
            }
            Ok(rest
                .chunks_exact(8)
                .take(row_count)
                .map(|c| u64::from_le_bytes(c.try_into().expect("8-byte chunk")))
                .collect())
        }
        BYTE_STREAM_SPLIT => {
            if rest.len() < row_count * 8 {
                return Err(AdaptiveDecodeError::Truncated);
            }
            let mut out = vec![0u64; row_count];
            for plane in 0..8usize {
                let base = plane * row_count;
                for (i, value) in out.iter_mut().enumerate() {
                    *value |= u64::from(rest[base + i]) << (8 * plane);
                }
            }
            Ok(out)
        }
        CONSTANT => {
            let value = read_u64(rest, 0)?;
            Ok(vec![value; row_count])
        }
        DICT => {
            let table_len = read_u32(rest, 0)? as usize;
            let dict_end = 4 + table_len * 8;
            let table_bytes = rest.get(4..dict_end).ok_or(AdaptiveDecodeError::Truncated)?;
            let table: Vec<u64> = table_bytes
                .chunks_exact(8)
                .map(|c| u64::from_le_bytes(c.try_into().expect("8-byte chunk")))
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
                let value = read_u64(rest, offset)?;
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

    fn round_trip(bits: &[u64]) {
        for candidate in candidates(bits) {
            let decoded = decode(&candidate, bits.len())
                .unwrap_or_else(|e| panic!("decode failed ({e}) for scheme {}", candidate[0]));
            assert_eq!(decoded, bits, "scheme {} mismatch", candidate[0]);
        }
    }

    /// Bit patterns from f64 values, so the tests exercise the real IEEE layouts.
    fn bits_of(values: &[f64]) -> Vec<u64> {
        values.iter().map(|v| v.to_bits()).collect()
    }

    #[test]
    fn empty_and_single() {
        round_trip(&[]);
        round_trip(&bits_of(&[3.14]));
    }

    #[test]
    fn special_values_round_trip_bit_exact() {
        // -0.0, +0.0, both infinities, and a signalling and quiet NaN payload must survive exactly.
        let values = vec![
            0.0_f64,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from_bits(0x7ff0_0000_0000_0001), // signalling NaN
            f64::from_bits(0xfff8_dead_beef_0000), // quiet NaN with payload
        ];
        let bits = bits_of(&values);
        round_trip(&bits);
        // Byte-stream-split specifically must be bit-exact, not value-equal (NaN != NaN).
        let encoded = encode_byte_stream_split(&bits);
        assert_eq!(decode(&encoded, bits.len()).unwrap(), bits);
    }

    #[test]
    fn constant_column_picks_constant() {
        let bits = vec![2.5_f64.to_bits(); 1000];
        let cands = candidates(&bits);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0][0], CONSTANT);
        round_trip(&bits);
    }

    #[test]
    fn smoothly_varying_series() {
        let values: Vec<f64> = (0..4000).map(|i| 20.0 + f64::from(i) * 0.01).collect();
        round_trip(&bits_of(&values));
    }

    #[test]
    fn low_cardinality_floats() {
        let palette = [1.5_f64, -2.25, 0.0, 100.125];
        let values: Vec<f64> = (0..4000).map(|i| palette[i % palette.len()]).collect();
        round_trip(&bits_of(&values));
    }

    #[test]
    fn high_cardinality_floats() {
        let values: Vec<f64> = (0..4000).map(|i| f64::from(i).sqrt() * 1.000_000_1).collect();
        round_trip(&bits_of(&values));
    }

    #[test]
    fn corrupt_headers_are_rejected_not_panicked() {
        // A DICT stream with an out-of-range index width (128).
        let mut dict = vec![DICT];
        dict.extend_from_slice(&1u32.to_le_bytes()); // table_len 1
        dict.extend_from_slice(&0u64.to_le_bytes()); // one dict entry
        dict.push(128); // idx_width > 64
        assert_eq!(decode(&dict, 4), Err(AdaptiveDecodeError::Malformed));
        // An RLE run longer than the row count must error, not over-allocate.
        let mut rle = vec![RLE];
        rle.extend_from_slice(&1u32.to_le_bytes());
        rle.extend_from_slice(&0u64.to_le_bytes());
        rle.extend_from_slice(&100u32.to_le_bytes());
        assert_eq!(decode(&rle, 3), Err(AdaptiveDecodeError::Malformed));
    }
}
