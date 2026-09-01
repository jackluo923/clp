//! Exact current-format timestamp values and dictionary construction.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use smallvec::SmallVec;

use super::WriterError;
use super::WriterLimits;
use super::primitive::AppendError;
use super::primitive::AppendResource;
use crate::timestamp::TimestampFormatError;
use crate::timestamp::TimestampPattern;
use crate::timestamp::TimestampPatternError;
use crate::timestamp::TimestampPatternLimits;

const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;

/// One exact timestamp value borrowed for a record append.
///
/// `lexeme` is the exact JSON value that extraction must reproduce. String timestamps therefore
/// include their surrounding quotes and JSON escapes, while numeric timestamps do not. `pattern`
/// is the resolved current-format C++ timestamp pattern, with the same quoting convention.
/// `range_key` is the authoritative timestamp descriptor stored in archive metadata (for example,
/// `ts` or `event.timestamp`). JSON parsing and timestamp-pattern discovery remain adapter work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampRef<'a> {
    epoch_nanoseconds: i64,
    lexeme: &'a str,
    pattern: &'a str,
    range_key: &'a str,
}

impl<'a> TimestampRef<'a> {
    /// Creates an exact timestamp value from adapter-resolved components.
    #[must_use]
    pub const fn new(
        epoch_nanoseconds: i64,
        lexeme: &'a str,
        pattern: &'a str,
        range_key: &'a str,
    ) -> Self {
        Self {
            epoch_nanoseconds,
            lexeme,
            pattern,
            range_key,
        }
    }

    /// Returns the signed epoch-nanosecond value stored in the column.
    #[must_use]
    pub const fn epoch_nanoseconds(self) -> i64 {
        self.epoch_nanoseconds
    }

    /// Returns the exact JSON lexeme that the pattern must reconstruct.
    #[must_use]
    pub const fn lexeme(self) -> &'a str {
        self.lexeme
    }

    /// Returns the exact resolved current-format timestamp pattern.
    #[must_use]
    pub const fn pattern(self) -> &'a str {
        self.pattern
    }

    /// Returns the authoritative timestamp descriptor stored with this column's range.
    #[must_use]
    pub const fn range_key(self) -> &'a str {
        self.range_key
    }
}

/// Crate-generated timestamp whose pattern is already proven to reproduce its exact lexeme.
///
/// This wrapper is public only because it appears in a hidden variant of the public, non-exhaustive
/// [`super::ValueRef`] enum. Its constructor and payload remain crate-private, so library callers
/// cannot bypass [`TimestampRef::new`]'s writer-side validation.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrevalidatedTimestampRef<'a> {
    value: TimestampRef<'a>,
}

impl<'a> PrevalidatedTimestampRef<'a> {
    pub(crate) const fn new(value: TimestampRef<'a>) -> Self {
        Self { value }
    }

    pub(super) const fn into_inner(self) -> TimestampRef<'a> {
        self.value
    }
}

/// Failure to validate an exact timestamp before archive state changes.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampError {
    /// The authoritative timestamp descriptor is empty.
    EmptyRangeKey,
    /// A later value for one timestamp node supplied a different authoritative descriptor.
    ConflictingRangeKey {
        /// Implicit schema-tree node ID of the timestamp column.
        node_id: u32,
    },
    /// The supplied resolved pattern is not valid current-format CLP-S.
    InvalidPattern {
        /// Pattern compilation failure.
        source: TimestampPatternError,
    },
    /// The epoch value cannot be marshalled through the supplied pattern.
    Format {
        /// Timestamp marshalling failure.
        source: TimestampFormatError,
    },
    /// Formatting the epoch and pattern did not reproduce the exact supplied lexeme.
    LexemeMismatch {
        /// Supplied lexeme size in bytes.
        lexeme_bytes: usize,
        /// Formatted value size in bytes.
        formatted_bytes: usize,
        /// First mismatching byte, or the common-prefix length when only sizes differ.
        first_difference: usize,
    },
}

impl Display for TimestampError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRangeKey => formatter.write_str("timestamp range descriptor is empty"),
            Self::ConflictingRangeKey { node_id } => write!(
                formatter,
                "timestamp node {node_id} was assigned conflicting range descriptors"
            ),
            Self::InvalidPattern { source } => {
                write!(formatter, "invalid resolved timestamp pattern: {source}")
            }
            Self::Format { source } => {
                write!(formatter, "timestamp value cannot be formatted: {source}")
            }
            Self::LexemeMismatch {
                lexeme_bytes,
                formatted_bytes,
                first_difference,
            } => write!(
                formatter,
                "timestamp lexeme ({lexeme_bytes} bytes) differs from the formatted value \
                 ({formatted_bytes} bytes) at byte {first_difference}"
            ),
        }
    }
}

impl Error for TimestampError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPattern { source } => Some(source),
            Self::Format { source } => Some(source),
            Self::EmptyRangeKey
            | Self::ConflictingRangeKey { .. }
            | Self::LexemeMismatch { .. } => None,
        }
    }
}

#[derive(Debug)]
struct TimestampRange {
    node_id: u32,
    key: String,
    start_milliseconds: i64,
    end_milliseconds: i64,
}

#[derive(Debug, Default)]
pub(super) struct TimestampDictionaryBuilder {
    ranges: Vec<TimestampRange>,
    range_indexes: HashMap<u32, usize>,
    patterns: Vec<TimestampPattern>,
    pattern_buckets: HashMap<u64, Vec<usize>>,
    pattern_bytes: u64,
}

impl TimestampDictionaryBuilder {
    pub(super) fn archive_bounds(&self) -> (i64, i64) {
        self.ranges.first().map_or((0, 0), |range| {
            (range.start_milliseconds, range.end_milliseconds)
        })
    }

    fn find_pattern(&self, raw: &str) -> Option<usize> {
        self.pattern_buckets
            .get(&hash_bytes(raw.as_bytes()))
            .and_then(|indexes| {
                indexes
                    .iter()
                    .copied()
                    .find(|index| self.patterns[*index].raw() == raw)
            })
    }

    pub(super) fn commit(&mut self, plan: TimestampPlan, reservations: TimestampReservations) {
        if plan.is_empty() {
            return;
        }
        for (hash, bucket) in reservations.new_pattern_buckets {
            self.pattern_buckets.insert(hash, bucket);
        }
        for (range_index, bounds) in plan.range_updates {
            let range = &mut self.ranges[range_index];
            range.start_milliseconds = bounds.start;
            range.end_milliseconds = bounds.end;
        }
        for range in plan.new_ranges {
            let index = self.ranges.len();
            self.range_indexes.insert(range.node_id, index);
            self.ranges.push(range);
        }
        for pattern in plan.new_patterns {
            let index = self.patterns.len();
            let hash = hash_bytes(pattern.raw().as_bytes());
            self.pattern_bytes += u64::try_from(pattern.raw().len())
                .expect("validated timestamp pattern size must fit u64");
            self.pattern_buckets
                .get_mut(&hash)
                .expect("reserved timestamp-pattern bucket must exist")
                .push(index);
            self.patterns.push(pattern);
        }
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, WriterError> {
        let capacity = self.encoded_size()?;
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(capacity)
            .map_err(|_| WriterError::AllocationFailed {
                requested: capacity,
            })?;
        append_u64(&mut encoded, usize_u64(self.ranges.len())?);
        for range in &self.ranges {
            append_u64(&mut encoded, usize_u64(range.key.len())?);
            encoded.extend_from_slice(range.key.as_bytes());
            append_u64(&mut encoded, 1);
            let node_id = i32::try_from(range.node_id).map_err(|_| WriterError::SizeOverflow)?;
            encoded.extend_from_slice(&node_id.to_le_bytes());
            append_u64(&mut encoded, 1);
            encoded.extend_from_slice(&range.start_milliseconds.to_le_bytes());
            encoded.extend_from_slice(&range.end_milliseconds.to_le_bytes());
        }
        append_u64(&mut encoded, usize_u64(self.patterns.len())?);
        // C++ serializes its string-pattern vector before its numeric-pattern map. For JSON input,
        // surrounding quotes distinguish those classes. Explicit IDs preserve lookup semantics.
        for quoted in [true, false] {
            for (pattern_id, pattern) in self.patterns.iter().enumerate() {
                if quoted != pattern.is_quoted() {
                    continue;
                }
                append_u64(&mut encoded, usize_u64(pattern_id)?);
                append_u64(&mut encoded, usize_u64(pattern.raw().len())?);
                encoded.extend_from_slice(pattern.raw().as_bytes());
            }
        }
        debug_assert_eq!(capacity, encoded.len());
        Ok(encoded)
    }

    fn encoded_size(&self) -> Result<usize, WriterError> {
        let mut size = size_of::<u64>();
        for range in &self.ranges {
            size = size
                .checked_add(5 * size_of::<u64>() + size_of::<i32>())
                .and_then(|value| value.checked_add(range.key.len()))
                .ok_or(WriterError::SizeOverflow)?;
        }
        size = size
            .checked_add(size_of::<u64>())
            .ok_or(WriterError::SizeOverflow)?;
        for pattern in &self.patterns {
            size = size
                .checked_add(2 * size_of::<u64>())
                .and_then(|value| value.checked_add(pattern.raw().len()))
                .ok_or(WriterError::SizeOverflow)?;
        }
        Ok(size)
    }
}

#[derive(Clone, Copy, Debug)]
struct MillisecondBounds {
    start: i64,
    end: i64,
}

impl MillisecondBounds {
    const fn include(self, other: Self) -> Self {
        Self {
            start: if self.start < other.start {
                self.start
            } else {
                other.start
            },
            end: if self.end > other.end {
                self.end
            } else {
                other.end
            },
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct TimestampPlan {
    new_ranges: Vec<TimestampRange>,
    new_range_indexes: HashMap<u32, usize>,
    range_updates: SmallVec<[(usize, MillisecondBounds); 1]>,
    new_patterns: Vec<TimestampPattern>,
    new_pattern_buckets: HashMap<u64, Vec<usize>>,
    added_key_bytes: u64,
    added_pattern_bytes: u64,
}

impl TimestampPlan {
    fn is_empty(&self) -> bool {
        self.new_ranges.is_empty() && self.range_updates.is_empty() && self.new_patterns.is_empty()
    }

    pub(super) const fn added_value_bytes(&self) -> Result<u64, AppendError> {
        match self.added_key_bytes.checked_add(self.added_pattern_bytes) {
            Some(bytes) => Ok(bytes),
            None => Err(AppendError::SizeOverflow),
        }
    }

    pub(super) fn resolve(
        &mut self,
        base: &TimestampDictionaryBuilder,
        node_id: u32,
        value: TimestampRef<'_>,
        prevalidated: bool,
        limits: WriterLimits,
        scratch: &mut String,
    ) -> Result<u64, AppendError> {
        self.validate_range(base, node_id, value, limits)?;
        let pattern_id = if let Some(index) = base.find_pattern(value.pattern()) {
            validate_untrusted_lexeme(
                &base.patterns[index],
                value,
                prevalidated,
                scratch,
                node_id,
            )?;
            usize_u64_append(index)?
        } else if let Some(index) = self.find_pattern(value.pattern()) {
            validate_untrusted_lexeme(
                &self.new_patterns[index],
                value,
                prevalidated,
                scratch,
                node_id,
            )?;
            usize_u64_append(
                base.patterns
                    .len()
                    .checked_add(index)
                    .ok_or(AppendError::SizeOverflow)?,
            )?
        } else {
            self.add_pattern(base, value, prevalidated, limits, scratch, node_id)?
        };
        Ok(pattern_id)
    }

    fn find_pattern(&self, raw: &str) -> Option<usize> {
        self.new_pattern_buckets
            .get(&hash_bytes(raw.as_bytes()))
            .and_then(|indexes| {
                indexes
                    .iter()
                    .copied()
                    .find(|index| self.new_patterns[*index].raw() == raw)
            })
    }

    fn add_pattern(
        &mut self,
        base: &TimestampDictionaryBuilder,
        value: TimestampRef<'_>,
        prevalidated: bool,
        limits: WriterLimits,
        scratch: &mut String,
        node_id: u32,
    ) -> Result<u64, AppendError> {
        check_limit(
            AppendResource::TimestampPatternBytes,
            usize_u64_append(value.pattern().len())?,
            limits.max_timestamp_pattern_bytes(),
        )?;
        let pattern_bytes = usize_u64_append(value.pattern().len())?;
        let resulting_pattern_bytes = base
            .pattern_bytes
            .checked_add(self.added_pattern_bytes)
            .and_then(|bytes| bytes.checked_add(pattern_bytes))
            .ok_or(AppendError::SizeOverflow)?;
        check_limit(
            AppendResource::TimestampPatternValueBytes,
            resulting_pattern_bytes,
            limits.max_timestamp_pattern_value_bytes(),
        )?;
        let resulting_patterns = usize_u64_append(base.patterns.len())?
            .checked_add(usize_u64_append(self.new_patterns.len())?)
            .and_then(|count| count.checked_add(1))
            .ok_or(AppendError::SizeOverflow)?;
        check_limit(
            AppendResource::TimestampPatterns,
            resulting_patterns,
            limits.max_timestamp_patterns(),
        )?;
        let pattern_limit =
            usize::try_from(limits.max_timestamp_pattern_bytes()).unwrap_or(usize::MAX);
        let lexeme_limit =
            usize::try_from(limits.max_timestamp_lexeme_bytes()).unwrap_or(usize::MAX);
        let pattern = TimestampPattern::compile(
            value.pattern(),
            TimestampPatternLimits::new(pattern_limit, lexeme_limit),
        )
        .map_err(|source| AppendError::Timestamp {
            node_id,
            reason: TimestampError::InvalidPattern { source },
        })?;
        validate_untrusted_lexeme(&pattern, value, prevalidated, scratch, node_id)?;

        let index = self.new_patterns.len();
        let hash = hash_bytes(pattern.raw().as_bytes());
        self.new_patterns
            .try_reserve(1)
            .map_err(|_| allocation(AppendResource::TimestampPatterns, 1))?;
        if let Some(bucket) = self.new_pattern_buckets.get_mut(&hash) {
            bucket
                .try_reserve(1)
                .map_err(|_| allocation(AppendResource::TimestampPatterns, 1))?;
        } else {
            self.new_pattern_buckets
                .try_reserve(1)
                .map_err(|_| allocation(AppendResource::TimestampPatterns, 1))?;
            let mut bucket = Vec::new();
            bucket
                .try_reserve_exact(1)
                .map_err(|_| allocation(AppendResource::TimestampPatterns, 1))?;
            self.new_pattern_buckets.insert(hash, bucket);
        }
        self.new_patterns.push(pattern);
        self.new_pattern_buckets
            .get_mut(&hash)
            .expect("staged timestamp-pattern bucket must exist")
            .push(index);
        self.added_pattern_bytes = self
            .added_pattern_bytes
            .checked_add(pattern_bytes)
            .ok_or(AppendError::SizeOverflow)?;
        usize_u64_append(
            base.patterns
                .len()
                .checked_add(index)
                .ok_or(AppendError::SizeOverflow)?,
        )
    }

    fn validate_range(
        &mut self,
        base: &TimestampDictionaryBuilder,
        node_id: u32,
        value: TimestampRef<'_>,
        limits: WriterLimits,
    ) -> Result<(), AppendError> {
        if value.range_key().is_empty() {
            return Err(AppendError::Timestamp {
                node_id,
                reason: TimestampError::EmptyRangeKey,
            });
        }
        check_limit(
            AppendResource::TimestampRangeKeyBytes,
            usize_u64_append(value.range_key().len())?,
            limits.max_timestamp_range_key_bytes(),
        )?;
        check_limit(
            AppendResource::TimestampLexemeBytes,
            usize_u64_append(value.lexeme().len())?,
            limits.max_timestamp_lexeme_bytes(),
        )?;
        let bounds = millisecond_bounds(value.epoch_nanoseconds());
        if let Some(range_index) = base.range_indexes.get(&node_id).copied() {
            if base.ranges[range_index].key != value.range_key() {
                return Err(conflicting_range_key(node_id));
            }
            if let Some((_, current)) = self
                .range_updates
                .iter_mut()
                .find(|(index, _)| *index == range_index)
            {
                *current = current.include(bounds);
                return Ok(());
            }
            self.range_updates
                .try_reserve(1)
                .map_err(|_| allocation(AppendResource::TimestampRanges, 1))?;
            let current = MillisecondBounds {
                start: base.ranges[range_index].start_milliseconds,
                end: base.ranges[range_index].end_milliseconds,
            };
            self.range_updates
                .push((range_index, current.include(bounds)));
            return Ok(());
        }
        if let Some(range_index) = self.new_range_indexes.get(&node_id).copied() {
            let range = &mut self.new_ranges[range_index];
            if range.key != value.range_key() {
                return Err(conflicting_range_key(node_id));
            }
            let combined = MillisecondBounds {
                start: range.start_milliseconds,
                end: range.end_milliseconds,
            }
            .include(bounds);
            range.start_milliseconds = combined.start;
            range.end_milliseconds = combined.end;
            return Ok(());
        }

        let resulting_ranges = usize_u64_append(base.ranges.len())?
            .checked_add(usize_u64_append(self.new_ranges.len())?)
            .and_then(|count| count.checked_add(1))
            .ok_or(AppendError::SizeOverflow)?;
        check_limit(
            AppendResource::TimestampRanges,
            resulting_ranges,
            limits.max_timestamp_ranges(),
        )?;
        let mut key = String::new();
        key.try_reserve_exact(value.range_key().len())
            .map_err(|_| {
                allocation(
                    AppendResource::TimestampRangeKeyBytes,
                    value.range_key().len(),
                )
            })?;
        key.push_str(value.range_key());
        self.new_ranges
            .try_reserve(1)
            .map_err(|_| allocation(AppendResource::TimestampRanges, 1))?;
        self.new_range_indexes
            .try_reserve(1)
            .map_err(|_| allocation(AppendResource::TimestampRanges, 1))?;
        let index = self.new_ranges.len();
        self.new_ranges.push(TimestampRange {
            node_id,
            key,
            start_milliseconds: bounds.start,
            end_milliseconds: bounds.end,
        });
        self.new_range_indexes.insert(node_id, index);
        self.added_key_bytes = self
            .added_key_bytes
            .checked_add(usize_u64_append(value.range_key().len())?)
            .ok_or(AppendError::SizeOverflow)?;
        Ok(())
    }
}

fn validate_lexeme(
    pattern: &TimestampPattern,
    value: TimestampRef<'_>,
    scratch: &mut String,
    node_id: u32,
) -> Result<(), AppendError> {
    scratch.clear();
    pattern
        .append_epoch_nanoseconds(value.epoch_nanoseconds(), scratch)
        .map_err(|source| AppendError::Timestamp {
            node_id,
            reason: TimestampError::Format { source },
        })?;
    if scratch == value.lexeme() {
        scratch.clear();
        return Ok(());
    }
    let first_difference = scratch
        .as_bytes()
        .iter()
        .zip(value.lexeme().as_bytes())
        .position(|(formatted, supplied)| formatted != supplied)
        .unwrap_or_else(|| scratch.len().min(value.lexeme().len()));
    let reason = TimestampError::LexemeMismatch {
        lexeme_bytes: value.lexeme().len(),
        formatted_bytes: scratch.len(),
        first_difference,
    };
    scratch.clear();
    Err(AppendError::Timestamp { node_id, reason })
}

fn validate_untrusted_lexeme(
    pattern: &TimestampPattern,
    value: TimestampRef<'_>,
    prevalidated: bool,
    scratch: &mut String,
    node_id: u32,
) -> Result<(), AppendError> {
    if prevalidated {
        Ok(())
    } else {
        validate_lexeme(pattern, value, scratch, node_id)
    }
}

const fn millisecond_bounds(timestamp: i64) -> MillisecondBounds {
    let whole = timestamp / NANOSECONDS_PER_MILLISECOND;
    let remainder = timestamp % NANOSECONDS_PER_MILLISECOND;
    MillisecondBounds {
        start: whole - if remainder < 0 { 1 } else { 0 },
        end: whole + if remainder > 0 { 1 } else { 0 },
    }
}

const fn conflicting_range_key(node_id: u32) -> AppendError {
    AppendError::Timestamp {
        node_id,
        reason: TimestampError::ConflictingRangeKey { node_id },
    }
}

pub(super) struct TimestampReservations {
    new_pattern_buckets: Vec<(u64, Vec<usize>)>,
}

pub(super) fn prepare_reservations(
    dictionary: &mut TimestampDictionaryBuilder,
    plan: &TimestampPlan,
) -> Result<TimestampReservations, AppendError> {
    if plan.is_empty() {
        return Ok(TimestampReservations {
            new_pattern_buckets: Vec::new(),
        });
    }
    dictionary
        .ranges
        .try_reserve(plan.new_ranges.len())
        .map_err(|_| allocation(AppendResource::TimestampRanges, plan.new_ranges.len()))?;
    dictionary
        .range_indexes
        .try_reserve(plan.new_ranges.len())
        .map_err(|_| allocation(AppendResource::TimestampRanges, plan.new_ranges.len()))?;
    dictionary
        .patterns
        .try_reserve(plan.new_patterns.len())
        .map_err(|_| allocation(AppendResource::TimestampPatterns, plan.new_patterns.len()))?;

    let mut counts = HashMap::<u64, usize>::new();
    counts
        .try_reserve(plan.new_pattern_buckets.len())
        .map_err(|_| {
            allocation(
                AppendResource::TimestampPatterns,
                plan.new_pattern_buckets.len(),
            )
        })?;
    for pattern in &plan.new_patterns {
        let count = counts
            .entry(hash_bytes(pattern.raw().as_bytes()))
            .or_default();
        *count = count.checked_add(1).ok_or(AppendError::SizeOverflow)?;
    }
    let new_bucket_count = counts
        .keys()
        .filter(|hash| !dictionary.pattern_buckets.contains_key(hash))
        .count();
    dictionary
        .pattern_buckets
        .try_reserve(new_bucket_count)
        .map_err(|_| allocation(AppendResource::TimestampPatterns, new_bucket_count))?;
    let mut new_pattern_buckets = Vec::new();
    new_pattern_buckets
        .try_reserve_exact(new_bucket_count)
        .map_err(|_| allocation(AppendResource::TimestampPatterns, new_bucket_count))?;
    for (hash, count) in counts {
        if let Some(bucket) = dictionary.pattern_buckets.get_mut(&hash) {
            bucket
                .try_reserve(count)
                .map_err(|_| allocation(AppendResource::TimestampPatterns, count))?;
        } else {
            let mut bucket = Vec::new();
            bucket
                .try_reserve_exact(count)
                .map_err(|_| allocation(AppendResource::TimestampPatterns, count))?;
            new_pattern_buckets.push((hash, bucket));
        }
    }
    Ok(TimestampReservations {
        new_pattern_buckets,
    })
}

const fn check_limit(resource: AppendResource, actual: u64, limit: u64) -> Result<(), AppendError> {
    if actual > limit {
        Err(AppendError::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

const fn allocation(resource: AppendResource, requested: usize) -> AppendError {
    AppendError::AllocationFailed {
        resource,
        requested,
    }
}

fn usize_u64_append(value: usize) -> Result<u64, AppendError> {
    u64::try_from(value).map_err(|_| AppendError::SizeOverflow)
}

fn usize_u64(value: usize) -> Result<u64, WriterError> {
    u64::try_from(value).map_err(|_| WriterError::SizeOverflow)
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= u64::from(bytes[index]);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_bounds_use_the_first_range_and_default_to_zero() {
        let mut dictionary = TimestampDictionaryBuilder::default();
        assert_eq!((0, 0), dictionary.archive_bounds());
        dictionary.ranges.extend([
            TimestampRange {
                node_id: 9,
                key: "first".to_owned(),
                start_milliseconds: -1_001,
                end_milliseconds: -1_000,
            },
            TimestampRange {
                node_id: 2,
                key: "second".to_owned(),
                start_milliseconds: 2_000,
                end_milliseconds: 2_001,
            },
        ]);
        assert_eq!((-1_001, -1_000), dictionary.archive_bounds());
    }

    #[test]
    fn outward_millisecond_bounds_match_cpp_truncation() {
        assert_eq!((-1, 0), bounds_tuple(millisecond_bounds(-1)));
        assert_eq!((0, 0), bounds_tuple(millisecond_bounds(0)));
        assert_eq!((0, 1), bounds_tuple(millisecond_bounds(1)));
        assert_eq!((-2, -1), bounds_tuple(millisecond_bounds(-1_000_001)));
        assert_eq!((1, 2), bounds_tuple(millisecond_bounds(1_000_001)));
    }

    #[test]
    fn record_plan_spills_multiple_existing_range_updates_without_losing_bounds() {
        let mut dictionary = TimestampDictionaryBuilder::default();
        dictionary.ranges.extend([
            TimestampRange {
                node_id: 1,
                key: "first".to_owned(),
                start_milliseconds: 0,
                end_milliseconds: 0,
            },
            TimestampRange {
                node_id: 2,
                key: "second".to_owned(),
                start_milliseconds: 10,
                end_milliseconds: 10,
            },
        ]);
        dictionary.range_indexes.extend([(1, 0), (2, 1)]);

        let mut plan = TimestampPlan::default();
        plan.validate_range(
            &dictionary,
            1,
            TimestampRef::new(-1, "0", r"\N", "first"),
            WriterLimits::default(),
        )
        .expect("stage first range update inline");
        plan.validate_range(
            &dictionary,
            2,
            TimestampRef::new(12_000_001, "0", r"\N", "second"),
            WriterLimits::default(),
        )
        .expect("stage second range update after spilling");
        assert!(plan.range_updates.spilled());

        dictionary.commit(
            plan,
            TimestampReservations {
                new_pattern_buckets: Vec::new(),
            },
        );
        assert_eq!(
            (-1, 0),
            bounds_tuple(MillisecondBounds {
                start: dictionary.ranges[0].start_milliseconds,
                end: dictionary.ranges[0].end_milliseconds,
            })
        );
        assert_eq!(
            (10, 13),
            bounds_tuple(MillisecondBounds {
                start: dictionary.ranges[1].start_milliseconds,
                end: dictionary.ranges[1].end_milliseconds,
            })
        );
    }

    const fn bounds_tuple(bounds: MillisecondBounds) -> (i64, i64) {
        (bounds.start, bounds.end)
    }
}
