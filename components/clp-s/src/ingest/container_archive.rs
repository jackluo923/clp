//! Streaming general-container ingestion into an archive-set session.
//!
//! This module owns the composition between `libarchive` member callbacks and the structured
//! stream adapters. It never buffers a complete container or member, never recursively opens a
//! member as another container, and preserves opaque member paths until caller metadata policy
//! explicitly converts them.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Read;

pub use clp_s_container::ContainerError;
pub use clp_s_container::ContainerLimits;
pub use clp_s_container::ContainerOptions;
pub use clp_s_container::EntryMetadata;
use clp_s_container::EntryReader;
use clp_s_container::EntryVisitor;
pub use clp_s_container::FormatPolicy;
use clp_s_container::VisitControl;
pub use clp_s_container::VisitOutcome;
use clp_s_container::visit_entries;

use super::InputCompressionPolicy;
use super::InputError;
use super::InputLimits;
use super::StructuredInputKind;
use super::StructuredStreamError;
use super::StructuredStreamOptions;
use super::ingest_structured_stream;
use super::probe_structured_input;
use crate::writer::ArchiveSetStatsCallback;
use crate::writer::ArchiveSetWriter;
use crate::writer::ArchiveSourceContext;
use crate::writer::FinalizedArchiveSink;

/// Container, member-decoder, parser, and archive-adapter configuration.
#[derive(Clone, Copy)]
pub struct ContainerArchiveOptions<'resolver> {
    container: ContainerOptions,
    member_input: InputLimits,
    member_compression: InputCompressionPolicy,
    stream: StructuredStreamOptions<'resolver>,
}

impl<'resolver> ContainerArchiveOptions<'resolver> {
    /// Creates bounded defaults and zstd-only member decoding matching the pinned C++ probe.
    #[must_use]
    pub const fn new(container: ContainerOptions) -> Self {
        Self {
            container,
            member_input: InputLimits::DEFAULT,
            member_compression: InputCompressionPolicy::ZstdOnly,
            stream: StructuredStreamOptions::new(),
        }
    }

    /// Returns the low-level container configuration.
    #[must_use]
    pub const fn container(self) -> ContainerOptions {
        self.container
    }

    /// Returns limits applied independently to every regular member's decoded stream.
    #[must_use]
    pub const fn member_input_limits(self) -> InputLimits {
        self.member_input
    }

    /// Returns the compression-wrapper policy for each member.
    #[must_use]
    pub const fn member_compression_policy(self) -> InputCompressionPolicy {
        self.member_compression
    }

    /// Returns parser and archive-adapter configuration for each member.
    #[must_use]
    pub const fn stream_options(self) -> StructuredStreamOptions<'resolver> {
        self.stream
    }

    /// Replaces limits applied independently to every regular member.
    #[must_use]
    pub const fn with_member_input_limits(mut self, limits: InputLimits) -> Self {
        self.member_input = limits;
        self
    }

    /// Replaces member compression-wrapper detection behavior.
    #[must_use]
    pub const fn with_member_compression_policy(mut self, policy: InputCompressionPolicy) -> Self {
        self.member_compression = policy;
        self
    }

    /// Replaces member parser and archive-adapter behavior.
    #[must_use]
    pub const fn with_stream_options(
        self,
        options: StructuredStreamOptions<'_>,
    ) -> ContainerArchiveOptions<'_> {
        ContainerArchiveOptions {
            container: self.container,
            member_input: self.member_input,
            member_compression: self.member_compression,
            stream: options,
        }
    }
}

impl Default for ContainerArchiveOptions<'_> {
    fn default() -> Self {
        Self::new(ContainerOptions::default())
    }
}

/// Failure selected by the member adapter or caller metadata policy.
#[non_exhaustive]
pub enum ContainerMemberError<E, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback, {
    /// Decoding or probing one member failed.
    Probe {
        /// Zero-based physical container header index.
        entry_index: u64,
        /// Exact opaque member pathname.
        path: Vec<u8>,
        /// Typed member-input failure.
        source: InputError,
    },
    /// A regular member contained neither supported structured data nor an empty stream.
    Unsupported {
        /// Zero-based physical container header index.
        entry_index: u64,
        /// Exact opaque member pathname.
        path: Vec<u8>,
        /// Detected unsupported kind.
        kind: StructuredInputKind,
    },
    /// Caller policy could not create source metadata from the opaque member path.
    SourceContext {
        /// Zero-based physical container header index.
        entry_index: u64,
        /// Exact opaque member pathname.
        path: Vec<u8>,
        /// Caller-selected metadata failure.
        source: E,
    },
    /// Parsing, conversion, source bracketing, or archive writing failed.
    Ingest {
        /// Zero-based physical container header index.
        entry_index: u64,
        /// Exact opaque member pathname.
        path: Vec<u8>,
        /// Structured stream failure.
        source: StructuredStreamError<S, C>,
    },
    /// Aggregate member telemetry could not be represented.
    SizeOverflow {
        /// Zero-based physical container header index.
        entry_index: u64,
        /// Exact opaque member pathname.
        path: Vec<u8>,
    },
}

impl<E, S, C> fmt::Debug for ContainerMemberError<E, S, C>
where
    E: Display,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: Display,
    C::Error: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl<E, S, C> ContainerMemberError<E, S, C>
where
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    /// Returns the zero-based physical header index of the failed regular entry.
    #[must_use]
    pub const fn entry_index(&self) -> u64 {
        match self {
            Self::Probe { entry_index, .. }
            | Self::Unsupported { entry_index, .. }
            | Self::SourceContext { entry_index, .. }
            | Self::Ingest { entry_index, .. }
            | Self::SizeOverflow { entry_index, .. } => *entry_index,
        }
    }

    /// Returns the exact opaque member path associated with the failure.
    #[must_use]
    pub fn path(&self) -> &[u8] {
        match self {
            Self::Probe { path, .. }
            | Self::Unsupported { path, .. }
            | Self::SourceContext { path, .. }
            | Self::Ingest { path, .. }
            | Self::SizeOverflow { path, .. } => path,
        }
    }
}

impl<E, S, C> Display for ContainerMemberError<E, S, C>
where
    E: Display,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
    S::Error: Display,
    C::Error: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let path = EscapedBytes(self.path());
        match self {
            Self::Probe { source, .. } => write!(
                formatter,
                "failed to probe container member {path} at entry {}: {source}",
                self.entry_index()
            ),
            Self::Unsupported { kind, .. } => write!(
                formatter,
                "container member {path} at entry {} contains unsupported {kind}",
                self.entry_index()
            ),
            Self::SourceContext { source, .. } => write!(
                formatter,
                "invalid source metadata for container member {path} at entry {}: {source}",
                self.entry_index()
            ),
            Self::Ingest { source, .. } => write!(
                formatter,
                "failed to ingest container member {path} at entry {}: {source}",
                self.entry_index()
            ),
            Self::SizeOverflow { .. } => write!(
                formatter,
                "container member telemetry overflow at {path}, entry {}",
                self.entry_index()
            ),
        }
    }
}

impl<E, S, C> Error for ContainerMemberError<E, S, C>
where
    E: Error + 'static,
    S: FinalizedArchiveSink + 'static,
    C: ArchiveSetStatsCallback + 'static,
    S::Error: Error + 'static,
    C::Error: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Probe { source, .. } => Some(source),
            Self::Unsupported { .. } | Self::SizeOverflow { .. } => None,
            Self::SourceContext { source, .. } => Some(source),
            Self::Ingest { source, .. } => Some(source),
        }
    }
}

/// Boxed top-level container failure with an owned, context-rich member error.
pub type ContainerArchiveError<E, S, C> = Box<ContainerError<ContainerMemberError<E, S, C>>>;

/// Successful container visit and aggregate structured-member telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContainerArchiveOutcome {
    visit: VisitOutcome,
    truncated_json_bytes: u64,
}

impl ContainerArchiveOutcome {
    /// Returns low-level container counters and completion state.
    #[must_use]
    pub const fn visit(self) -> VisitOutcome {
        self.visit
    }

    /// Returns bytes ignored across incomplete final JSON objects in regular members.
    #[must_use]
    pub const fn truncated_json_bytes(self) -> u64 {
        self.truncated_json_bytes
    }
}

struct EscapedBytes<'path>(&'path [u8]);

impl Display for EscapedBytes<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("'")?;
        for &byte in self.0 {
            match byte {
                b'\\' => formatter.write_str("\\\\")?,
                b'\'' => formatter.write_str("\\'")?,
                0x20..=0x7e => write!(formatter, "{}", char::from(byte))?,
                _ => write!(formatter, "\\x{byte:02x}")?,
            }
        }
        formatter.write_str("'")
    }
}

struct ArchiveSetMemberVisitor<'archive, 'resolver, F, S, C> {
    archive_set: &'archive mut ArchiveSetWriter<S, C>,
    options: ContainerArchiveOptions<'resolver>,
    source_context: F,
    truncated_json_bytes: u64,
}

impl<F, E, S, C> EntryVisitor for ArchiveSetMemberVisitor<'_, '_, F, S, C>
where
    F: FnMut(&EntryMetadata) -> Result<ArchiveSourceContext, E>,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback,
{
    type Error = ContainerMemberError<E, S, C>;

    fn visit(
        &mut self,
        metadata: &EntryMetadata,
        body: &mut EntryReader<'_>,
    ) -> Result<VisitControl, Self::Error> {
        let mut input = probe_structured_input(
            body,
            self.options.member_input,
            self.options.member_compression,
        )
        .map_err(|source| ContainerMemberError::Probe {
            entry_index: metadata.entry_index(),
            path: metadata.path().to_vec(),
            source,
        })?;
        let kind = input.kind();
        if matches!(
            kind,
            StructuredInputKind::LogText | StructuredInputKind::Unknown
        ) {
            return Err(ContainerMemberError::Unsupported {
                entry_index: metadata.entry_index(),
                path: metadata.path().to_vec(),
                kind,
            });
        }
        let source = (self.source_context)(metadata).map_err(|source| {
            ContainerMemberError::SourceContext {
                entry_index: metadata.entry_index(),
                path: metadata.path().to_vec(),
                source,
            }
        })?;
        let stats = ingest_structured_stream(
            &mut input,
            kind,
            source,
            self.archive_set,
            self.options.stream,
        )
        .map_err(|source| ContainerMemberError::Ingest {
            entry_index: metadata.entry_index(),
            path: metadata.path().to_vec(),
            source,
        })?;
        self.truncated_json_bytes = self
            .truncated_json_bytes
            .checked_add(stats.truncated_json_bytes())
            .ok_or_else(|| ContainerMemberError::SizeOverflow {
                entry_index: metadata.entry_index(),
                path: metadata.path().to_vec(),
            })?;
        Ok(VisitControl::Continue)
    }
}

/// Streams every supported regular member into one caller-owned archive-set session.
///
/// `raw_fallback_name` is used only when `libarchive` selects its raw format. Real container member
/// paths remain exact opaque bytes. `source_context` is invoked only for JSON, KV-IR, or empty
/// regular members, in physical header order. Returning a context error rejects that member before
/// opening an archive source.
///
/// # Errors
///
/// Preserves every typed [`ContainerError`] classification, with member probe, unsupported-kind,
/// source-context, and structured-ingestion failures in [`ContainerError::Visitor`]. Effects from
/// earlier records or already published rotated archives are not rolled back.
pub fn ingest_container_archive_set<R, F, E, S, C>(
    source: R,
    raw_fallback_name: &[u8],
    archive_set: &mut ArchiveSetWriter<S, C>,
    options: ContainerArchiveOptions<'_>,
    source_context: F,
) -> Result<ContainerArchiveOutcome, ContainerArchiveError<E, S, C>>
where
    R: Read,
    F: FnMut(&EntryMetadata) -> Result<ArchiveSourceContext, E>,
    S: FinalizedArchiveSink,
    C: ArchiveSetStatsCallback, {
    let mut visitor = ArchiveSetMemberVisitor {
        archive_set,
        options,
        source_context,
        truncated_json_bytes: 0,
    };
    let visit = visit_entries(source, raw_fallback_name, options.container, &mut visitor)
        .map_err(Box::new)?;
    Ok(ContainerArchiveOutcome {
        visit,
        truncated_json_bytes: visitor.truncated_json_bytes,
    })
}
