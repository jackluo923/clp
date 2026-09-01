//! Bounded, streaming access to containers supported by `libarchive`.
//!
//! [`visit_entries`] never extracts a member to the filesystem and never buffers a complete
//! container or member. It visits regular members synchronously in physical archive order. Member
//! paths are opaque bytes: the adapter neither requires UTF-8 nor interprets `..`, absolute paths,
//! or platform separators.
//! A zero-byte input is a successfully completed, zero-entry container because `libarchive`
//! explicitly recognizes its empty format. [`ContainerError::NotContainer`] is reserved for an
//! input for which no enabled format bidder wins before the first header; failures after a format
//! is recognized are [`ContainerError::Corrupt`].
//!
//! The caller owns all output transaction semantics. A later corrupt member can follow successful
//! callbacks whose effects are already visible; this crate reports the error and does not attempt
//! rollback. An [`EntryReader`] is valid only during its callback and cannot be retained:
//!
//! ```compile_fail
//! use clp_s_container::{EntryMetadata, EntryReader, EntryVisitor, VisitControl};
//!
//! struct Invalid<'a> {
//!     saved: Option<&'a mut EntryReader<'a>>,
//! }
//!
//! impl EntryVisitor for Invalid<'static> {
//!     type Error = std::convert::Infallible;
//!
//!     fn visit(
//!         &mut self,
//!         _metadata: &EntryMetadata,
//!         body: &mut EntryReader<'_>,
//!     ) -> Result<VisitControl, Self::Error> {
//!         self.saved = Some(body);
//!         Ok(VisitControl::Continue)
//!     }
//! }
//! ```
//!
//! Native pointers and callback plumbing are private. Panics from a caller-provided
//! [`std::io::Read`] implementation are contained before returning through C. Visitor panics occur
//! outside a C call and unwind normally in Rust.
//!
//! Building requires a pkg-config discoverable `libarchive` 3.8.0 or newer and its headers. The
//! private shim embeds the canonical path of that selected shared object and resolves its API with
//! `dlopen`/`dlsym`. This prevents a downstream Rust binary from silently rebinding the crate to an
//! older same-SONAME system library; deployments must keep the selected shared object at that path.

#![deny(warnings)]
#![deny(unsafe_op_in_unsafe_fn)]

mod entry;
mod errors;
mod options;
mod sys;

use std::io::Read;

pub use entry::EntryReader;
pub use entry::EntryVisitor;
pub use errors::ArchiveFailure;
pub use errors::ArchivePhase;
pub use errors::ContainerError;
pub use errors::EntryReadError;
pub use errors::InputFailure;
pub use errors::InputFailureKind;
pub use errors::NoProgress;
use errors::archive_failure;
use errors::input_failure_from_callback;
use errors::top_error_from_entry;
use errors::top_error_from_native;
pub use options::ContainerLimits;
pub use options::ContainerOptions;
pub use options::ContainerStats;
pub use options::EntryMetadata;
pub use options::FormatPolicy;
pub use options::LimitResource;
pub use options::LimitViolation;
pub use options::VisitControl;
pub use options::VisitOutcome;
use sys::HeaderStatus;
use sys::NativeArchive;
use sys::NativeFailure;
use sys::NativePolicy;

enum RunCompletion {
    Completed,
    Cancelled,
}

/// Streams every regular member through `visitor` in physical header order.
///
/// For [`FormatPolicy::CppCompatible`], `raw_fallback_name` replaces `libarchive`'s synthetic
/// pathname only when the selected format is raw. Archive member names ignore it. Empty fallback
/// names are valid. The slice need only live for this call because callback metadata owns a copy.
///
/// This adapter does not recurse into a member that is itself an archive or compressed stream;
/// callers may explicitly invoke another adapter layer and enforce their own recursion depth.
/// Returning [`VisitControl::Continue`] guarantees the complete logical entry is drained and
/// validated before the next callback. Cancellation deliberately stops without draining the
/// current entry.
///
/// # Errors
///
/// Returns a typed initialization, unrecognized-format, corruption, input, limit, progress,
/// visitor, or cleanup failure. Effects from earlier successful visitor calls are not rolled back.
pub fn visit_entries<R, V>(
    source: R,
    raw_fallback_name: &[u8],
    options: ContainerOptions,
    visitor: &mut V,
) -> Result<VisitOutcome, ContainerError<V::Error>>
where
    R: Read,
    V: EntryVisitor, {
    let policy = match options.policy() {
        FormatPolicy::CppCompatible => NativePolicy::CppCompatible,
        FormatPolicy::Strict => NativePolicy::Strict,
    };
    let limits = options.limits();
    let mut archive = NativeArchive::open(source, limits.max_input_bytes(), policy)
        .map_err(|failure| top_error_from_native(failure, true))?;
    let mut stats = ContainerStats::default();
    let run_result = run_entries(
        &mut archive,
        raw_fallback_name,
        options,
        visitor,
        &mut stats,
    );
    stats.input_bytes = archive.input_bytes();
    let cleanup_result = close_and_free(&mut archive);
    stats.input_bytes = archive.input_bytes();

    match (run_result, cleanup_result) {
        (Ok(RunCompletion::Completed), Ok(())) => Ok(VisitOutcome::Completed(stats)),
        (Ok(RunCompletion::Cancelled), Ok(())) => Ok(VisitOutcome::Cancelled(stats)),
        (Ok(_), Err(cleanup)) => Err(ContainerError::Corrupt(cleanup)),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(cleanup)) => Err(ContainerError::Cleanup {
            primary: Box::new(primary),
            cleanup,
        }),
    }
}

#[allow(clippy::too_many_lines)]
fn run_entries<R, V>(
    archive: &mut NativeArchive<R>,
    raw_fallback_name: &[u8],
    options: ContainerOptions,
    visitor: &mut V,
    stats: &mut ContainerStats,
) -> Result<RunCompletion, ContainerError<V::Error>>
where
    R: Read,
    V: EntryVisitor, {
    let limits = options.limits();
    let mut first_header = true;
    loop {
        match archive
            .next_header()
            .map_err(|failure| top_error_from_native(failure, first_header))?
        {
            HeaderStatus::Eof => {
                if first_header {
                    if !archive.has_format() {
                        return Err(ContainerError::NotContainer(ArchiveFailure {
                            phase: ArchivePhase::Header,
                            status: 1,
                            errno: 0,
                            message: "input ended before libarchive recognized a format"
                                .to_string(),
                        }));
                    }
                    validate_first_header(archive, options, stats)?;
                }
                return Ok(RunCompletion::Completed);
            }
            HeaderStatus::Header => {}
        }

        if first_header {
            validate_first_header(archive, options, stats)?;
            first_header = false;
        }

        let entry_index = stats.entries_seen;
        let entries_actual = stats
            .entries_seen
            .checked_add(1)
            .ok_or(ContainerError::SizeOverflow)?;
        if entries_actual > limits.max_entries() {
            return Err(ContainerError::Limit(LimitViolation::new(
                LimitResource::Entries,
                entries_actual,
                limits.max_entries(),
                Some(entry_index),
            )));
        }
        stats.entries_seen = entries_actual;

        if !archive.current_is_regular() || archive.current_is_hardlink() {
            stats.special_entries_skipped = stats
                .special_entries_skipped
                .checked_add(1)
                .ok_or(ContainerError::SizeOverflow)?;
            archive
                .skip_data()
                .map_err(|failure| top_error_from_native(failure, false))?;
            continue;
        }

        let path = current_path(archive, raw_fallback_name, limits, entry_index)?;
        let declared_size = archive
            .current_size()
            .map_err(|failure| top_error_from_native(failure, false))?;
        let metadata = EntryMetadata {
            path,
            entry_index,
            declared_size,
        };
        stats.regular_entries_visited = stats
            .regular_entries_visited
            .checked_add(1)
            .ok_or(ContainerError::SizeOverflow)?;

        let mut body = EntryReader::new(
            archive,
            limits,
            entry_index,
            declared_size,
            &mut stats.decoded_bytes,
        );
        let visit_result = visitor.visit(&metadata, &mut body);
        if let Some(error) = body.terminal.clone() {
            return Err(top_error_from_entry(error));
        }
        match visit_result {
            Err(source) => return Err(ContainerError::Visitor(source)),
            Ok(VisitControl::Cancel) => return Ok(RunCompletion::Cancelled),
            Ok(VisitControl::Continue) => body.drain().map_err(top_error_from_entry)?,
        }
    }
}

fn validate_first_header<R: Read, E>(
    archive: &NativeArchive<R>,
    options: ContainerOptions,
    stats: &mut ContainerStats,
) -> Result<(), ContainerError<E>> {
    if FormatPolicy::Strict == options.policy() && archive.is_mtree() {
        return Err(ContainerError::NotContainer(ArchiveFailure {
            phase: ArchivePhase::Header,
            status: -1,
            errno: 0,
            message: "strict policy rejects libarchive's mtree format".to_string(),
        }));
    }
    let filter_layers = u64::from(
        archive
            .filter_count()
            .map_err(|failure| top_error_from_native(failure, false))?,
    );
    let limit = options.limits().max_filter_layers();
    if filter_layers > limit {
        return Err(ContainerError::Limit(LimitViolation::new(
            LimitResource::FilterLayers,
            filter_layers,
            limit,
            None,
        )));
    }
    stats.filter_layers = filter_layers;
    Ok(())
}

fn current_path<R: Read, E>(
    archive: &NativeArchive<R>,
    raw_fallback_name: &[u8],
    limits: ContainerLimits,
    entry_index: u64,
) -> Result<Vec<u8>, ContainerError<E>> {
    if archive.is_raw() {
        check_path_limit(raw_fallback_name.len(), limits, entry_index)?;
        Ok(raw_fallback_name.to_vec())
    } else {
        let path_length = archive
            .current_path_length()
            .map_err(|failure| top_error_from_native(failure, false))?;
        check_path_limit(path_length, limits, entry_index)?;
        archive
            .copy_current_path(path_length)
            .map_err(|failure| top_error_from_native(failure, false))
    }
}

fn check_path_limit<E>(
    path_length: usize,
    limits: ContainerLimits,
    entry_index: u64,
) -> Result<(), ContainerError<E>> {
    let path_length = u64::try_from(path_length).map_err(|_| ContainerError::SizeOverflow)?;
    if path_length > limits.max_path_bytes() {
        Err(ContainerError::Limit(LimitViolation::new(
            LimitResource::PathBytes,
            path_length,
            limits.max_path_bytes(),
            Some(entry_index),
        )))
    } else {
        Ok(())
    }
}

fn close_and_free<R: Read>(archive: &mut NativeArchive<R>) -> Result<(), ArchiveFailure> {
    let close = archive.close().err().map(cleanup_failure_from_native);
    let free = archive.free().err().map(cleanup_failure_from_native);
    match (close, free) {
        (None, None) => Ok(()),
        (Some(failure), None) | (None, Some(failure)) => Err(failure),
        (Some(mut close), Some(free)) => {
            close.message.push_str("; free also failed: ");
            close.message.push_str(&free.to_string());
            Err(close)
        }
    }
}

fn cleanup_failure_from_native(failure: NativeFailure) -> ArchiveFailure {
    match failure {
        NativeFailure::Archive(source) => archive_failure(source),
        NativeFailure::Callback(source) => ArchiveFailure {
            phase: ArchivePhase::Close,
            status: -1,
            errno: 0,
            message: input_failure_from_callback(source).to_string(),
        },
        NativeFailure::RuntimeVersion { actual, minimum } => ArchiveFailure {
            phase: ArchivePhase::Close,
            status: -1,
            errno: 0,
            message: format!("libarchive runtime {actual} is older than {minimum}"),
        },
        NativeFailure::Allocation => ArchiveFailure {
            phase: ArchivePhase::Free,
            status: -1,
            errno: 0,
            message: "native allocation failed during cleanup".to_string(),
        },
    }
}
