use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use crate::options::LimitViolation;
use crate::sys::CallbackFailure;
use crate::sys::NativeArchiveError;
use crate::sys::NativeFailure;
use crate::sys::NativePhase;

/// Operation phase reported by a native archive failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArchivePhase {
    /// Enabling filters or formats.
    Configure,
    /// Opening the streaming callbacks.
    Open,
    /// Reading an archive header.
    Header,
    /// Reading current-entry metadata.
    Metadata,
    /// Reading current-entry body data.
    Data,
    /// Skipping a non-regular entry.
    Skip,
    /// Closing the archive decoder.
    Close,
    /// Freeing the archive decoder.
    Free,
}

/// Stable native failure details copied before the `libarchive` handle advances or closes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveFailure {
    pub(crate) phase: ArchivePhase,
    pub(crate) status: i32,
    pub(crate) errno: i32,
    pub(crate) message: String,
}

impl ArchiveFailure {
    /// Returns the failed operation phase.
    #[must_use]
    pub const fn phase(&self) -> ArchivePhase {
        self.phase
    }

    /// Returns the raw `libarchive` status when available.
    #[must_use]
    pub const fn status(&self) -> i32 {
        self.status
    }

    /// Returns the native errno when available, or zero.
    #[must_use]
    pub const fn errno(&self) -> i32 {
        self.errno
    }

    /// Returns copied error detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for ArchiveFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "container {:?} failed with status {} and errno {}: {}",
            self.phase, self.status, self.errno, self.message
        )
    }
}

impl Error for ArchiveFailure {}

/// Stable classification of a caller-provided reader failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InputFailureKind {
    /// [`std::io::Read::read`] returned an I/O error.
    Io,
    /// [`std::io::Read::read`] panicked; the panic was contained before returning through C.
    Panicked,
    /// A broken reader returned more bytes than its output capacity.
    ContractViolation,
}

/// Copied detail for a caller-provided reader failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputFailure {
    kind: InputFailureKind,
    message: String,
    raw_os_error: Option<i32>,
}

impl InputFailure {
    /// Returns the stable failure classification.
    #[must_use]
    pub const fn kind(&self) -> InputFailureKind {
        self.kind
    }

    /// Returns copied failure detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns an operating-system error number when one was supplied.
    #[must_use]
    pub const fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }
}

impl Display for InputFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "container input {:?}: {}",
            self.kind, self.message
        )
    }
}

impl Error for InputFailure {}

/// A successful native data-block call repeatedly returned no bytes and did not reach EOF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoProgress {
    pub(crate) entry_index: u64,
    pub(crate) consecutive_blocks: u64,
}

impl NoProgress {
    /// Returns the zero-based physical archive-entry index.
    #[must_use]
    pub const fn entry_index(self) -> u64 {
        self.entry_index
    }

    /// Returns the first rejected consecutive zero-block count.
    #[must_use]
    pub const fn consecutive_blocks(self) -> u64 {
        self.consecutive_blocks
    }
}

impl Display for NoProgress {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "archive entry {} returned {} consecutive zero-length blocks",
            self.entry_index, self.consecutive_blocks
        )
    }
}

impl Error for NoProgress {}

/// Failure produced while reading one callback-scoped [`crate::EntryReader`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EntryReadError {
    /// The recognized container became invalid while reading entry data.
    Corrupt(ArchiveFailure),
    /// The physical caller-provided reader failed.
    Input(InputFailure),
    /// A configured resource limit was exceeded.
    Limit(LimitViolation),
    /// Successful native calls stopped making progress.
    NoProgress(NoProgress),
    /// Size or position accounting overflowed.
    SizeOverflow,
}

impl Display for EntryReadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(source) => Display::fmt(source, formatter),
            Self::Input(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::NoProgress(source) => Display::fmt(source, formatter),
            Self::SizeOverflow => formatter.write_str("container entry size overflow"),
        }
    }
}

impl Error for EntryReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Corrupt(source) => Some(source),
            Self::Input(source) => Some(source),
            Self::Limit(source) => Some(source),
            Self::NoProgress(source) => Some(source),
            Self::SizeOverflow => None,
        }
    }
}

/// Initialization, streaming, visitor, or cleanup failure from [`crate::visit_entries`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ContainerError<E> {
    /// The runtime `libarchive` is older than the build contract.
    UnsupportedRuntime {
        /// Runtime numeric version.
        actual: i32,
        /// Minimum numeric version.
        minimum: i32,
    },
    /// Allocating the native archive handle failed.
    Allocation,
    /// No accepted archive format matched the input.
    NotContainer(ArchiveFailure),
    /// A recognized archive was corrupt or a native operation failed.
    Corrupt(ArchiveFailure),
    /// The caller-provided physical reader failed.
    Input(InputFailure),
    /// A configured resource limit was exceeded.
    Limit(LimitViolation),
    /// Successful native calls stopped making progress.
    NoProgress(NoProgress),
    /// The visitor returned an error.
    Visitor(E),
    /// Size or position accounting overflowed.
    SizeOverflow,
    /// Cleanup also failed after a primary operation failure.
    Cleanup {
        /// Original operation failure.
        primary: Box<Self>,
        /// Close or free failure encountered during mandatory cleanup.
        cleanup: ArchiveFailure,
    },
}

impl<E: Display> Display for ContainerError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedRuntime { actual, minimum } => write!(
                formatter,
                "libarchive runtime version {actual} is older than required {minimum}"
            ),
            Self::Allocation => formatter.write_str("failed to allocate libarchive reader"),
            Self::NotContainer(source) => write!(formatter, "input is not a container: {source}"),
            Self::Corrupt(source) => Display::fmt(source, formatter),
            Self::Input(source) => Display::fmt(source, formatter),
            Self::Limit(source) => Display::fmt(source, formatter),
            Self::NoProgress(source) => Display::fmt(source, formatter),
            Self::Visitor(source) => write!(formatter, "container visitor failed: {source}"),
            Self::SizeOverflow => formatter.write_str("container size overflow"),
            Self::Cleanup { primary, cleanup } => {
                write!(formatter, "{primary}; cleanup also failed: {cleanup}")
            }
        }
    }
}

impl<E: Error + 'static> Error for ContainerError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotContainer(source) | Self::Corrupt(source) => Some(source),
            Self::Input(source) => Some(source),
            Self::Limit(source) => Some(source),
            Self::NoProgress(source) => Some(source),
            Self::Visitor(source) => Some(source),
            Self::Cleanup { primary, .. } => Some(primary),
            Self::UnsupportedRuntime { .. } | Self::Allocation | Self::SizeOverflow => None,
        }
    }
}

pub fn top_error_from_entry<E>(error: EntryReadError) -> ContainerError<E> {
    match error {
        EntryReadError::Corrupt(source) => ContainerError::Corrupt(source),
        EntryReadError::Input(source) => ContainerError::Input(source),
        EntryReadError::Limit(source) => ContainerError::Limit(source),
        EntryReadError::NoProgress(source) => ContainerError::NoProgress(source),
        EntryReadError::SizeOverflow => ContainerError::SizeOverflow,
    }
}

pub fn top_error_from_native<E>(failure: NativeFailure, before_first: bool) -> ContainerError<E> {
    match failure {
        NativeFailure::Allocation => ContainerError::Allocation,
        NativeFailure::RuntimeVersion { actual, minimum } => {
            ContainerError::UnsupportedRuntime { actual, minimum }
        }
        NativeFailure::Callback(CallbackFailure::InputLimit { actual, limit }) => {
            ContainerError::Limit(LimitViolation::new(
                crate::LimitResource::InputBytes,
                actual,
                limit,
                None,
            ))
        }
        NativeFailure::Callback(CallbackFailure::SizeOverflow) => ContainerError::SizeOverflow,
        NativeFailure::Callback(source) => {
            ContainerError::Input(input_failure_from_callback(source))
        }
        NativeFailure::Archive(source) => {
            let not_container = before_first && !source.recognized_format;
            let source = archive_failure(source);
            if not_container {
                ContainerError::NotContainer(source)
            } else {
                ContainerError::Corrupt(source)
            }
        }
    }
}

pub fn entry_error_from_native(failure: NativeFailure) -> EntryReadError {
    match failure {
        NativeFailure::Callback(CallbackFailure::InputLimit { actual, limit }) => {
            EntryReadError::Limit(LimitViolation::new(
                crate::LimitResource::InputBytes,
                actual,
                limit,
                None,
            ))
        }
        NativeFailure::Callback(CallbackFailure::SizeOverflow)
        | NativeFailure::RuntimeVersion { .. }
        | NativeFailure::Allocation => EntryReadError::SizeOverflow,
        NativeFailure::Callback(source) => {
            EntryReadError::Input(input_failure_from_callback(source))
        }
        NativeFailure::Archive(source) => EntryReadError::Corrupt(archive_failure(source)),
    }
}

pub fn input_failure_from_callback(source: CallbackFailure) -> InputFailure {
    match source {
        CallbackFailure::Input(source) => InputFailure {
            kind: InputFailureKind::Io,
            message: source.to_string(),
            raw_os_error: source.raw_os_error(),
        },
        CallbackFailure::InputPanicked => InputFailure {
            kind: InputFailureKind::Panicked,
            message: "caller-provided Read implementation panicked".to_string(),
            raw_os_error: None,
        },
        CallbackFailure::InvalidReadCount { returned, capacity } => InputFailure {
            kind: InputFailureKind::ContractViolation,
            message: format!(
                "Read implementation returned {returned} bytes for a {capacity}-byte buffer"
            ),
            raw_os_error: None,
        },
        CallbackFailure::InputLimit { actual, limit } => InputFailure {
            kind: InputFailureKind::ContractViolation,
            message: format!("input limit {limit} was exceeded at byte {actual}"),
            raw_os_error: None,
        },
        CallbackFailure::SizeOverflow => InputFailure {
            kind: InputFailureKind::ContractViolation,
            message: "input size accounting overflowed".to_string(),
            raw_os_error: None,
        },
    }
}

pub fn archive_failure(source: NativeArchiveError) -> ArchiveFailure {
    let NativeArchiveError {
        phase,
        status,
        errno,
        message,
        ..
    } = source;
    ArchiveFailure {
        phase: match phase {
            NativePhase::Configure => ArchivePhase::Configure,
            NativePhase::Open => ArchivePhase::Open,
            NativePhase::Header => ArchivePhase::Header,
            NativePhase::Metadata => ArchivePhase::Metadata,
            NativePhase::Data => ArchivePhase::Data,
            NativePhase::Skip => ArchivePhase::Skip,
            NativePhase::Close => ArchivePhase::Close,
            NativePhase::Free => ArchivePhase::Free,
        },
        status,
        errno,
        message: String::from_utf8_lossy(&message).into_owned(),
    }
}

pub fn corrupt_entry_failure(message: &str) -> EntryReadError {
    EntryReadError::Corrupt(ArchiveFailure {
        phase: ArchivePhase::Data,
        status: -1,
        errno: 0,
        message: message.to_string(),
    })
}
