//! Exact integers spanning the signed-negative and unsigned-positive 64-bit domains.
use crate::adaptive_int::{self, AdaptiveDecodeError as Error};
use crate::encoded_numeric::{self, NumericColumn, NumericEncoding};

/// Exact JSON integer. Small unsigned values normalize to signed storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegerValue {
    Signed(i64),
    Unsigned(u64),
}
impl IntegerValue {
    pub fn exact(self) -> i128 {
        match self {
            Self::Signed(v) => v as i128,
            Self::Unsigned(v) => v as i128,
        }
    }
    pub fn bits(self) -> u64 {
        match self {
            Self::Signed(v) => v as u64,
            Self::Unsigned(v) => v,
        }
    }
    pub fn as_i64(self) -> Option<i64> {
        i64::try_from(self.exact()).ok()
    }
    pub fn as_u64(self) -> Option<u64> {
        u64::try_from(self.exact()).ok()
    }
    pub fn is_large(self) -> bool {
        self.exact() > i64::MAX as i128
    }
    /// Exact ordering against binary64 without rounding the integer.
    pub fn compare_float(self, operand: f64) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering::*;
        if operand.is_nan() {
            return None;
        }
        if operand >= 18446744073709551616.0 {
            return Some(Less);
        }
        if operand < -9223372036854775808.0 {
            return Some(Greater);
        }
        let integer = operand.trunc() as i128;
        Some(match self.exact().cmp(&integer) {
            Equal if operand > integer as f64 => Less,
            Equal if operand < integer as f64 => Greater,
            order => order,
        })
    }
}

/// Writer buffer with an unsigned bitmap allocated only on first large value.
#[derive(Debug, Default)]
pub(crate) struct IntegerBuffer {
    pub(crate) bits: Vec<u8>,
    pub(crate) bitmap: Vec<u8>,
}
impl IntegerBuffer {
    fn next_bitmap_len(&self, value: IntegerValue) -> usize {
        if self.bitmap.is_empty() && !value.is_large() {
            0
        } else {
            (self.bits.len() / 8 + 1).div_ceil(8)
        }
    }
    pub(crate) fn additional_len(&self, value: IntegerValue) -> usize {
        8 + self.next_bitmap_len(value) - self.bitmap.len()
    }
    pub(crate) fn reserve(
        &mut self,
        value: IntegerValue,
    ) -> Result<(), std::collections::TryReserveError> {
        self.bits.try_reserve(8)?;
        self.bitmap
            .try_reserve(self.next_bitmap_len(value) - self.bitmap.len())
    }
    pub(crate) fn append(&mut self, value: IntegerValue) {
        let row = self.bits.len() / 8;
        self.bitmap.resize(self.next_bitmap_len(value), 0);
        if value.is_large() {
            self.bitmap[row / 8] |= 1 << (row % 8);
        }
        self.bits.extend_from_slice(&value.bits().to_le_bytes());
    }
    pub(crate) fn wire_len(&self) -> usize {
        1 + self.bits.len() + self.bitmap.len()
    }
    #[cfg(test)]
    pub(crate) fn candidates(&self) -> Vec<Vec<u8>> {
        self.candidates_with_effort(crate::EncodingEffort::Balanced).collect()
    }
    pub(crate) fn candidates_with_effort(&self, effort: crate::EncodingEffort) -> impl Iterator<Item = Vec<u8>> + '_ {
        let values: Vec<i64> = self
            .bits
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        adaptive_int::owned_candidates_with_effort(values, effort)
            .map(|mut payload| {
                payload.reserve(1 + self.bitmap.len());
                payload.insert(0, u8::from(!self.bitmap.is_empty()));
                payload.extend_from_slice(&self.bitmap);
                payload
            })
    }
}

fn split(raw: &[u8], rows: usize) -> Result<(&[u8], &[u8]), Error> {
    let flag = *raw.first().ok_or(Error::Truncated)?;
    if flag > 1 {
        return Err(Error::Malformed);
    }
    let bitmap_len = if flag == 1 { rows.div_ceil(8) } else { 0 };
    let end = raw
        .len()
        .checked_sub(bitmap_len)
        .filter(|&n| n >= 1)
        .ok_or(Error::Truncated)?;
    let bitmap = &raw[end..];
    if rows % 8 != 0 && bitmap.last().is_some_and(|b| b >> (rows % 8) != 0) {
        return Err(Error::Malformed);
    }
    Ok((&raw[1..end], bitmap))
}

/// Prepare one adaptive frame, retaining the underlying compact integer encoding.
pub(crate) fn prepare(
    raw: &[u8],
    rows: usize,
    out: &mut Vec<u8>,
) -> Result<Option<NumericEncoding>, Error> {
    let (payload, bitmap) = split(raw, rows)?;
    out.push(raw[0]);
    let encoding = encoded_numeric::prepare(payload, rows, false, out)?;
    if encoding.is_none() {
        adaptive_int::decode_to_le(payload, rows, out)?;
    }
    out.extend_from_slice(bitmap);
    Ok(encoding)
}

/// A single physical column retaining exact signed and unsigned values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixedIntegerColumn<'a> {
    data: NumericColumn<'a>,
    bitmap: &'a [u8],
}
impl<'a> MixedIntegerColumn<'a> {
    pub(crate) fn new(
        raw: &'a [u8],
        rows: usize,
        encoding: Option<NumericEncoding>,
    ) -> Result<Self, Error> {
        let (payload, bitmap) = split(raw, rows)?;
        let data = if let Some(encoding) = encoding {
            NumericColumn::encoded(payload, rows, encoding)?
        } else {
            if payload.len() != rows.checked_mul(8).ok_or(Error::Malformed)? {
                return Err(Error::Malformed);
            }
            NumericColumn::plain(payload)
        };
        Ok(Self { data, bitmap })
    }
    pub(crate) fn signed_column(self) -> Option<crate::archive::I64Column<'a>> {
        (!self.has_large_uint64()).then_some(crate::archive::I64Column { data: self.data })
    }
    pub fn has_large_uint64(self) -> bool {
        !self.bitmap.is_empty()
    }
    /// Canonical signed bytes are available only when no unsigned bitmap is present.
    pub fn canonical_signed_bytes(self) -> Option<&'a [u8]> {
        if self.has_large_uint64() {
            None
        } else {
            self.data.canonical_bytes()
        }
    }
    pub fn signed_constant(self) -> Option<u64> {
        if self.has_large_uint64() {
            None
        } else {
            self.data.constant()
        }
    }
    pub fn append_bits(self, output: &mut Vec<u64>) {
        self.data.append_bits(output);
    }
    pub fn len(self) -> usize {
        self.data.len()
    }
    pub fn is_empty(self) -> bool {
        self.data.len() == 0
    }
    /// Whether this row's bit pattern represents an unsigned value above i64::MAX.
    pub fn is_large_at(self, row: usize) -> bool {
        row < self.len() && !self.bitmap.is_empty() && self.bitmap[row / 8] & (1 << (row % 8)) != 0
    }
    fn value(self, row: usize, bits: u64) -> IntegerValue {
        if !self.bitmap.is_empty() && self.bitmap[row / 8] & (1 << (row % 8)) != 0 {
            IntegerValue::Unsigned(bits)
        } else {
            IntegerValue::Signed(bits as i64)
        }
    }
    pub fn get(self, row: usize) -> Option<IntegerValue> {
        self.data.get(row).map(|bits| self.value(row, bits))
    }
    /// Reuses a prior row from this column when walking restartable deltas.
    pub fn get_with_previous(self, previous: Option<(usize, IntegerValue)>, row: usize) -> Option<IntegerValue> {
        self.data.get_with_previous(previous.map(|(r, v)| (r, v.bits())), row).map(|bits| self.value(row, bits))
    }
    pub fn iter(self) -> impl ExactSizeIterator<Item = IntegerValue> + DoubleEndedIterator + 'a {
        self.data
            .iter()
            .enumerate()
            .map(move |(row, bits)| self.value(row, bits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lazy_candidates_preserve_eager_bytes_and_order_at_every_effort() {
        for count in [0, 1, 9, crate::delta_blocks::ROWS + 3] {
            let mut buffer = IntegerBuffer::default();
            let values: Vec<_> = (0..count).map(|row| {
                if row % 13 == 0 { IntegerValue::Unsigned(u64::MAX - row as u64) }
                else { IntegerValue::Signed(row as i64 / 7 - 100) }
            }).collect();
            for &value in &values { buffer.append(value); }
            let bits: Vec<_> = values.iter().map(|v| v.bits() as i64).collect();
            for effort in [crate::EncodingEffort::Fast, crate::EncodingEffort::Balanced, crate::EncodingEffort::Exhaustive] {
                let expected: Vec<_> = adaptive_int::candidates_with_effort(&bits, effort).map(|payload| {
                    let mut bytes = vec![u8::from(!buffer.bitmap.is_empty())];
                    bytes.extend_from_slice(&payload);
                    bytes.extend_from_slice(&buffer.bitmap);
                    bytes
                }).collect();
                assert_eq!(buffer.candidates_with_effort(effort).collect::<Vec<_>>(), expected);
            }
        }
    }
    #[test]
    fn optional_bitmap_and_all_adaptive_candidates_roundtrip() {
        for count in [1_usize, 7, 8, 9, 65, 257] {
            for large in [false, true] {
                let values: Vec<_> = (0..count)
                    .map(|i| {
                        if large && i == count - 1 {
                            IntegerValue::Unsigned(u64::MAX)
                        } else if i % 3 == 0 {
                            IntegerValue::Signed(i64::MIN)
                        } else {
                            IntegerValue::Signed(i as i64)
                        }
                    })
                    .collect();
                let mut buffer = IntegerBuffer::default();
                for &value in &values {
                    let before = buffer.wire_len();
                    let added = buffer.additional_len(value);
                    buffer.reserve(value).unwrap();
                    buffer.append(value);
                    assert_eq!(buffer.wire_len(), before + added);
                }
                assert_eq!(
                    buffer.bitmap.len(),
                    if large { count.div_ceil(8) } else { 0 }
                );
                for raw in buffer.candidates() {
                    let mut prepared = Vec::new();
                    let encoding = prepare(&raw, count, &mut prepared).unwrap();
                    let column = MixedIntegerColumn::new(&prepared, count, encoding).unwrap();
                    assert_eq!(column.has_large_uint64(), large);
                    assert_eq!(column.iter().collect::<Vec<_>>(), values);
                    for (index, &value) in values.iter().enumerate() {
                        assert_eq!(column.get(index), Some(value));
                    }
                }
            }
        }
    }
    #[test]
    fn same_bits_retain_distinct_signedness() {
        let values = [
            IntegerValue::Signed(-1),
            IntegerValue::Unsigned(u64::MAX),
            IntegerValue::Signed(i64::MIN),
            IntegerValue::Unsigned(1 << 63),
            IntegerValue::Unsigned(9368605428256079743),
        ];
        let mut buffer = IntegerBuffer::default();
        for value in values {
            buffer.append(value);
        }
        for raw in buffer.candidates() {
            let mut prepared = Vec::new();
            let encoding = prepare(&raw, values.len(), &mut prepared).unwrap();
            assert_eq!(
                MixedIntegerColumn::new(&prepared, values.len(), encoding)
                    .unwrap()
                    .iter()
                    .collect::<Vec<_>>(),
                values
            );
        }
    }
    #[test]
    fn exact_float_comparisons_at_boundaries() {
        use std::cmp::Ordering::*;
        assert_eq!(
            IntegerValue::Unsigned(u64::MAX).compare_float(18446744073709551616.0),
            Some(Less)
        );
        assert_eq!(
            IntegerValue::Signed(i64::MAX).compare_float(9223372036854775808.0),
            Some(Less)
        );
        assert_eq!(IntegerValue::Signed(-1).compare_float(-1.5), Some(Greater));
        assert_eq!(IntegerValue::Unsigned(0).as_i64(), Some(0));
        assert_eq!(IntegerValue::Signed(-1).as_u64(), None);
    }
}
