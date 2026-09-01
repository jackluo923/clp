//! Precompiled timestamp-pattern lookup for archive extraction and search.
//!
//! [`TimestampPatternCatalog`] compiles every resolved pattern in an archive once. Canonical
//! zero-based pattern IDs use direct vector indexing on the record hot path; sparse or reordered
//! IDs use a compact sorted index instead of allocating up to the largest untrusted wire ID.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use crate::archive::TimestampDictionary;
use crate::timestamp::TimestampFormatError;
use crate::timestamp::TimestampPattern;
use crate::timestamp::TimestampPatternError;
use crate::timestamp::TimestampPatternLimits;

/// Resource limits applied while precompiling an archive's timestamp patterns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampPatternCatalogLimits {
    patterns: u64,
    total_pattern_bytes: u64,
    pattern: TimestampPatternLimits,
}

impl TimestampPatternCatalogLimits {
    /// Creates explicit aggregate and per-pattern limits.
    #[must_use]
    pub const fn new(
        max_patterns: u64,
        max_total_pattern_bytes: u64,
        pattern_limits: TimestampPatternLimits,
    ) -> Self {
        Self {
            patterns: max_patterns,
            total_pattern_bytes: max_total_pattern_bytes,
            pattern: pattern_limits,
        }
    }

    /// Maximum number of patterns compiled into one catalog.
    #[must_use]
    pub const fn max_patterns(self) -> u64 {
        self.patterns
    }

    /// Maximum cumulative raw pattern bytes compiled into one catalog.
    #[must_use]
    pub const fn max_total_pattern_bytes(self) -> u64 {
        self.total_pattern_bytes
    }

    /// Per-pattern compilation limits.
    #[must_use]
    pub const fn pattern_limits(self) -> TimestampPatternLimits {
        self.pattern
    }
}

impl Default for TimestampPatternCatalogLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(1_048_576, 256 * MEBIBYTE, TimestampPatternLimits::default())
    }
}

/// One explicitly identified, precompiled archive timestamp pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledTimestampPattern {
    id: u64,
    pattern: TimestampPattern,
}

impl CompiledTimestampPattern {
    /// Returns the explicit wire ID from the archive.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns the precompiled pattern.
    #[must_use]
    pub const fn pattern(&self) -> &TimestampPattern {
        &self.pattern
    }
}

/// An archive's timestamp patterns compiled once for repeated record formatting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimestampPatternCatalog {
    patterns: Vec<CompiledTimestampPattern>,
    index: PatternIndex,
}

impl TimestampPatternCatalog {
    /// Compiles all resolved patterns in a validated archive timestamp dictionary.
    ///
    /// Canonical dictionaries whose IDs are `0..len` need no secondary lookup allocation.
    /// Other valid ID layouts receive an index with exactly one entry per pattern, regardless of
    /// the largest wire ID.
    ///
    /// # Errors
    ///
    /// Returns a resource error before compilation when aggregate limits are exceeded, an
    /// allocation error when bounded reservations fail, or a pattern error annotated with its
    /// serialized index and explicit wire ID.
    pub fn compile(
        dictionary: &TimestampDictionary,
        limits: TimestampPatternCatalogLimits,
    ) -> Result<Self, TimestampPatternCatalogError> {
        let pattern_count = u64::try_from(dictionary.patterns().len())
            .map_err(|_| TimestampPatternCatalogError::SizeOverflow)?;
        if pattern_count > limits.patterns {
            return Err(TimestampPatternCatalogError::PatternCountTooLarge {
                actual: pattern_count,
                limit: limits.patterns,
            });
        }

        let total_pattern_bytes = dictionary.patterns().iter().try_fold(
            0_u64,
            |total, entry| -> Result<u64, TimestampPatternCatalogError> {
                let bytes = u64::try_from(entry.raw().len())
                    .map_err(|_| TimestampPatternCatalogError::SizeOverflow)?;
                total
                    .checked_add(bytes)
                    .ok_or(TimestampPatternCatalogError::SizeOverflow)
            },
        )?;
        if total_pattern_bytes > limits.total_pattern_bytes {
            return Err(TimestampPatternCatalogError::TotalPatternBytesTooLarge {
                actual: total_pattern_bytes,
                limit: limits.total_pattern_bytes,
            });
        }

        let capacity = dictionary.patterns().len();
        let mut patterns = Vec::new();
        patterns.try_reserve_exact(capacity).map_err(|_| {
            TimestampPatternCatalogError::AllocationFailed {
                requested: capacity,
            }
        })?;
        let mut direct = true;
        for (pattern_index, entry) in dictionary.patterns().iter().enumerate() {
            let expected_id = u64::try_from(pattern_index)
                .map_err(|_| TimestampPatternCatalogError::SizeOverflow)?;
            direct &= entry.id() == expected_id;
            let pattern =
                TimestampPattern::compile(entry.raw(), limits.pattern).map_err(|source| {
                    TimestampPatternCatalogError::InvalidPattern {
                        pattern_index,
                        pattern_id: entry.id(),
                        source,
                    }
                })?;
            patterns.push(CompiledTimestampPattern {
                id: entry.id(),
                pattern,
            });
        }

        let index = if direct {
            PatternIndex::Direct
        } else {
            let mut sparse = Vec::new();
            sparse.try_reserve_exact(capacity).map_err(|_| {
                TimestampPatternCatalogError::AllocationFailed {
                    requested: capacity,
                }
            })?;
            sparse.extend(
                patterns
                    .iter()
                    .enumerate()
                    .map(|(pattern_index, entry)| (entry.id, pattern_index)),
            );
            sparse.sort_unstable_by_key(|&(pattern_id, _)| pattern_id);
            PatternIndex::Sparse(sparse)
        };

        Ok(Self { patterns, index })
    }

    /// Returns compiled patterns in timestamp-dictionary serialization order.
    #[must_use]
    pub fn patterns(&self) -> &[CompiledTimestampPattern] {
        &self.patterns
    }

    /// Finds a compiled entry by its explicit wire ID.
    #[must_use]
    pub fn get(&self, pattern_id: u64) -> Option<&CompiledTimestampPattern> {
        self.pattern_index(pattern_id)
            .and_then(|index| self.patterns.get(index))
    }

    /// Finds a precompiled pattern by its explicit wire ID.
    #[must_use]
    pub fn pattern(&self, pattern_id: u64) -> Option<&TimestampPattern> {
        self.get(pattern_id).map(CompiledTimestampPattern::pattern)
    }

    /// Returns the number of compiled patterns.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Returns whether no patterns were compiled.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Appends a timestamp through an explicitly identified precompiled pattern.
    ///
    /// # Errors
    ///
    /// Returns [`TimestampCatalogFormatError::UnknownPatternId`] when the ID is absent, or wraps
    /// the formatter error with the corresponding ID. The destination buffer retains its original
    /// contents on either error.
    pub fn append_epoch_nanoseconds(
        &self,
        pattern_id: u64,
        timestamp: i64,
        buffer: &mut String,
    ) -> Result<(), TimestampCatalogFormatError> {
        let pattern = self
            .pattern(pattern_id)
            .ok_or(TimestampCatalogFormatError::UnknownPatternId { pattern_id })?;
        pattern
            .append_epoch_nanoseconds(timestamp, buffer)
            .map_err(|source| TimestampCatalogFormatError::Format { pattern_id, source })
    }

    fn pattern_index(&self, pattern_id: u64) -> Option<usize> {
        match &self.index {
            PatternIndex::Direct => usize::try_from(pattern_id)
                .ok()
                .filter(|&index| index < self.patterns.len()),
            PatternIndex::Sparse(index) => index
                .binary_search_by_key(&pattern_id, |&(id, _)| id)
                .ok()
                .map(|position| index[position].1),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PatternIndex {
    Direct,
    Sparse(Vec<(u64, usize)>),
}

/// Failure to precompile an archive's timestamp patterns.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampPatternCatalogError {
    /// The dictionary contains too many patterns.
    PatternCountTooLarge {
        /// Number of patterns in the dictionary.
        actual: u64,
        /// Configured maximum pattern count.
        limit: u64,
    },
    /// The dictionary's cumulative raw pattern bytes exceed the configured limit.
    TotalPatternBytesTooLarge {
        /// Cumulative raw pattern bytes.
        actual: u64,
        /// Configured maximum cumulative bytes.
        limit: u64,
    },
    /// Checked size arithmetic or a platform conversion overflowed.
    SizeOverflow,
    /// A bounded vector reservation failed.
    AllocationFailed {
        /// Number of elements requested.
        requested: usize,
    },
    /// One resolved archive pattern could not be compiled.
    InvalidPattern {
        /// Pattern's zero-based serialized index.
        pattern_index: usize,
        /// Pattern's explicit wire ID.
        pattern_id: u64,
        /// Pattern validation failure.
        source: TimestampPatternError,
    },
}

impl Display for TimestampPatternCatalogError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PatternCountTooLarge { actual, limit } => write!(
                formatter,
                "timestamp pattern count {actual} exceeds catalog limit {limit}"
            ),
            Self::TotalPatternBytesTooLarge { actual, limit } => write!(
                formatter,
                "timestamp pattern bytes {actual} exceed catalog limit {limit}"
            ),
            Self::SizeOverflow => formatter.write_str("timestamp catalog size overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve bounded timestamp-catalog allocation of {requested} elements"
            ),
            Self::InvalidPattern {
                pattern_index,
                pattern_id,
                source,
            } => write!(
                formatter,
                "invalid timestamp pattern {pattern_index} with ID {pattern_id}: {source}"
            ),
        }
    }
}

impl Error for TimestampPatternCatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPattern { source, .. } => Some(source),
            Self::PatternCountTooLarge { .. }
            | Self::TotalPatternBytesTooLarge { .. }
            | Self::SizeOverflow
            | Self::AllocationFailed { .. } => None,
        }
    }
}

/// Failure to format a timestamp through a [`TimestampPatternCatalog`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimestampCatalogFormatError {
    /// No compiled archive pattern has the requested wire ID.
    UnknownPatternId {
        /// Missing wire ID.
        pattern_id: u64,
    },
    /// The selected compiled pattern could not format the value.
    Format {
        /// Selected wire ID.
        pattern_id: u64,
        /// Timestamp formatting failure.
        source: TimestampFormatError,
    },
}

impl Display for TimestampCatalogFormatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPatternId { pattern_id } => {
                write!(formatter, "unknown timestamp pattern ID {pattern_id}")
            }
            Self::Format { pattern_id, source } => {
                write!(
                    formatter,
                    "timestamp pattern ID {pattern_id} failed: {source}"
                )
            }
        }
    }
}

impl Error for TimestampCatalogFormatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Format { source, .. } => Some(source),
            Self::UnknownPatternId { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::TimestampDictionaryLimits;

    fn dictionary(patterns: &[(u64, &str)]) -> TimestampDictionary {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(
            &u64::try_from(patterns.len())
                .expect("test count fits u64")
                .to_le_bytes(),
        );
        for &(pattern_id, raw) in patterns {
            bytes.extend_from_slice(&pattern_id.to_le_bytes());
            bytes.extend_from_slice(
                &u64::try_from(raw.len())
                    .expect("test pattern length fits u64")
                    .to_le_bytes(),
            );
            bytes.extend_from_slice(raw.as_bytes());
        }
        TimestampDictionary::decode(bytes, TimestampDictionaryLimits::default())
            .expect("decode test timestamp dictionary")
    }

    #[test]
    fn directly_indexes_canonical_cpp_ids_and_reuses_the_output_buffer() {
        let dictionary = dictionary(&[(0, r"\L"), (1, r"\Y")]);
        let catalog =
            TimestampPatternCatalog::compile(&dictionary, TimestampPatternCatalogLimits::default())
                .expect("compile canonical patterns");

        assert_eq!(2, catalog.len());
        assert!(matches!(catalog.index, PatternIndex::Direct));
        assert_eq!(r"\L", catalog.pattern(0).expect("pattern zero").raw());
        assert_eq!(r"\Y", catalog.pattern(1).expect("pattern one").raw());
        let mut output = String::from("prefix:");
        catalog
            .append_epoch_nanoseconds(0, 1_700_000_000_123_000_000, &mut output)
            .expect("format C++ oracle timestamp");
        assert_eq!("prefix:1700000000123", output);
    }

    #[test]
    fn indexes_sparse_reordered_ids_without_allocating_to_the_largest_id() {
        let dictionary = dictionary(&[(u64::MAX, r"\L"), (7, r"\Y")]);
        let catalog =
            TimestampPatternCatalog::compile(&dictionary, TimestampPatternCatalogLimits::default())
                .expect("compile sparse patterns");

        let PatternIndex::Sparse(index) = &catalog.index else {
            panic!("noncanonical IDs must use the sparse index");
        };
        assert_eq!(2, index.len());
        assert_eq!(r"\L", catalog.pattern(u64::MAX).expect("large ID").raw());
        assert_eq!(r"\Y", catalog.pattern(7).expect("reordered ID").raw());
        assert_eq!(u64::MAX, catalog.patterns()[0].id());
        assert_eq!(7, catalog.patterns()[1].id());
    }

    #[test]
    fn annotates_invalid_patterns_with_index_and_wire_id() {
        let dictionary = dictionary(&[(9, r"\L"), (42, r"\?")]);
        let error =
            TimestampPatternCatalog::compile(&dictionary, TimestampPatternCatalogLimits::default())
                .expect_err("unresolved CAT pattern must fail");

        assert!(matches!(
            error,
            TimestampPatternCatalogError::InvalidPattern {
                pattern_index: 1,
                pattern_id: 42,
                source: TimestampPatternError::UnresolvedCatDirective {
                    index: 1,
                    directive: b'?',
                },
            }
        ));
    }

    #[test]
    fn enforces_aggregate_limits_before_compilation() {
        let dictionary = dictionary(&[(0, r"\L"), (1, r"\Y")]);
        let limits = TimestampPatternCatalogLimits::new(1, 4, TimestampPatternLimits::default());
        assert!(matches!(
            TimestampPatternCatalog::compile(&dictionary, limits),
            Err(TimestampPatternCatalogError::PatternCountTooLarge {
                actual: 2,
                limit: 1,
            })
        ));

        let limits = TimestampPatternCatalogLimits::new(2, 3, TimestampPatternLimits::default());
        assert!(matches!(
            TimestampPatternCatalog::compile(&dictionary, limits),
            Err(TimestampPatternCatalogError::TotalPatternBytesTooLarge {
                actual: 4,
                limit: 3,
            })
        ));
    }

    #[test]
    fn reports_unknown_ids_without_changing_existing_output() {
        let dictionary = dictionary(&[(0, r"\L")]);
        let catalog =
            TimestampPatternCatalog::compile(&dictionary, TimestampPatternCatalogLimits::default())
                .expect("compile pattern");
        let mut output = String::from("unchanged");
        assert_eq!(
            Err(TimestampCatalogFormatError::UnknownPatternId { pattern_id: 8 }),
            catalog.append_epoch_nanoseconds(8, 0, &mut output)
        );
        assert_eq!("unchanged", output);
    }
}
