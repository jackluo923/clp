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
//!   Offered twice: with the table in first-appearance order and the narrowest indices, and with
//!   the table sorted by value and indices one or two whole bytes wide. Sorted, a slowly varying
//!   signal has slowly varying indices, and whole bytes keep them aligned, which zstd exploits as
//!   it does integers; filters still test each distinct value once.
//! - `RLE` — `(value, run-length)` pairs (long constant runs).
//! - `CONSTANT` — a single value (the whole column is one number).
//! - `DECIMAL` — the integers `m` and one scale `k` such that every value is `m / 10^k`, or
//!   `m * (1 / 10^k)` with a rounded reciprocal, rounded
//!   to a double, or to a float when the column holds only floats (values a converter widened from
//!   float32), with the integers in one of the integer schemes. Sensor readings and prices usually
//!   have a few decimal digits, so their mantissas are small integers that compress as integer
//!   columns do. Applies only when that reproduces every value bit for bit, so `-0.0`, `NaN` and
//!   infinities rule it out.

use std::collections::HashMap;
use std::sync::Arc;

use crate::adaptive_int::{AdaptiveDecodeError, bit_width_for, pack_bits_iter, packed_len, unpack_bits};
use crate::encoding_effort::EncodingEffort;

const PLAIN: u8 = 0;
const BYTE_STREAM_SPLIT: u8 = 1;
const DICT: u8 = 2;
const CONSTANT: u8 = 3;
const RLE: u8 = 4;
const DECIMAL: u8 = 5;

/// `DECIMAL` header bit: values are rounded to a float, then widened.
const SINGLE: u8 = 0x80;
/// `DECIMAL` header bit: multiply by the rounded reciprocal instead of dividing.
const MULTIPLY: u8 = 0x40;
/// `DECIMAL` header bit: mantissas are eight byte planes, without an integer scheme tag.
const BYTE_PLANES: u8 = 0x20;

/// Powers of ten a decimal scale divides by. Each is an exact double, as is every mantissa below
/// [`MAX_MANTISSA`], so `m / 10^k` is one correctly rounded division.
const POWERS: [f64; 16] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
];

/// Mantissas stay below 2^53, where every integer is an exact double.
const MAX_MANTISSA: f64 = 9_007_199_254_740_992.0;

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

/// A complete, bounded dictionary in first-appearance order. Keep the index built during the
/// statistics pass so each dictionary candidate can use it without rebuilding the map.
struct Dictionary {
    values: Vec<u64>,
    index_of: HashMap<u64, usize>,
}

struct Stats {
    dictionary: Option<Dictionary>,
    runs: usize,
    decimal_possible: bool,
}

impl Stats {
    fn cardinality(&self) -> Option<usize> {
        self.dictionary.as_ref().map(|dictionary| dictionary.values.len())
    }
}

/// All decimal modes reject negative zero, nonfinite values and magnitudes at least 2^53.
/// Positive IEEE bit patterns have the same order as finite magnitudes, so this needs no cast
/// or floating-point arithmetic. These are necessary conditions, not an approximate fit test.
fn decimal_value_possible(bits: u64) -> bool {
    bits != (-0.0_f64).to_bits() && bits & 0x7fff_ffff_ffff_ffff < MAX_MANTISSA.to_bits()
}

/// One-pass bounded dictionary, run count and cheap exact decimal eligibility flags.
fn scan(bits: &[u64]) -> Stats {
    let mut runs = 1usize;
    let mut dictionary = Some(Dictionary {
        values: vec![bits[0]],
        index_of: HashMap::from([(bits[0], 0)]),
    });
    let mut decimal_possible = decimal_value_possible(bits[0]);
    for window in bits.windows(2) {
        if window[0] == window[1] {
            continue;
        }
        runs += 1;
        decimal_possible &= decimal_value_possible(window[1]);
        if let Some(dict) = &mut dictionary {
            if let std::collections::hash_map::Entry::Vacant(entry) = dict.index_of.entry(window[1]) {
                if dict.values.len() == MAX_DICT_CARDINALITY {
                    // Do not retain an incomplete dictionary or allow it to grow with the input.
                    dictionary = None;
                } else {
                    entry.insert(dict.values.len());
                    dict.values.push(window[1]);
                }
            }
        }
    }
    Stats { dictionary, runs, decimal_possible }
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

fn dictionary_indices<'a>(bits: &'a [u64], dictionary: &'a Dictionary) -> impl Iterator<Item = usize> + 'a {
    let mut previous = None;
    bits.iter().map(move |&value| {
        if let Some((last, index)) = previous {
            if last == value { return index; }
        }
        let index = dictionary.index_of[&value];
        previous = Some((value, index));
        index
    })
}

fn encode_dict(bits: &[u64], dictionary: &Dictionary) -> Vec<u8> {
    let table = &dictionary.values;
    let idx_width = bit_width_for((table.len() as u64).saturating_sub(1));
    let mut out =
        Vec::with_capacity(1 + 4 + table.len() * 8 + 1 + packed_len(bits.len(), idx_width));
    out.push(DICT);
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for &v in table {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(idx_width);
    pack_bits_iter(dictionary_indices(bits, dictionary).map(|index| index as u64), idx_width, &mut out);
    out
}

/// A `DICT` stream whose table is sorted by value and whose indices are 8 or 16 bits wide. Only
/// for a column of at most 65,536 distinct values.
fn encode_sorted_dict(bits: &[u64], dictionary: &Dictionary) -> Vec<u8> {
    let table = &dictionary.values;
    let mut order: Vec<usize> = (0..table.len()).collect();
    // Total order on the values; bit patterns are distinct, so ties cannot occur.
    order.sort_unstable_by(|&a, &b| f64::from_bits(table[a]).total_cmp(&f64::from_bits(table[b])));
    let mut remap = vec![0u64; table.len()];
    for (sorted, &original) in order.iter().enumerate() {
        remap[original] = sorted as u64;
    }
    let idx_width = if table.len() <= 1 << 8 { 8 } else { 16 };
    let mut out =
        Vec::with_capacity(1 + 4 + table.len() * 8 + 1 + packed_len(bits.len(), idx_width));
    out.push(DICT);
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for &original in &order {
        out.extend_from_slice(&table[original].to_le_bytes());
    }
    out.push(idx_width);
    pack_bits_iter(dictionary_indices(bits, dictionary).map(|index| remap[index]), idx_width, &mut out);
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

/// The value `mantissa` stands for under a `DECIMAL` header, as a raw bit pattern.
fn decimal_value(mantissa: i64, header: u8) -> u64 {
    let power = POWERS[usize::from(header & 0x0f)];
    // Multiplication by a rounded reciprocal is intentionally distinct from division. DBC
    // converters commonly produce the former, and the two operations can differ by one ULP.
    let value = if header & MULTIPLY != 0 {
        mantissa as f64 * (1.0 / power)
    } else {
        mantissa as f64 / power
    };
    if header & SINGLE == 0 { value.to_bits() } else { f64::from(value as f32).to_bits() }
}

/// The mantissa that reproduces `bits` exactly under `header`, if one does.
fn mantissa(bits: u64, header: u8) -> Option<i64> {
    let scaled = (f64::from_bits(bits) * POWERS[usize::from(header & 0x0f)]).round();
    // Also rejects NaN and infinities.
    if !(scaled.abs() < MAX_MANTISSA) {
        return None;
    }
    let mantissa = scaled as i64;
    (decimal_value(mantissa, header) == bits).then_some(mantissa)
}

/// Materialize integers only after every distinct pattern has passed the exact reconstruction
/// check. Adjacent equal patterns reuse their integer without either hashing or redoing math.
fn proven_mantissas(bits: &[u64], header: u8) -> Vec<i64> {
    let power = POWERS[usize::from(header & 0x0f)];
    let mut previous = None;
    bits.iter().map(|&value| {
        if let Some((last, integer)) = previous {
            if last == value { return integer; }
        }
        let integer = (f64::from_bits(value) * power).round() as i64;
        previous = Some((value, integer));
        integer
    }).collect()
}

/// A complete dictionary is a proof over the full column, not a sample. If the cardinality cap
/// was exceeded, retain the original full-row check rather than treating the partial set as proof.
fn verified_mantissas(bits: &[u64], header: u8, stats: &Stats) -> Option<Vec<i64>> {
    if let Some(dictionary) = &stats.dictionary {
        dictionary.values.iter().all(|&value| mantissa(value, header).is_some())
            .then(|| proven_mantissas(bits, header))
    } else {
        bits.iter().map(|&value| mantissa(value, header)).collect()
    }
}

/// Preserve the legacy row-order scale search and its first successful representation, including
/// repeated values encountered after the scale changes. Only the final exact check can be reduced
/// to distinct values: removing repeats from the scale search could change its chosen header.
fn decimal_mantissas_with_stats(bits: &[u64], stats: &Stats) -> Option<(u8, Vec<i64>)> {
    if !stats.decimal_possible { return None; }
    let at = |kind: u8| {
        let mut scale = 0;
        let mut previous = None;
        for &b in bits {
            if previous == Some(b) { continue; }
            while mantissa(b, kind | scale).is_none() {
                scale += 1;
                if usize::from(scale) == POWERS.len() {
                    return None;
                }
            }
            previous = Some(b);
        }
        let header = kind | scale;
        verified_mantissas(bits, header, stats)
            .map(|mantissas| (header, mantissas))
    };
    at(0).or_else(|| {
        stats.dictionary.as_ref().map_or(bits, |dictionary| &dictionary.values).iter()
            .all(|&b| f64::from(f64::from_bits(b) as f32).to_bits() == b)
            .then(|| at(SINGLE))
            .flatten()
    })
}

#[cfg(test)]
fn decimal_mantissas(bits: &[u64]) -> Option<(u8, Vec<i64>)> {
    decimal_mantissas_with_stats(bits, &scan(bits))
}

fn encode_decimal(header: u8, packed: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + packed.len());
    out.push(DECIMAL);
    out.push(header);
    out.extend_from_slice(&packed);
    out
}

/// Find a scale for exact multiplication. Sampling can only discard an optional candidate;
/// every emitted candidate is verified against every input bit pattern. Unlike division,
/// multiplication by rounded reciprocals is not monotonic in the scale, so check each scale
/// independently and do not carry a previous successful scale forward between values.
fn multiplication_mantissas_with_stats(bits: &[u64], max_scale: u8, stats: &Stats) -> Option<(u8, Vec<i64>)> {
    if bits.len() < 2 || !stats.decimal_possible { return None; }
    let samples = bits.len().min(256);
    for scale in 0..=max_scale {
        let header = MULTIPLY | scale;
        // Include both endpoints and spread interior samples over the entire physical column.
        if !(0..samples).all(|i| {
            let position = (i as u128 * (bits.len() - 1) as u128 / (samples - 1) as u128) as usize;
            mantissa(bits[position], header).is_some()
        }) {
            continue;
        }
        if let Some(values) = verified_mantissas(bits, header, stats) {
            return Some((header, values));
        }
    }
    None
}

#[cfg(test)]
fn multiplication_mantissas(bits: &[u64], max_scale: u8) -> Option<(u8, Vec<i64>)> {
    if bits.len() < 2 { return None; }
    multiplication_mantissas_with_stats(bits, max_scale, &scan(bits))
}

fn encode_decimal_planes(header: u8, mantissas: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + mantissas.len() * 8);
    out.push(DECIMAL);
    out.push(header | BYTE_PLANES);
    for plane in 0..8 {
        for &value in mantissas {
            out.push(((value as u64) >> (8 * plane)) as u8);
        }
    }
    out
}

/// Lazily encoded candidates worth zstd-measuring for this float column, each a full scheme byte +
/// payload. `bits` are the raw `f64::to_bits` patterns; `decimal` also offers the `DECIMAL`
/// scheme, once per integer scheme of its mantissas, after the others. The caller compresses each and keeps the
/// smallest.
#[must_use]
#[allow(dead_code)] // Retained for baseline research examples and codec regression tests.
pub fn candidates(bits: &[u64], decimal: bool) -> impl Iterator<Item = Vec<u8>> + '_ {
    let stats = (!bits.is_empty()).then(|| Arc::new(scan(bits)));
    legacy_candidates(bits, decimal, EncodingEffort::Balanced, stats)
}

fn legacy_candidates(
    bits: &[u64], decimal: bool, effort: EncodingEffort, stats: Option<Arc<Stats>>,
) -> impl Iterator<Item = Vec<u8>> + '_ {
    let decimal = decimal
        && stats.as_ref().is_some_and(|stats| stats.cardinality() != Some(1) && stats.decimal_possible)
        && bits.len() >= 2;
    let fixed_stats = stats.clone();
    let fixed = (0..5).filter_map(move |scheme| {
        let Some(stats) = &fixed_stats else {
            return (scheme == 0).then(|| vec![PLAIN]);
        };
        if stats.cardinality() == Some(1) {
            return (scheme == 0).then(|| encode_constant(bits[0]));
        }
        match scheme {
            0 => Some(encode_plain(bits)),
            1 => Some(encode_byte_stream_split(bits)),
            2 => stats.dictionary.as_ref()
                .filter(|dictionary| dictionary.values.len() < bits.len())
                .map(|dictionary| encode_dict(bits, dictionary)),
            3 if stats.runs * 2 < bits.len() => Some(encode_rle(bits, stats.runs)),
            4 => stats.dictionary.as_ref()
                .filter(|dictionary| dictionary.values.len() < bits.len())
                .map(|dictionary| encode_sorted_dict(bits, dictionary)),
            _ => None,
        }
    });
    // Searched for only once the fixed schemes have been measured.
    let decimals = std::iter::once(())
        .filter(move |()| decimal)
        .filter_map(move |()| decimal_mantissas_with_stats(bits, stats.as_ref().expect("nonempty decimal column")))
        .flat_map(move |(header, mantissas)| {
            crate::adaptive_int::candidates_with_effort(&mantissas, effort)
                .map(|packed| encode_decimal(header, packed))
                .collect::<Vec<_>>()
        });
    fixed.chain(decimals)
}

/// Adaptive candidates controlled by a shared integer/float effort setting. Fast retains the
/// established float codecs. Balanced adds sample-gated exact multiplication through scale 6
/// with packed deltas or byte planes. Exhaustive also searches scales through 15, measures all
/// integer schemes for the mantissas, and offers the legacy division/float32 modes.
/// The caller measures final compressed size and retains the existing codec whenever it wins.
#[must_use]
pub fn candidates_with_effort(
    bits: &[u64], effort: EncodingEffort,
) -> impl Iterator<Item = Vec<u8>> + '_ {
    let stats = (!bits.is_empty()).then(|| Arc::new(scan(bits)));
    let base = legacy_candidates(bits, effort == EncodingEffort::Exhaustive, effort, stats.clone());
    let multiplication = std::iter::once(())
        .filter(move |()| effort != EncodingEffort::Fast)
        .filter_map(move |()| {
            let stats = stats.as_ref()?;
            if stats.cardinality() == Some(1) { return None; }
            multiplication_mantissas_with_stats(bits, if effort == EncodingEffort::Balanced { 6 } else { 15 }, stats)
        })
        .flat_map(move |(header, mantissas)| {
            let mut streams: Vec<_> = if effort == EncodingEffort::Balanced {
                crate::adaptive_int::packed_delta_candidate(&mantissas)
                    .map(|packed| encode_decimal(header, packed))
                    .into_iter().collect()
            } else {
                crate::adaptive_int::candidates_with_effort(&mantissas, effort)
                    .map(|packed| encode_decimal(header, packed))
                    .collect()
            };
            streams.push(encode_decimal_planes(header, &mantissas));
            streams
        });
    base.chain(multiplication)
}

/// Whether `candidate` is a `DECIMAL` stream. Multiplication and byte planes require readers
/// newer than the original version 0.11 decimal implementation.
#[must_use]
pub fn is_decimal(candidate: &[u8]) -> bool {
    candidate.first() == Some(&DECIMAL)
}

/// The values of a `DECIMAL` payload: its header, then its mantissas in an integer scheme.
fn decimal_values(
    rest: &[u8], row_count: usize,
) -> Result<impl Iterator<Item = u64> + '_, AdaptiveDecodeError> {
    let (&header, packed) = rest.split_first().ok_or(AdaptiveDecodeError::Truncated)?;
    if header & 0x10 != 0 || header & (SINGLE | MULTIPLY) == (SINGLE | MULTIPLY) {
        return Err(AdaptiveDecodeError::Malformed);
    }
    let mantissas = if header & BYTE_PLANES == 0 {
        Some(crate::adaptive_int::decode(packed, row_count)?)
    } else {
        let size = row_count.checked_mul(8).ok_or(AdaptiveDecodeError::Malformed)?;
        if packed.len() < size { return Err(AdaptiveDecodeError::Truncated); }
        None
    };
    Ok((0..row_count).map(move |row| {
        let mantissa = match &mantissas {
            Some(values) => values[row],
            None => (0..8).fold(0i64, |value, plane| {
                value | (i64::from(packed[plane * row_count + row]) << (8 * plane))
            }),
        };
        decimal_value(mantissa, header)
    }))
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
        DECIMAL => Ok(decimal_values(rest, row_count)?.collect()),
        other => Err(AdaptiveDecodeError::UnknownScheme(other)),
    }
}

/// Appends bit-exact canonical bytes without an intermediate vector of float bits.
/// The destination must be discarded on error.
pub(crate) fn decode_to_le(
    bytes: &[u8], row_count: usize, out: &mut Vec<u8>,
) -> Result<(), AdaptiveDecodeError> {
    let (&scheme, rest) = bytes.split_first().ok_or(AdaptiveDecodeError::Truncated)?;
    let size = row_count.checked_mul(8).ok_or(AdaptiveDecodeError::Malformed)?;
    match scheme {
        PLAIN => out.extend_from_slice(rest.get(..size).ok_or(AdaptiveDecodeError::Truncated)?),
        BYTE_STREAM_SPLIT => {
            if rest.len() < size { return Err(AdaptiveDecodeError::Truncated); }
            let start = out.len();
            out.resize(
                start
                    .checked_add(size)
                    .ok_or(AdaptiveDecodeError::Malformed)?,
                0,
            );
            for plane in 0..8 {
                for row in 0..row_count {
                    out[start + row * 8 + plane] = rest[plane * row_count + row];
                }
            }
        }
        CONSTANT => {
            let value = read_u64(rest, 0)?.to_le_bytes();
            out.reserve(size);
            for _ in 0..row_count { out.extend_from_slice(&value); }
        }
        DICT => crate::adaptive_int::decode_dictionary_to_le(rest, row_count, out)?,
        RLE => crate::adaptive_int::decode_runs_to_le(rest, row_count, out)?,
        DECIMAL => {
            let values = decimal_values(rest, row_count)?;
            out.reserve(size);
            for value in values { out.extend_from_slice(&value.to_le_bytes()); }
        }
        other => return Err(AdaptiveDecodeError::UnknownScheme(other)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn lazy_candidates_keep_wire_order() {
        let values: Vec<_> = [1, 3, 2]
            .into_iter()
            .flat_map(|value| [value; 16])
            .collect();
        let mut candidates = super::candidates(&values, true);
        for scheme in [
            super::PLAIN,
            super::BYTE_STREAM_SPLIT,
            super::DICT,
            super::RLE,
            super::DICT,
        ] {
            let candidate = candidates.next().expect("candidate in historical order");
            assert_eq!(scheme, candidate[0]);
            assert_eq!(values, super::decode(&candidate, values.len()).unwrap());
        }
        assert_eq!(None, candidates.next());
        assert_eq!(None, candidates.next());
    }
    use super::*;

    fn round_trip(bits: &[u64]) {
        for candidate in candidates(bits, true) {
            let decoded = decode(&candidate, bits.len())
                .unwrap_or_else(|e| panic!("decode failed ({e}) for scheme {}", candidate[0]));
            assert_eq!(decoded, bits, "scheme {} mismatch", candidate[0]);
            let mut bytes = vec![0xab; 3];
            decode_to_le(&candidate, bits.len(), &mut bytes).unwrap();
            let expected: Vec<u8> = bits.iter().flat_map(|v| v.to_le_bytes()).collect();
            assert_eq!(&bytes[..3], &[0xab; 3]);
            assert_eq!(&bytes[3..], expected);
            for end in (0..candidate.len()).filter(|&i| i < 32 || i + 1 == candidate.len() || i % 997 == 0) {
                let old = decode(&candidate[..end], bits.len());
                let new = decode_to_le(&candidate[..end], bits.len(), &mut Vec::new());
                assert_eq!(old.map(|_| ()), new, "truncation {end}, scheme {}", candidate[0]);
            }
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
        let cands: Vec<_> = candidates(&bits, true).collect();
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
    fn sorted_dictionaries_order_values_and_align_indices() {
        let values = [2.5_f64, -1.0, 7.25, -1.0, 2.5, 100.0, -0.0, 0.0];
        let bits: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
        let stream = encode_sorted_dict(&bits, scan(&bits).dictionary.as_ref().unwrap());
        assert_eq!(bits, decode(&stream, bits.len()).unwrap());
        let table: Vec<f64> = stream[5..5 + 6 * 8]
            .chunks_exact(8)
            .map(|c| f64::from_bits(u64::from_le_bytes(c.try_into().unwrap())))
            .collect();
        assert_eq!(vec![-1.0, -0.0, 0.0, 2.5, 7.25, 100.0], table);
        assert!(table[1].is_sign_negative());
        assert_eq!(8, stream[5 + 6 * 8]);
        assert_eq!(&[3, 0, 4, 0, 3, 5, 1, 2], &stream[5 + 6 * 8 + 1..]);
        // More than 256 distinct values take 16-bit indices.
        let many: Vec<u64> = (0..1000).map(|i| f64::from(999 - i % 300).to_bits()).collect();
        let stream = encode_sorted_dict(&many, scan(&many).dictionary.as_ref().unwrap());
        assert_eq!(16, stream[5 + 300 * 8]);
        assert_eq!(many, decode(&stream, many.len()).unwrap());
        let mut le = Vec::new();
        decode_to_le(&stream, many.len(), &mut le).unwrap();
        assert_eq!(many.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(), le);
    }

    #[test]
    fn decimal_columns_use_the_smallest_exact_scale() {
        let parsed = |texts: &[&str]| -> Vec<u64> { texts.iter().map(|t| t.parse::<f64>().unwrap().to_bits()).collect() };
        // Doubles parsed from decimal text: the scale is the most fractional digits.
        let bits = parsed(&["20.5", "-3.25", "1040", "0.125", "1e-3"]);
        assert_eq!(Some((3, vec![20_500, -3_250, 1_040_000, 125, 1])), decimal_mantissas(&bits));
        // Widened float32 values take their float's shortest decimal, not the widened digits.
        let widened: Vec<u64> = [1035.3_f32, 0.1, -7.75].iter().map(|&v| f64::from(v).to_bits()).collect();
        assert_eq!(Some((SINGLE | 2, vec![103_530, 10, -775])), decimal_mantissas(&widened));
        // Integral values need no scale.
        assert_eq!(Some((0, vec![3, -4])), decimal_mantissas(&parsed(&["3", "-4"])));
        // Values that no scale reproduces rule the scheme out.
        for odd in [&["1.5", "-0"][..], &["1.5", "0.30000000000000004"], &["1.5", "1e300"]] {
            assert_eq!(None, decimal_mantissas(&parsed(odd)), "{odd:?}");
        }
        assert_eq!(None, decimal_mantissas(&[1.5_f64.to_bits(), f64::NAN.to_bits()]));
        assert_eq!(None, decimal_mantissas(&[1.5_f64.to_bits(), f64::INFINITY.to_bits()]));
    }

    #[test]
    fn decimal_candidates_round_trip_and_are_offered_last() {
        let readings: Vec<f64> = (0..4000).map(|i| f64::from(i % 997) * 0.01 - 3.0).collect();
        let parsed: Vec<u64> = readings.iter().map(|v| format!("{v:.2}").parse::<f64>().unwrap().to_bits()).collect();
        let widened: Vec<u64> = readings.iter().map(|&v| f64::from(v as f32).to_bits()).collect();
        for bits in [&parsed, &widened] {
            round_trip(bits);
            let schemes: Vec<u8> = candidates(bits, true).map(|c| c[0]).collect();
            assert_eq!(&[PLAIN, BYTE_STREAM_SPLIT, DICT, DICT], &schemes[..4]);
            assert!(schemes.len() > 4 && schemes[4..].iter().all(|&s| s == DECIMAL), "{schemes:?}");
            assert!(candidates(bits, false).all(|c| c[0] != DECIMAL));
        }
        // A column of one value stays constant.
        assert_eq!(vec![CONSTANT], candidates(&[parsed[0]; 9], true).map(|c| c[0]).collect::<Vec<_>>());
    }

    #[test]
    fn corrupt_headers_are_rejected_not_panicked() {
        // A DECIMAL stream with a scale beyond 10^15.
        assert_eq!(decode(&[DECIMAL, 16, 0, 0], 1), Err(AdaptiveDecodeError::Malformed));
        assert_eq!(decode(&[DECIMAL], 1), Err(AdaptiveDecodeError::Truncated));
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

    fn round_trip_effort(bits: &[u64], effort: EncodingEffort) -> Vec<Vec<u8>> {
        let candidates: Vec<_> = candidates_with_effort(bits, effort).collect();
        assert!(!candidates.is_empty());
        let expected: Vec<_> = bits.iter().flat_map(|value| value.to_le_bytes()).collect();
        for candidate in &candidates {
            assert_eq!(decode(candidate, bits.len()).unwrap(), bits, "scheme {:?}", &candidate[..candidate.len().min(2)]);
            let mut le = vec![0xa5; 3];
            decode_to_le(candidate, bits.len(), &mut le).unwrap();
            assert_eq!(&le[..3], &[0xa5; 3]);
            assert_eq!(&le[3..], &expected);
            for end in (0..candidate.len()).filter(|&i| i < 32 || i + 1 == candidate.len() || i % 997 == 0) {
                assert_eq!(decode(&candidate[..end], bits.len()).map(|_| ()),
                    decode_to_le(&candidate[..end], bits.len(), &mut Vec::new()),
                    "truncation {end}, header {:?}", &candidate[..candidate.len().min(2)]);
            }
        }
        candidates
    }

    #[test]
    fn multiplication_uses_original_reconstruction_arithmetic() {
        // Multiplying by the rounded reciprocal reproduces DBC arithmetic exactly. Dividing
        // the same integer by the power of ten would silently change the lowest mantissa bit.
        let values: Vec<_> = [-37i64, 0, 35, 79].into_iter()
            .map(|value| (value as f64 * 0.01).to_bits()).collect();
        assert_ne!((35.0_f64 / 100.0).to_bits(), values[2]);
        assert_eq!(Some((MULTIPLY | 2, vec![-37, 0, 35, 79])), multiplication_mantissas(&values, 6));
        let encoded = round_trip_effort(&values, EncodingEffort::Balanced);
        assert!(encoded.iter().any(|bytes| bytes[..2] == [DECIMAL, MULTIPLY | 2]));
        assert!(encoded.iter().any(|bytes| bytes[..2] == [DECIMAL, MULTIPLY | BYTE_PLANES | 2]));
        assert!(candidates_with_effort(&values, EncodingEffort::Fast).all(|bytes| bytes[0] != DECIMAL));
    }

    #[test]
    fn multiplication_effort_gates_rare_scales_and_preserves_legacy_candidates() {
        let values: Vec<_> = [123_456_789i64, 123_456_791, -123_456_789].into_iter()
            .map(|value| (value as f64 * 1e-8).to_bits()).collect();
        assert!(multiplication_mantissas(&values, 6).is_none());
        assert_eq!(Some((MULTIPLY | 8, vec![123_456_789, 123_456_791, -123_456_789])), multiplication_mantissas(&values, 15));
        assert!(round_trip_effort(&values, EncodingEffort::Balanced).iter().all(|bytes| bytes[0] != DECIMAL));
        assert!(round_trip_effort(&values, EncodingEffort::Exhaustive).iter()
            .any(|bytes| bytes[0] == DECIMAL && bytes[1] & MULTIPLY != 0));
        assert_eq!(candidates(&values, false).collect::<Vec<_>>(),
            candidates_with_effort(&values, EncodingEffort::Fast).collect::<Vec<_>>());
        assert!(candidates(&values, true).all(|bytes| bytes[0] != DECIMAL || bytes[1] & (MULTIPLY | BYTE_PLANES) == 0));
    }

    #[test]
    fn multiplication_rejects_special_values_overflow_and_unsampled_exceptions() {
        for special in [(-0.0_f64).to_bits(), f64::INFINITY.to_bits(), f64::NEG_INFINITY.to_bits(),
            0x7ff0_0000_0000_0001, 0xfff8_dead_beef_0000, f64::MAX.to_bits(), MAX_MANTISSA.to_bits(),
            (-MAX_MANTISSA).to_bits(), 1u64] {
            let mut values: Vec<_> = (0..1024).map(|value| (f64::from(value) * 0.01).to_bits()).collect();
            // Position 1 is absent from the 256 evenly spread samples: a passing sample must
            // never replace the mandatory full-column exact reconstruction check.
            values[1] = special;
            assert!(multiplication_mantissas(&values, 15).is_none(), "bits {special:x}");
            for effort in [EncodingEffort::Fast, EncodingEffort::Balanced, EncodingEffort::Exhaustive] {
                assert!(round_trip_effort(&values, effort).iter()
                    .all(|bytes| bytes[0] != DECIMAL || bytes[1] & MULTIPLY == 0));
            }
        }
    }

    #[test]
    fn multiplication_empty_constant_and_planes_validate_headers() {
        for effort in [EncodingEffort::Fast, EncodingEffort::Balanced, EncodingEffort::Exhaustive] {
            assert_eq!(round_trip_effort(&[], effort), vec![vec![PLAIN]]);
            for value in [(-0.0_f64).to_bits(), 0x7ff0_0000_0000_0001, 1.25_f64.to_bits()] {
                let candidates = round_trip_effort(&[value; 8], effort);
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0][0], CONSTANT);
            }
        }
        for header in [0x10, MULTIPLY | 0x10, BYTE_PLANES | 0x10, SINGLE | MULTIPLY, SINGLE | MULTIPLY | BYTE_PLANES] {
            assert_eq!(decode(&[DECIMAL, header], 0), Err(AdaptiveDecodeError::Malformed));
            assert_eq!(decode_to_le(&[DECIMAL, header], 0, &mut Vec::new()), Err(AdaptiveDecodeError::Malformed));
        }
        assert_eq!(decode(&[DECIMAL, MULTIPLY | BYTE_PLANES], 1), Err(AdaptiveDecodeError::Truncated));
        assert_eq!(decode(&[DECIMAL, MULTIPLY | BYTE_PLANES], usize::MAX), Err(AdaptiveDecodeError::Malformed));
    }

    #[test]
    fn multiplication_packed_deltas_cross_restart_blocks() {
        let bits: Vec<_> = (0..65_545).map(|value| ((value as f64 - 32_000.0) * 0.01).to_bits()).collect();
        let candidates: Vec<_> = candidates_with_effort(&bits, EncodingEffort::Balanced)
            .filter(|candidate| candidate[0] == DECIMAL).collect();
        assert_eq!(candidates.len(), 2);
        for candidate in candidates {
            assert_eq!(decode(&candidate, bits.len()).unwrap(), bits);
            let mut bytes = Vec::new();
            decode_to_le(&candidate, bits.len(), &mut bytes).unwrap();
            assert_eq!(bytes, bits.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>());
        }
        // The exact-integer boundary is inclusive up to 2^53-1, and sign bits in byte planes
        // must reconstruct the signed mantissa rather than an unsigned conversion to f64.
        let extremes = [(-(MAX_MANTISSA - 1.0)).to_bits(), (MAX_MANTISSA - 1.0).to_bits()];
        assert_eq!(Some((MULTIPLY, vec![-9_007_199_254_740_991, 9_007_199_254_740_991])), multiplication_mantissas(&extremes, 6));
        round_trip_effort(&extremes, EncodingEffort::Balanced);
    }

    #[test]
    fn reused_dictionary_preserves_first_appearance_and_repeated_codes() {
        let palette = [3.25_f64.to_bits(), (-0.0_f64).to_bits(), 0.0_f64.to_bits(),
            0x7ff0_0000_0000_0001, 0xfff8_dead_beef_0000];
        let values: Vec<_> = [0usize, 0, 1, 2, 1, 3, 4, 0].into_iter()
            .flat_map(|index| [palette[index]; 129]).collect();
        let stats = scan(&values);
        let dictionary = stats.dictionary.as_ref().unwrap();
        assert_eq!(dictionary.values, palette);
        assert_eq!(stats.cardinality(), Some(palette.len()));
        assert_eq!(stats.runs, 7);
        assert!(!stats.decimal_possible);
        assert_eq!(dictionary_indices(&values, dictionary).collect::<Vec<_>>(),
            [0usize, 0, 1, 2, 1, 3, 4, 0].into_iter().flat_map(|index| [index; 129]).collect::<Vec<_>>());
        for encoded in [encode_dict(&values, dictionary), encode_sorted_dict(&values, dictionary)] {
            assert_eq!(decode(&encoded, values.len()).unwrap(), values);
            assert_eq!(u32::from_le_bytes(encoded[1..5].try_into().unwrap()) as usize, palette.len());
        }
        // IEEE-pattern equality retains both zeros and both NaN payloads as separate keys.
        assert_eq!(dictionary.index_of.len(), 5);
    }

    #[test]
    fn bounded_dictionary_and_high_cardinality_decimal_fallback() {
        let mut values: Vec<_> = (0..MAX_DICT_CARDINALITY).map(|value| (value as f64).to_bits()).collect();
        let capped = scan(&values);
        assert_eq!(capped.cardinality(), Some(MAX_DICT_CARDINALITY));
        assert_eq!(capped.dictionary.as_ref().unwrap().index_of.len(), MAX_DICT_CARDINALITY);
        values.push((MAX_DICT_CARDINALITY as f64).to_bits());
        values.push((MAX_DICT_CARDINALITY as f64 + 1.0).to_bits());
        let overflow = scan(&values);
        assert!(overflow.dictionary.is_none());
        assert!(overflow.decimal_possible);
        let (header, integers) = multiplication_mantissas_with_stats(&values, 6, &overflow).unwrap();
        assert_eq!(header, MULTIPLY);
        assert_eq!(integers, (0..values.len() as i64).collect::<Vec<_>>());

        // This finite pattern passes the cheap flags but no decimal scale. It is absent from
        // the 256 samples, after the dictionary has exceeded its cap, requiring full-row proof.
        values[MAX_DICT_CARDINALITY] = f64::MIN_POSITIVE.to_bits();
        let overflow = scan(&values);
        assert!(overflow.dictionary.is_none());
        assert!(overflow.decimal_possible);
        assert!(multiplication_mantissas_with_stats(&values, 15, &overflow).is_none());
        values.push(f64::NAN.to_bits());
        assert!(!scan(&values).decimal_possible, "eligibility checks continue after dropping the dictionary");
    }

    #[test]
    fn distinct_decimal_proof_checks_unsampled_values_and_preserves_mantissas() {
        let source = [35.0_f64 * 0.01, -37.0_f64 * 0.01, 1.25, 0.0];
        let mut values: Vec<_> = (0..4097).map(|index| source[index % source.len()].to_bits()).collect();
        let stats = scan(&values);
        assert_eq!(stats.cardinality(), Some(4));
        let (header, integers) = multiplication_mantissas_with_stats(&values, 6, &stats).unwrap();
        assert_eq!(header, MULTIPLY | 2);
        assert_eq!(integers, (0..values.len()).map(|index| [35, -37, 125, 0][index % 4]).collect::<Vec<_>>());
        assert_eq!(verified_mantissas(&values, header, &stats),
            values.iter().map(|&value| mantissa(value, header)).collect::<Option<Vec<_>>>());

        // Index 1 is not a sample. The complete distinct table must still reject it before
        // generating any unchecked row mantissas; finite/min-max checks alone cannot do so.
        values[1] = f64::MIN_POSITIVE.to_bits();
        let stats = scan(&values);
        assert_eq!(stats.cardinality(), Some(5));
        assert!(stats.decimal_possible);
        assert!(multiplication_mantissas_with_stats(&values, 15, &stats).is_none());
        assert!(candidates_with_effort(&values, EncodingEffort::Balanced).all(|bytes| bytes[0] != DECIMAL));
    }
}
