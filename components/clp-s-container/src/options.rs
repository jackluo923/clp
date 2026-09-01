use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

const MEBIBYTE: u64 = 1024 * 1024;
const GIBIBYTE: u64 = 1024 * MEBIBYTE;

/// Container format-selection behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum FormatPolicy {
    /// Matches the pinned C++ setup: enable every filter and format, then register raw last.
    ///
    /// `libarchive`'s permissive mtree bidder remains enabled, including its known ability to
    /// misclassify some decompressed text streams before the raw fallback can win.
    CppCompatible,
    /// Rejects raw streams and mtree matches while accepting recognized archive formats.
    /// `libarchive`'s explicit empty format is recognized and completes with no callbacks.
    #[default]
    Strict,
}

/// Independent resource limits for one physical container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContainerLimits {
    pub(crate) input_bytes: u64,
    pub(crate) entry_decoded_bytes: u64,
    pub(crate) total_decoded_bytes: u64,
    pub(crate) entries: u64,
    pub(crate) path_bytes: u64,
    pub(crate) sparse_gap_bytes: u64,
    pub(crate) zero_progress_blocks: u64,
    pub(crate) filter_layers: u64,
}

impl ContainerLimits {
    /// Safety-oriented defaults suitable for large local log containers.
    pub const DEFAULT: Self = Self {
        input_bytes: 64 * GIBIBYTE,
        entry_decoded_bytes: 64 * GIBIBYTE,
        total_decoded_bytes: 256 * GIBIBYTE,
        entries: 1_000_000,
        path_bytes: MEBIBYTE,
        sparse_gap_bytes: GIBIBYTE,
        zero_progress_blocks: 4,
        filter_layers: 8,
    };

    /// Returns the physical compressed-input limit.
    #[must_use]
    pub const fn max_input_bytes(self) -> u64 {
        self.input_bytes
    }

    /// Returns the decoded-byte limit for each regular entry.
    #[must_use]
    pub const fn max_entry_decoded_bytes(self) -> u64 {
        self.entry_decoded_bytes
    }

    /// Returns the decoded-byte limit across all regular entries.
    #[must_use]
    pub const fn max_total_decoded_bytes(self) -> u64 {
        self.total_decoded_bytes
    }

    /// Returns the total header-count limit, including skipped special entries.
    #[must_use]
    pub const fn max_entries(self) -> u64 {
        self.entries
    }

    /// Returns the byte-length limit for one member path or raw fallback name.
    #[must_use]
    pub const fn max_path_bytes(self) -> u64 {
        self.path_bytes
    }

    /// Returns the maximum zero-filled gap before one sparse data block.
    #[must_use]
    pub const fn max_sparse_gap_bytes(self) -> u64 {
        self.sparse_gap_bytes
    }

    /// Returns the permitted consecutive successful zero-length data blocks.
    #[must_use]
    pub const fn max_zero_progress_blocks(self) -> u64 {
        self.zero_progress_blocks
    }

    /// Returns the maximum number of filters reported by `libarchive`.
    #[must_use]
    pub const fn max_filter_layers(self) -> u64 {
        self.filter_layers
    }

    /// Replaces the physical compressed-input limit.
    #[must_use]
    pub const fn with_max_input_bytes(mut self, limit: u64) -> Self {
        self.input_bytes = limit;
        self
    }

    /// Replaces the per-entry decoded-byte limit.
    #[must_use]
    pub const fn with_max_entry_decoded_bytes(mut self, limit: u64) -> Self {
        self.entry_decoded_bytes = limit;
        self
    }

    /// Replaces the total decoded-byte limit.
    #[must_use]
    pub const fn with_max_total_decoded_bytes(mut self, limit: u64) -> Self {
        self.total_decoded_bytes = limit;
        self
    }

    /// Replaces the archive-header count limit.
    #[must_use]
    pub const fn with_max_entries(mut self, limit: u64) -> Self {
        self.entries = limit;
        self
    }

    /// Replaces the member-path byte-length limit.
    #[must_use]
    pub const fn with_max_path_bytes(mut self, limit: u64) -> Self {
        self.path_bytes = limit;
        self
    }

    /// Replaces the maximum sparse gap before one data block.
    #[must_use]
    pub const fn with_max_sparse_gap_bytes(mut self, limit: u64) -> Self {
        self.sparse_gap_bytes = limit;
        self
    }

    /// Replaces the consecutive successful zero-block limit.
    #[must_use]
    pub const fn with_max_zero_progress_blocks(mut self, limit: u64) -> Self {
        self.zero_progress_blocks = limit;
        self
    }

    /// Replaces the `libarchive` filter-layer limit.
    #[must_use]
    pub const fn with_max_filter_layers(mut self, limit: u64) -> Self {
        self.filter_layers = limit;
        self
    }
}

impl Default for ContainerLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Configuration for one container visit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct ContainerOptions {
    policy: FormatPolicy,
    limits: ContainerLimits,
}

impl ContainerOptions {
    /// Creates options with an explicit format policy and the default limits.
    #[must_use]
    pub const fn new(policy: FormatPolicy) -> Self {
        Self {
            policy,
            limits: ContainerLimits::DEFAULT,
        }
    }

    /// Returns the format-selection policy.
    #[must_use]
    pub const fn policy(self) -> FormatPolicy {
        self.policy
    }

    /// Returns the resource limits.
    #[must_use]
    pub const fn limits(self) -> ContainerLimits {
        self.limits
    }

    /// Replaces the resource limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: ContainerLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Resource guarded by [`ContainerLimits`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LimitResource {
    /// Bytes pulled from the caller-provided physical reader.
    InputBytes,
    /// Decoded logical bytes in one regular entry, including sparse zeros.
    EntryDecodedBytes,
    /// Decoded logical bytes across all regular entries.
    TotalDecodedBytes,
    /// Archive headers, including skipped special entries.
    Entries,
    /// Opaque bytes in one member path or raw fallback name.
    PathBytes,
    /// Zero-filled bytes before one sparse data block.
    SparseGapBytes,
    /// Consecutive successful zero-length blocks without progress.
    ZeroProgressBlocks,
    /// Filters detected by `libarchive` for the physical input.
    FilterLayers,
}

impl Display for LimitResource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InputBytes => "physical input bytes",
            Self::EntryDecodedBytes => "decoded entry bytes",
            Self::TotalDecodedBytes => "total decoded bytes",
            Self::Entries => "archive entries",
            Self::PathBytes => "member path bytes",
            Self::SparseGapBytes => "sparse gap bytes",
            Self::ZeroProgressBlocks => "zero-progress blocks",
            Self::FilterLayers => "container filter layers",
        })
    }
}

/// Exact measurement for one rejected limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitViolation {
    resource: LimitResource,
    actual: u64,
    limit: u64,
    entry_index: Option<u64>,
}

impl LimitViolation {
    pub(crate) const fn new(
        resource: LimitResource,
        actual: u64,
        limit: u64,
        entry_index: Option<u64>,
    ) -> Self {
        Self {
            resource,
            actual,
            limit,
            entry_index,
        }
    }

    /// Returns the limited resource.
    #[must_use]
    pub const fn resource(self) -> LimitResource {
        self.resource
    }

    /// Returns the first rejected size or count.
    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    /// Returns the configured maximum.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }

    /// Returns the zero-based physical archive-entry index when applicable.
    #[must_use]
    pub const fn entry_index(self) -> Option<u64> {
        self.entry_index
    }
}

impl Display for LimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(entry_index) = self.entry_index {
            write!(
                formatter,
                "{} {} at archive entry {entry_index} exceeds limit {}",
                self.actual, self.resource, self.limit
            )
        } else {
            write!(
                formatter,
                "{} {} exceeds limit {}",
                self.actual, self.resource, self.limit
            )
        }
    }
}

impl Error for LimitViolation {}

/// Streaming counters accumulated by a visit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContainerStats {
    pub(crate) input_bytes: u64,
    pub(crate) decoded_bytes: u64,
    pub(crate) entries_seen: u64,
    pub(crate) regular_entries_visited: u64,
    pub(crate) special_entries_skipped: u64,
    pub(crate) filter_layers: u64,
}

impl ContainerStats {
    /// Returns physical bytes pulled from the caller-provided reader.
    #[must_use]
    pub const fn input_bytes(self) -> u64 {
        self.input_bytes
    }

    /// Returns decoded logical regular-entry bytes, including sparse zeros.
    #[must_use]
    pub const fn decoded_bytes(self) -> u64 {
        self.decoded_bytes
    }

    /// Returns archive headers observed, including skipped special entries.
    #[must_use]
    pub const fn entries_seen(self) -> u64 {
        self.entries_seen
    }

    /// Returns regular-entry callbacks invoked.
    #[must_use]
    pub const fn regular_entries_visited(self) -> u64 {
        self.regular_entries_visited
    }

    /// Returns non-regular or hardlink entries skipped.
    #[must_use]
    pub const fn special_entries_skipped(self) -> u64 {
        self.special_entries_skipped
    }

    /// Returns filters reported by `libarchive` after format detection.
    #[must_use]
    pub const fn filter_layers(self) -> u64 {
        self.filter_layers
    }
}

/// Successful completion or callback-controlled cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VisitOutcome {
    /// Every header and regular member was processed.
    Completed(ContainerStats),
    /// A regular-member callback requested early cancellation.
    Cancelled(ContainerStats),
}

impl VisitOutcome {
    /// Returns counters accumulated before completion or cancellation.
    #[must_use]
    pub const fn stats(self) -> ContainerStats {
        match self {
            Self::Completed(stats) | Self::Cancelled(stats) => stats,
        }
    }
}

/// Owned metadata for one regular entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryMetadata {
    pub(crate) path: Vec<u8>,
    pub(crate) entry_index: u64,
    pub(crate) declared_size: Option<u64>,
}

impl EntryMetadata {
    /// Returns the pathname exactly as supplied by the archive or raw fallback.
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// Returns the zero-based physical header index, including skipped entries.
    #[must_use]
    pub const fn entry_index(&self) -> u64 {
        self.entry_index
    }

    /// Returns the uncompressed header size when `libarchive` marked it as known.
    ///
    /// This is only a hint. The authoritative logical byte count is obtained by reading the body
    /// through EOF and is available in [`ContainerStats::decoded_bytes`].
    #[must_use]
    pub const fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }
}

/// Visitor decision after one regular entry callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisitControl {
    /// Drain any unread entry bytes and continue with the next header.
    Continue,
    /// Stop immediately without visiting another entry.
    Cancel,
}
