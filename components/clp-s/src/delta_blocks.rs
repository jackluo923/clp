//! Independently restartable delta blocks. Boundaries match the timestamp pruning index.
use crate::adaptive_int::{AdaptiveDecodeError as Error, packed_len};

pub const ROWS: usize = 65_536;
pub const SCHEME: u8 = 6;
/// Width marker for a directory of blocks with independent minimum/width headers.
pub const LOCAL_WIDTH: u8 = 255;

/// Payload: minimum delta (i64), packed width (u8), then fixed-stride blocks.
/// Each block contains an absolute first value followed by packed delta offsets.
#[derive(Clone, Copy, Debug)]
pub(crate) struct View<'a> {
    bytes: &'a [u8],
    rows: usize,
    minimum: u64,
    width: u8,
    stride: usize,
    length: usize,
}
impl<'a> View<'a> {
    pub(crate) fn new(bytes: &'a [u8], rows: usize) -> Result<Self, Error> {
        let header = bytes.get(..9).ok_or(Error::Truncated)?;
        let minimum = u64::from_le_bytes(header[..8].try_into().unwrap());
        let width = header[8];
        if width == LOCAL_WIDTH {
            return Self::new_local(bytes, rows, minimum);
        }
        if width > 64 {
            return Err(Error::Malformed);
        }
        let stride = 8 + packed_len(ROWS - 1, width);
        let full = rows / ROWS;
        let tail = rows % ROWS;
        let length = full
            .checked_mul(stride)
            .and_then(|n| n.checked_add(9))
            .and_then(|n| {
                n.checked_add(if tail == 0 {
                    0
                } else {
                    8 + packed_len(tail - 1, width)
                })
            })
            .ok_or(Error::Malformed)?;
        if bytes.len() < length {
            return Err(Error::Truncated);
        }
        Ok(Self {
            bytes,
            rows,
            minimum,
            width,
            stride,
            length,
        })
    }
    // Local layout: total rows(u64), marker(u8), absolute byte offsets(u64)
    // for each block plus a final end offset, then blocks containing
    // minimum(i64), width(u8), anchor(i64), and packed delta offsets.
    fn new_local(bytes: &'a [u8], rows: usize, total: u64) -> Result<Self, Error> {
        let total = usize::try_from(total).map_err(|_| Error::Malformed)?;
        if rows > total { return Err(Error::Truncated); }
        let blocks = total.div_ceil(ROWS);
        let directory_end = blocks.checked_add(1).and_then(|n| n.checked_mul(8))
            .and_then(|n| n.checked_add(9)).ok_or(Error::Malformed)?;
        if bytes.len() < directory_end { return Err(Error::Truncated); }
        let at = |block: usize| -> Result<usize, Error> {
            usize::try_from(u64::from_le_bytes(bytes[9 + block * 8..17 + block * 8].try_into().unwrap()))
                .map_err(|_| Error::Malformed)
        };
        let mut start = directory_end;
        for block in 0..blocks {
            if at(block)? != start { return Err(Error::Malformed); }
            let end = at(block + 1)?;
            let payload = bytes.get(start..end).ok_or(Error::Truncated)?;
            let width = *payload.get(8).ok_or(Error::Truncated)?;
            if width > 64 { return Err(Error::Malformed); }
            let count = (total - block * ROWS).min(ROWS);
            let expected = 17 + packed_len(count - 1, width);
            if payload.len() != expected { return Err(Error::Malformed); }
            start = end;
        }
        if at(blocks)? != start { return Err(Error::Malformed); }
        Ok(Self { bytes, rows, minimum: total as u64, width: LOCAL_WIDTH, stride: 0, length: start })
    }
    /// Reopen a span already checked by NumericColumn::encoded. Slice views keep
    /// the original directory; do not rescan it for each sparse row access.
    pub(crate) fn from_validated(bytes: &'a [u8], rows: usize) -> Self {
        if bytes[8] != LOCAL_WIDTH { return Self::new(bytes, rows).expect("validated delta blocks"); }
        let total = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let blocks = (total as usize).div_ceil(ROWS);
        let length = u64::from_le_bytes(bytes[9 + blocks * 8..17 + blocks * 8].try_into().unwrap()) as usize;
        Self { bytes, rows, minimum: total, width: LOCAL_WIDTH, stride: 0, length }
    }
    fn parameters(self, row: usize) -> (usize, u64, u8) {
        if self.width != LOCAL_WIDTH {
            return (9 + row / ROWS * self.stride, self.minimum, self.width);
        }
        let index = 9 + row / ROWS * 8;
        let start = u64::from_le_bytes(self.bytes[index..index + 8].try_into().unwrap()) as usize;
        let minimum = u64::from_le_bytes(self.bytes[start..start + 8].try_into().unwrap());
        (start + 9, minimum, self.bytes[start + 8])
    }
    pub(crate) fn is_constant_stride(self) -> bool {
        self.width == 0
    }
    pub(crate) fn wire_len(self) -> usize {
        self.length
    }
    fn anchor(self, row: usize) -> u64 {
        let (offset, _, _) = self.parameters(row);
        u64::from_le_bytes(self.bytes[offset..offset + 8].try_into().unwrap())
    }
    fn delta(self, row: usize) -> u64 {
        debug_assert!(row % ROWS != 0);
        let (anchor, minimum, width) = self.parameters(row);
        if width == 64 {
            let start = anchor + row % ROWS * 8;
            let offset = u64::from_le_bytes(self.bytes[start..start + 8].try_into().unwrap());
            return minimum.wrapping_add(offset);
        }
        let bit = (row % ROWS - 1) * usize::from(width);
        let start = anchor + 8 + bit / 8;
        let count = ((bit % 8) + usize::from(width)).div_ceil(8);
        let mut word = 0_u128;
        for (i, &byte) in self.bytes[start..start + count].iter().enumerate() {
            word |= u128::from(byte) << (8 * i);
        }
        let offset = (word >> (bit % 8)) & ((1_u128 << width) - 1);
        minimum.wrapping_add(offset as u64)
    }
    pub(crate) fn get(self, row: usize) -> Option<u64> {
        if row >= self.rows {
            return None;
        }
        let (_, minimum, width) = self.parameters(row);
        if width == 0 {
            return Some(
                self.anchor(row)
                    .wrapping_add(minimum.wrapping_mul((row % ROWS) as u64)),
            );
        }
        let mut value = self.anchor(row);
        for index in row / ROWS * ROWS + 1..=row {
            value = value.wrapping_add(self.delta(index));
        }
        Some(value)
    }
    pub(crate) fn linear(self) -> Option<(u64, u64)> {
        if self.rows == 0 || self.width != 0 {
            return None;
        }
        let first = self.anchor(0);
        first.checked_add(self.minimum.checked_mul((self.rows - 1) as u64)?)?;
        for row in (0..self.rows).step_by(ROWS) {
            if self.anchor(row) != first + self.minimum * row as u64 {
                return None;
            }
        }
        Some((first, self.minimum))
    }
    /// Reuse a cursor only within its block; crossing a boundary starts at the new anchor.
    pub(crate) fn from_previous(self, previous: Option<(usize, u64)>, row: usize) -> Option<u64> {
        if row >= self.rows {
            return None;
        }
        if let Some((start, mut value)) = previous {
            if start / ROWS == row / ROWS {
                let (_, minimum, width) = self.parameters(row);
                if width == 0 {
                    return Some(
                        value.wrapping_add(
                            minimum.wrapping_mul(row.wrapping_sub(start) as u64),
                        ),
                    );
                }
                if start <= row {
                    for at in start + 1..=row {
                        value = value.wrapping_add(self.delta(at));
                    }
                } else {
                    for at in (row + 1..=start).rev() {
                        value = value.wrapping_sub(self.delta(at));
                    }
                }
                return Some(value);
            }
        }
        self.get(row)
    }
    pub(crate) fn iter_from(self, start: usize) -> Iter<'a> {
        Iter {
            view: self,
            row: start.min(self.rows),
            value: None,
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Iter<'a> {
    view: View<'a>,
    row: usize,
    value: Option<u64>,
}
impl Iterator for Iter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        if self.row >= self.view.rows {
            return None;
        }
        let value = if self.row % ROWS == 0 {
            self.view.anchor(self.row)
        } else if let Some(previous) = self.value {
            previous.wrapping_add(self.view.delta(self.row))
        } else {
            self.view.get(self.row)?
        };
        self.row += 1;
        self.value = Some(value);
        Some(value)
    }
    fn nth(&mut self, n: usize) -> Option<u64> {
        if n != 0 {
            self.row = self.row.saturating_add(n).min(self.view.rows);
            self.value = None;
        }
        self.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.view.rows - self.row;
        (n, Some(n))
    }
}
impl ExactSizeIterator for Iter<'_> {}
impl std::iter::FusedIterator for Iter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive_int::{candidates, decode, decode_to_le};
    use crate::encoded_numeric::{NumericColumn, NumericEncoding, prepare};
    #[test]
    fn blocks_roundtrip_seek_and_preserve_slices_and_sparse_predicates() {
        let values: Vec<i64> = (0..ROWS * 3 + 17)
            .map(|i| {
                // Large discontinuities must not expand the within-block delta width.
                let base = [i64::MIN + 1000, i64::MAX - 1_000_000, -300_000, 17][i / ROWS];
                base + (i % ROWS) as i64 * 3 + (i % 5) as i64
            })
            .collect();
        let wires: Vec<_> = candidates(&values).filter(|c| c[0] == SCHEME).collect();
        assert_eq!(wires.len(), 3); // Column-wide packed, fixed-width and chunk-local layouts.
        assert!(wires[0].len() < values.len());
        for wire in wires {
            assert_eq!(decode(&wire, values.len()).unwrap(), values);
            let mut expanded = Vec::new();
            decode_to_le(&wire, values.len(), &mut expanded).unwrap();
            assert_eq!(
                expanded,
                values
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>()
            );
            let mut compact = Vec::new();
            assert_eq!(
                prepare(&wire, values.len(), false, &mut compact).unwrap(),
                Some(NumericEncoding::BlockDelta)
            );
            let column =
                NumericColumn::encoded(&compact, values.len(), NumericEncoding::BlockDelta).unwrap();
            assert!(column.canonical_bytes().is_none());
            assert_eq!(column.iter().map(|v| v as i64).collect::<Vec<_>>(), values);
            assert_eq!(
                column.iter().rev().map(|v| v as i64).collect::<Vec<_>>(),
                values.iter().rev().copied().collect::<Vec<_>>()
            );
            for start in [
                0,
                1,
                ROWS - 1,
                ROWS,
                ROWS + 1,
                ROWS * 2 - 1,
                ROWS * 2,
                values.len() - 1,
            ] {
                assert_eq!(column.get(start), Some(values[start] as u64));
                assert_eq!(column.iter().nth(start), Some(values[start] as u64));
                assert_eq!(
                    column.iter().nth_back(values.len() - start - 1),
                    Some(values[start] as u64)
                );
                let slice = column
                    .slice(start, (values.len() - start).min(123))
                    .unwrap();
                let mut actual = Vec::new();
                slice.append_bits(&mut actual);
                assert_eq!(
                    actual,
                    values[start..start + slice.len()]
                        .iter()
                        .map(|v| *v as u64)
                        .collect::<Vec<_>>()
                );
                let mut bitmap = vec![0; slice.len()];
                slice.or_matches(0, &mut bitmap, 1, |v| v % 3 == 0);
                assert_eq!(
                    bitmap,
                    actual
                        .iter()
                        .map(|v| u8::from(*v % 3 == 0))
                        .collect::<Vec<_>>()
                );
            }
            let mut previous = None;
            for row in (0..values.len())
                .step_by(137)
                .chain([ROWS * 2, 1, ROWS, values.len() - 1])
            {
                let value = column.get_with_previous(previous, row).unwrap();
                assert_eq!(value as i64, values[row]);
                previous = Some((row, value));
            }
            // Damaging a preceding chunk's anchor has no effect on the next chunk's values.
            let mut changed = compact.clone();
            let anchor = View::new(&compact, values.len()).unwrap().parameters(0).0;
            changed[anchor..anchor + 8].fill(0xff);
            let view = View::new(&changed, values.len()).unwrap();
            assert_eq!(
                view.iter_from(ROWS).take(3).collect::<Vec<_>>(),
                values[ROWS..ROWS + 3]
                    .iter()
                    .map(|v| *v as u64)
                    .collect::<Vec<_>>()
            );
            for end in [0, 8, 9, 16, compact.len() - 1] {
                assert!(View::new(&compact[..end], values.len()).is_err());
            }
            changed[8] = 65;
            assert!(View::new(&changed, values.len()).is_err());
            assert!(View::new(&compact, usize::MAX).is_err());
        }
    }
    #[test]
    fn constant_stride_deltas_are_random_access_and_prove_linearity_across_anchors() {
        let values: Vec<_> = (0..ROWS * 2 + 3).map(|i| 10 + i as i64 * 7).collect();
        let wire = candidates(&values).find(|c| c[0] == SCHEME).unwrap();
        assert_eq!(wire.len(), 10 + 3 * 8);
        let view = View::new(&wire[1..], values.len()).unwrap();
        assert_eq!(view.linear(), Some((10, 7)));
        for row in [0, ROWS - 1, ROWS, values.len() - 1] {
            assert_eq!(view.get(row), Some(values[row] as u64));
        }
        let mut changed = wire[1..].to_vec();
        changed[17] ^= 1;
        assert!(
            View::new(&changed, values.len())
                .unwrap()
                .linear()
                .is_none()
        );
    }

    #[test]
    fn local_widths_confine_large_in_chunk_jumps_and_validate_the_directory() {
        let values: Vec<i64> = (0..ROWS * 3 + 5).map(|row| {
            let i = (row % ROWS) as i64;
            match row / ROWS {
                0 => 1_000_000_000_000 + i * 3 + i % 2,
                1 => 1_000_000_000_000 + i * 3 + if i > 1000 { 1 << 40 } else { 0 },
                2 => i64::MIN + i * 7 + i % 4,
                _ => 17,
            }
        }).collect();
        let wires: Vec<_> = candidates(&values).filter(|wire| wire[0] == SCHEME).collect();
        let local = wires.iter().find(|wire| wire[9] == LOCAL_WIDTH).unwrap();
        let global = &wires[0];
        assert!(local.len() < global.len() / 2);
        let view = View::new(&local[1..], values.len()).unwrap();
        assert_eq!((0..4).map(|block| view.parameters(block * ROWS).2).collect::<Vec<_>>(), [2, 41, 3, 0]);
        assert_eq!(decode(local, values.len()).unwrap(), values);
        // Prefix/sliced views retain the original directory, including its tail.
        let mut compact = Vec::new();
        prepare(local, values.len(), false, &mut compact).unwrap();
        let column = NumericColumn::encoded(&compact, values.len(), NumericEncoding::BlockDelta).unwrap();
        for start in [0, ROWS - 2, ROWS + 999, ROWS * 2, ROWS * 3] {
            let count = (values.len() - start).min(5);
            assert_eq!(column.slice(start, count).unwrap().iter().map(|v| v as i64).collect::<Vec<_>>(), values[start..start + count]);
        }
        for end in [0, 8, 9, 16, 48, compact.len() - 1] {
            assert!(View::new(&compact[..end], values.len()).is_err());
        }
        let mut bad = compact.clone();
        bad[9..17].fill(0); // Directory cannot point into itself.
        assert!(View::new(&bad, values.len()).is_err());
        let mut bad = compact.clone();
        bad[17..25].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(View::new(&bad, values.len()).is_err());
        let mut bad = compact.clone();
        bad[view.parameters(ROWS).0 - 1] = 65;
        assert!(View::new(&bad, values.len()).is_err());
        let mut bad = compact.clone();
        bad[..8].fill(0xff);
        assert!(View::new(&bad, values.len()).is_err());
        // Signed subtraction would overflow, but modular deltas remain lossless.
        let wrapped: Vec<_> = (0..ROWS + 3).map(|i| if i % 2 == 0 { i64::MIN } else { i64::MAX }).collect();
        let wire = candidates(&wrapped).find(|wire| wire[0] == SCHEME && wire[9] == LOCAL_WIDTH).unwrap();
        assert_eq!(decode(&wire, wrapped.len()).unwrap(), wrapped);
    }

    #[test]
    fn local_widths_can_win_after_zstd_against_all_normal_candidates() {
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut value = 1_000_000_000i64;
        let values: Vec<_> = (0..ROWS * 3 + 11).map(|row| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            value += ((state >> 32) & [255, 65535, 3, 15][row / ROWS]) as i64;
            value
        }).collect();
        let (bytes, winner) = candidates(&values).map(|wire| {
            let size = zstd::bulk::compress(&wire, 3).unwrap().len();
            (size, wire)
        }).min_by_key(|(size, _)| *size).unwrap();
        assert_eq!(winner[0], SCHEME, "smallest candidate: {bytes} bytes");
        assert_eq!(winner[9], LOCAL_WIDTH, "smallest candidate: {bytes} bytes");
        assert_eq!(decode(&winner, values.len()).unwrap(), values);
    }
}
