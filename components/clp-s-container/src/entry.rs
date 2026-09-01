use std::io;
use std::io::Read;

use crate::errors::EntryReadError;
use crate::errors::NoProgress;
use crate::errors::corrupt_entry_failure;
use crate::errors::entry_error_from_native;
use crate::options::ContainerLimits;
use crate::options::EntryMetadata;
use crate::options::LimitResource;
use crate::options::LimitViolation;
use crate::options::VisitControl;
use crate::sys::BlockStatus;
use crate::sys::NativeArchive;
use crate::sys::NativeBlockInfo;
use crate::sys::NativeFailure;

/// Synchronous callback for one regular archive member.
pub trait EntryVisitor {
    /// Caller-selected callback failure.
    type Error;

    /// Consumes any desired bytes and selects whether iteration continues.
    ///
    /// `body` is ephemeral and cannot outlive this invocation. Returning [`VisitControl::Continue`]
    /// automatically drains unread bytes through EOF so truncation and limits are still checked.
    ///
    /// # Errors
    ///
    /// Returns the visitor-defined error when the caller chooses to stop with a failure.
    fn visit(
        &mut self,
        metadata: &EntryMetadata,
        body: &mut EntryReader<'_>,
    ) -> Result<VisitControl, Self::Error>;
}

impl<F, E> EntryVisitor for F
where
    F: for<'entry> FnMut(&EntryMetadata, &mut EntryReader<'entry>) -> Result<VisitControl, E>,
{
    type Error = E;

    fn visit(
        &mut self,
        metadata: &EntryMetadata,
        body: &mut EntryReader<'_>,
    ) -> Result<VisitControl, Self::Error> {
        self(metadata, body)
    }
}

pub trait BlockSource {
    fn read_block(&mut self) -> Result<BlockStatus, NativeFailure>;
    fn copy_block(&self, source_offset: usize, output: &mut [u8]);
    fn release_block(&mut self);
}

impl<R: Read> BlockSource for NativeArchive<R> {
    fn read_block(&mut self) -> Result<BlockStatus, NativeFailure> {
        self.read_block()
    }

    fn copy_block(&self, source_offset: usize, output: &mut [u8]) {
        self.copy_block(source_offset, output);
    }

    fn release_block(&mut self) {
        self.release_block();
    }
}

/// Forward-only logical reader for one regular entry.
///
/// Sparse gaps are materialized as zero bytes. The reader owns no native pointer; the private
/// native layer retains the current block and refuses to advance `libarchive` until this reader
/// releases it.
pub struct EntryReader<'archive> {
    source: &'archive mut dyn BlockSource,
    limits: ContainerLimits,
    entry_index: u64,
    declared_size: Option<u64>,
    pending: Option<NativeBlockInfo>,
    pending_consumed: usize,
    sparse_remaining: u64,
    logical_position: u64,
    entry_decoded: u64,
    total_decoded: &'archive mut u64,
    zero_progress: u64,
    eof: bool,
    pub(super) terminal: Option<EntryReadError>,
}

impl<'archive> EntryReader<'archive> {
    pub(super) fn new(
        source: &'archive mut dyn BlockSource,
        limits: ContainerLimits,
        entry_index: u64,
        declared_size: Option<u64>,
        total_decoded: &'archive mut u64,
    ) -> Self {
        Self {
            source,
            limits,
            entry_index,
            declared_size,
            pending: None,
            pending_consumed: 0,
            sparse_remaining: 0,
            logical_position: 0,
            entry_decoded: 0,
            total_decoded,
            zero_progress: 0,
            // This shortcut deliberately fixes the pinned C++ zero-size ZIP hang. A data-descriptor
            // entry whose size is not known has `None` and still drives the native stream.
            eof: Some(0) == declared_size,
            terminal: None,
        }
    }

    /// Returns the header-declared size when it was known.
    ///
    /// This is a hint rather than validation. [`Self::decoded_bytes`] reports bytes actually read
    /// or automatically drained so far.
    #[must_use]
    pub const fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }

    /// Returns logical bytes delivered or drained so far for this entry.
    #[must_use]
    pub const fn decoded_bytes(&self) -> u64 {
        self.entry_decoded
    }

    /// Reads bytes while preserving typed archive, input, limit, and progress failures.
    ///
    /// An empty output returns zero without advancing the container. Once an error occurs, later
    /// reads return the same terminal error.
    ///
    /// # Errors
    ///
    /// Returns the preserved archive, input, limit, no-progress, or accounting failure.
    pub fn read_typed(&mut self, output: &mut [u8]) -> Result<usize, EntryReadError> {
        if output.is_empty() {
            return Ok(0);
        }
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }

        let result = self.read_once(output);
        if let Err(error) = &result {
            self.terminal = Some(error.clone());
        }
        result
    }

    fn read_once(&mut self, output: &mut [u8]) -> Result<usize, EntryReadError> {
        loop {
            if 0 < self.sparse_remaining {
                let write = output
                    .len()
                    .min(usize::try_from(self.sparse_remaining).unwrap_or(usize::MAX));
                self.account(write)?;
                output[..write].fill(0);
                let write = u64::try_from(write).map_err(|_| EntryReadError::SizeOverflow)?;
                self.sparse_remaining -= write;
                return usize::try_from(write).map_err(|_| EntryReadError::SizeOverflow);
            }

            if let Some(block) = self.pending
                && self.pending_consumed < block.len()
            {
                let write = output.len().min(block.len() - self.pending_consumed);
                self.account(write)?;
                self.source
                    .copy_block(self.pending_consumed, &mut output[..write]);
                self.pending_consumed += write;
                if self.pending_consumed == block.len() {
                    self.source.release_block();
                    self.pending = None;
                    self.pending_consumed = 0;
                }
                return Ok(write);
            }

            if self.eof {
                return Ok(0);
            }
            self.load_block()?;
        }
    }

    pub(super) fn drain(&mut self) -> Result<(), EntryReadError> {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            if 0 == self.read_typed(&mut buffer)? {
                return Ok(());
            }
        }
    }

    fn load_block(&mut self) -> Result<(), EntryReadError> {
        loop {
            let status = self.source.read_block().map_err(entry_error_from_native)?;
            let block = match status {
                BlockStatus::Eof => {
                    self.eof = true;
                    return Ok(());
                }
                BlockStatus::Block(block) => block,
            };
            if 0 == block.len() {
                self.source.release_block();
                self.zero_progress = self
                    .zero_progress
                    .checked_add(1)
                    .ok_or(EntryReadError::SizeOverflow)?;
                if self.zero_progress > self.limits.max_zero_progress_blocks() {
                    let error = EntryReadError::NoProgress(NoProgress {
                        entry_index: self.entry_index,
                        consecutive_blocks: self.zero_progress,
                    });
                    return self.fail(error);
                }
                continue;
            }
            self.zero_progress = 0;
            let offset = u64::try_from(block.offset()).map_err(|_| {
                corrupt_entry_failure("libarchive reported a negative data-block offset")
            })?;
            if offset < self.logical_position {
                return self.fail(corrupt_entry_failure(
                    "libarchive data blocks moved backward or overlapped",
                ));
            }
            let gap = offset - self.logical_position;
            if gap > self.limits.max_sparse_gap_bytes() {
                return self.fail(EntryReadError::Limit(LimitViolation::new(
                    LimitResource::SparseGapBytes,
                    gap,
                    self.limits.max_sparse_gap_bytes(),
                    Some(self.entry_index),
                )));
            }
            self.sparse_remaining = gap;
            self.pending = Some(block);
            self.pending_consumed = 0;
            return Ok(());
        }
    }

    fn account(&mut self, bytes: usize) -> Result<(), EntryReadError> {
        let bytes = u64::try_from(bytes).map_err(|_| EntryReadError::SizeOverflow)?;
        let entry_actual = self
            .entry_decoded
            .checked_add(bytes)
            .ok_or(EntryReadError::SizeOverflow)?;
        if entry_actual > self.limits.max_entry_decoded_bytes() {
            return self.fail(EntryReadError::Limit(LimitViolation::new(
                LimitResource::EntryDecodedBytes,
                entry_actual,
                self.limits.max_entry_decoded_bytes(),
                Some(self.entry_index),
            )));
        }
        let total_actual = self
            .total_decoded
            .checked_add(bytes)
            .ok_or(EntryReadError::SizeOverflow)?;
        if total_actual > self.limits.max_total_decoded_bytes() {
            return self.fail(EntryReadError::Limit(LimitViolation::new(
                LimitResource::TotalDecodedBytes,
                total_actual,
                self.limits.max_total_decoded_bytes(),
                Some(self.entry_index),
            )));
        }
        let logical_position = self
            .logical_position
            .checked_add(bytes)
            .ok_or(EntryReadError::SizeOverflow)?;
        self.entry_decoded = entry_actual;
        *self.total_decoded = total_actual;
        self.logical_position = logical_position;
        Ok(())
    }

    fn fail<T>(&mut self, error: EntryReadError) -> Result<T, EntryReadError> {
        self.terminal = Some(error.clone());
        Err(error)
    }
}

impl Read for EntryReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_typed(output).map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ZeroBlockSource {
        blocks: u64,
        pending: bool,
    }

    impl BlockSource for ZeroBlockSource {
        fn read_block(&mut self) -> Result<BlockStatus, NativeFailure> {
            self.blocks -= 1;
            self.pending = true;
            Ok(BlockStatus::Block(NativeBlockInfo::for_test(0, 0)))
        }

        fn copy_block(&self, _source_offset: usize, _output: &mut [u8]) {
            panic!("a zero-length native block must never be copied");
        }

        fn release_block(&mut self) {
            assert!(self.pending);
            self.pending = false;
        }
    }

    #[test]
    fn rejects_repeated_success_without_progress() {
        let mut source = ZeroBlockSource {
            blocks: 3,
            pending: false,
        };
        let limits = ContainerLimits::DEFAULT.with_max_zero_progress_blocks(2);
        let mut total = 0;
        let mut reader = EntryReader::new(&mut source, limits, 7, None, &mut total);
        let error = reader
            .read_typed(&mut [0_u8; 1])
            .expect_err("the third empty block must hit the progress bound");
        assert!(matches!(
            error,
            EntryReadError::NoProgress(NoProgress {
                entry_index: 7,
                consecutive_blocks: 3
            })
        ));
        assert_eq!(source.blocks, 0);
        assert!(!source.pending);
        assert_eq!(total, 0);
    }
}
