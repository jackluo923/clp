use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::iter::FusedIterator;

use crate::LogOrderColumn;
use crate::LogOrderCursor;

const MAX_LOG_EVENT_COUNT: u64 = (i64::MAX as u64) + 1;

/// One decoded table participating in an ordered row merge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrderedMergeTable<'table> {
    table_index: usize,
    log_order: LogOrderColumn<'table>,
}

impl<'table> OrderedMergeTable<'table> {
    /// Associates a stable archive table index with its validated log-order column.
    #[must_use]
    pub const fn new(table_index: usize, log_order: LogOrderColumn<'table>) -> Self {
        Self {
            table_index,
            log_order,
        }
    }

    /// Returns the stable archive table index reported in merged rows.
    #[must_use]
    pub const fn table_index(self) -> usize {
        self.table_index
    }

    /// Returns the table's zero-copy log-order column.
    #[must_use]
    pub const fn log_order(self) -> LogOrderColumn<'table> {
        self.log_order
    }
}

/// Resource bounds for constructing an [`OrderedRowMerge`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrderedMergeLimits {
    tables: u64,
    records: u64,
}

impl OrderedMergeLimits {
    /// Creates explicit table and aggregate-record bounds.
    #[must_use]
    pub const fn new(max_tables: u64, max_records: u64) -> Self {
        Self {
            tables: max_tables,
            records: max_records,
        }
    }

    /// Returns the maximum number of participating tables.
    #[must_use]
    pub const fn max_tables(self) -> u64 {
        self.tables
    }

    /// Returns the maximum aggregate record count.
    #[must_use]
    pub const fn max_records(self) -> u64 {
        self.records
    }
}

impl Default for OrderedMergeLimits {
    fn default() -> Self {
        Self::new(1024 * 1024, 128 * 1024 * 1024)
    }
}

/// Resource identified by an ordered-merge construction error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OrderedMergeResource {
    /// Per-table cursor state.
    Tables,
    /// Binary-heap heads, bounded by the table count.
    HeapHeads,
    /// Aggregate rows across participating tables.
    Records,
}

impl Display for OrderedMergeResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tables => "ordered-merge tables",
            Self::HeapHeads => "ordered-merge heap heads",
            Self::Records => "ordered-merge records",
        })
    }
}

/// One globally ordered row coordinate.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OrderedRow {
    table_index: usize,
    row_index: usize,
    log_event_idx: i64,
}

impl OrderedRow {
    /// Returns the stable archive table index.
    #[must_use]
    pub const fn table_index(self) -> usize {
        self.table_index
    }

    /// Returns the zero-based row within that table.
    #[must_use]
    pub const fn row_index(self) -> usize {
        self.row_index
    }

    /// Returns the archive-local canonical event index.
    #[must_use]
    pub const fn log_event_idx(self) -> i64 {
        self.log_event_idx
    }
}

/// Failure to construct or advance an ordered row merge.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OrderedMergeError {
    /// A configured bound was exceeded before allocating or merging.
    LimitExceeded {
        /// Bounded resource.
        resource: OrderedMergeResource,
        /// Actual requested amount.
        actual: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// A bounded construction allocation failed.
    AllocationFailed {
        /// State being allocated.
        resource: OrderedMergeResource,
        /// Requested element count.
        requested: usize,
    },
    /// Checked count arithmetic or conversion overflowed.
    SizeOverflow,
    /// The aggregate row domain cannot be represented by nonnegative signed 64-bit indexes.
    RecordDomainTooLarge {
        /// Aggregate row count.
        actual: u64,
        /// Largest representable canonical row count.
        maximum: u64,
    },
    /// Two input columns claim the same stable table index.
    DuplicateTableIndex {
        /// Repeated table index.
        table_index: usize,
    },
    /// A cursor's exact length disagrees with its source column.
    CursorLengthMismatch {
        /// Stable table index.
        table_index: usize,
        /// Source column row count.
        expected: usize,
        /// Cursor row count.
        actual: usize,
    },
    /// A table's next index is not strictly greater than its preceding index.
    TableOrderViolation {
        /// Stable table index.
        table_index: usize,
        /// Row containing the invalid index.
        row_index: usize,
        /// Previous row's index.
        previous: i64,
        /// Invalid next index.
        current: i64,
    },
    /// A merged index is negative.
    NegativeLogEventIndex {
        /// Stable table index.
        table_index: usize,
        /// Row containing the invalid index.
        row_index: usize,
        /// Negative index.
        actual: i64,
    },
    /// A nonnegative index repeats one already emitted by the merge.
    DuplicateLogEventIndex {
        /// Stable table index of the later occurrence.
        table_index: usize,
        /// Row containing the later occurrence.
        row_index: usize,
        /// Repeated index.
        log_event_idx: i64,
    },
    /// The next smallest index skips the required contiguous value.
    LogEventIndexGap {
        /// Stable table index of the row after the gap.
        table_index: usize,
        /// Row containing the index after the gap.
        row_index: usize,
        /// Required next index.
        expected: u64,
        /// Actual next index.
        actual: i64,
    },
    /// An internal pre-reserved-state invariant was violated.
    InternalInvariant {
        /// Specific invariant.
        invariant: OrderedMergeInvariant,
    },
}

impl Display for OrderedMergeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                resource,
                actual,
                limit,
            } => write!(formatter, "{resource} {actual} exceeds limit {limit}"),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "could not reserve {requested} elements for {resource}"
            ),
            Self::SizeOverflow => formatter.write_str("ordered-merge size overflow"),
            Self::RecordDomainTooLarge { actual, maximum } => write!(
                formatter,
                "ordered-merge record count {actual} exceeds signed index domain {maximum}"
            ),
            Self::DuplicateTableIndex { table_index } => {
                write!(formatter, "ordered merge repeats table index {table_index}")
            }
            Self::CursorLengthMismatch {
                table_index,
                expected,
                actual,
            } => write!(
                formatter,
                "table {table_index} log-order column has {expected} rows but its cursor has \
                 {actual}"
            ),
            Self::TableOrderViolation {
                table_index,
                row_index,
                previous,
                current,
            } => write!(
                formatter,
                "table {table_index} log-order row {row_index} index {current} does not strictly \
                 follow {previous}"
            ),
            Self::NegativeLogEventIndex {
                table_index,
                row_index,
                actual,
            } => write!(
                formatter,
                "table {table_index} log-order row {row_index} has negative index {actual}"
            ),
            Self::DuplicateLogEventIndex {
                table_index,
                row_index,
                log_event_idx,
            } => write!(
                formatter,
                "table {table_index} log-order row {row_index} repeats index {log_event_idx}"
            ),
            Self::LogEventIndexGap {
                table_index,
                row_index,
                expected,
                actual,
            } => write!(
                formatter,
                "table {table_index} log-order row {row_index} has index {actual}, expected \
                 contiguous index {expected}"
            ),
            Self::InternalInvariant { invariant } => {
                write!(
                    formatter,
                    "ordered-merge internal invariant failed: {invariant}"
                )
            }
        }
    }
}

impl Error for OrderedMergeError {}

/// Internal invariant named in [`OrderedMergeError::InternalInvariant`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OrderedMergeInvariant {
    /// A heap head referenced absent cursor state.
    HeapStateIndex,
    /// Remaining rows reached zero before a heap head was consumed.
    RemainingRows,
    /// The canonical next-index counter overflowed.
    ExpectedIndex,
    /// A cursor reported more remaining rows than its source column.
    CursorRows,
    /// A heap insertion would exceed the capacity reserved during construction.
    HeapCapacity,
    /// Rows remain but no heap head or deferred error can produce them.
    MissingHeapHead,
}

impl Display for OrderedMergeInvariant {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HeapStateIndex => "heap state index is out of bounds",
            Self::RemainingRows => "remaining row count underflow",
            Self::ExpectedIndex => "expected log-event index overflow",
            Self::CursorRows => "cursor remaining rows exceed its source column",
            Self::HeapCapacity => "heap exceeded its pre-reserved capacity",
            Self::MissingHeapHead => "remaining rows have no heap head",
        })
    }
}

/// Bounded k-way merge of decoded tables in canonical archive-local event order.
///
/// Construction allocates all cursor and heap storage. Advancing performs no allocations and has
/// `O(log tables)` heap cost per row. Each item is either one row coordinate or the single terminal
/// validation error; after an error the iterator is fused.
#[derive(Debug)]
pub struct OrderedRowMerge<'table> {
    core: MergeCore<LogOrderCursor<'table>>,
}

impl<'table> OrderedRowMerge<'table> {
    /// Constructs a bounded merge from one log-order column per participating table.
    ///
    /// # Errors
    ///
    /// Returns a typed error before iteration when table/record limits are exceeded, counts cannot
    /// be represented, a table index repeats, a source cursor length is incoherent, or bounded
    /// state allocation fails. Value-order corruption is reported lazily by [`Iterator::next`].
    pub fn new(
        tables: &[OrderedMergeTable<'table>],
        limits: OrderedMergeLimits,
    ) -> Result<Self, OrderedMergeError> {
        let core = build_merge(tables, limits, |table| {
            (
                table.table_index,
                table.log_order.cursor(),
                table.log_order.len(),
            )
        })?;
        Ok(Self { core })
    }

    /// Returns the number of participating tables, including empty tables.
    #[must_use]
    pub const fn table_count(&self) -> usize {
        self.core.table_count
    }

    /// Returns the checked aggregate row count supplied at construction.
    #[must_use]
    pub const fn total_rows(&self) -> usize {
        self.core.total_rows
    }

    /// Returns unconsumed data rows assuming the remaining input is valid.
    ///
    /// This becomes zero when a validation error is emitted. It is not an exact iterator-item
    /// count because a future error fuses the iterator before all corrupt rows are yielded.
    #[must_use]
    pub const fn remaining_rows(&self) -> usize {
        self.core.remaining_rows
    }
}

impl Iterator for OrderedRowMerge<'_> {
    type Item = Result<OrderedRow, OrderedMergeError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.core.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.core.size_hint()
    }
}

impl FusedIterator for OrderedRowMerge<'_> {}

#[derive(Debug)]
struct TableState<C> {
    table_index: usize,
    row_count: usize,
    cursor: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeapHead {
    log_event_idx: i64,
    table_index: usize,
    row_index: usize,
    state_index: usize,
}

impl Ord for HeapHead {
    fn cmp(&self, other: &Self) -> Ordering {
        self.log_event_idx
            .cmp(&other.log_event_idx)
            .then_with(|| self.table_index.cmp(&other.table_index))
            .then_with(|| self.row_index.cmp(&other.row_index))
            .then_with(|| self.state_index.cmp(&other.state_index))
    }
}

impl PartialOrd for HeapHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
struct MergeCore<C> {
    states: Vec<TableState<C>>,
    heap: BinaryHeap<Reverse<HeapHead>>,
    pending_error: Option<OrderedMergeError>,
    table_count: usize,
    total_rows: usize,
    remaining_rows: usize,
    next_expected: u64,
    failed: bool,
}

impl<C> MergeCore<C>
where
    C: ExactSizeIterator<Item = i64>,
{
    fn terminal_error(
        &mut self,
        error: OrderedMergeError,
    ) -> Result<OrderedRow, OrderedMergeError> {
        self.failed = true;
        self.pending_error = None;
        self.remaining_rows = 0;
        self.heap.clear();
        Err(error)
    }

    fn advance_head(&mut self, head: HeapHead) -> Result<(), OrderedMergeError> {
        let state =
            self.states
                .get_mut(head.state_index)
                .ok_or(OrderedMergeError::InternalInvariant {
                    invariant: OrderedMergeInvariant::HeapStateIndex,
                })?;
        let row_index = state.row_count.checked_sub(state.cursor.len()).ok_or(
            OrderedMergeError::InternalInvariant {
                invariant: OrderedMergeInvariant::CursorRows,
            },
        )?;
        let Some(log_event_idx) = state.cursor.next() else {
            return Ok(());
        };
        if log_event_idx <= head.log_event_idx {
            self.pending_error = Some(OrderedMergeError::TableOrderViolation {
                table_index: state.table_index,
                row_index,
                previous: head.log_event_idx,
                current: log_event_idx,
            });
            return Ok(());
        }
        if self.heap.len() == self.heap.capacity() {
            return Err(OrderedMergeError::InternalInvariant {
                invariant: OrderedMergeInvariant::HeapCapacity,
            });
        }
        self.heap.push(Reverse(HeapHead {
            log_event_idx,
            table_index: state.table_index,
            row_index,
            state_index: head.state_index,
        }));
        Ok(())
    }
}

impl<C> Iterator for MergeCore<C>
where
    C: ExactSizeIterator<Item = i64>,
{
    type Item = Result<OrderedRow, OrderedMergeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        if let Some(error) = self.pending_error.take() {
            return Some(self.terminal_error(error));
        }
        let Some(Reverse(head)) = self.heap.pop() else {
            if 0 == self.remaining_rows {
                return None;
            }
            return Some(self.terminal_error(OrderedMergeError::InternalInvariant {
                invariant: OrderedMergeInvariant::MissingHeapHead,
            }));
        };

        if head.log_event_idx < 0 {
            return Some(
                self.terminal_error(OrderedMergeError::NegativeLogEventIndex {
                    table_index: head.table_index,
                    row_index: head.row_index,
                    actual: head.log_event_idx,
                }),
            );
        }
        let Ok(actual) = u64::try_from(head.log_event_idx) else {
            return Some(self.terminal_error(OrderedMergeError::SizeOverflow));
        };
        match actual.cmp(&self.next_expected) {
            Ordering::Less => {
                return Some(
                    self.terminal_error(OrderedMergeError::DuplicateLogEventIndex {
                        table_index: head.table_index,
                        row_index: head.row_index,
                        log_event_idx: head.log_event_idx,
                    }),
                );
            }
            Ordering::Greater => {
                return Some(self.terminal_error(OrderedMergeError::LogEventIndexGap {
                    table_index: head.table_index,
                    row_index: head.row_index,
                    expected: self.next_expected,
                    actual: head.log_event_idx,
                }));
            }
            Ordering::Equal => {}
        }

        self.remaining_rows = match self.remaining_rows.checked_sub(1) {
            Some(remaining) => remaining,
            None => {
                return Some(self.terminal_error(OrderedMergeError::InternalInvariant {
                    invariant: OrderedMergeInvariant::RemainingRows,
                }));
            }
        };
        self.next_expected = match self.next_expected.checked_add(1) {
            Some(expected) => expected,
            None => {
                return Some(self.terminal_error(OrderedMergeError::InternalInvariant {
                    invariant: OrderedMergeInvariant::ExpectedIndex,
                }));
            }
        };
        if let Err(error) = self.advance_head(head) {
            return Some(self.terminal_error(error));
        }

        Some(Ok(OrderedRow {
            table_index: head.table_index,
            row_index: head.row_index,
            log_event_idx: head.log_event_idx,
        }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.failed || 0 == self.remaining_rows && self.pending_error.is_none() {
            (0, Some(0))
        } else if self.pending_error.is_some() {
            (1, Some(1))
        } else {
            (1, Some(self.remaining_rows))
        }
    }
}

impl<C> FusedIterator for MergeCore<C> where C: ExactSizeIterator<Item = i64> {}

fn build_merge<I, C, F>(
    inputs: &[I],
    limits: OrderedMergeLimits,
    mut make_source: F,
) -> Result<MergeCore<C>, OrderedMergeError>
where
    C: ExactSizeIterator<Item = i64>,
    F: FnMut(&I) -> (usize, C, usize), {
    check_limit(OrderedMergeResource::Tables, inputs.len(), limits.tables)?;

    let mut states = Vec::new();
    states
        .try_reserve_exact(inputs.len())
        .map_err(|_| OrderedMergeError::AllocationFailed {
            resource: OrderedMergeResource::Tables,
            requested: inputs.len(),
        })?;
    let mut total_rows = 0_usize;
    for input in inputs {
        let (table_index, cursor, row_count) = make_source(input);
        if cursor.len() != row_count {
            return Err(OrderedMergeError::CursorLengthMismatch {
                table_index,
                expected: row_count,
                actual: cursor.len(),
            });
        }
        total_rows = total_rows
            .checked_add(row_count)
            .ok_or(OrderedMergeError::SizeOverflow)?;
        check_limit(OrderedMergeResource::Records, total_rows, limits.records)?;
        states.push(TableState {
            table_index,
            row_count,
            cursor,
        });
    }
    let total_rows_u64 = u64::try_from(total_rows).map_err(|_| OrderedMergeError::SizeOverflow)?;
    if total_rows_u64 > MAX_LOG_EVENT_COUNT {
        return Err(OrderedMergeError::RecordDomainTooLarge {
            actual: total_rows_u64,
            maximum: MAX_LOG_EVENT_COUNT,
        });
    }

    states.sort_unstable_by_key(|state| state.table_index);
    for pair in states.windows(2) {
        if pair[0].table_index == pair[1].table_index {
            return Err(OrderedMergeError::DuplicateTableIndex {
                table_index: pair[0].table_index,
            });
        }
    }

    let mut heap = BinaryHeap::new();
    heap.try_reserve_exact(inputs.len())
        .map_err(|_| OrderedMergeError::AllocationFailed {
            resource: OrderedMergeResource::HeapHeads,
            requested: inputs.len(),
        })?;
    for (state_index, state) in states.iter_mut().enumerate() {
        let row_index = state.row_count.checked_sub(state.cursor.len()).ok_or(
            OrderedMergeError::InternalInvariant {
                invariant: OrderedMergeInvariant::CursorRows,
            },
        )?;
        if let Some(log_event_idx) = state.cursor.next() {
            heap.push(Reverse(HeapHead {
                log_event_idx,
                table_index: state.table_index,
                row_index,
                state_index,
            }));
        }
    }

    Ok(MergeCore {
        states,
        heap,
        pending_error: None,
        table_count: inputs.len(),
        total_rows,
        remaining_rows: total_rows,
        next_expected: 0,
        failed: false,
    })
}

fn check_limit(
    resource: OrderedMergeResource,
    actual: usize,
    limit: u64,
) -> Result<(), OrderedMergeError> {
    let actual = u64::try_from(actual).map_err(|_| OrderedMergeError::SizeOverflow)?;
    if actual > limit {
        Err(OrderedMergeError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::LogOrderLocator;
    use crate::archive::ArchiveCatalogLimits;
    use crate::archive::ColumnLimits;
    use crate::archive::PackedStreamLimits;
    use crate::archive::SingleFileArchiveReader;

    const CPP_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    #[derive(Clone, Copy)]
    struct TestTable<'a> {
        table_index: usize,
        indexes: &'a [i64],
    }

    fn merge_test<'a>(
        tables: &'a [TestTable<'a>],
        limits: OrderedMergeLimits,
    ) -> Result<MergeCore<std::iter::Copied<std::slice::Iter<'a, i64>>>, OrderedMergeError> {
        build_merge(tables, limits, |table| {
            (
                table.table_index,
                table.indexes.iter().copied(),
                table.indexes.len(),
            )
        })
    }

    fn row(table_index: usize, row_index: usize, log_event_idx: i64) -> OrderedRow {
        OrderedRow {
            table_index,
            row_index,
            log_event_idx,
        }
    }

    #[test]
    fn empty_merge_is_exactly_empty_and_fused() {
        let mut merge = merge_test(&[], OrderedMergeLimits::default()).expect("empty merge");

        assert_eq!(0, merge.table_count);
        assert_eq!(0, merge.total_rows);
        assert_eq!(0, merge.remaining_rows);
        assert_eq!((0, Some(0)), merge.size_hint());
        assert_eq!(None, merge.next());
        assert_eq!(None, merge.next());

        let mut public_merge =
            OrderedRowMerge::new(&[], OrderedMergeLimits::default()).expect("empty public merge");
        assert_eq!(0, public_merge.total_rows());
        assert_eq!(None, public_merge.next());
    }

    #[test]
    fn merges_one_table_and_many_uneven_interleaved_tables() {
        let one = [TestTable {
            table_index: 12,
            indexes: &[0, 1, 2],
        }];
        let mut merge = merge_test(&one, OrderedMergeLimits::default()).expect("one table");
        assert_eq!(3, merge.total_rows);
        assert_eq!((1, Some(3)), merge.size_hint());
        assert_eq!(
            vec![Ok(row(12, 0, 0)), Ok(row(12, 1, 1)), Ok(row(12, 2, 2))],
            merge.by_ref().collect::<Vec<_>>()
        );
        assert_eq!(0, merge.remaining_rows);
        assert_eq!(None, merge.next());

        let interleaved = [
            TestTable {
                table_index: 7,
                indexes: &[0, 3, 6],
            },
            TestTable {
                table_index: 2,
                indexes: &[1, 4],
            },
            TestTable {
                table_index: 9,
                indexes: &[2, 5, 7, 8],
            },
            TestTable {
                table_index: 20,
                indexes: &[],
            },
        ];
        let merge =
            merge_test(&interleaved, OrderedMergeLimits::default()).expect("interleaved tables");
        assert_eq!(4, merge.table_count);
        assert_eq!(9, merge.total_rows);
        assert_eq!(
            vec![
                Ok(row(7, 0, 0)),
                Ok(row(2, 0, 1)),
                Ok(row(9, 0, 2)),
                Ok(row(7, 1, 3)),
                Ok(row(2, 1, 4)),
                Ok(row(9, 1, 5)),
                Ok(row(7, 2, 6)),
                Ok(row(9, 2, 7)),
                Ok(row(9, 3, 8)),
            ],
            merge.collect::<Vec<_>>()
        );
    }

    #[test]
    fn reports_negative_duplicate_and_gap_then_fuses() {
        let cases = [
            (
                vec![TestTable {
                    table_index: 3,
                    indexes: &[-1],
                }],
                Vec::<Result<OrderedRow, OrderedMergeError>>::new(),
                OrderedMergeError::NegativeLogEventIndex {
                    table_index: 3,
                    row_index: 0,
                    actual: -1,
                },
            ),
            (
                vec![
                    TestTable {
                        table_index: 5,
                        indexes: &[0, 2],
                    },
                    TestTable {
                        table_index: 6,
                        indexes: &[1, 2],
                    },
                ],
                vec![Ok(row(5, 0, 0)), Ok(row(6, 0, 1)), Ok(row(5, 1, 2))],
                OrderedMergeError::DuplicateLogEventIndex {
                    table_index: 6,
                    row_index: 1,
                    log_event_idx: 2,
                },
            ),
            (
                vec![TestTable {
                    table_index: 8,
                    indexes: &[0, 2],
                }],
                vec![Ok(row(8, 0, 0))],
                OrderedMergeError::LogEventIndexGap {
                    table_index: 8,
                    row_index: 1,
                    expected: 1,
                    actual: 2,
                },
            ),
        ];

        for (tables, expected_rows, expected_error) in cases {
            let mut merge = merge_test(&tables, OrderedMergeLimits::default()).expect("construct");
            for expected in expected_rows {
                assert_eq!(Some(expected), merge.next());
            }
            assert_eq!(Some(Err(expected_error)), merge.next());
            assert_eq!(0, merge.remaining_rows);
            assert_eq!((0, Some(0)), merge.size_hint());
            assert_eq!(None, merge.next());
            assert_eq!(None, merge.next());
        }
    }

    #[test]
    fn reports_per_table_duplicate_and_regression_after_last_valid_row() {
        let cases = [(&[0, 1, 1][..], 1), (&[0, 1, 0][..], 0)];
        for (indexes, invalid) in cases {
            let tables = [TestTable {
                table_index: 4,
                indexes,
            }];
            let mut merge = merge_test(&tables, OrderedMergeLimits::default()).expect("construct");
            assert_eq!(Some(Ok(row(4, 0, 0))), merge.next());
            assert_eq!(Some(Ok(row(4, 1, 1))), merge.next());
            assert_eq!((1, Some(1)), merge.size_hint());
            assert_eq!(
                Some(Err(OrderedMergeError::TableOrderViolation {
                    table_index: 4,
                    row_index: 2,
                    previous: 1,
                    current: invalid,
                })),
                merge.next()
            );
            assert_eq!(0, merge.remaining_rows);
            assert_eq!(None, merge.next());
        }
    }

    #[test]
    fn enforces_limits_duplicate_table_ids_and_cursor_lengths() {
        let tables = [
            TestTable {
                table_index: 1,
                indexes: &[0],
            },
            TestTable {
                table_index: 2,
                indexes: &[1],
            },
        ];
        assert_eq!(
            OrderedMergeError::LimitExceeded {
                resource: OrderedMergeResource::Tables,
                actual: 2,
                limit: 1,
            },
            merge_test(&tables, OrderedMergeLimits::new(1, 2))
                .expect_err("table limit before allocation")
        );
        assert_eq!(
            OrderedMergeError::LimitExceeded {
                resource: OrderedMergeResource::Records,
                actual: 2,
                limit: 1,
            },
            merge_test(&tables, OrderedMergeLimits::new(2, 1)).expect_err("record limit")
        );

        let duplicate_tables = [
            TestTable {
                table_index: 3,
                indexes: &[0],
            },
            TestTable {
                table_index: 3,
                indexes: &[1],
            },
        ];
        assert_eq!(
            OrderedMergeError::DuplicateTableIndex { table_index: 3 },
            merge_test(&duplicate_tables, OrderedMergeLimits::default())
                .expect_err("duplicate table identity")
        );

        let bad_length = [TestTable {
            table_index: 5,
            indexes: &[0],
        }];
        assert_eq!(
            OrderedMergeError::CursorLengthMismatch {
                table_index: 5,
                expected: 2,
                actual: 1,
            },
            build_merge(&bad_length, OrderedMergeLimits::default(), |table| (
                table.table_index,
                table.indexes.iter().copied(),
                2
            ),)
            .expect_err("cursor length mismatch")
        );
    }

    #[test]
    fn merges_the_committed_cpp_fixture_through_public_columns() {
        let mut archive = SingleFileArchiveReader::open(Cursor::new(CPP_FIXTURE))
            .expect("open committed C++ fixture");
        let catalog = archive
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("read committed C++ catalog");
        let stream = archive
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read fixture stream");
        let mut tables = catalog
            .schema_tables(0, &stream, ColumnLimits::default())
            .expect("select fixture tables");
        let table = tables
            .next()
            .expect("fixture table")
            .expect("decode fixture table");
        assert!(tables.next().is_none());
        let locator = LogOrderLocator::discover(catalog.schema_tree())
            .expect("valid metadata")
            .expect("fixture records log order");
        let log_order = locator
            .locate(table.schema(), table.table())
            .expect("valid table")
            .expect("table records log order");
        let input = [OrderedMergeTable::new(table.table_index(), log_order)];

        let mut merge = OrderedRowMerge::new(&input, OrderedMergeLimits::default())
            .expect("construct fixture merge");
        assert_eq!(1, merge.table_count());
        assert_eq!(1, merge.total_rows());
        assert_eq!(Some(Ok(row(0, 0, 0))), merge.next());
        assert_eq!(0, merge.remaining_rows());
        assert_eq!(None, merge.next());
    }
}
