//! Optional per-table timestamp chunk index. Versioned metadata packet 0x81.
//!
//! For every timestamp column of every schema table, the minimum and maximum epoch nanoseconds of
//! each run of `chunk_rows` consecutive rows, in row order. A query's time window then bounds the
//! rows that can match: a table none of whose chunks meets the window is skipped, and the rest are
//! evaluated only over the span from the first to the last chunk that meets it. The columns are
//! stored as before, so the index costs only its bounds. Readers that predate it skip the packet.
use serde::{Deserialize, Serialize};

pub const PACKET_TYPE: u8 = 0x81;

/// Rows per chunk the writer uses. Readers take the value recorded in each packet.
pub const CHUNK_ROWS: u32 = 65_536;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TimestampChunks {
    pub version: u32,
    pub chunk_rows: u32,
    pub columns: Vec<ChunkColumn>,
}

/// One timestamp column of one schema table: `[min, max]` epoch nanoseconds per chunk.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChunkColumn {
    pub schema: i32,
    pub node: u32,
    pub bounds: Vec<[i64; 2]>,
}

/// The rows of one table a time window can reach.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowSpan {
    /// No usable index: every row is a candidate.
    All,
    /// No chunk meets the window: no row can match.
    Empty,
    /// Only rows in this half-open range can match.
    Rows(usize, usize),
}

impl RowSpan {
    /// The rows both spans allow.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, span) | (span, Self::All) => span,
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (Self::Rows(a, b), Self::Rows(c, d)) => {
                let (start, end) = (a.max(c), b.min(d));
                if start < end { Self::Rows(start, end) } else { Self::Empty }
            }
        }
    }
}

impl TimestampChunks {
    /// Decodes a packet. Unknown versions, a zero chunk size and trailing bytes are rejected.
    ///
    /// # Errors
    ///
    /// Returns a description of the malformed or unsupported packet.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut decoder = rmp_serde::Deserializer::new(std::io::Cursor::new(bytes));
        let chunks = Self::deserialize(&mut decoder).map_err(|error| error.to_string())?;
        if decoder.position() != bytes.len() as u64 {
            return Err("trailing timestamp chunk bytes".into());
        }
        if chunks.version != 1 {
            return Err("unsupported timestamp chunk version".into());
        }
        if chunks.chunk_rows == 0 {
            return Err("timestamp chunk size is zero".into());
        }
        Ok(chunks)
    }

    /// Encodes this index as a packet payload.
    ///
    /// # Errors
    ///
    /// Returns a description of a serialization failure.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        rmp_serde::to_vec(self).map_err(|error| error.to_string())
    }

    /// The `[min, max]` of each run of `chunk_rows` epochs, in order.
    #[must_use]
    pub fn bounds(epochs: impl IntoIterator<Item = i64>, chunk_rows: u32) -> Vec<[i64; 2]> {
        let mut bounds = Vec::new();
        let mut current: Option<[i64; 2]> = None;
        let mut rows = 0;
        for epoch in epochs {
            let chunk = current.get_or_insert([epoch, epoch]);
            chunk[0] = chunk[0].min(epoch);
            chunk[1] = chunk[1].max(epoch);
            rows += 1;
            if rows == chunk_rows {
                bounds.extend(current.take());
                rows = 0;
            }
        }
        bounds.extend(current);
        bounds
    }

    /// The column of `schema` holding `node`, if indexed.
    #[must_use]
    pub fn column(&self, schema: i32, node: u32) -> Option<&ChunkColumn> {
        self.columns.iter().find(|column| column.schema == schema && column.node == node)
    }

    /// The rows of a `rows`-row table whose `node` values can lie in `[low, high]`.
    #[must_use]
    pub fn row_span(&self, schema: i32, node: u32, rows: usize, low: i64, high: i64) -> RowSpan {
        let Some(column) = self.column(schema, node) else {
            return RowSpan::All;
        };
        let chunk_rows = self.chunk_rows as usize;
        // An index that does not describe exactly this table's rows is ignored.
        if column.bounds.len() != rows.div_ceil(chunk_rows) {
            return RowSpan::All;
        }
        if low > high {
            return RowSpan::Empty;
        }
        let mut overlapping = column
            .bounds
            .iter()
            .enumerate()
            .filter(|(_, [min, max])| *min <= high && *max >= low)
            .map(|(chunk, _)| chunk);
        let Some(first) = overlapping.next() else {
            return RowSpan::Empty;
        };
        let last = overlapping.last().unwrap_or(first);
        RowSpan::Rows(first * chunk_rows, ((last + 1) * chunk_rows).min(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_cover_each_run_of_rows_and_the_last_partial_run() {
        assert_eq!(vec![[3, 9], [1, 7], [8, 8]], TimestampChunks::bounds([5, 3, 9, 1, 7, 2, 8], 3));
        assert_eq!(vec![[3, 9], [1, 8]], TimestampChunks::bounds([5, 3, 9, 1, 7, 8], 3));
        assert!(TimestampChunks::bounds([], 3).is_empty());
    }

    #[test]
    fn row_spans_cover_exactly_the_chunks_that_meet_the_window() {
        // Ten rows in chunks of four: [0,4) holds 10..=13, [4,8) 20..=23, [8,10) 30..=31.
        let epochs = [10, 11, 12, 13, 20, 21, 22, 23, 30, 31];
        let chunks = TimestampChunks {
            version: 1,
            chunk_rows: 4,
            columns: vec![ChunkColumn { schema: 7, node: 2, bounds: TimestampChunks::bounds(epochs, 4) }],
        };
        assert_eq!(RowSpan::Rows(4, 8), chunks.row_span(7, 2, 10, 21, 21));
        assert_eq!(RowSpan::Rows(0, 8), chunks.row_span(7, 2, 10, 13, 20));
        assert_eq!(RowSpan::Rows(8, 10), chunks.row_span(7, 2, 10, 31, i64::MAX));
        assert_eq!(RowSpan::Rows(0, 10), chunks.row_span(7, 2, 10, i64::MIN, i64::MAX));
        assert_eq!(RowSpan::Empty, chunks.row_span(7, 2, 10, 14, 19));
        assert_eq!(RowSpan::Empty, chunks.row_span(7, 2, 10, 40, 30));
        // Another table, another column, or a row count the bounds do not describe: no index.
        assert_eq!(RowSpan::All, chunks.row_span(8, 2, 10, 21, 21));
        assert_eq!(RowSpan::All, chunks.row_span(7, 3, 10, 21, 21));
        assert_eq!(RowSpan::All, chunks.row_span(7, 2, 13, 21, 21));
        assert_eq!(RowSpan::Rows(4, 6), RowSpan::Rows(2, 6).intersect(RowSpan::Rows(4, 9)));
        assert_eq!(RowSpan::Empty, RowSpan::Rows(0, 4).intersect(RowSpan::Rows(4, 9)));
        assert_eq!(RowSpan::Rows(1, 2), RowSpan::All.intersect(RowSpan::Rows(1, 2)));
        let decoded = TimestampChunks::decode(&chunks.encode().unwrap()).unwrap();
        assert_eq!(chunks, decoded);
        assert!(TimestampChunks::decode(&TimestampChunks { version: 2, ..chunks.clone() }.encode().unwrap()).is_err());
        assert!(TimestampChunks::decode(&TimestampChunks { chunk_rows: 0, ..chunks }.encode().unwrap()).is_err());
    }
}
