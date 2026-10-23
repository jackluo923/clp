//! Experimental, lossless float transforms for the public CAN benchmark.
//!
//! These are custom trials, not canonical ALP or Gorilla implementations. Every candidate has
//! a self-contained header and a restart directory at the existing 65,536-row block boundary.
//! The benchmark applies Zstd to the complete result, including metadata and exceptions.

use crate::adaptive_int::{bit_width_for, pack_bits};

const ROWS: usize = 65_536;
const MAGIC: &[u8; 4] = b"FCT1";
const HEADER: usize = 21;
const POWERS: [f64; 16] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
];
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    DecimalFor = 0,
    DecimalDelta = 1,
    DecimalByteSplit = 2,
    Xor = 3,
    XorByteSplit = 4,
    Gorilla = 5,
    Float32ByteSplit = 6,
    DecimalMultiplyFor = 7,
    DecimalMultiplyDelta = 8,
    DecimalMultiplyByteSplit = 9,
}

impl Kind {
    pub fn all() -> &'static [Self] {
        &[
            Self::DecimalFor,
            Self::DecimalDelta,
            Self::DecimalByteSplit,
            Self::Xor,
            Self::XorByteSplit,
            Self::Gorilla,
            Self::Float32ByteSplit,
            Self::DecimalMultiplyFor,
            Self::DecimalMultiplyDelta,
            Self::DecimalMultiplyByteSplit,
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::DecimalFor => "trial_decimal_for_exceptions",
            Self::DecimalDelta => "trial_decimal_delta_exceptions",
            Self::DecimalByteSplit => "trial_decimal_byte_split_exceptions",
            Self::Xor => "trial_xor",
            Self::XorByteSplit => "trial_xor_byte_split",
            Self::Gorilla => "trial_gorilla",
            Self::Float32ByteSplit => "trial_float32_byte_split_exceptions",
            Self::DecimalMultiplyFor => "trial_decimal_multiply_for_exceptions",
            Self::DecimalMultiplyDelta => "trial_decimal_multiply_delta_exceptions",
            Self::DecimalMultiplyByteSplit => "trial_decimal_multiply_byte_split_exceptions",
        }
    }

    fn from_tag(tag: u8) -> Result<Self, String> {
        Self::all()
            .iter()
            .copied()
            .find(|kind| *kind as u8 == tag)
            .ok_or_else(|| format!("unknown trial codec {tag}"))
    }

    fn is_decimal(self) -> bool {
        matches!(
            self,
            Self::DecimalFor
                | Self::DecimalDelta
                | Self::DecimalByteSplit
                | Self::DecimalMultiplyFor
                | Self::DecimalMultiplyDelta
                | Self::DecimalMultiplyByteSplit
        )
    }

    fn uses_decimal_multiplication(self) -> bool {
        matches!(
            self,
            Self::DecimalMultiplyFor | Self::DecimalMultiplyDelta | Self::DecimalMultiplyByteSplit
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub rows: u64,
    pub blocks: u64,
    pub exceptions: u64,
    pub scale_blocks: [u64; 16],
}

/// Byte layout: magic(4), kind(1), rows(8), restart rows(4), blocks(4), absolute
/// payload offsets(8 each, including a final end offset), then independent blocks.
pub fn encode(bits: &[u64], kind: Kind) -> Vec<u8> {
    let blocks = bits.len().div_ceil(ROWS);
    let mut out = Vec::with_capacity(HEADER + (blocks + 1) * 8 + bits.len() * 8);
    out.extend_from_slice(MAGIC);
    out.push(kind as u8);
    out.extend_from_slice(&(bits.len() as u64).to_le_bytes());
    out.extend_from_slice(&(ROWS as u32).to_le_bytes());
    out.extend_from_slice(&(blocks as u32).to_le_bytes());
    out.resize(HEADER + (blocks + 1) * 8, 0);
    for (index, block) in bits.chunks(ROWS).enumerate() {
        let offset = out.len() as u64;
        out[HEADER + index * 8..HEADER + (index + 1) * 8].copy_from_slice(&offset.to_le_bytes());
        match kind {
            Kind::DecimalFor
            | Kind::DecimalDelta
            | Kind::DecimalByteSplit
            | Kind::DecimalMultiplyFor
            | Kind::DecimalMultiplyDelta
            | Kind::DecimalMultiplyByteSplit => {
                encode_decimal(block, kind, &mut out);
            }
            Kind::Xor | Kind::XorByteSplit => encode_xor(block, kind, &mut out),
            Kind::Gorilla => encode_gorilla(block, &mut out),
            Kind::Float32ByteSplit => encode_float32(block, &mut out),
        }
    }
    let end = out.len() as u64;
    out[HEADER + blocks * 8..HEADER + (blocks + 1) * 8].copy_from_slice(&end.to_le_bytes());
    out
}

struct Frame<'a> {
    bytes: &'a [u8],
    kind: Kind,
    rows: usize,
    blocks: usize,
}

impl<'a> Frame<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.get(..4) != Some(MAGIC.as_slice()) {
            return Err("invalid trial codec magic".into());
        }
        let kind = Kind::from_tag(*bytes.get(4).ok_or("truncated kind")?)?;
        let rows = usize::try_from(u64_at(bytes, 5)?).map_err(|_| "too many rows")?;
        if u32_at(bytes, 13)? as usize != ROWS {
            return Err("invalid restart boundary".into());
        }
        let blocks = u32_at(bytes, 17)? as usize;
        if blocks != rows.div_ceil(ROWS) {
            return Err("inconsistent block count".into());
        }
        let directory_end = HEADER
            .checked_add((blocks + 1).checked_mul(8).ok_or("directory overflow")?)
            .ok_or("directory overflow")?;
        if directory_end > bytes.len() || u64_at(bytes, HEADER)? != directory_end as u64 {
            return Err("invalid first block offset".into());
        }
        let mut previous = directory_end as u64;
        for block in 1..=blocks {
            let end = u64_at(bytes, HEADER + block * 8)?;
            if end <= previous || end > bytes.len() as u64 {
                return Err("invalid block offset".into());
            }
            previous = end;
        }
        if previous != bytes.len() as u64 {
            return Err("unexpected trailing data".into());
        }
        Ok(Self {
            bytes,
            kind,
            rows,
            blocks,
        })
    }

    fn block(&self, index: usize) -> (&'a [u8], usize) {
        let start = u64_at(self.bytes, HEADER + index * 8).unwrap() as usize;
        let end = u64_at(self.bytes, HEADER + (index + 1) * 8).unwrap() as usize;
        (
            &self.bytes[start..end],
            (self.rows - index * ROWS).min(ROWS),
        )
    }
}

pub fn decode(bytes: &[u8]) -> Result<Vec<u64>, String> {
    let frame = Frame::new(bytes)?;
    let mut out = Vec::with_capacity(frame.rows);
    for index in 0..frame.blocks {
        let (block, rows) = frame.block(index);
        match frame.kind {
            Kind::DecimalFor
            | Kind::DecimalDelta
            | Kind::DecimalByteSplit
            | Kind::DecimalMultiplyFor
            | Kind::DecimalMultiplyDelta
            | Kind::DecimalMultiplyByteSplit => {
                decode_decimal(block, rows, frame.kind, &mut out)?;
            }
            Kind::Xor | Kind::XorByteSplit => decode_xor(block, rows, frame.kind, &mut out)?,
            Kind::Gorilla => decode_gorilla(block, rows, &mut out)?,
            Kind::Float32ByteSplit => decode_float32(block, rows, &mut out)?,
        }
    }
    Ok(out)
}

/// Read counts without reconstructing values. This checks framing and each diagnostic header;
/// the benchmark must still call `decode` and compare all original bits for full validation.
pub fn diagnostics(bytes: &[u8]) -> Result<Diagnostics, String> {
    let frame = Frame::new(bytes)?;
    let mut result = Diagnostics {
        rows: frame.rows as u64,
        blocks: frame.blocks as u64,
        ..Diagnostics::default()
    };
    for index in 0..frame.blocks {
        let (block, rows) = frame.block(index);
        let exceptions = if frame.kind.is_decimal() {
            let scale = *block.first().ok_or("missing decimal scale")? as usize;
            if scale >= POWERS.len() {
                return Err("invalid decimal scale".into());
            }
            result.scale_blocks[scale] += 1;
            u32_at(block, 5)? as usize
        } else if frame.kind == Kind::Float32ByteSplit {
            u32_at(block, 0)? as usize
        } else {
            0
        };
        if exceptions > rows {
            return Err("too many exceptions".into());
        }
        result.exceptions += exceptions as u64;
    }
    Ok(result)
}

fn mantissa(bits: u64, scale: usize, multiply: bool) -> Option<i64> {
    let value = f64::from_bits(bits);
    let scaled = (value * POWERS[scale]).round();
    if !scaled.is_finite() || scaled.abs() >= MAX_EXACT_INTEGER {
        return None;
    }
    let integer = scaled as i64;
    let restored = if multiply {
        (integer as f64) * (1.0 / POWERS[scale])
    } else {
        (integer as f64) / POWERS[scale]
    };
    (restored.to_bits() == bits).then_some(integer)
}

/// Score up to 256 evenly spread samples per scale by packed mantissa span and 10-byte
/// exceptions. This is deliberately inexpensive and is not ALP's exponent-selection algorithm.
/// The full block is checked after selection, so sampling affects size only, never correctness.
fn choose_scale(bits: &[u64], multiply: bool) -> usize {
    let samples = bits.len().min(256);
    let mut best = (usize::MAX, 0);
    for scale in 0..POWERS.len() {
        let (mut lo, mut hi, mut failures, mut matches) = (i64::MAX, i64::MIN, 0, 0);
        for index in 0..samples {
            let pos = if samples == 1 {
                0
            } else {
                index * (bits.len() - 1) / (samples - 1)
            };
            if let Some(value) = mantissa(bits[pos], scale, multiply) {
                lo = lo.min(value);
                hi = hi.max(value);
                matches += 1;
            } else {
                failures += 1;
            }
        }
        let width = if matches == 0 {
            0
        } else {
            bit_width_for((hi - lo) as u64) as usize
        };
        let score = samples * width + failures * 80;
        if score < best.0 {
            best = (score, scale);
        }
    }
    best.1
}

fn encode_decimal(bits: &[u64], kind: Kind, out: &mut Vec<u8>) {
    let multiply = kind.uses_decimal_multiplication();
    let scale = choose_scale(bits, multiply);
    let mut integers = Vec::with_capacity(bits.len());
    let mut exceptions = Vec::new();
    let mut previous = 0;
    for (position, &value) in bits.iter().enumerate() {
        match mantissa(value, scale, multiply) {
            Some(integer) => previous = integer,
            None => exceptions.push((position as u16, value)),
        }
        integers.push(previous);
    }
    let encoded = match kind {
        Kind::DecimalFor | Kind::DecimalMultiplyFor => encode_for(&integers),
        Kind::DecimalDelta | Kind::DecimalMultiplyDelta => encode_delta(&integers),
        Kind::DecimalByteSplit | Kind::DecimalMultiplyByteSplit => {
            let mut payload = Vec::with_capacity(integers.len() * 8);
            byte_split_u64(
                &integers.iter().map(|&v| v as u64).collect::<Vec<_>>(),
                &mut payload,
            );
            payload
        }
        _ => unreachable!(),
    };
    out.push(scale as u8);
    out.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    out.extend_from_slice(&(exceptions.len() as u32).to_le_bytes());
    out.extend_from_slice(&encoded);
    append_exceptions(&exceptions, out);
}

fn encode_for(values: &[i64]) -> Vec<u8> {
    let lo = *values.iter().min().unwrap();
    let hi = *values.iter().max().unwrap();
    let width = bit_width_for((hi - lo) as u64);
    let mut out = vec![1]; // Production integer FOR tag.
    out.extend_from_slice(&lo.to_le_bytes());
    out.push(width);
    pack_bits(
        &values.iter().map(|&v| (v - lo) as u64).collect::<Vec<_>>(),
        width,
        &mut out,
    );
    out
}

fn encode_delta(values: &[i64]) -> Vec<u8> {
    let deltas: Vec<i64> = values.windows(2).map(|p| p[1] - p[0]).collect();
    let lo = deltas.iter().copied().min().unwrap_or(0);
    let hi = deltas.iter().copied().max().unwrap_or(0);
    let width = bit_width_for((hi - lo) as u64);
    let mut out = vec![crate::delta_blocks::SCHEME];
    out.extend_from_slice(&lo.to_le_bytes());
    out.push(width);
    out.extend_from_slice(&values[0].to_le_bytes());
    pack_bits(
        &deltas.iter().map(|&v| (v - lo) as u64).collect::<Vec<_>>(),
        width,
        &mut out,
    );
    out
}

fn decode_decimal(bytes: &[u8], rows: usize, kind: Kind, out: &mut Vec<u64>) -> Result<(), String> {
    let scale = *bytes.first().ok_or("missing decimal scale")? as usize;
    let power = *POWERS.get(scale).ok_or("invalid decimal scale")?;
    let length = u32_at(bytes, 1)? as usize;
    let exceptions = u32_at(bytes, 5)? as usize;
    let payload = bytes
        .get(9..9 + length)
        .ok_or("truncated decimal integers")?;
    let integers = if matches!(
        kind,
        Kind::DecimalByteSplit | Kind::DecimalMultiplyByteSplit
    ) {
        unsplit_u64(payload, rows)?
            .into_iter()
            .map(|v| v as i64)
            .collect()
    } else {
        crate::adaptive_int::decode(payload, rows).map_err(|e| e.to_string())?
    };
    let start = out.len();
    if kind.uses_decimal_multiplication() {
        // Keep this operation distinct from division: the rounded reciprocal and product can
        // reproduce source-generated `raw * scale` bits that division cannot reproduce.
        let reciprocal = 1.0 / power;
        out.extend(
            integers
                .into_iter()
                .map(|v| ((v as f64) * reciprocal).to_bits()),
        );
    } else {
        out.extend(integers.into_iter().map(|v| ((v as f64) / power).to_bits()));
    }
    apply_exceptions(&bytes[9 + length..], exceptions, &mut out[start..])
}

fn encode_xor(bits: &[u64], kind: Kind, out: &mut Vec<u8>) {
    let mut previous = 0;
    let xors: Vec<u64> = bits
        .iter()
        .map(|&value| {
            let xor = previous ^ value;
            previous = value;
            xor
        })
        .collect();
    if kind == Kind::XorByteSplit {
        byte_split_u64(&xors, out);
    } else {
        for value in xors {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn decode_xor(bytes: &[u8], rows: usize, kind: Kind, out: &mut Vec<u64>) -> Result<(), String> {
    if bytes.len() != rows * 8 {
        return Err("invalid XOR block length".into());
    }
    let mut previous = 0;
    if kind == Kind::XorByteSplit {
        for xor in unsplit_u64(bytes, rows)? {
            previous ^= xor;
            out.push(previous);
        }
    } else {
        for chunk in bytes.chunks_exact(8) {
            previous ^= u64::from_le_bytes(chunk.try_into().unwrap());
            out.push(previous);
        }
    }
    Ok(())
}

fn encode_float32(bits: &[u64], out: &mut Vec<u8>) {
    let mut singles = Vec::with_capacity(bits.len());
    let mut exceptions = Vec::new();
    let mut previous = 0;
    for (position, &bits) in bits.iter().enumerate() {
        let value = f64::from_bits(bits) as f32;
        if (value as f64).to_bits() == bits {
            previous = value.to_bits();
        } else {
            exceptions.push((position as u16, bits));
        }
        singles.push(previous);
    }
    out.extend_from_slice(&(exceptions.len() as u32).to_le_bytes());
    for plane in 0..4 {
        out.extend(singles.iter().map(|&value| (value >> (plane * 8)) as u8));
    }
    append_exceptions(&exceptions, out);
}

fn decode_float32(bytes: &[u8], rows: usize, out: &mut Vec<u64>) -> Result<(), String> {
    let exceptions = u32_at(bytes, 0)? as usize;
    let planes = bytes
        .get(4..4 + rows * 4)
        .ok_or("truncated float32 planes")?;
    let start = out.len();
    for row in 0..rows {
        let bits = u32::from_le_bytes([
            planes[row],
            planes[rows + row],
            planes[rows * 2 + row],
            planes[rows * 3 + row],
        ]);
        out.push((f32::from_bits(bits) as f64).to_bits());
    }
    apply_exceptions(&bytes[4 + rows * 4..], exceptions, &mut out[start..])
}

fn byte_split_u64(values: &[u64], out: &mut Vec<u8>) {
    for plane in 0..8 {
        out.extend(values.iter().map(|&value| (value >> (plane * 8)) as u8));
    }
}

fn unsplit_u64(bytes: &[u8], rows: usize) -> Result<Vec<u64>, String> {
    if bytes.len() != rows * 8 {
        return Err("invalid byte-plane length".into());
    }
    let mut values = vec![0; rows];
    for plane in 0..8 {
        for (row, value) in values.iter_mut().enumerate() {
            *value |= u64::from(bytes[plane * rows + row]) << (plane * 8);
        }
    }
    Ok(values)
}

fn append_exceptions(exceptions: &[(u16, u64)], out: &mut Vec<u8>) {
    // Positions and original bits are separate streams so Zstd sees the repeated position bytes.
    for &(position, _) in exceptions {
        out.extend_from_slice(&position.to_le_bytes());
    }
    for &(_, value) in exceptions {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn apply_exceptions(bytes: &[u8], count: usize, values: &mut [u64]) -> Result<(), String> {
    if count > values.len() || bytes.len() != count * 10 {
        return Err("invalid exception length".into());
    }
    let mut previous = None;
    for index in 0..count {
        let position =
            u16::from_le_bytes(bytes[index * 2..index * 2 + 2].try_into().unwrap()) as usize;
        if position >= values.len() || previous.is_some_and(|p| position <= p) {
            return Err("invalid exception position".into());
        }
        values[position] = u64_at(bytes, count * 2 + index * 8)?;
        previous = Some(position);
    }
    Ok(())
}

fn encode_gorilla(bits: &[u64], out: &mut Vec<u8>) {
    let mut writer = BitWriter::default();
    writer.write(bits[0], 64);
    let mut previous = bits[0];
    let mut window = None;
    for &value in &bits[1..] {
        let xor = previous ^ value;
        previous = value;
        if xor == 0 {
            writer.write(0, 1);
            continue;
        }
        writer.write(1, 1);
        let leading = xor.leading_zeros() as u8;
        let trailing = xor.trailing_zeros() as u8;
        if let Some((old_leading, old_trailing)) = window {
            if leading >= old_leading && trailing >= old_trailing {
                writer.write(0, 1);
                writer.write(xor >> old_trailing, 64 - old_leading - old_trailing);
                continue;
            }
        }
        writer.write(1, 1);
        let width = 64 - leading - trailing;
        writer.write(u64::from(leading), 6);
        writer.write(u64::from(width - 1), 6);
        writer.write(xor >> trailing, width);
        window = Some((leading, trailing));
    }
    out.extend_from_slice(&writer.finish());
}

fn decode_gorilla(bytes: &[u8], rows: usize, out: &mut Vec<u64>) -> Result<(), String> {
    let mut reader = BitReader::new(bytes);
    let mut previous = reader.read(64)?;
    out.push(previous);
    let mut window = None;
    for _ in 1..rows {
        if reader.read(1)? != 0 {
            let (leading, trailing) = if reader.read(1)? == 0 {
                window.ok_or("missing XOR window")?
            } else {
                let leading = reader.read(6)? as u8;
                let width = reader.read(6)? as u8 + 1;
                if leading + width > 64 {
                    return Err("invalid XOR window".into());
                }
                let next = (leading, 64 - leading - width);
                window = Some(next);
                next
            };
            previous ^= reader.read(64 - leading - trailing)? << trailing;
        }
        out.push(previous);
    }
    if !reader.finished() {
        return Err("invalid XOR padding or trailing data".into());
    }
    Ok(())
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    accumulator: u128,
    available: u8,
}

impl BitWriter {
    fn write(&mut self, value: u64, width: u8) {
        self.accumulator |= u128::from(value) << self.available;
        self.available += width;
        while self.available >= 8 {
            self.bytes.push(self.accumulator as u8);
            self.accumulator >>= 8;
            self.available -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.available != 0 {
            self.bytes.push(self.accumulator as u8);
        }
        self.bytes
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    position: usize,
    accumulator: u128,
    available: u8,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            accumulator: 0,
            available: 0,
        }
    }

    fn read(&mut self, width: u8) -> Result<u64, String> {
        while self.available < width {
            self.accumulator |=
                u128::from(*self.bytes.get(self.position).ok_or("truncated XOR bits")?)
                    << self.available;
            self.available += 8;
            self.position += 1;
        }
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let value = self.accumulator as u64 & mask;
        self.accumulator >>= width;
        self.available -= width;
        Ok(value)
    }

    fn finished(&self) -> bool {
        self.position == self.bytes.len() && self.available < 8 && self.accumulator == 0
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or("truncated u32")?
            .try_into()
            .unwrap(),
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or("truncated u64")?
            .try_into()
            .unwrap(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(bits: &[u64]) {
        for &kind in Kind::all() {
            let encoded = encode(bits, kind);
            assert_eq!(decode(&encoded).unwrap(), bits, "{}", kind.name());
            let diagnostics = diagnostics(&encoded).unwrap();
            assert_eq!(diagnostics.rows, bits.len() as u64);
            assert_eq!(diagnostics.blocks, bits.len().div_ceil(ROWS) as u64);
        }
    }

    #[test]
    fn empty_and_singletons() {
        verify(&[]);
        for bits in [0, 1, u64::MAX, (-0.0f64).to_bits(), f64::MAX.to_bits()] {
            verify(&[bits]);
        }
    }

    #[test]
    fn ieee_edges_preserve_every_bit() {
        verify(&[
            0,
            1,
            2,
            0x000f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x8000_0000_0000_0000,
            0x8000_0000_0000_0001,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff0_0000_0000_0001,
            0x7ff8_0000_0000_0000,
            0x7ff8_0000_0000_1234,
            0xfff8_9876_5432_1000,
            f64::MAX.to_bits(),
            f64::MIN.to_bits(),
            f64::MIN_POSITIVE.to_bits(),
            68.92999999999999f64.to_bits(),
            68.93f64.to_bits(),
        ]);
    }

    #[test]
    fn arbitrary_ieee_patterns() {
        let mut seed = 0x31fd_4789_abc0_0192u64;
        let bits: Vec<_> = (0..10_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            })
            .collect();
        verify(&bits);
    }

    #[test]
    fn decimal_fit_and_exceptions() {
        let mut bits: Vec<_> = (0..1_000).map(|v| (v as f64 / 10.0).to_bits()).collect();
        bits[3] = (-0.0f64).to_bits();
        bits[15] = 0x7ff8_0000_0000_1234;
        bits[99] = f64::INFINITY.to_bits();
        bits[333] = 68.92999999999999f64.to_bits();
        for kind in [Kind::DecimalFor, Kind::DecimalDelta, Kind::DecimalByteSplit] {
            let encoded = encode(&bits, kind);
            assert_eq!(decode(&encoded).unwrap(), bits);
            let diagnostics = diagnostics(&encoded).unwrap();
            assert_eq!(diagnostics.exceptions, 4);
            assert_eq!(diagnostics.scale_blocks[1], 1);
        }
    }

    #[test]
    fn float32_narrowing_and_exceptions() {
        let mut bits: Vec<_> = (0..1_000)
            .map(|v| ((v as f32 / 10.0) as f64).to_bits())
            .collect();
        bits.extend([0.1f64.to_bits(), (-0.0f64).to_bits(), 0x7ff8_0000_0000_1234]);
        let encoded = encode(&bits, Kind::Float32ByteSplit);
        assert_eq!(decode(&encoded).unwrap(), bits);
        assert_eq!(diagnostics(&encoded).unwrap().exceptions, 2);
    }

    #[test]
    fn decimal_multiplication_reproduces_native_source_arithmetic() {
        // Decimal division and multiplication by its rounded reciprocal are mathematically
        // equivalent, but not bit-equivalent floating point operations. Never round either.
        let source = (35.0f64 * 0.01).to_bits();
        assert_ne!(source, (35.0f64 / 100.0).to_bits());
        assert_eq!(mantissa(source, 2, false), None);
        assert_eq!(mantissa(source, 2, true), Some(35));

        let mut bits: Vec<_> = (1..=10_000)
            .map(|raw| (raw as f64 * 0.01).to_bits())
            .collect();
        bits.extend([
            (-0.0f64).to_bits(),
            0x7ff8_0000_0000_1234,
            f64::INFINITY.to_bits(),
        ]);
        for kind in [
            Kind::DecimalMultiplyFor,
            Kind::DecimalMultiplyDelta,
            Kind::DecimalMultiplyByteSplit,
        ] {
            let encoded = encode(&bits, kind);
            assert_eq!(decode(&encoded).unwrap(), bits);
            let diagnostic = diagnostics(&encoded).unwrap();
            assert_eq!(diagnostic.exceptions, 3);
            assert_eq!(diagnostic.scale_blocks[2], 1);
        }
    }

    #[test]
    fn independent_restarts_at_existing_boundary() {
        let bits: Vec<_> = (0..ROWS * 2 + 3)
            .map(|row| {
                if row == ROWS - 1 || row == ROWS {
                    u64::MAX
                } else if row > ROWS {
                    ((row % 100) as f64 / 1_000.0).to_bits()
                } else {
                    ((row % 100) as f64 / 10.0).to_bits()
                }
            })
            .collect();
        verify(&bits);
        for &kind in Kind::all() {
            let encoded = encode(&bits, kind);
            let frame = Frame::new(&encoded).unwrap();
            for index in 0..frame.blocks {
                let (block, rows) = frame.block(index);
                let source = &bits[index * ROWS..index * ROWS + rows];
                let single = encode(source, kind);
                let single_frame = Frame::new(&single).unwrap();
                assert_eq!(
                    block,
                    single_frame.block(0).0,
                    "{} block {index}",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn repeated_values_and_changing_xor_windows() {
        let mut bits = vec![1.0f64.to_bits(); 400];
        bits.extend((0..400).map(|v| 1.0f64.to_bits() ^ (1 << (v % 64))));
        bits.extend([0, u64::MAX, 0, 0, u64::MAX, 1]);
        verify(&bits);
    }

    #[test]
    fn malformed_streams_are_rejected() {
        let bits = [0.1f64.to_bits(), 0.2f64.to_bits(), (-0.0f64).to_bits()];
        for &kind in Kind::all() {
            let encoded = encode(&bits, kind);
            for end in 0..encoded.len() {
                assert!(
                    decode(&encoded[..end]).is_err(),
                    "{} truncation {end}",
                    kind.name()
                );
            }
            let mut wrong = encoded.clone();
            wrong.push(0);
            assert!(decode(&wrong).is_err());
            wrong = encoded.clone();
            wrong[4] = 255;
            assert!(decode(&wrong).is_err());
            wrong = encoded;
            wrong[HEADER..HEADER + 8].copy_from_slice(&0u64.to_le_bytes());
            assert!(decode(&wrong).is_err());
        }
    }
}
