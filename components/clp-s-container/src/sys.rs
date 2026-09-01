use std::ffi::c_int;
use std::ffi::c_void;
use std::io;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::ptr;
use std::ptr::NonNull;

const INPUT_BUFFER_BYTES: usize = 64 * 1024;
const MINIMUM_RUNTIME_VERSION: i32 = 3_008_000;

const NATIVE_ERROR: c_int = -1;
const NATIVE_OK: c_int = 0;
const NATIVE_EOF: c_int = 1;

#[repr(C)]
struct CArchive {
    _private: [u8; 0],
}

type CReadCallback = unsafe extern "C" fn(*mut c_void, *mut *const u8) -> i64;

unsafe extern "C" {
    fn clp_s_container_archive_new() -> *mut CArchive;
    fn clp_s_container_archive_configure(archive: *mut CArchive, policy: c_int) -> c_int;
    fn clp_s_container_archive_open(
        archive: *mut CArchive,
        client_data: *mut c_void,
        read_callback: Option<CReadCallback>,
    ) -> c_int;
    fn clp_s_container_archive_next_header(archive: *mut CArchive) -> c_int;
    fn clp_s_container_archive_data_skip(archive: *mut CArchive) -> c_int;
    fn clp_s_container_archive_data_block(
        archive: *mut CArchive,
        buffer: *mut *const u8,
        length: *mut usize,
        offset: *mut i64,
    ) -> c_int;
    fn clp_s_container_archive_close(archive: *mut CArchive) -> c_int;
    fn clp_s_container_archive_free(archive: *mut CArchive) -> c_int;

    fn clp_s_container_archive_current_is_regular(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_current_is_hardlink(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_current_size(
        archive: *const CArchive,
        is_set: *mut c_int,
        size: *mut i64,
    ) -> c_int;
    fn clp_s_container_archive_current_path_length(
        archive: *const CArchive,
        length: *mut usize,
    ) -> c_int;
    fn clp_s_container_archive_current_path_copy(
        archive: *const CArchive,
        output: *mut u8,
        length: usize,
    ) -> c_int;
    fn clp_s_container_archive_is_raw(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_is_mtree(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_has_format(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_filter_count(archive: *const CArchive, count: *mut c_int) -> c_int;

    fn clp_s_container_archive_last_status(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_errno(archive: *const CArchive) -> c_int;
    fn clp_s_container_archive_error_length(archive: *const CArchive, length: *mut usize) -> c_int;
    fn clp_s_container_archive_error_copy(
        archive: *const CArchive,
        output: *mut u8,
        length: usize,
    ) -> c_int;
    fn clp_s_container_runtime_version() -> c_int;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativePolicy {
    CppCompatible = 0,
    Strict = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativePhase {
    Configure,
    Open,
    Header,
    Metadata,
    Data,
    Skip,
    Close,
    Free,
}

#[derive(Debug)]
pub struct NativeArchiveError {
    pub phase: NativePhase,
    pub status: i32,
    pub errno: i32,
    pub message: Vec<u8>,
    pub recognized_format: bool,
}

#[derive(Debug)]
pub enum CallbackFailure {
    Input(io::Error),
    InputPanicked,
    InvalidReadCount { returned: usize, capacity: usize },
    InputLimit { actual: u64, limit: u64 },
    SizeOverflow,
}

#[derive(Debug)]
pub enum NativeFailure {
    Allocation,
    RuntimeVersion { actual: i32, minimum: i32 },
    Callback(CallbackFailure),
    Archive(NativeArchiveError),
}

pub enum HeaderStatus {
    Header,
    Eof,
}

pub enum BlockStatus {
    Block(NativeBlockInfo),
    Eof,
}

#[derive(Clone, Copy, Debug)]
pub struct NativeBlockInfo {
    length: usize,
    offset: i64,
}

impl NativeBlockInfo {
    #[cfg(test)]
    pub(super) const fn for_test(length: usize, offset: i64) -> Self {
        Self { length, offset }
    }

    pub(super) const fn len(&self) -> usize {
        self.length
    }

    pub(super) const fn offset(&self) -> i64 {
        self.offset
    }
}

struct InputPipe<R> {
    reader: R,
    buffer: Box<[u8]>,
    bytes_read: u64,
    max_bytes: u64,
    failure: Option<CallbackFailure>,
    stopped: bool,
}

impl<R> InputPipe<R> {
    fn new(reader: R, max_bytes: u64) -> Self {
        Self {
            reader,
            buffer: vec![0_u8; INPUT_BUFFER_BYTES].into_boxed_slice(),
            bytes_read: 0,
            max_bytes,
            failure: None,
            stopped: false,
        }
    }

    fn fail(&mut self, failure: CallbackFailure) -> i64 {
        self.failure = Some(failure);
        self.stopped = true;
        -1
    }
}

unsafe extern "C" fn input_read_callback<R: Read>(
    client_data: *mut c_void,
    output: *mut *const u8,
) -> i64 {
    if client_data.is_null() || output.is_null() {
        return -1;
    }
    // SAFETY: NativeArchive passes the stable address of its boxed InputPipe and keeps that box
    // alive until after archive_read_free. libarchive serializes callbacks for one archive.
    let pipe = unsafe { &mut *client_data.cast::<InputPipe<R>>() };
    if pipe.stopped {
        return -1;
    }

    // Always provide a live pointer, including for EOF. The C shim ignores it when zero bytes are
    // returned, while this avoids handing libarchive an indeterminate pointer.
    // SAFETY: `output` was validated non-null and points to libarchive-owned callback storage.
    unsafe {
        output.write(pipe.buffer.as_ptr());
    }

    let Some(remaining) = pipe.max_bytes.checked_sub(pipe.bytes_read) else {
        return pipe.fail(CallbackFailure::SizeOverflow);
    };
    let probing_limit = 0 == remaining;
    let capacity = if probing_limit {
        1
    } else {
        pipe.buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX))
    };

    let read_result = loop {
        let result = catch_unwind(AssertUnwindSafe(|| {
            pipe.reader.read(&mut pipe.buffer[..capacity])
        }));
        match result {
            Ok(Err(source)) if io::ErrorKind::Interrupted == source.kind() => {}
            _ => break result,
        }
    };

    let read = match read_result {
        Ok(Ok(read)) => read,
        Ok(Err(source)) => return pipe.fail(CallbackFailure::Input(source)),
        Err(_) => return pipe.fail(CallbackFailure::InputPanicked),
    };
    if read > capacity {
        return pipe.fail(CallbackFailure::InvalidReadCount {
            returned: read,
            capacity,
        });
    }
    if probing_limit && 0 < read {
        let Some(actual) = pipe.max_bytes.checked_add(1) else {
            return pipe.fail(CallbackFailure::SizeOverflow);
        };
        return pipe.fail(CallbackFailure::InputLimit {
            actual,
            limit: pipe.max_bytes,
        });
    }
    let Ok(read_u64) = u64::try_from(read) else {
        return pipe.fail(CallbackFailure::SizeOverflow);
    };
    let Some(bytes_read) = pipe.bytes_read.checked_add(read_u64) else {
        return pipe.fail(CallbackFailure::SizeOverflow);
    };
    pipe.bytes_read = bytes_read;
    i64::try_from(read).unwrap_or_else(|_| pipe.fail(CallbackFailure::SizeOverflow))
}

pub struct NativeArchive<R: Read> {
    raw: Option<NonNull<CArchive>>,
    pipe: Box<InputPipe<R>>,
    opened: bool,
    closed: bool,
    pending_block: Option<PendingBlock>,
}

struct PendingBlock {
    pointer: *const u8,
    length: usize,
}

impl<R: Read> NativeArchive<R> {
    pub(super) fn open(
        reader: R,
        max_input_bytes: u64,
        policy: NativePolicy,
    ) -> Result<Self, NativeFailure> {
        // SAFETY: This function takes no pointers and returns a process-global numeric version.
        let runtime_version = unsafe { clp_s_container_runtime_version() };
        if runtime_version < MINIMUM_RUNTIME_VERSION {
            return Err(NativeFailure::RuntimeVersion {
                actual: runtime_version,
                minimum: MINIMUM_RUNTIME_VERSION,
            });
        }

        // SAFETY: The shim returns either null or a uniquely owned allocation.
        let raw = NonNull::new(unsafe { clp_s_container_archive_new() })
            .ok_or(NativeFailure::Allocation)?;
        let mut archive = Self {
            raw: Some(raw),
            pipe: Box::new(InputPipe::new(reader, max_input_bytes)),
            opened: false,
            closed: false,
            pending_block: None,
        };

        // SAFETY: `raw` is a live uniquely owned shim handle and the policy values are frozen by
        // the shim header.
        let configured =
            unsafe { clp_s_container_archive_configure(archive.raw_ptr(), policy as c_int) };
        if NATIVE_OK != configured {
            return Err(archive.failure(NativePhase::Configure));
        }

        let client_data = std::ptr::from_mut(archive.pipe.as_mut()).cast::<c_void>();
        // SAFETY: `client_data` points into a stable Box owned by `archive`. It remains alive until
        // after the shim handle is closed and freed. The monomorphized callback matches its type.
        let opened = unsafe {
            clp_s_container_archive_open(
                archive.raw_ptr(),
                client_data,
                Some(input_read_callback::<R>),
            )
        };
        if NATIVE_OK != opened {
            return Err(archive.failure(NativePhase::Open));
        }
        archive.opened = true;
        Ok(archive)
    }

    pub(super) fn next_header(&mut self) -> Result<HeaderStatus, NativeFailure> {
        assert!(
            self.pending_block.is_none(),
            "the current native block must be released before reading another header"
        );
        // SAFETY: The handle is live, opened, and exclusively borrowed.
        match unsafe { clp_s_container_archive_next_header(self.raw_ptr()) } {
            NATIVE_OK => Ok(HeaderStatus::Header),
            NATIVE_EOF => Ok(HeaderStatus::Eof),
            _ => Err(self.failure(NativePhase::Header)),
        }
    }

    pub(super) fn current_is_regular(&self) -> bool {
        // SAFETY: A successful next_header established a current entry and the handle is live.
        0 != unsafe { clp_s_container_archive_current_is_regular(self.raw_ptr()) }
    }

    pub(super) fn current_is_hardlink(&self) -> bool {
        // SAFETY: A successful next_header established a current entry and the handle is live.
        0 != unsafe { clp_s_container_archive_current_is_hardlink(self.raw_ptr()) }
    }

    pub(super) fn current_size(&self) -> Result<Option<u64>, NativeFailure> {
        let mut is_set = 0;
        let mut size = 0_i64;
        // SAFETY: Output pointers are live and a successful next_header established an entry.
        let result = unsafe {
            clp_s_container_archive_current_size(self.raw_ptr(), &raw mut is_set, &raw mut size)
        };
        if NATIVE_OK != result {
            return Err(self.archive_failure(NativePhase::Metadata));
        }
        if 0 == is_set {
            return Ok(None);
        }
        u64::try_from(size).map(Some).map_err(|_| {
            Self::custom_archive_failure(
                NativePhase::Metadata,
                b"libarchive reported a negative entry size".to_vec(),
            )
        })
    }

    pub(super) fn current_path_length(&self) -> Result<usize, NativeFailure> {
        let mut length = 0_usize;
        // SAFETY: The output pointer is live and a successful next_header established an entry.
        let result =
            unsafe { clp_s_container_archive_current_path_length(self.raw_ptr(), &raw mut length) };
        if NATIVE_OK == result {
            Ok(length)
        } else {
            Err(self.archive_failure(NativePhase::Metadata))
        }
    }

    pub(super) fn copy_current_path(&self, length: usize) -> Result<Vec<u8>, NativeFailure> {
        let mut path = vec![0_u8; length];
        let output = if path.is_empty() {
            ptr::null_mut()
        } else {
            path.as_mut_ptr()
        };
        // SAFETY: `output` is null exactly for zero length and otherwise points to `length`
        // writable bytes. No archive call occurred since current_path_length.
        let result =
            unsafe { clp_s_container_archive_current_path_copy(self.raw_ptr(), output, length) };
        if NATIVE_OK == result {
            Ok(path)
        } else {
            Err(self.archive_failure(NativePhase::Metadata))
        }
    }

    pub(super) fn is_raw(&self) -> bool {
        // SAFETY: The handle is live and format detection has run by the first header.
        0 != unsafe { clp_s_container_archive_is_raw(self.raw_ptr()) }
    }

    pub(super) fn is_mtree(&self) -> bool {
        // SAFETY: The handle is live and format detection has run by the first header.
        0 != unsafe { clp_s_container_archive_is_mtree(self.raw_ptr()) }
    }

    pub(super) fn has_format(&self) -> bool {
        // SAFETY: The handle is live and format detection has run by the first header or EOF.
        0 != unsafe { clp_s_container_archive_has_format(self.raw_ptr()) }
    }

    pub(super) fn filter_count(&self) -> Result<u32, NativeFailure> {
        let mut count = 0;
        // SAFETY: The output pointer and handle are live.
        let result =
            unsafe { clp_s_container_archive_filter_count(self.raw_ptr(), &raw mut count) };
        if NATIVE_OK != result {
            return Err(self.archive_failure(NativePhase::Metadata));
        }
        u32::try_from(count).map_err(|_| {
            Self::custom_archive_failure(
                NativePhase::Metadata,
                b"libarchive reported a negative filter count".to_vec(),
            )
        })
    }

    pub(super) fn skip_data(&mut self) -> Result<(), NativeFailure> {
        assert!(
            self.pending_block.is_none(),
            "the current native block must be released before skipping entry data"
        );
        // SAFETY: The handle is live and exclusively borrowed with a current entry.
        let result = unsafe { clp_s_container_archive_data_skip(self.raw_ptr()) };
        if NATIVE_OK == result || NATIVE_EOF == result {
            Ok(())
        } else {
            Err(self.failure(NativePhase::Skip))
        }
    }

    pub(super) fn read_block(&mut self) -> Result<BlockStatus, NativeFailure> {
        assert!(
            self.pending_block.is_none(),
            "the current native block must be released before advancing libarchive"
        );
        let mut pointer = ptr::null();
        let mut length = 0_usize;
        let mut offset = 0_i64;
        // SAFETY: Output pointers and the exclusively borrowed live archive handle are valid.
        let result = unsafe {
            clp_s_container_archive_data_block(
                self.raw_ptr(),
                &raw mut pointer,
                &raw mut length,
                &raw mut offset,
            )
        };
        match result {
            NATIVE_EOF => Ok(BlockStatus::Eof),
            NATIVE_OK if 0 < length && pointer.is_null() => Err(Self::custom_archive_failure(
                NativePhase::Data,
                b"libarchive returned a null nonempty data block".to_vec(),
            )),
            NATIVE_OK => {
                self.pending_block = Some(PendingBlock { pointer, length });
                Ok(BlockStatus::Block(NativeBlockInfo { length, offset }))
            }
            _ => Err(self.failure(NativePhase::Data)),
        }
    }

    /// Copies from the current libarchive block without exposing its native pointer.
    ///
    /// The block pointer remains valid until the next libarchive operation. `read_block` refuses
    /// to perform that operation while a block is pending, and only `release_block` clears the
    /// guard. Consequently no safe caller can retain or dereference a block after invalidation.
    pub(super) fn copy_block(&self, source_offset: usize, output: &mut [u8]) {
        let block = self
            .pending_block
            .as_ref()
            .expect("copying requires a pending native block");
        assert!(source_offset <= block.length);
        assert!(output.len() <= block.length - source_offset);
        if output.is_empty() {
            return;
        }
        // SAFETY: `read_block` validated a non-null pointer for a nonempty block. The pending-block
        // guard proves that no subsequent libarchive operation has invalidated it. Bounds are
        // asserted above and `output` is independently caller-owned, so the regions do not overlap.
        unsafe {
            ptr::copy_nonoverlapping(
                block.pointer.add(source_offset),
                output.as_mut_ptr(),
                output.len(),
            );
        }
    }

    pub(super) fn release_block(&mut self) {
        assert!(
            self.pending_block.take().is_some(),
            "releasing requires a pending native block"
        );
    }

    pub(super) const fn input_bytes(&self) -> u64 {
        self.pipe.bytes_read
    }

    pub(super) fn close(&mut self) -> Result<(), NativeFailure> {
        if !self.opened || self.closed {
            return Ok(());
        }
        // Closing invalidates any block retained after visitor cancellation or failure. The raw
        // pointer never left this private type, so clearing the guard before the C call is safe.
        self.pending_block = None;
        // Mark closed before reporting an error so Drop never invokes close twice.
        self.closed = true;
        // SAFETY: The handle is live, opened, and exclusively borrowed.
        let result = unsafe { clp_s_container_archive_close(self.raw_ptr()) };
        if NATIVE_OK == result {
            Ok(())
        } else {
            Err(self.failure(NativePhase::Close))
        }
    }

    pub(super) fn free(&mut self) -> Result<(), NativeFailure> {
        let Some(raw) = self.raw.take() else {
            return Ok(());
        };
        // SAFETY: Taking the Option transfers the one owned shim handle. The shim always releases
        // its allocation, including when archive_read_free reports an error.
        let result = unsafe { clp_s_container_archive_free(raw.as_ptr()) };
        if NATIVE_OK == result {
            Ok(())
        } else {
            Err(NativeFailure::Archive(NativeArchiveError {
                phase: NativePhase::Free,
                status: result,
                errno: 0,
                message: b"archive_read_free failed".to_vec(),
                recognized_format: true,
            }))
        }
    }

    fn failure(&mut self, phase: NativePhase) -> NativeFailure {
        self.pipe
            .failure
            .take()
            .map_or_else(|| self.archive_failure(phase), NativeFailure::Callback)
    }

    fn archive_failure(&self, phase: NativePhase) -> NativeFailure {
        let raw = self.raw_ptr();
        // SAFETY: Query functions borrow the live handle and do not mutate or invalidate it.
        let status = unsafe { clp_s_container_archive_last_status(raw) };
        // SAFETY: See above.
        let errno = unsafe { clp_s_container_archive_errno(raw) };
        // SAFETY: See above.
        let recognized_format = 0 != unsafe { clp_s_container_archive_has_format(raw) };
        let mut length = 0_usize;
        // SAFETY: `length` is writable and the handle is live.
        let length_result = unsafe { clp_s_container_archive_error_length(raw, &raw mut length) };
        let message = if NATIVE_OK == length_result {
            let mut message = vec![0_u8; length];
            let output = if message.is_empty() {
                ptr::null_mut()
            } else {
                message.as_mut_ptr()
            };
            // SAFETY: `output` is null only for zero length and otherwise has `length` writable
            // bytes. No intervening archive operation changed the error string.
            let copy_result = unsafe { clp_s_container_archive_error_copy(raw, output, length) };
            if NATIVE_OK == copy_result {
                message
            } else {
                b"failed to copy libarchive error text".to_vec()
            }
        } else {
            b"libarchive did not provide error text".to_vec()
        };
        NativeFailure::Archive(NativeArchiveError {
            phase,
            status,
            errno,
            message,
            recognized_format,
        })
    }

    const fn custom_archive_failure(phase: NativePhase, message: Vec<u8>) -> NativeFailure {
        NativeFailure::Archive(NativeArchiveError {
            phase,
            status: NATIVE_ERROR,
            errno: 0,
            message,
            recognized_format: true,
        })
    }

    const fn raw_ptr(&self) -> *mut CArchive {
        self.raw
            .expect("native archive methods require a live handle")
            .as_ptr()
    }
}

impl<R: Read> Drop for NativeArchive<R> {
    fn drop(&mut self) {
        let _ = self.close();
        let _ = self.free();
    }
}
