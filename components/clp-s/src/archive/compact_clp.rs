//! Borrow adaptive CLP payloads; prepare only codecs that require random-access conversion.
use super::ColumnLimits;
use crate::adaptive_int::AdaptiveDecodeError as Error;
use crate::encoded_numeric::{NumericColumn, NumericEncoding};
use std::ops::Range;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Storage {
    Delta {
        range: Range<usize>,
        checkpoints: std::sync::OnceLock<Vec<u64>>,
    },
    Borrowed {
        range: Range<usize>,
        encoding: Option<NumericEncoding>,
    },
    Prepared {
        bytes: Vec<u8>,
        encoding: Option<NumericEncoding>,
    },
}

impl Storage {
    fn prepare_descriptors(stored: &[u8], range: Range<usize>, rows: usize) -> Result<Self, Error> {
        if rows > 0 && stored.get(range.start) == Some(&2) {
            let range = range.start + 1..range.end;
            let checkpoints = std::sync::OnceLock::new();
            super::clp_descriptors::Descriptors::delta(
                stored.get(range.clone()).ok_or(Error::Truncated)?,
                rows,
                &checkpoints,
            )?;
            return Ok(Self::Delta { range, checkpoints });
        }
        Self::prepare(stored, range, rows)
    }
    fn descriptors<'a>(
        &'a self,
        stored: &'a [u8],
        rows: usize,
    ) -> Result<super::clp_descriptors::Descriptors<'a>, Error> {
        match self {
            Self::Delta { range, checkpoints } => super::clp_descriptors::Descriptors::delta(
                stored.get(range.clone()).ok_or(Error::Truncated)?,
                rows,
                checkpoints,
            ),
            _ => self
                .view(stored, rows)
                .map(super::clp_descriptors::Descriptors::Numeric),
        }
    }

    fn prepare(stored: &[u8], range: Range<usize>, rows: usize) -> Result<Self, Error> {
        let bytes = stored.get(range.clone()).ok_or(Error::Truncated)?;
        if rows == 0 && bytes.is_empty() {
            return Ok(Self::Borrowed {
                range,
                encoding: None,
            });
        }
        let (&tag, _) = bytes.split_first().ok_or(Error::Truncated)?;
        if matches!(tag, 0 | 1 | 3 | 5) {
            let end = if tag == 0 {
                range
                    .start
                    .checked_add(1)
                    .and_then(|n| rows.checked_mul(8).and_then(|size| n.checked_add(size)))
                    .ok_or(Error::Malformed)?
            } else {
                range.end
            };
            if end > range.end {
                return Err(Error::Truncated);
            }
            return Ok(Self::Borrowed {
                range: range.start + 1..end,
                encoding: crate::encoded_numeric::encoding(bytes, false),
            });
        }
        let mut output = Vec::new();
        let encoding = crate::encoded_numeric::prepare(bytes, rows, false, &mut output)?;
        if encoding.is_none() {
            crate::adaptive_int::decode_to_le(bytes, rows, &mut output)?;
        }
        Ok(Self::Prepared {
            bytes: output,
            encoding,
        })
    }

    fn view<'a>(&'a self, stored: &'a [u8], rows: usize) -> Result<NumericColumn<'a>, Error> {
        let (bytes, encoding) = match self {
            Self::Delta { .. } => return Err(Error::Malformed),
            Self::Borrowed { range, encoding } => (
                stored.get(range.clone()).ok_or(Error::Truncated)?,
                *encoding,
            ),
            Self::Prepared { bytes, encoding } => (bytes.as_slice(), *encoding),
        };
        match encoding {
            Some(encoding) => NumericColumn::encoded(bytes, rows, encoding),
            None if bytes.len() == rows.checked_mul(8).ok_or(Error::Malformed)? => {
                Ok(NumericColumn::plain(bytes))
            }
            None => Err(Error::Malformed),
        }
    }
}

/// Reader-only views for a CLP string column. Archive bytes and numeric tags stay unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClpColumnLayout {
    descriptors: Storage,
    variables: Storage,
    pub(crate) variable_count: usize,
}

impl ClpColumnLayout {
    pub(crate) fn prepare(stored: &[u8], rows: usize, limits: ColumnLimits) -> Result<Self, Error> {
        let read = |at: usize| {
            let end = at.checked_add(8).ok_or(Error::Malformed)?;
            Ok::<_, Error>(u64::from_le_bytes(
                stored
                    .get(at..end)
                    .ok_or(Error::Truncated)?
                    .try_into()
                    .unwrap(),
            ))
        };
        let length = usize::try_from(read(0)?).map_err(|_| Error::Malformed)?;
        let end = length.checked_add(8).ok_or(Error::Malformed)?;
        let count = read(end)?;
        if count > limits.max_encoded_variables_per_column() || count > 1u64 << 40 {
            return Err(Error::Malformed);
        }
        let count = usize::try_from(count).map_err(|_| Error::Malformed)?;
        let start = end.checked_add(8).ok_or(Error::Malformed)?;
        if count == 0 && start != stored.len() {
            return Err(Error::Malformed);
        }
        Ok(Self {
            descriptors: Storage::prepare_descriptors(stored, 8..end, rows)?,
            variables: Storage::prepare(stored, start..stored.len(), count)?,
            variable_count: count,
        })
    }

    pub(crate) fn logical_size(&self, rows: usize) -> Result<usize, Error> {
        rows.checked_add(1)
            .and_then(|n| n.checked_add(self.variable_count))
            .and_then(|n| n.checked_mul(8))
            .ok_or(Error::Malformed)
    }

    pub(crate) fn views<'a>(
        &'a self,
        stored: &'a [u8],
        rows: usize,
    ) -> Result<(super::clp_descriptors::Descriptors<'a>, NumericColumn<'a>), Error> {
        Ok((
            self.descriptors.descriptors(stored, rows)?,
            self.variables.view(stored, self.variable_count)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(descriptors: &[u8], variables: &[u8], count: usize) -> Vec<u8> {
        let mut bytes = (descriptors.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(descriptors);
        bytes.extend_from_slice(&(count as u64).to_le_bytes());
        bytes.extend_from_slice(variables);
        bytes
    }

    #[test]
    fn views_match_every_adaptive_codec_and_borrow_plain_variables() {
        for values in [vec![0, -1, i64::MIN, i64::MAX, 12, 12, 12, 3], vec![7; 64]] {
            let descriptors: Vec<_> = (0..values.len()).map(|i| ((i as i64) << 24) | 3).collect();
            for d in crate::adaptive_int::candidates(&descriptors) {
                for v in crate::adaptive_int::candidates(&values) {
                    let bytes = pack(&d, &v, values.len());
                    let layout = ClpColumnLayout::prepare(
                        &bytes,
                        descriptors.len(),
                        ColumnLimits::default(),
                    )
                    .unwrap();
                    let (ds, vs) = layout.views(&bytes, descriptors.len()).unwrap();
                    assert_eq!(ds.iter().map(|v| v as i64).collect::<Vec<_>>(), descriptors);
                    assert_eq!(vs.iter().map(|v| v as i64).collect::<Vec<_>>(), values);
                    if v[0] == 0 {
                        assert_eq!(vs.bytes.as_ptr(), bytes[16 + d.len() + 1..].as_ptr());
                    }
                    // Truncation must fail before a view can expose invalid memory.
                    for end in 0..bytes.len() {
                        let truncated = &bytes[..end];
                        assert!(
                            ClpColumnLayout::prepare(
                                truncated,
                                descriptors.len(),
                                ColumnLimits::default()
                            )
                            .and_then(|layout| layout
                                .views(truncated, descriptors.len())
                                .map(|_| ()))
                            .is_err(),
                            "truncation={end} descriptor_codec={} variable_codec={}",
                            d[0],
                            v[0]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn no_variables_and_bounds_are_preserved() {
        let descriptors = crate::adaptive_int::candidates(&[0, 0]).next().unwrap();
        let bytes = pack(&descriptors, &[], 0);
        let layout = ClpColumnLayout::prepare(&bytes, 2, ColumnLimits::default()).unwrap();
        assert!(layout.views(&bytes, 2).unwrap().1.len() == 0);
        assert_eq!(layout.logical_size(2).unwrap(), 24);
        assert!(layout.logical_size(usize::MAX).is_err());
        let bytes = pack(&descriptors, &[0], 1u64.wrapping_shl(40) as usize + 1);
        assert!(ClpColumnLayout::prepare(&bytes, 2, ColumnLimits::default()).is_err());
    }
}
