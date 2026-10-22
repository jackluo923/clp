//! Reader-only numeric representations. No tags or bytes written to archives change.
use std::iter::FusedIterator;
use crate::adaptive_int::{AdaptiveDecodeError as Error, unpack_bits_iter};

/// Compact numeric layout carried by an in-memory column slot, never serialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumericEncoding {
    /// One little-endian 64-bit value.
    Constant,
    /// A value table followed by packed row indices.
    Dictionary,
    /// Value/end pairs; cumulative ends replace the wire run lengths.
    RunEnds,
}

pub(crate) fn encoding(bytes: &[u8], float: bool) -> Option<NumericEncoding> {
    match (bytes.first().copied()?, float) {
        (5, false) | (3, true) => Some(NumericEncoding::Constant),
        (3, false) | (2, true) => Some(NumericEncoding::Dictionary),
        (4, _) => Some(NumericEncoding::RunEnds),
        _ => None,
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let end = offset.checked_add(4).ok_or(Error::Malformed)?;
    Ok(u32::from_le_bytes(bytes.get(offset..end).ok_or(Error::Truncated)?.try_into().unwrap()))
}
fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let end = offset.checked_add(8).ok_or(Error::Malformed)?;
    Ok(u64::from_le_bytes(bytes.get(offset..end).ok_or(Error::Truncated)?.try_into().unwrap()))
}
fn dictionary(bytes: &[u8]) -> Result<(usize, &[u8], u8, &[u8]), Error> {
    let count = u32_at(bytes, 0)? as usize;
    let end = count.checked_mul(8).and_then(|n| n.checked_add(4)).ok_or(Error::Malformed)?;
    let values = bytes.get(4..end).ok_or(Error::Truncated)?;
    let width = *bytes.get(end).ok_or(Error::Truncated)?;
    let codes = &bytes[end + 1..];
    Ok((count, values, width, codes))
}

/// Validate the span even when every representable code is a valid index. Small
/// packed widths otherwise check a byte at a time; unused padding bits are ignored.
fn validate_dictionary_codes(codes: &[u8], width: u8, rows: usize, entries: usize) -> Result<(), Error> {
    let decoded = unpack_bits_iter(codes, width, rows)?;
    if (entries as u128) >= (1u128 << width) { return Ok(()); }
    if rows >= 256 && matches!(width, 1 | 2 | 4 | 8) {
        let lanes = 8 / usize::from(width);
        let mask = (1usize << width) - 1;
        let mut valid = [true; 256];
        for (byte, valid) in valid.iter_mut().enumerate() {
            *valid = (0..lanes).all(|lane| ((byte >> (lane * usize::from(width))) & mask) < entries);
        }
        let full_bytes = rows / lanes;
        if codes[..full_bytes].iter().any(|&byte| !valid[usize::from(byte)]) {
            return Err(Error::Truncated);
        }
        for lane in 0..rows % lanes {
            let code = (usize::from(codes[full_bytes]) >> (lane * usize::from(width))) & mask;
            if code >= entries { return Err(Error::Truncated); }
        }
    } else {
        for code in decoded {
            if code >= entries as u64 { return Err(Error::Truncated); }
        }
    }
    Ok(())
}

/// Each input byte expands to LANES predicate masks. Blending preserves any
/// previous True/False/Unknown result on rows this predicate does not match.
fn or_dictionary_bytes<const LANES: usize>(codes: &[u8], start: usize, bitmap: &mut [u8],
    truth: u8, accepted: &[bool]) {
    let width = 8 / LANES;
    let code_mask = (1usize << width) - 1;
    let head = ((LANES - start % LANES) % LANES).min(bitmap.len());
    let (prefix, rest) = bitmap.split_at_mut(head);
    for (i, dest) in prefix.iter_mut().enumerate() {
        let row = start + i;
        let code = (usize::from(codes[row / LANES]) >> ((row % LANES) * width)) & code_mask;
        if accepted[code] { *dest = truth; }
    }
    let mut lookup = [[0u8; LANES]; 256];
    for (byte, masks) in lookup.iter_mut().enumerate() {
        for (lane, mask) in masks.iter_mut().enumerate() {
            let code = (byte >> (lane * width)) & code_mask;
            *mask = if accepted.get(code).copied().unwrap_or(false) { u8::MAX } else { 0 };
        }
    }
    let full = rest.len() / LANES;
    let byte_start = (start + head) / LANES;
    let (middle, tail) = rest.split_at_mut(full * LANES);
    for (dest, &code_byte) in middle.as_chunks_mut::<LANES>().0.iter_mut()
        .zip(&codes[byte_start..byte_start + full]) {
        for (value, &mask) in dest.iter_mut().zip(&lookup[usize::from(code_byte)]) {
            *value = (*value & !mask) | (truth & mask);
        }
    }
    if !tail.is_empty() {
        let byte = usize::from(codes[byte_start + full]);
        for (lane, dest) in tail.iter_mut().enumerate() {
            if accepted[(byte >> (lane * width)) & code_mask] { *dest = truth; }
        }
    }
}

/// Appends a compact representation, or leaves the destination alone for other codecs.
/// A failed destination is discarded by the caller, as with the canonical decoder.
pub(crate) fn prepare(bytes: &[u8], rows: usize, float: bool, out: &mut Vec<u8>)
    -> Result<Option<NumericEncoding>, Error>
{
    let Some(encoding) = encoding(bytes, float) else { return Ok(None); };
    let rest = &bytes[1..];
    match encoding {
        NumericEncoding::Constant => out.extend_from_slice(rest.get(..8).ok_or(Error::Truncated)?),
        NumericEncoding::Dictionary => {
            let (_, _, width, _) = dictionary(rest)?;
            let count = u32_at(rest, 0)? as usize;
            let packed = rows.checked_mul(usize::from(width)).ok_or(Error::Malformed)?.div_ceil(8);
            let length = count.checked_mul(8).and_then(|n| n.checked_add(5))
                .and_then(|n| n.checked_add(packed)).ok_or(Error::Malformed)?;
            // Check the packed span here; the table decoder validates every index once
            // when constructing NumericColumn, before any value is accessed.
            let (_, _, _, codes) = dictionary(rest)?;
            let _ = unpack_bits_iter(codes, width, rows)?;
            out.extend_from_slice(rest.get(..length).ok_or(Error::Truncated)?);
        }
        NumericEncoding::RunEnds => {
            let runs = u32_at(rest, 0)? as usize;
            let needed = runs.checked_mul(12).and_then(|n| n.checked_add(4)).ok_or(Error::Malformed)?;
            if rest.len() < needed { return Err(Error::Truncated); }
            out.extend_from_slice(&(runs as u32).to_le_bytes());
            let mut end = 0usize;
            for run in 0..runs {
                let value = u64_at(rest, 4 + run * 12)?;
                end = end.checked_add(u32_at(rest, 12 + run * 12)? as usize).ok_or(Error::Malformed)?;
                if end > rows { return Err(Error::Malformed); }
                let end32 = u32::try_from(end).map_err(|_| Error::Malformed)?;
                out.extend_from_slice(&value.to_le_bytes());
                out.extend_from_slice(&end32.to_le_bytes());
            }
            if end != rows { return Err(Error::Malformed); }
        }
    }
    Ok(Some(encoding))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NumericColumn<'a> {
    pub(crate) bytes: &'a [u8],
    encoding: Option<NumericEncoding>,
    offset: usize,
    len: usize,
}

impl<'a> NumericColumn<'a> {
    pub(crate) const fn plain(bytes: &'a [u8]) -> Self {
        Self { bytes, encoding: None, offset: 0, len: bytes.len() / 8 }
    }

    /// Validate independently: public table-decoder callers may supply their own slots.
    pub(crate) fn encoded(bytes: &'a [u8], rows: usize, encoding: NumericEncoding) -> Result<Self, Error> {
        match encoding {
            NumericEncoding::Constant => { u64_at(bytes, 0)?; }
            NumericEncoding::Dictionary => {
                let (count, _, width, codes) = dictionary(bytes)?;
                validate_dictionary_codes(codes, width, rows, count)?;
            }
            NumericEncoding::RunEnds => {
                let runs = u32_at(bytes, 0)? as usize;
                let needed = runs.checked_mul(12).and_then(|n| n.checked_add(4)).ok_or(Error::Malformed)?;
                if bytes.len() < needed { return Err(Error::Truncated); }
                let mut previous = 0;
                for run in 0..runs {
                    let end = u32_at(bytes, 12 + run * 12)? as usize;
                    if end < previous || end > rows { return Err(Error::Malformed); }
                    previous = end;
                }
                if previous != rows { return Err(Error::Malformed); }
            }
        }
        Ok(Self { bytes, encoding: Some(encoding), offset: 0, len: rows })
    }

    pub(crate) const fn len(self) -> usize { self.len }
    pub(crate) const fn canonical_bytes(self) -> Option<&'a [u8]> {
        if self.encoding.is_none() { Some(self.bytes) } else { None }
    }
    pub(crate) fn constant(self) -> Option<u64> {
        (self.len != 0 && self.encoding == Some(NumericEncoding::Constant))
            .then(|| u64_at(self.bytes, 0).unwrap())
    }
    fn run_end(self, run: usize) -> usize { u32_at(self.bytes, 12 + run * 12).unwrap() as usize }
    fn run_value(self, run: usize) -> u64 { u64_at(self.bytes, 4 + run * 12).unwrap() }
    fn find_run(self, row: usize) -> usize {
        let mut low = 0;
        let mut high = u32_at(self.bytes, 0).unwrap() as usize;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.run_end(mid) <= row { low = mid + 1; } else { high = mid; }
        }
        low
    }
    fn code(codes: &[u8], width: u8, row: usize) -> usize {
        if width == 0 { return 0; }
        let bit = row * usize::from(width);
        let start = bit / 8;
        let shift = bit % 8;
        let length = (shift + usize::from(width)).div_ceil(8);
        let mut word = 0u128;
        for (i, &byte) in codes[start..start + length].iter().enumerate() {
            word |= u128::from(byte) << (i * 8);
        }
        ((word >> shift) & ((1u128 << width) - 1)) as usize
    }
    /// Iterate a validated packed span, starting at any row without walking skipped rows.
    fn codes(codes: &[u8], width: u8, start: usize, count: usize) -> impl Iterator<Item = u64> + '_ {
        let bit = start * usize::from(width);
        let mut input = codes[bit / 8..].iter();
        let shift = bit % 8;
        let (mut acc, mut nbits) = if shift != 0 && count != 0 {
            (u128::from(*input.next().expect("validated packed span")) >> shift, 8 - shift)
        } else { (0, 0) };
        let width = usize::from(width);
        let mask = (1u128 << width) - 1;
        (0..count).map(move |_| {
            while nbits < width {
                acc |= u128::from(*input.next().expect("validated packed span")) << nbits;
                nbits += 8;
            }
            let value = (acc & mask) as u64;
            acc >>= width;
            nbits -= width;
            value
        })
    }
    pub(crate) fn get(self, index: usize) -> Option<u64> {
        if index >= self.len { return None; }
        let row = self.offset + index;
        Some(match self.encoding {
            None => u64_at(self.bytes, row * 8).unwrap(),
            Some(NumericEncoding::Constant) => u64_at(self.bytes, 0).unwrap(),
            Some(NumericEncoding::Dictionary) => {
                let (_, values, width, codes) = dictionary(self.bytes).unwrap();
                u64_at(values, Self::code(codes, width, row) * 8).unwrap()
            }
            Some(NumericEncoding::RunEnds) => self.run_value(self.find_run(row)),
        })
    }
    /// A cheap proof for validators; a failed proof falls back to checking referenced rows.
    pub(crate) fn encoded_values_all(self, mut check: impl FnMut(u64) -> bool) -> Option<bool> {
        Some(match self.encoding? {
            NumericEncoding::Constant => check(u64_at(self.bytes, 0).unwrap()),
            NumericEncoding::Dictionary => {
                let (_, values, _, _) = dictionary(self.bytes).unwrap();
                values.chunks_exact(8).all(|v| check(u64::from_le_bytes(v.try_into().unwrap())))
            }
            NumericEncoding::RunEnds => (0..u32_at(self.bytes, 0).unwrap() as usize)
                .all(|run| check(self.run_value(run))),
        })
    }
    pub(crate) fn slice(self, start: usize, count: usize) -> Option<Self> {
        if start.checked_add(count)? > self.len { return None; }
        if self.encoding.is_none() {
            return Some(Self::plain(&self.bytes[start * 8..(start + count) * 8]));
        }
        Some(Self { offset: self.offset + start, len: count, ..self })
    }
    /// Materialize output in one sequential pass, dispatching once per column.
    pub(crate) fn append_bits(self, output: &mut Vec<u64>) {
        output.reserve(self.len);
        match self.encoding {
            None => output.extend(self.bytes.as_chunks::<8>().0.iter().copied().map(u64::from_le_bytes)),
            Some(NumericEncoding::Constant) => output.resize(output.len() + self.len, u64_at(self.bytes, 0).unwrap()),
            Some(NumericEncoding::Dictionary) => {
                let (_, values, width, codes) = dictionary(self.bytes).unwrap();
                let values = values.as_chunks::<8>().0;
                for code in Self::codes(codes, width, self.offset, self.len) {
                    output.push(u64::from_le_bytes(values[code as usize]));
                }
            }
            Some(NumericEncoding::RunEnds) => {
                let end = self.offset + self.len;
                let mut position = self.offset;
                let mut run = self.find_run(position);
                while position < end {
                    let next = self.run_end(run).min(end);
                    output.resize(output.len() + next - position, self.run_value(run));
                    position = next;
                    run += 1;
                }
            }
        }
    }

    pub(crate) fn iter(self) -> NumericIter<'a> {
        let (front_run, back_run) = if self.len != 0 && self.encoding == Some(NumericEncoding::RunEnds) {
            (self.find_run(self.offset), self.find_run(self.offset + self.len - 1))
        } else { (0, 0) };
        NumericIter { column: self, front: 0, back: self.len, front_run, back_run }
    }

    /// OR matching rows into an existing bitmap without expanding values. Returns
    /// false for canonical columns so their existing vector loops remain available.
    pub(crate) fn or_matches(self, skip: usize, bitmap: &mut [u8], truth: u8,
        mut matches: impl FnMut(u64) -> bool) -> bool
    {
        let Some(encoding) = self.encoding else { return false; };
        let count = bitmap.len().min(self.len.saturating_sub(skip));
        if count == 0 { return true; }
        let bitmap = &mut bitmap[..count];
        let start = self.offset + skip;
        match encoding {
            NumericEncoding::Constant => {
                if matches(u64_at(self.bytes, 0).unwrap()) { bitmap.fill(truth); }
            }
            NumericEncoding::Dictionary => {
                let (_, values, width, codes) = dictionary(self.bytes).unwrap();
                let accepted: Vec<bool> = values.chunks_exact(8)
                    .map(|v| matches(u64::from_le_bytes(v.try_into().unwrap()))).collect();
                if accepted.iter().all(|value| *value) {
                    bitmap.fill(truth);
                } else if accepted.iter().any(|value| *value) {
                    // Amortize the byte lookup table only on larger spans.
                    match (width, count >= 256) {
                        (1, true) => or_dictionary_bytes::<8>(codes, start, bitmap, truth, &accepted),
                        (2, true) => or_dictionary_bytes::<4>(codes, start, bitmap, truth, &accepted),
                        (4, true) => or_dictionary_bytes::<2>(codes, start, bitmap, truth, &accepted),
                        (8, true) => or_dictionary_bytes::<1>(codes, start, bitmap, truth, &accepted),
                        _ => for (dest, code) in bitmap.iter_mut().zip(Self::codes(codes, width, start, count)) {
                            if accepted[code as usize] { *dest = truth; }
                        },
                    }
                }
            }
            NumericEncoding::RunEnds => {
                let end = start + count;
                let mut run = self.find_run(start);
                let mut position = start;
                while position < end {
                    let next = self.run_end(run).min(end);
                    if next > position && matches(self.run_value(run)) {
                        bitmap[position - start..next - start].fill(truth);
                    }
                    position = next;
                    run += 1;
                }
            }
        }
        true
    }
}

pub(crate) struct NumericIter<'a> {
    column: NumericColumn<'a>,
    front: usize,
    back: usize,
    front_run: usize,
    back_run: usize,
}
impl Iterator for NumericIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        if self.front == self.back { return None; }
        let row = self.front;
        self.front += 1;
        if self.column.encoding == Some(NumericEncoding::RunEnds) {
            while self.column.run_end(self.front_run) <= self.column.offset + row { self.front_run += 1; }
            Some(self.column.run_value(self.front_run))
        } else { self.column.get(row) }
    }
    fn size_hint(&self) -> (usize, Option<usize>) { let n = self.back - self.front; (n, Some(n)) }
}
impl DoubleEndedIterator for NumericIter<'_> {
    fn next_back(&mut self) -> Option<u64> {
        if self.front == self.back { return None; }
        self.back -= 1;
        if self.column.encoding == Some(NumericEncoding::RunEnds) {
            while self.back_run > 0 && self.column.run_end(self.back_run - 1) > self.column.offset + self.back {
                self.back_run -= 1;
            }
            Some(self.column.run_value(self.back_run))
        } else { self.column.get(self.back) }
    }
}
impl ExactSizeIterator for NumericIter<'_> {}
impl FusedIterator for NumericIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{adaptive_int, adaptive_float};

    fn check(wire: &[u8], expected: &[u64], float: bool) {
        let mut bytes = Vec::new();
        let kind = prepare(wire, expected.len(), float, &mut bytes).unwrap().unwrap();
        let column = NumericColumn::encoded(&bytes, expected.len(), kind).unwrap();
        assert_eq!(column.canonical_bytes(), None);
        assert_eq!(column.iter().collect::<Vec<_>>(), expected);
        let mut appended = vec![123];
        column.append_bits(&mut appended);
        assert_eq!(&appended[1..], expected);
        assert_eq!(column.iter().rev().collect::<Vec<_>>(), expected.iter().rev().copied().collect::<Vec<_>>());
        for (i, &value) in expected.iter().enumerate() { assert_eq!(column.get(i), Some(value)); }
        assert_eq!(column.get(expected.len()), None);
        let mut both = column.iter();
        let (mut left, mut right) = (0, expected.len());
        while left < right {
            assert_eq!(both.len(), right - left);
            assert_eq!(both.next(), Some(expected[left])); left += 1;
            if left < right { right -= 1; assert_eq!(both.next_back(), Some(expected[right])); }
        }
        assert_eq!(both.next(), None);
        assert_eq!(both.next_back(), None);
        for start in [0, expected.len() / 2, expected.len()] {
            let slice = column.slice(start, expected.len() - start).unwrap();
            assert_eq!(slice.iter().collect::<Vec<_>>(), expected[start..]);
            let mut appended = Vec::new();
            slice.append_bits(&mut appended);
            assert_eq!(appended, expected[start..]);
            for skip in [0, 1, 4, expected.len() + 1] {
                let mut actual = vec![2; 13]; // preserve previous OR operands on nonmatches
                let mut wanted = actual.clone();
                for (dest, value) in wanted.iter_mut().zip(expected[start..].iter().skip(skip)) {
                    if value & 1 == 0 { *dest = 1; }
                }
                assert!(slice.or_matches(skip, &mut actual, 1, |v| v & 1 == 0));
                assert_eq!(actual, wanted, "{kind:?}, start={start}, skip={skip}");
            }
        }
        assert!(column.slice(expected.len(), 1).is_none());
        // The established decoder is the independent oracle, including floating bits.
        let old: Vec<u64> = if float {
            adaptive_float::decode(wire, expected.len()).unwrap()
        } else { adaptive_int::decode(wire, expected.len()).unwrap().into_iter().map(|v| v as u64).collect() };
        assert_eq!(old, expected);
    }

    #[test]
    fn compact_views_preserve_values_and_bitmap_semantics() {
        for float in [false, true] {
            for value in [0, 1, u64::MAX, i64::MIN as u64, (-0.0f64).to_bits(), f64::MAX.to_bits(), 0x7ff8000000001234] {
                let mut wire = vec![if float { 3 } else { 5 }];
                wire.extend(value.to_le_bytes());
                check(&wire, &vec![value; 39], float);
                check(&wire, &[], float);
            }
            let values = [0u64, u64::MAX, i64::MIN as u64, 42, 0x7ff8000000001234];
            for width in [3, 7, 8, 9, 31, 63, 64] {
                let indices: Vec<_> = (0..39).map(|i| (i % values.len()) as u64).collect();
                let mut wire = vec![if float { 2 } else { 3 }];
                wire.extend((values.len() as u32).to_le_bytes());
                for value in values { wire.extend(value.to_le_bytes()); }
                wire.push(width);
                adaptive_int::pack_bits(&indices, width, &mut wire);
                let expected: Vec<_> = indices.iter().map(|&i| values[i as usize]).collect();
                check(&wire, &expected, float);
            }
            let mut wire = vec![4];
            let runs = [(9u64, 0u32), (1, 5), (99, 0), (2, 23), (3, 11), (4, 0)];
            wire.extend((runs.len() as u32).to_le_bytes());
            let mut expected = Vec::new();
            for (v, count) in runs { wire.extend(v.to_le_bytes()); wire.extend(count.to_le_bytes()); expected.extend(std::iter::repeat_n(v, count as usize)); }
            check(&wire, &expected, float);
        }
    }

    #[test]
    fn comparisons_scale_with_values_or_runs_not_rows() {
        let rows = 10000;
        let mut constant = vec![5]; constant.extend(42u64.to_le_bytes());
        let mut dictionary = vec![3]; dictionary.extend(2u32.to_le_bytes());
        dictionary.extend(42u64.to_le_bytes()); dictionary.extend(43u64.to_le_bytes()); dictionary.push(1);
        adaptive_int::pack_bits(&vec![0; rows], 1, &mut dictionary);
        let mut runs = vec![4]; runs.extend(2u32.to_le_bytes());
        for value in [42u64, 43] { runs.extend(value.to_le_bytes()); runs.extend((rows as u32 / 2).to_le_bytes()); }
        for (wire, comparisons) in [(constant, 1), (dictionary, 2), (runs, 2)] {
            let mut bytes = Vec::new();
            let kind = prepare(&wire, rows, false, &mut bytes).unwrap().unwrap();
            assert!(bytes.len() < rows);
            let column = NumericColumn::encoded(&bytes, rows, kind).unwrap();
            let mut calls = 0;
            let mut bitmap = vec![0; rows];
            column.or_matches(0, &mut bitmap, 1, |v| { calls += 1; v == 42 });
            assert_eq!(calls, comparisons);
            assert_eq!(bitmap, column.iter().map(|v| u8::from(v == 42)).collect::<Vec<_>>());
        }
    }

    #[test]
    fn packed_dictionary_fast_paths_match_scalar_with_slices_and_tri_states() {
        for width in [1u8, 2, 3, 4, 7, 8] {
            for entries in [1usize << width, (1usize << width) - 1] {
                let rows = 777;
                let indices: Vec<u64> = (0..rows).map(|i| ((i * 17 + i / 3) % entries) as u64).collect();
                let mut bytes = (entries as u32).to_le_bytes().to_vec();
                for value in 0..entries { bytes.extend((value as u64).to_le_bytes()); }
                bytes.push(width);
                adaptive_int::pack_bits(&indices, width, &mut bytes);
                let column = NumericColumn::encoded(&bytes, rows, NumericEncoding::Dictionary).unwrap();
                for start in 0..17 {
                    let slice = column.slice(start, rows - start).unwrap();
                    for skip in [0, 1, 7] {
                        for count in [0, 1, 255, 256, 257, rows] {
                            for mode in 0..3 {
                                let predicate = |v: u64| match mode { 0 => v % 3 == 0, 1 => true, _ => false };
                                let mut actual: Vec<u8> = (0..count).map(|i| (i % 3) as u8).collect();
                                let mut expected = actual.clone();
                                for (dest, &value) in expected.iter_mut().zip(indices.iter().skip(start + skip)) {
                                    if predicate(value) { *dest = 1; }
                                }
                                assert!(slice.or_matches(skip, &mut actual, 1, predicate));
                                assert_eq!(actual, expected, "width={width} entries={entries} start={start} skip={skip} count={count}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dictionary_validation_proofs_still_check_spans_codes_and_padding() {
        for width in [1u8, 2, 4, 8] {
            let rows = 513usize;
            let full_space = 1usize << width;
            let mut codes = Vec::new();
            adaptive_int::pack_bits(&vec![0; rows], width, &mut codes);
            let tail_bits = (rows * usize::from(width)) % 8;
            if tail_bits != 0 { *codes.last_mut().unwrap() |= !((1u8 << tail_bits) - 1); }
            for entries in [full_space - 1, full_space] {
                assert!(validate_dictionary_codes(&codes, width, rows, entries).is_ok());
                assert!(validate_dictionary_codes(&codes[..codes.len() - 1], width, rows, entries).is_err());
            }
            for row in [0, 101, rows - 1] {
                let mut bad = codes.clone();
                let bit = row * usize::from(width);
                bad[bit / 8] |= ((full_space - 1) as u8) << (bit % 8);
                assert!(validate_dictionary_codes(&bad, width, rows, full_space - 1).is_err(), "width={width}, row={row}");
            }
        }
        assert!(validate_dictionary_codes(&[], 65, 0, 1).is_err());
        assert!(validate_dictionary_codes(&[], 0, 1000, 0).is_err());
        assert!(validate_dictionary_codes(&[], 0, 1000, 1).is_ok());
        assert!(validate_dictionary_codes(&[], 64, usize::MAX, 1).is_err());
    }

    #[test]
    fn malformed_compact_columns_are_rejected_without_expansion() {
        let mut constant = vec![5]; constant.extend(42u64.to_le_bytes());
        let mut dictionary = vec![3]; dictionary.extend(1u32.to_le_bytes()); dictionary.extend(42u64.to_le_bytes()); dictionary.push(1); dictionary.push(0);
        let mut runs = vec![4]; runs.extend(1u32.to_le_bytes()); runs.extend(42u64.to_le_bytes()); runs.extend(1u32.to_le_bytes());
        for wire in [&constant, &dictionary, &runs] {
            for end in 1..wire.len() {
                assert!(prepare(&wire[..end], 1, false, &mut Vec::new()).is_err(), "{wire:?} at {end}");
            }
        }
        dictionary[14] = 1; // code outside the one-entry dictionary
        let mut bytes = Vec::new();
        let kind = prepare(&dictionary, 1, false, &mut bytes).unwrap().unwrap();
        assert!(NumericColumn::encoded(&bytes, 1, kind).is_err());
        dictionary[13] = 65;
        assert!(prepare(&dictionary, 1, false, &mut Vec::new()).is_err());
        for count in [0u32, 2, u32::MAX] {
            runs[13..17].copy_from_slice(&count.to_le_bytes());
            assert!(prepare(&runs, 1, false, &mut Vec::new()).is_err());
        }
        let mut bytes = vec![2, 0, 0, 0];
        for end in [3u32, 2] { bytes.extend(0u64.to_le_bytes()); bytes.extend(end.to_le_bytes()); }
        assert!(NumericColumn::encoded(&bytes, 3, NumericEncoding::RunEnds).is_err());
        let mut unchanged = vec![1, 2, 3];
        assert_eq!(prepare(&[0], 100, false, &mut unchanged).unwrap(), None);
        assert_eq!(unchanged, [1, 2, 3]);
    }
}
