//! Stream delta descriptors into predicates; retain only one checkpoint per 64 rows.
//! The original packed bits remain borrowed. Checkpoints are published after a full
//! traversal, and are built lazily if a caller first requests random access.
use crate::adaptive_int::{AdaptiveDecodeError as Error, unpack_bits_iter};
use crate::encoded_numeric::{NumericColumn, NumericIter};
use std::sync::OnceLock;
const BLOCK: usize = 64;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Descriptors<'a> {
    Numeric(NumericColumn<'a>),
    Delta(Delta<'a>),
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Delta<'a> {
    first: u64,
    minimum: u64,
    width: u8,
    packed: &'a [u8],
    rows: usize,
    checkpoints: &'a OnceLock<Vec<u64>>,
}
impl<'a> Descriptors<'a> {
    pub(crate) fn delta(
        bytes: &'a [u8],
        rows: usize,
        checkpoints: &'a OnceLock<Vec<u64>>,
    ) -> Result<Self, Error> {
        let header = bytes.get(..17).ok_or(Error::Truncated)?;
        let first = u64::from_le_bytes(header[..8].try_into().unwrap());
        let minimum = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let width = header[16];
        let packed = &bytes[17..];
        let _ = unpack_bits_iter(packed, width, rows.checked_sub(1).ok_or(Error::Malformed)?)?;
        Ok(Self::Delta(Delta {
            first,
            minimum,
            width,
            packed,
            rows,
            checkpoints,
        }))
    }
    pub(crate) fn len(self) -> usize {
        match self {
            Self::Numeric(n) => n.len(),
            Self::Delta(d) => d.rows,
        }
    }
    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub(crate) fn get(self, row: usize) -> Option<u64> {
        match self {
            Self::Numeric(n) => n.get(row),
            Self::Delta(d) => {
                if row >= d.rows {
                    return None;
                }
                if row == 0 {
                    return Some(d.first);
                }
                let checkpoints = d
                    .checkpoints
                    .get_or_init(|| d.iter(false).step_by(BLOCK).collect());
                let start = row / BLOCK * BLOCK;
                Some(d.advance(checkpoints[row / BLOCK], start, row))
            }
        }
    }
    pub(crate) fn iter(self) -> DescriptorIter<'a> {
        match self {
            Self::Numeric(n) => DescriptorIter::Numeric(n.iter()),
            Self::Delta(d) => DescriptorIter::Delta(d.iter(d.checkpoints.get().is_none())),
        }
    }
    pub(crate) fn cursor(self) -> DescriptorCursor<'a> {
        DescriptorCursor {
            column: self,
            last: None,
        }
    }
}
impl<'a> Delta<'a> {
    fn iter(self, record: bool) -> DeltaIter<'a> {
        DeltaIter {
            delta: self,
            bits: Bits::new(self.packed, self.width, 0),
            row: 0,
            value: self.first,
            checkpoints: record.then(|| Vec::with_capacity(self.rows.div_ceil(BLOCK))),
        }
    }
    fn advance(self, mut value: u64, start: usize, end: usize) -> u64 {
        let mut bits = Bits::new(self.packed, self.width, start);
        for _ in start..end {
            value = value.wrapping_add(self.minimum.wrapping_add(bits.next()));
        }
        value
    }
}
/// Forward cursors reuse the previous value for adjacent selected rows. Sparse
/// jumps use checkpoints, so reconstruction never walks all rejected rows again.
pub(crate) struct DescriptorCursor<'a> {
    column: Descriptors<'a>,
    last: Option<(usize, u64)>,
}
impl DescriptorCursor<'_> {
    pub(crate) fn get(&mut self, row: usize) -> Option<u64> {
        if row >= self.column.len() {
            return None;
        }
        let value = match (self.column, self.last) {
            (Descriptors::Delta(d), Some((last, value))) if row >= last && row - last < BLOCK => {
                d.advance(value, last, row)
            }
            _ => self.column.get(row)?,
        };
        self.last = Some((row, value));
        Some(value)
    }
}
pub(crate) enum DescriptorIter<'a> {
    Numeric(NumericIter<'a>),
    Delta(DeltaIter<'a>),
}
impl Iterator for DescriptorIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        match self {
            Self::Numeric(n) => n.next(),
            Self::Delta(d) => d.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match self {
            Self::Numeric(n) => n.len(),
            Self::Delta(d) => d.delta.rows - d.row,
        };
        (n, Some(n))
    }
}
impl ExactSizeIterator for DescriptorIter<'_> {}
pub(crate) struct DeltaIter<'a> {
    delta: Delta<'a>,
    bits: Bits<'a>,
    row: usize,
    value: u64,
    checkpoints: Option<Vec<u64>>,
}
impl Iterator for DeltaIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        if self.row == self.delta.rows {
            return None;
        }
        if self.row > 0 {
            self.value = self
                .value
                .wrapping_add(self.delta.minimum.wrapping_add(self.bits.next()));
        }
        if self.row % BLOCK == 0 {
            if let Some(checkpoints) = &mut self.checkpoints {
                checkpoints.push(self.value);
            }
        }
        self.row += 1;
        if self.row == self.delta.rows {
            if let Some(checkpoints) = self.checkpoints.take() {
                let _ = self.delta.checkpoints.set(checkpoints);
            }
        }
        Some(self.value)
    }
}
/// Refill by machine words, with a byte tail; no per-value bounds-validation pass.
struct Bits<'a> {
    bytes: &'a [u8],
    width: u32,
    mask: u128,
    acc: u128,
    bits: u32,
    at: usize,
}
impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8], width: u8, start: usize) -> Self {
        let bit = start * usize::from(width);
        let mut at = bit / 8;
        let shift = bit % 8;
        let (acc, bits) = if shift != 0 && at < bytes.len() {
            let v = u128::from(bytes[at]) >> shift;
            at += 1;
            (v, (8 - shift) as u32)
        } else {
            (0, 0)
        };
        Self {
            bytes,
            width: u32::from(width),
            mask: (1u128 << width) - 1,
            acc,
            bits,
            at,
        }
    }
    fn next(&mut self) -> u64 {
        if self.bits < self.width && self.bytes.len() - self.at >= 8 {
            self.acc |= u128::from(u64::from_le_bytes(
                self.bytes[self.at..self.at + 8].try_into().unwrap(),
            )) << self.bits;
            self.at += 8;
            self.bits += 64;
        }
        while self.bits < self.width {
            self.acc |= u128::from(self.bytes[self.at]) << self.bits;
            self.at += 1;
            self.bits += 8;
        }
        let value = (self.acc & self.mask) as u64;
        self.acc >>= self.width;
        self.bits -= self.width;
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_delta_iteration_and_random_access_match_reference() {
        for width in 0..=64 {
            for rows in [1usize, 2, 63, 64, 65, 129, 1025] {
                let offsets: Vec<u64> = (0..rows - 1)
                    .map(|i| {
                        (i as u64).wrapping_mul(0x9e3779b97f4a7c15)
                            & (((1u128 << width) - 1) as u64)
                    })
                    .collect();
                let mut packed = vec![0u8; ((rows - 1) * width as usize).div_ceil(8)];
                for (i, &v) in offsets.iter().enumerate() {
                    for bit in 0..width {
                        if (v >> bit) & 1 != 0 {
                            let b = i * width as usize + bit as usize;
                            packed[b / 8] |= 1 << (b % 8);
                        }
                    }
                }
                let mut bytes = u64::MAX.to_le_bytes().to_vec();
                bytes.extend_from_slice(&(-7i64).to_le_bytes());
                bytes.push(width);
                bytes.extend_from_slice(&packed);
                let expected: Vec<_> = std::iter::once(u64::MAX)
                    .chain(offsets.iter().scan(u64::MAX, |v, d| {
                        *v = v.wrapping_add((-7i64) as u64).wrapping_add(*d);
                        Some(*v)
                    }))
                    .collect();
                let cache = OnceLock::new();
                let column = Descriptors::delta(&bytes, rows, &cache).unwrap();
                assert!(cache.get().is_none());
                assert_eq!(column.iter().collect::<Vec<_>>(), expected);
                assert_eq!(cache.get().unwrap().len(), rows.div_ceil(BLOCK));
                for row in (0..rows).rev() {
                    assert_eq!(column.get(row), Some(expected[row]));
                }
                let mut cursor = column.cursor();
                for row in (0..rows).step_by(3).chain((0..rows).rev()) {
                    assert_eq!(cursor.get(row), Some(expected[row]));
                }
                assert_eq!(column.get(rows), None);
                assert_eq!(cursor.get(rows), None);
                // Random access before a full traversal builds exactly the same sparse index.
                let cache = OnceLock::new();
                let column = Descriptors::delta(&bytes, rows, &cache).unwrap();
                assert_eq!(column.get(rows - 1), expected.last().copied());
                for end in 0..bytes.len() {
                    assert!(Descriptors::delta(&bytes[..end], rows, &OnceLock::new()).is_err());
                }
            }
        }
    }
}
