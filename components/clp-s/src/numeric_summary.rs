//! Optional compression-time numeric summaries. Versioned metadata packet 0x80.
//! Integer buckets preserve typed eligibility; sums use checked signed 128-bit arithmetic.
use serde::{Deserialize, Serialize};

pub const PACKET_TYPE: u8 = 0x80;
pub fn words(v: i128) -> [u64; 2] {
    [v as u64, (v >> 64) as u64]
}
pub fn integer(v: [u64; 2]) -> i128 {
    (i128::from(v[1] as i64) << 64) | i128::from(v[0])
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IntegerSummary {
    pub count: u64,
    pub sum: [u64; 2],
    pub min: [u64; 2],
    pub max: [u64; 2],
}
impl IntegerSummary {
    pub fn add(&mut self, v: i128) -> Option<()> {
        let sum = integer(self.sum).checked_add(v)?;
        if self.count == 0 || v < integer(self.min) {
            self.min = words(v);
        }
        if self.count == 0 || v > integer(self.max) {
            self.max = words(v);
        }
        self.count = self.count.checked_add(1)?;
        self.sum = words(sum);
        Some(())
    }
    pub fn merge(&mut self, other: &Self) -> Option<()> {
        if other.count == 0 {
            return Some(());
        }
        let sum = integer(self.sum).checked_add(integer(other.sum))?;
        if self.count == 0 || integer(other.min) < integer(self.min) {
            self.min = other.min;
        }
        if self.count == 0 || integer(other.max) > integer(self.max) {
            self.max = other.max;
        }
        self.count = self.count.checked_add(other.count)?;
        self.sum = words(sum);
        Some(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FloatSummary {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}
impl FloatSummary {
    pub fn add(&mut self, v: f64) -> Option<()> {
        // DuckDB orders NaN above all finite values and infinities.
        if self.count == 0 || self.min.is_nan() || v < self.min {
            self.min = v;
        }
        if self.count == 0 || v.is_nan() || v > self.max {
            self.max = v;
        }
        self.count = self.count.checked_add(1)?;
        self.sum += v;
        Some(())
    }
    pub fn merge(&mut self, other: &Self) -> Option<()> {
        if other.count == 0 {
            return Some(());
        }
        if self.count == 0 || self.min.is_nan() || other.min < self.min {
            self.min = other.min;
        }
        if self.count == 0 || other.max.is_nan() || other.max > self.max {
            self.max = other.max;
        }
        self.count = self.count.checked_add(other.count)?;
        self.sum += other.sum;
        Some(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ColumnSummary {
    pub schema: i32,
    pub node: u32,
    pub negative: IntegerSummary,
    pub nonnegative: IntegerSummary,
    pub unsigned: IntegerSummary,
    pub float: FloatSummary,
}
impl ColumnSummary {
    pub fn add_integer(&mut self, v: i128) -> Option<()> {
        if v < 0 {
            self.negative.add(v)
        } else if v > i128::from(i64::MAX) {
            self.unsigned.add(v)
        } else {
            self.nonnegative.add(v)
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NumericSummaries {
    pub version: u32,
    pub columns: Vec<ColumnSummary>,
}

impl NumericSummaries {
    /// Decode a bounded optional packet. Unknown versions are not usable.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut decoder = rmp_serde::Deserializer::new(std::io::Cursor::new(bytes));
        let s = Self::deserialize(&mut decoder).map_err(|e| e.to_string())?;
        if decoder.position() != bytes.len() as u64 {
            return Err("trailing numeric summary bytes".into());
        }
        if s.version != 1 {
            return Err("unsupported numeric summary version".into());
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_writer_emits_summary_metadata() {
        use crate::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
        use std::io::Cursor;
        let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        let fields = [FieldRef::new(b"v", ValueRef::I64(42))];
        writer.append_record(RecordRef::new(&fields)).unwrap();
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        let mut reader = crate::archive::SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
        let metadata = reader
            .read_metadata(crate::archive::MetadataLimits::default())
            .unwrap();
        let summaries = NumericSummaries::decode(metadata.numeric_summaries().unwrap()).unwrap();
        assert!(
            summaries
                .columns
                .iter()
                .any(|s| s.nonnegative.count == 1 && integer(s.nonnegative.sum) == 42)
        );
    }
    #[test]
    fn signed_unsigned_buckets_and_exact_merge() {
        let mut s = ColumnSummary::default();
        for v in [
            i128::from(i64::MIN),
            -1,
            0,
            i128::from(i64::MAX),
            i128::from(u64::MAX),
        ] {
            s.add_integer(v).unwrap();
        }
        assert_eq!(s.negative.count, 2);
        assert_eq!(s.nonnegative.count, 2);
        assert_eq!(s.unsigned.count, 1);
        let mut all = IntegerSummary::default();
        for bucket in [&s.negative, &s.nonnegative, &s.unsigned] {
            all.merge(bucket).unwrap();
        }
        assert_eq!(all.count, 5);
        assert_eq!(integer(all.sum), i128::from(u64::MAX) - 2);
        assert_eq!(integer(all.min), i128::from(i64::MIN));
        assert_eq!(integer(all.max), i128::from(u64::MAX));
    }
    #[test]
    fn version_trailing_bytes_and_checked_overflow() {
        let mut bytes = rmp_serde::to_vec(&NumericSummaries {
            version: 1,
            columns: vec![],
        })
        .unwrap();
        assert!(NumericSummaries::decode(&bytes).is_ok());
        bytes.push(0);
        assert!(NumericSummaries::decode(&bytes).is_err());
        assert!(
            NumericSummaries::decode(
                &rmp_serde::to_vec(&NumericSummaries {
                    version: 2,
                    columns: vec![]
                })
                .unwrap()
            )
            .is_err()
        );
        let mut s = IntegerSummary {
            count: 1,
            sum: words(i128::MAX),
            ..Default::default()
        };
        assert!(s.add(1).is_none());
    }
    #[test]
    fn float_nan_order_matches_duckdb() {
        let mut s = FloatSummary::default();
        for v in [f64::NAN, 1.0, -2.0] {
            s.add(v).unwrap();
        }
        assert_eq!(s.min, -2.0);
        assert!(s.max.is_nan());
        assert!(s.sum.is_nan());
    }
}
