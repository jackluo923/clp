use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_s_ffi::*;

const MINIMAL_ARCHIVE: &str = "sfa-v0.5.0-minimal-cpp.bin";
const MINIMAL_JSONL: &str = "sfa-v0.5.0-minimal-cpp-input.jsonl";
const LOG_ORDER_ARCHIVE: &str = "sfa-v0.5.0-log-order-cpp.bin";
const ARRAY_DIRECTORY: &str = "sfa-v0.5.0-unstructured-arrays-cpp-dir";
const FOUR_BYTE_KV_IR_ORACLE_HEX: &str =
    include_str!("../../clp-s/tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
const EIGHT_BYTE_KV_IR_ORACLE_HEX: &str =
    include_str!("../../clp-s/tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");
const KV_IR_FIXTURE_METADATA: &[u8] = concat!(
    r#"{"USER_DEFINED_METADATA":{"fixture":"rust-kv-ir-reader-v1"}"#,
    r#","VARIABLES_SCHEMA_ID":"com.yscope.clp.VariablesSchemaV2""#,
    r#","VARIABLE_ENCODING_METHODS_ID":"com.yscope.clp.VariableEncodingMethodsV1""#,
    r#","VERSION":"0.1.0"}"#,
)
.as_bytes();

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../clp-s/tests/fixtures")
        .join(name)
}

static NEXT_TEMPORARY_KV_IR_FILE: AtomicU64 = AtomicU64::new(0);

struct TemporaryKvIrFile {
    path: PathBuf,
}

impl TemporaryKvIrFile {
    fn from_raw_stream(raw: &[u8]) -> Self {
        let sequence = NEXT_TEMPORARY_KV_IR_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clp-s-ffi-kv-ir-{}-{sequence}.clp.zst",
            std::process::id()
        ));
        let compressed = zstd::stream::encode_all(raw, 1).expect("compress KV-IR fixture");
        std::fs::write(&path, compressed).expect("write temporary KV-IR fixture");
        Self { path }
    }
}

impl Drop for TemporaryKvIrFile {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

struct ArchiveHandle(*mut ClpSArchive);

impl Drop for ArchiveHandle {
    fn drop(&mut self) {
        // SAFETY: The wrapper owns exactly one live handle.
        unsafe {
            clp_s_v1_archive_free(self.0);
        }
    }
}

struct QueryHandle(*mut ClpSQuery);

impl Drop for QueryHandle {
    fn drop(&mut self) {
        // SAFETY: The wrapper owns exactly one live handle.
        unsafe {
            clp_s_v1_query_free(self.0);
        }
    }
}

struct KvIrScannerHandle(*mut ClpSV2KvIrScanner);

impl Drop for KvIrScannerHandle {
    fn drop(&mut self) {
        // SAFETY: The wrapper owns exactly one live handle.
        unsafe {
            clp_s_v2_kv_ir_scanner_free(self.0);
        }
    }
}

struct SerializerHandle(*mut ClpSKvIrSerializer);

impl Drop for SerializerHandle {
    fn drop(&mut self) {
        // SAFETY: The wrapper owns exactly one live handle.
        unsafe {
            clp_s_v1_kv_ir_serializer_free(self.0);
        }
    }
}

struct DeserializerHandle(*mut ClpSKvIrDeserializer);

impl DeserializerHandle {
    fn free(&mut self) {
        // SAFETY: This wrapper owns exactly one live handle when the pointer is non-null.
        unsafe {
            clp_s_v1_kv_ir_deserializer_free(self.0);
        }
        self.0 = ptr::null_mut();
    }
}

impl Drop for DeserializerHandle {
    fn drop(&mut self) {
        self.free();
    }
}

struct EventHandle(*mut ClpSKvIrEvent);

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: The wrapper owns exactly one live handle.
        unsafe {
            clp_s_v1_kv_ir_event_free(self.0);
        }
    }
}

#[derive(Debug)]
struct InputState {
    bytes: Vec<u8>,
    offset: usize,
    calls: usize,
    fail_on_call: Option<usize>,
    over_report: bool,
}

impl InputState {
    const fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            offset: 0,
            calls: 0,
            fail_on_call: None,
            over_report: false,
        }
    }
}

unsafe extern "C" fn read_input(
    context: *mut c_void,
    destination: *mut u8,
    capacity: usize,
    out_read: *mut usize,
) -> u32 {
    if context.is_null() || destination.is_null() || out_read.is_null() || 0 == capacity {
        return CLP_S_KV_IR_READ_ERROR;
    }
    // SAFETY: Tests pass one live InputState exclusively for each synchronous callback.
    let state = unsafe { &mut *context.cast::<InputState>() };
    state.calls += 1;
    if state.fail_on_call == Some(state.calls) {
        // SAFETY: The ABI lends one writable size output for this callback.
        unsafe {
            out_read.write(0);
        }
        return CLP_S_KV_IR_READ_ERROR;
    }
    if state.over_report {
        // SAFETY: The ABI lends one writable size output for this callback.
        unsafe {
            out_read.write(capacity.saturating_add(1));
        }
        return CLP_S_KV_IR_READ_OK;
    }
    let length = capacity.min(state.bytes.len().saturating_sub(state.offset));
    if 0 != length {
        // SAFETY: Both regions are live for `length` bytes and cannot overlap.
        unsafe {
            ptr::copy_nonoverlapping(state.bytes.as_ptr().add(state.offset), destination, length);
        }
        state.offset += length;
    }
    // SAFETY: The ABI lends one writable size output for this callback.
    unsafe {
        out_read.write(length);
    }
    CLP_S_KV_IR_READ_OK
}

fn tiny_chunk_options() -> ClpSKvIrDeserializerOptions {
    ClpSKvIrDeserializerOptions {
        max_read_chunk_bytes: 1,
        ..ClpSKvIrDeserializerOptions::default()
    }
}

fn new_deserializer(
    state: &mut InputState,
    options: Option<&ClpSKvIrDeserializerOptions>,
) -> DeserializerHandle {
    let mut handle = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Callback state, options, handle output, and diagnostics remain live and disjoint.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            options.map_or(ptr::null(), ptr::from_ref),
            Some(read_input),
            (&raw mut *state).cast(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(0, error.required);
    assert!(!handle.is_null());
    DeserializerHandle(handle)
}

fn deserializer_metadata(deserializer: &DeserializerHandle) -> Vec<u8> {
    let mut view = ClpSKvIrByteView {
        data: ptr::dangling(),
        length: u64::MAX,
    };
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    // SAFETY: The handle and outputs remain live and disjoint; the view is copied immediately.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_metadata_view(deserializer.0, &raw mut view, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    let length = usize::try_from(view.length).expect("metadata length fits usize");
    if 0 == length {
        assert!(view.data.is_null());
        Vec::new()
    } else {
        assert!(!view.data.is_null());
        // SAFETY: The ABI lends these bytes through deserializer lifetime; copy them now.
        unsafe { slice::from_raw_parts(view.data, length) }.to_vec()
    }
}

fn next_event(deserializer: &DeserializerHandle) -> (ClpSStatus, Option<EventHandle>, String) {
    let mut handle = ptr::dangling_mut::<ClpSKvIrEvent>();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: The deserializer and outputs remain live and disjoint for the call.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_next_event(deserializer.0, &raw mut handle, &raw mut error)
    };
    let event = (!handle.is_null()).then_some(EventHandle(handle));
    (status, event, error_text(&text).to_owned())
}

const fn zero_event_view() -> ClpSKvIrEventView {
    ClpSKvIrEventView {
        arena: ptr::null(),
        arena_length: 0,
        auto_nodes: ptr::null(),
        auto_node_count: 0,
        user_nodes: ptr::null(),
        user_node_count: 0,
        utc_offset_millis: 0,
        stream_index: 0,
        unit_index: 0,
        event_index: 0,
        input_offset: 0,
        reserved: [0; 4],
    }
}

fn next_event_with_view(
    deserializer: &DeserializerHandle,
) -> (ClpSStatus, Option<(EventHandle, ClpSKvIrEventView)>, String) {
    let mut handle = ptr::dangling_mut::<ClpSKvIrEvent>();
    let mut view = ClpSKvIrEventView {
        arena: ptr::dangling(),
        arena_length: u64::MAX,
        auto_nodes: ptr::dangling(),
        auto_node_count: u64::MAX,
        user_nodes: ptr::dangling(),
        user_node_count: u64::MAX,
        utc_offset_millis: i64::MAX,
        stream_index: u64::MAX,
        unit_index: u64::MAX,
        event_index: u64::MAX,
        input_offset: u64::MAX,
        reserved: [u64::MAX; 4],
    };
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: The deserializer and outputs remain live and disjoint for the call.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_next_event_with_view(
            deserializer.0,
            &raw mut handle,
            &raw mut view,
            &raw mut error,
        )
    };
    let event = (!handle.is_null()).then_some((EventHandle(handle), view));
    if event.is_none() {
        assert!(view.arena.is_null());
        assert!(view.auto_nodes.is_null());
        assert!(view.user_nodes.is_null());
        assert_eq!(0, view.arena_length);
        assert_eq!(0, view.auto_node_count);
        assert_eq!(0, view.user_node_count);
        assert_eq!([0; 4], view.reserved);
    }
    (status, event, error_text(&text).to_owned())
}

fn event_view(event: &EventHandle) -> ClpSKvIrEventView {
    let mut view = zero_event_view();
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    // SAFETY: The event and caller-owned outputs remain live and disjoint.
    let status = unsafe { clp_s_v1_kv_ir_event_view(event.0, &raw mut view, &raw mut error) };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    view
}

fn event_nodes(view: &ClpSKvIrEventView, auto_generated: bool) -> &[ClpSKvIrEventNode] {
    let (nodes, count) = if auto_generated {
        (view.auto_nodes, view.auto_node_count)
    } else {
        (view.user_nodes, view.user_node_count)
    };
    let count = usize::try_from(count).expect("event node count fits usize");
    if 0 == count {
        assert!(nodes.is_null());
        &[]
    } else {
        assert!(!nodes.is_null());
        // SAFETY: The event owning the view remains live throughout callers' use.
        unsafe { slice::from_raw_parts(nodes, count) }
    }
}

fn arena_bytes(view: &ClpSKvIrEventView) -> &[u8] {
    let length = usize::try_from(view.arena_length).expect("arena length fits usize");
    if 0 == length {
        &[]
    } else {
        assert!(!view.arena.is_null());
        // SAFETY: The view's event remains live throughout callers' use.
        unsafe { slice::from_raw_parts(view.arena, length) }
    }
}

fn span_bytes(arena: &[u8], span: ClpSKvIrEventSpan) -> &[u8] {
    let start = usize::try_from(span.offset()).expect("span offset fits usize");
    let length = usize::try_from(span.length()).expect("span length fits usize");
    &arena[start..start + length]
}

fn serialize_event_stream(encoding: u32, user_generated: &[u8]) -> Vec<u8> {
    let options = ClpSKvIrSerializerOptions {
        encoding,
        ..ClpSKvIrSerializerOptions::default()
    };
    let serializer = new_serializer(Some(&options), None);
    let (status, event_bytes, message) = append_maps(&serializer, &[0x80], user_generated);
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    assert!(0 < event_bytes);
    let mut eof_bytes = u64::MAX;
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    // SAFETY: The serializer and caller-owned outputs remain live and disjoint.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_finish(serializer.0, &raw mut eof_bytes, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(1, eof_bytes);
    pending_output(&serializer)
}

const fn error_buffer(storage: &mut [u8]) -> ClpSErrorBuffer {
    ClpSErrorBuffer {
        data: storage.as_mut_ptr(),
        capacity: storage.len(),
        required: usize::MAX,
    }
}

fn error_text(storage: &[u8]) -> &str {
    let end = storage
        .iter()
        .position(|byte| 0 == *byte)
        .unwrap_or(storage.len());
    str::from_utf8(&storage[..end]).expect("diagnostics are UTF-8")
}

fn open_archive(path: &Path) -> ArchiveHandle {
    let path = path.to_str().expect("fixture path is UTF-8").as_bytes();
    let mut handle = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Every pointer names live storage for the duration of the call.
    let status = unsafe {
        clp_s_v1_archive_open(path.as_ptr(), path.len(), &raw mut handle, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(0, error.required);
    assert!(!handle.is_null());
    ArchiveHandle(handle)
}

fn compile_query(query: &str, flags: u32) -> QueryHandle {
    let mut handle = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Every pointer names live storage for the duration of the call.
    let status = unsafe {
        clp_s_v1_query_compile(
            query.as_ptr(),
            query.len(),
            flags,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(0, error.required);
    assert!(!handle.is_null());
    QueryHandle(handle)
}

fn new_serializer(
    options: Option<&ClpSKvIrSerializerOptions>,
    metadata: Option<&[u8]>,
) -> SerializerHandle {
    let mut handle = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    let (metadata_data, metadata_length) =
        metadata.map_or((ptr::null(), 0), |bytes| (bytes.as_ptr(), bytes.len()));
    // SAFETY: Every pointer names live storage for the duration of the call, or is a documented
    // null optional input.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(
            options.map_or(ptr::null(), std::ptr::from_ref),
            metadata_data,
            metadata_length,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(0, error.required);
    assert!(!handle.is_null());
    SerializerHandle(handle)
}

fn pending_output(serializer: &SerializerHandle) -> Vec<u8> {
    let mut view = ClpSKvIrPendingView {
        data: ptr::dangling(),
        length: usize::MAX,
    };
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    // SAFETY: The handle and caller-owned outputs remain live and disjoint.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_pending_view(serializer.0, &raw mut view, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    if 0 == view.length {
        assert!(view.data.is_null());
        Vec::new()
    } else {
        assert!(!view.data.is_null());
        // SAFETY: The ABI lends exactly `length` bytes until the next mutating call; copy now.
        unsafe { slice::from_raw_parts(view.data, view.length) }.to_vec()
    }
}

fn serializer_stats(serializer: &SerializerHandle) -> ClpSKvIrSerializerStats {
    let mut snapshot = ClpSKvIrSerializerStats {
        log_events: u64::MAX,
        schema_nodes: u64::MAX,
        utc_offset_changes: u64::MAX,
        serialized_bytes: u64::MAX,
        pending_bytes: u64::MAX,
        is_finished: u32::MAX,
        reserved: u32::MAX,
    };
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    // SAFETY: The handle and caller-owned outputs remain live and disjoint.
    let call_status =
        unsafe { clp_s_v1_kv_ir_serializer_stats(serializer.0, &raw mut snapshot, &raw mut error) };
    assert_eq!(CLP_S_STATUS_OK, call_status, "{}", error_text(&text));
    snapshot
}

fn append_maps(
    serializer: &SerializerHandle,
    auto_generated: &[u8],
    user_generated: &[u8],
) -> (ClpSStatus, u64, String) {
    let mut committed = u64::MAX;
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: The handle and every caller-owned region remain live and disjoint for the call.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_append_msgpack_maps(
            serializer.0,
            auto_generated.as_ptr(),
            auto_generated.len(),
            user_generated.as_ptr(),
            user_generated.len(),
            &raw mut committed,
            &raw mut error,
        )
    };
    (status, committed, error_text(&text).to_owned())
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedRecord {
    json: Vec<u8>,
    table_index: u64,
    row_index: u64,
    log_event_idx: Option<u64>,
}

#[derive(Default)]
struct CallbackState {
    records: Vec<OwnedRecord>,
    stop_at: Option<(usize, u32)>,
}

unsafe extern "C" fn collect_record(user_data: *mut c_void, record: *const ClpSRecord) -> u32 {
    if user_data.is_null() || record.is_null() {
        return CLP_S_CALLBACK_ERROR;
    }
    // SAFETY: The test passes live callback state and the ABI promises one live record for this
    // synchronous invocation.
    let (state, record) = unsafe { (&mut *user_data.cast::<CallbackState>(), &*record) };
    if (0 != record.json_length && record.json.is_null()) || 0 != record.reserved {
        return CLP_S_CALLBACK_ERROR;
    }
    let json = if 0 == record.json_length {
        &[]
    } else {
        // SAFETY: The ABI promises `json_length` borrowed bytes until this callback returns.
        unsafe { slice::from_raw_parts(record.json, record.json_length) }
    };
    if json.ends_with(b"\n") {
        return CLP_S_CALLBACK_ERROR;
    }
    let log_event_idx = match record.has_log_event_idx {
        0 => None,
        1 => Some(record.log_event_idx),
        _ => return CLP_S_CALLBACK_ERROR,
    };
    state.records.push(OwnedRecord {
        json: json.to_vec(),
        table_index: record.table_index,
        row_index: record.row_index,
        log_event_idx,
    });
    state
        .stop_at
        .filter(|(index, _disposition)| *index == state.records.len())
        .map_or(CLP_S_CALLBACK_CONTINUE, |(_index, disposition)| disposition)
}

fn extract(
    archive: &ArchiveHandle,
    flags: u32,
    state: &mut CallbackState,
) -> (ClpSStatus, u64, String) {
    let mut delivered = u64::MAX;
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Both handles and every caller-owned region remain live for this synchronous call.
    let status = unsafe {
        clp_s_v1_extract(
            archive.0,
            flags,
            Some(collect_record),
            (&raw mut *state).cast(),
            &raw mut delivered,
            &raw mut error,
        )
    };
    (status, delivered, error_text(&text).to_owned())
}

fn search(
    archive: &ArchiveHandle,
    query: &QueryHandle,
    state: &mut CallbackState,
) -> (ClpSStatus, u64, String) {
    let mut delivered = u64::MAX;
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Both handles and every caller-owned region remain live for this synchronous call.
    let status = unsafe {
        clp_s_v1_search(
            archive.0,
            query.0,
            Some(collect_record),
            (&raw mut *state).cast(),
            &raw mut delivered,
            &raw mut error,
        )
    };
    (status, delivered, error_text(&text).to_owned())
}

#[test]
fn reports_frozen_abi_and_library_versions() {
    assert_eq!(CLP_S_ABI_VERSION, clp_s_v1_abi_version());
    assert_eq!(1, CLP_S_ABI_VERSION);
    assert_eq!(0, CLP_S_STATUS_OK);
    assert_eq!(1, CLP_S_STATUS_INVALID_ARGUMENT);
    assert_eq!(2, CLP_S_STATUS_IO);
    assert_eq!(3, CLP_S_STATUS_ARCHIVE);
    assert_eq!(4, CLP_S_STATUS_QUERY);
    assert_eq!(5, CLP_S_STATUS_CANCELLED);
    assert_eq!(6, CLP_S_STATUS_CALLBACK_ERROR);
    assert_eq!(7, CLP_S_STATUS_PANIC);
    assert_eq!(8, CLP_S_STATUS_BUFFER_TOO_SMALL);
    assert_eq!(9, CLP_S_STATUS_KV_IR_INVALID_DATA);
    assert_eq!(10, CLP_S_STATUS_LIMIT_EXCEEDED);
    assert_eq!(11, CLP_S_STATUS_ALLOCATION_FAILED);
    assert_eq!(12, CLP_S_STATUS_INVALID_STATE);
    assert_eq!(13, CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR);
    assert_eq!(14, CLP_S_STATUS_KV_IR_INCOMPLETE);
    assert_eq!(15, CLP_S_STATUS_EOF);
    assert_eq!(16, CLP_S_STATUS_KV_IR_ROOT_NOT_MAP);
    assert_eq!(0, CLP_S_CALLBACK_CONTINUE);
    assert_eq!(1, CLP_S_CALLBACK_CANCEL);
    assert_eq!(2, CLP_S_CALLBACK_ERROR);
    assert_eq!(1, CLP_S_EXTRACT_LOG_ORDER);
    assert_eq!(1, CLP_S_QUERY_IGNORE_CASE);
    assert_eq!(0, CLP_S_V2_VALUE_ABSENT);
    assert_eq!(1, CLP_S_V2_VALUE_BOOLEAN);
    assert_eq!(2, CLP_S_V2_VALUE_INTEGER);
    assert_eq!(3, CLP_S_V2_VALUE_FLOAT);
    assert_eq!(4, CLP_S_V2_VALUE_STRING);
    assert_eq!(5, CLP_S_V2_VALUE_TIMESTAMP);
    assert_eq!(6, CLP_S_V2_VALUE_UNSUPPORTED);
    assert_eq!(0, CLP_S_KV_IR_ENCODING_DEFAULT);
    assert_eq!(4, CLP_S_KV_IR_ENCODING_FOUR_BYTE);
    assert_eq!(8, CLP_S_KV_IR_ENCODING_EIGHT_BYTE);
    assert_eq!(0, CLP_S_KV_IR_READ_OK);
    assert_eq!(1, CLP_S_KV_IR_READ_ERROR);
    assert_eq!(0, CLP_S_KV_IR_VALUE_OBJECT);
    assert_eq!(1, CLP_S_KV_IR_VALUE_INTEGER);
    assert_eq!(2, CLP_S_KV_IR_VALUE_FLOAT);
    assert_eq!(3, CLP_S_KV_IR_VALUE_BOOLEAN);
    assert_eq!(4, CLP_S_KV_IR_VALUE_STRING);
    assert_eq!(5, CLP_S_KV_IR_VALUE_ARRAY_JSON);
    assert_eq!(6, CLP_S_KV_IR_VALUE_NULL);
    assert_eq!(7, CLP_S_KV_IR_VALUE_EMPTY_OBJECT);
    assert_eq!(104, std::mem::size_of::<ClpSKvIrSerializerOptions>());
    assert_eq!(48, std::mem::size_of::<ClpSKvIrSerializerStats>());
    assert_eq!(72, std::mem::size_of::<ClpSKvIrDeserializerOptions>());
    assert_eq!(16, std::mem::size_of::<ClpSKvIrByteView>());
    assert_eq!(8, std::mem::size_of::<ClpSKvIrEventSpan>());
    assert_eq!(32, std::mem::size_of::<ClpSKvIrEventNode>());
    assert_eq!(120, std::mem::size_of::<ClpSKvIrEventView>());
    assert_eq!(16, std::mem::size_of::<ClpSV2ProjectedField>());
    assert_eq!(40, std::mem::size_of::<ClpSV2Value>());

    let mut required = 0;
    // SAFETY: Null with zero capacity is a documented size probe; `required` is writable.
    let probe = unsafe { clp_s_v1_library_version(ptr::null_mut(), 0, &raw mut required) };
    assert_eq!(CLP_S_STATUS_BUFFER_TOO_SMALL, probe);
    assert!(required > 1);

    let mut short = [0xff_u8; 2];
    let mut short_required = 0;
    // SAFETY: Both output regions are writable and disjoint.
    let status = unsafe {
        clp_s_v1_library_version(short.as_mut_ptr(), short.len(), &raw mut short_required)
    };
    assert_eq!(CLP_S_STATUS_BUFFER_TOO_SMALL, status);
    assert_eq!(required, short_required);
    assert_eq!(0, short[1]);

    let mut complete = vec![0_u8; required];
    let mut complete_required = 0;
    // SAFETY: Both output regions are writable and disjoint.
    let status = unsafe {
        clp_s_v1_library_version(
            complete.as_mut_ptr(),
            complete.len(),
            &raw mut complete_required,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status);
    assert_eq!(required, complete_required);
    assert_eq!(Some(&0), complete.last());
    assert_eq!(
        env!("CARGO_PKG_VERSION").as_bytes(),
        &complete[..required - 1]
    );
}

const fn v2_value_sentinel() -> ClpSV2Value {
    ClpSV2Value {
        kind: u32::MAX,
        reserved: u32::MAX,
        integer: i64::MIN,
        real: f64::from_bits(u64::MAX - 1),
        text: ptr::dangling(),
        text_length: usize::MAX,
    }
}

fn assert_v2_value_sentinel(value: &ClpSV2Value) {
    assert_eq!(u32::MAX, value.kind);
    assert_eq!(u32::MAX, value.reserved);
    assert_eq!(i64::MIN, value.integer);
    assert_eq!(u64::MAX - 1, value.real.to_bits());
    assert_eq!(ptr::dangling::<u8>(), value.text);
    assert_eq!(usize::MAX, value.text_length);
}

#[test]
fn kv_ir_v2_scanner_projects_exact_typed_width_and_owns_its_query() {
    let stream = TemporaryKvIrFile::from_raw_stream(&decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX));
    let query = compile_query("message:TASK*", CLP_S_QUERY_IGNORE_CASE);
    let names: [&[u8]; 6] = [b"message", b"seq", b"ok", b"ratio", b"none", b"missing"];
    let fields: Vec<ClpSV2ProjectedField> = names
        .iter()
        .map(|name| ClpSV2ProjectedField {
            path: name.as_ptr(),
            path_length: name.len(),
        })
        .collect();
    let path = stream.path.to_str().expect("temporary path is UTF-8");
    let mut scanner = ptr::dangling_mut::<ClpSV2KvIrScanner>();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: All inputs and outputs are live and disjoint for this synchronous call.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_open(
            path.as_ptr(),
            path.len(),
            query.0,
            fields.as_ptr(),
            fields.len(),
            &raw mut scanner,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert!(!scanner.is_null());
    let scanner = KvIrScannerHandle(scanner);
    drop(query);

    let sentinel = v2_value_sentinel();
    let mut values = [sentinel; 7];
    let mut has_row = u32::MAX;
    // SAFETY: The scanner is live, six output elements are required, and seven are writable.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_next_row(
            scanner.0,
            values.as_mut_ptr(),
            &raw mut has_row,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(1, has_row);
    assert!(values[..6].iter().all(|value| 0 == value.reserved));
    assert_eq!(CLP_S_V2_VALUE_STRING, values[0].kind);
    assert_eq!(12, values[0].text_length);
    assert!(!values[0].text.is_null());
    // SAFETY: The scanner lends exactly `text_length` bytes until its next next-row call.
    let message = unsafe { slice::from_raw_parts(values[0].text, values[0].text_length) }.to_vec();
    assert_eq!(b"task 42 done", message.as_slice());
    assert_eq!(CLP_S_V2_VALUE_INTEGER, values[1].kind);
    assert_eq!(7, values[1].integer);
    assert_eq!(CLP_S_V2_VALUE_BOOLEAN, values[2].kind);
    assert_eq!(1, values[2].integer);
    assert_eq!(CLP_S_V2_VALUE_FLOAT, values[3].kind);
    assert_eq!(1.25_f64.to_bits(), values[3].real.to_bits());
    assert_eq!(CLP_S_V2_VALUE_ABSENT, values[4].kind);
    assert_eq!(CLP_S_V2_VALUE_ABSENT, values[5].kind);
    for value in &values[1..6] {
        assert!(value.text.is_null());
        assert_eq!(0, value.text_length);
    }
    assert_v2_value_sentinel(&values[6]);

    values[..6].fill(sentinel);
    has_row = u32::MAX;
    // SAFETY: The scanner and all outputs remain live and disjoint.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_next_row(
            scanner.0,
            values.as_mut_ptr(),
            &raw mut has_row,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(0, has_row);
    for value in &values {
        assert_v2_value_sentinel(value);
    }
}

#[test]
fn kv_ir_v2_scanner_reports_the_whole_streams_field_types() {
    let stream = TemporaryKvIrFile::from_raw_stream(&decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX));
    // A filter that matches nothing still yields the schema of the whole stream.
    let query = compile_query("message:no-such-message", 0);
    let path = stream.path.to_str().expect("temporary path is UTF-8");
    let mut scanner = ptr::dangling_mut::<ClpSV2KvIrScanner>();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: All inputs and outputs are live and disjoint for this synchronous call.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_open(
            path.as_ptr(),
            path.len(),
            query.0,
            ptr::null(),
            0,
            &raw mut scanner,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    let scanner = KvIrScannerHandle(scanner);

    let mut entries = ptr::dangling::<ClpSV2KvIrFieldType>();
    let mut count = usize::MAX;
    // SAFETY: The scanner is live and both outputs are writable.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_field_types(
            scanner.0,
            &raw mut entries,
            &raw mut count,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert!(!entries.is_null());
    assert!(count > 0);
    // SAFETY: The scanner owns `count` entries and their paths until it is freed.
    let entries = unsafe { slice::from_raw_parts(entries, count) };
    let types: Vec<(Vec<u8>, u8)> = entries
        .iter()
        .map(|entry| {
            // SAFETY: Each path is `path_length` bytes owned by the scanner.
            let path = unsafe { slice::from_raw_parts(entry.path, entry.path_length) };
            (path.to_vec(), entry.node_type)
        })
        .collect();
    let type_of = |name: &[u8]| -> Vec<u8> {
        types
            .iter()
            .filter(|(path, _)| path.as_slice() == name)
            .map(|(_, node_type)| *node_type)
            .collect()
    };
    assert_eq!(vec![kv_ir_node::STRING], type_of(b"message"), "{types:?}");
    assert_eq!(vec![kv_ir_node::BOOLEAN], type_of(b"ok"), "{types:?}");
    assert_eq!(vec![kv_ir_node::FLOAT], type_of(b"ratio"), "{types:?}");
    assert_eq!(vec![kv_ir_node::OBJECT], type_of(b"empty"), "{types:?}");
    // A null-valued key is an object node in the schema tree.
    assert_eq!(vec![kv_ir_node::OBJECT], type_of(b"none"), "{types:?}");
    // `seq` and `missing` live in the auto-generated namespace, which is not
    // reported: certification only ever looks at user keys.
    assert!(type_of(b"seq").is_empty(), "{types:?}");
    assert!(type_of(b"missing").is_empty(), "{types:?}");
    assert!(type_of(b"no-such-key").is_empty(), "{types:?}");
    assert!(types.iter().all(|(path, _)| !path.is_empty()));

    // Null handles and null outputs are rejected, and the outputs are reset first.
    let mut entries = ptr::dangling::<ClpSV2KvIrFieldType>();
    let mut count = usize::MAX;
    // SAFETY: The outputs are writable; the handle is deliberately null.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_field_types(
            ptr::null(),
            &raw mut entries,
            &raw mut count,
            &raw mut error,
        )
    };
    assert_ne!(CLP_S_STATUS_OK, status);
    assert!(entries.is_null());
    assert_eq!(0, count);
    // SAFETY: The scanner is live; the entry output is deliberately null.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_field_types(scanner.0, ptr::null_mut(), &raw mut count, &raw mut error)
    };
    assert_ne!(CLP_S_STATUS_OK, status);
}

#[test]
fn kv_ir_v2_scanner_rejects_fatal_post_event_truncation() {
    let mut raw = decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX);
    assert_eq!(
        Some(0),
        raw.pop(),
        "canonical fixture ends with explicit stream end"
    );
    let stream = TemporaryKvIrFile::from_raw_stream(&raw);
    let query = compile_query("*: *", 0);
    let path = stream.path.to_str().expect("temporary path is UTF-8");
    let mut scanner = ptr::dangling_mut::<ClpSV2KvIrScanner>();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Null fields are valid for a zero field count; every other region is live.
    let status = unsafe {
        clp_s_v2_kv_ir_scanner_open(
            path.as_ptr(),
            path.len(),
            query.0,
            ptr::null(),
            0,
            &raw mut scanner,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_ARCHIVE, status);
    assert!(
        scanner.is_null(),
        "open initializes its handle before failing"
    );
    assert!(
        error_text(&text).contains("truncated"),
        "{}",
        error_text(&text)
    );
}

#[test]
fn validates_pointer_lengths_flags_and_error_buffers_before_work() {
    let mut archive = ptr::dangling_mut::<ClpSArchive>();
    let mut text = [0xff_u8; 12];
    let mut error = error_buffer(&mut text);
    // SAFETY: The nonzero length intentionally tests the null input shape; outputs are valid.
    let status = unsafe { clp_s_v1_archive_open(ptr::null(), 1, &raw mut archive, &raw mut error) };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(archive.is_null(), "output is nulled before validation");
    assert_eq!(0, text[text.len() - 1], "short diagnostics are terminated");
    assert!(error.required > text.len());

    let mut query = ptr::dangling_mut::<ClpSQuery>();
    let invalid_utf8 = [0xff];
    let mut query_text = [0_u8; 128];
    let mut query_error = error_buffer(&mut query_text);
    // SAFETY: Inputs and outputs name valid disjoint storage.
    let status = unsafe {
        clp_s_v1_query_compile(
            invalid_utf8.as_ptr(),
            invalid_utf8.len(),
            0,
            &raw mut query,
            &raw mut query_error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(query.is_null());
    assert!(error_text(&query_text).contains("UTF-8"));

    let mut malformed_error = ClpSErrorBuffer {
        data: ptr::null_mut(),
        capacity: 1,
        required: 0,
    };
    query = ptr::dangling_mut::<ClpSQuery>();
    // SAFETY: The malformed data/capacity pair is the tested input; its structure is writable.
    let status = unsafe {
        clp_s_v1_query_compile(
            b"*".as_ptr(),
            1,
            0,
            &raw mut query,
            &raw mut malformed_error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(query.is_null());
    assert!(malformed_error.required > 1);

    let mut flag_text = [0_u8; 128];
    let mut flag_error = error_buffer(&mut flag_text);
    // SAFETY: Inputs and outputs name valid disjoint storage.
    let status =
        unsafe { clp_s_v1_query_compile(b"*".as_ptr(), 1, 2, &raw mut query, &raw mut flag_error) };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(query.is_null());
    assert!(error_text(&flag_text).contains("unknown query flags"));

    // SAFETY: Null frees are explicitly documented no-ops.
    unsafe {
        clp_s_v1_archive_free(ptr::null_mut());
        clp_s_v1_query_free(ptr::null_mut());
    }
}

#[test]
fn malformed_query_has_a_stable_category_and_caller_owned_detail() {
    let mut query = ptr::null_mut();
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    // SAFETY: Inputs and outputs name valid disjoint storage.
    let status =
        unsafe { clp_s_v1_query_compile(b"(".as_ptr(), 1, 0, &raw mut query, &raw mut error) };
    assert_eq!(CLP_S_STATUS_QUERY, status);
    assert!(query.is_null());
    assert!(error.required > 1);
    assert!(error_text(&text).contains("failed to parse KQL"));
}

#[test]
fn sfa_extraction_is_exact_repeatable_and_exposes_optional_log_order() {
    let minimal = open_archive(&fixture(MINIMAL_ARCHIVE));
    let expected = std::fs::read(fixture(MINIMAL_JSONL)).expect("read minimal JSONL");
    let expected = expected
        .strip_suffix(b"\n")
        .expect("fixture ends in newline");

    for _attempt in 0..2 {
        let mut state = CallbackState::default();
        let (status, delivered, error) = extract(&minimal, 0, &mut state);
        assert_eq!(CLP_S_STATUS_OK, status, "{error}");
        assert_eq!(1, delivered);
        assert_eq!(1, state.records.len());
        assert_eq!(expected, state.records[0].json);
        assert!(!state.records[0].json.ends_with(b"\n"));
        assert_eq!(None, state.records[0].log_event_idx);
    }

    let ordered = open_archive(&fixture(LOG_ORDER_ARCHIVE));
    let mut state = CallbackState::default();
    let (status, delivered, error) = extract(&ordered, CLP_S_EXTRACT_LOG_ORDER, &mut state);
    assert_eq!(CLP_S_STATUS_OK, status, "{error}");
    assert_eq!(6, delivered);
    assert_eq!(
        (0_u64..6).map(Some).collect::<Vec<_>>(),
        state
            .records
            .iter()
            .map(|record| record.log_event_idx)
            .collect::<Vec<_>>()
    );
}

#[test]
fn sfa_search_reuses_query_and_archive_handles() {
    let archive = open_archive(&fixture(MINIMAL_ARCHIVE));
    let insensitive = compile_query("level:info", CLP_S_QUERY_IGNORE_CASE);
    let sensitive = compile_query("level:info", 0);

    for _attempt in 0..2 {
        let mut state = CallbackState::default();
        let (status, delivered, error) = search(&archive, &insensitive, &mut state);
        assert_eq!(CLP_S_STATUS_OK, status, "{error}");
        assert_eq!(1, delivered);
        assert_eq!(1, state.records.len());
        assert!(
            state.records[0]
                .json
                .windows(14)
                .any(|bytes| bytes == b"\"level\":\"INFO\"")
        );
        assert_eq!(None, state.records[0].log_event_idx);
    }

    let mut state = CallbackState::default();
    let (status, delivered, error) = search(&archive, &sensitive, &mut state);
    assert_eq!(CLP_S_STATUS_OK, status, "{error}");
    assert_eq!(0, delivered);
    assert_eq!(state.records, [] as [OwnedRecord; 0]);
}

#[test]
fn directory_archive_search_and_extraction_use_the_same_handles() {
    let archive = open_archive(&fixture(ARRAY_DIRECTORY));
    let mut extracted = CallbackState::default();
    let (status, delivered, error) = extract(&archive, 0, &mut extracted);
    assert_eq!(CLP_S_STATUS_OK, status, "{error}");
    assert_eq!(6, delivered);

    let query = compile_query("kind:1", 0);
    let mut matched = CallbackState::default();
    let (status, delivered, error) = search(&archive, &query, &mut matched);
    assert_eq!(CLP_S_STATUS_OK, status, "{error}");
    assert_eq!(1, delivered);
    assert_eq!(1, matched.records.len());
    assert!(
        matched.records[0]
            .json
            .windows(8)
            .any(|bytes| bytes == b"\"kind\":1")
    );
}

#[test]
fn callbacks_control_cancellation_and_report_failure_without_partial_records() {
    let archive = open_archive(&fixture(LOG_ORDER_ARCHIVE));

    let mut cancelled = CallbackState {
        stop_at: Some((1, CLP_S_CALLBACK_CANCEL)),
        ..CallbackState::default()
    };
    let (status, delivered, error) = extract(&archive, 0, &mut cancelled);
    assert_eq!(CLP_S_STATUS_CANCELLED, status);
    assert_eq!(1, delivered);
    assert_eq!(1, cancelled.records.len());
    assert!(error.contains("cancellation"));

    let query = compile_query("*", 0);
    let mut failed = CallbackState {
        stop_at: Some((2, 99)),
        ..CallbackState::default()
    };
    let (status, delivered, error) = search(&archive, &query, &mut failed);
    assert_eq!(CLP_S_STATUS_CALLBACK_ERROR, status);
    assert_eq!(2, delivered);
    assert_eq!(2, failed.records.len());
    assert!(error.contains("unknown disposition 99"));
}

#[test]
fn null_handles_and_callbacks_are_rejected_and_counters_are_initialized() {
    let mut delivered = u64::MAX;
    let mut text = [0_u8; 128];
    let mut error = error_buffer(&mut text);
    // SAFETY: The null archive and callback are tested inputs; outputs are live and writable.
    let status = unsafe {
        clp_s_v1_extract(
            ptr::null(),
            0,
            None,
            ptr::null_mut(),
            &raw mut delivered,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(0, delivered);
    assert!(error_text(&text).contains("archive handle"));

    let archive = open_archive(&fixture(MINIMAL_ARCHIVE));
    delivered = u64::MAX;
    // SAFETY: The live archive and outputs remain valid; the null callback is the tested input.
    let status = unsafe {
        clp_s_v1_extract(
            archive.0,
            0,
            None,
            ptr::null_mut(),
            &raw mut delivered,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(0, delivered);
    assert!(error_text(&text).contains("callback"));
}

#[test]
fn kv_ir_serializer_matches_cpp_oracle_and_streams_borrowed_output() {
    let serializer = new_serializer(None, Some(br#"{"fixture":"rust-kv-ir-reader-v1"}"#));
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    let mut offset_bytes = u64::MAX;
    // SAFETY: The handle and every output region remain live and disjoint for the call.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_change_utc_offset(
            serializer.0,
            3_600_000,
            &raw mut offset_bytes,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(9, offset_bytes);

    let (status, event_bytes, message) = append_maps(&serializer, &auto_map(), &user_map());
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    assert!(event_bytes > 0);

    let mut eof_bytes = u64::MAX;
    // SAFETY: The handle and every output region remain live and disjoint for the call.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_finish(serializer.0, &raw mut eof_bytes, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(1, eof_bytes);

    let expected = decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX);
    assert_eq!(expected, pending_output(&serializer));
    let snapshot = serializer_stats(&serializer);
    assert_eq!(1, snapshot.log_events);
    assert_eq!(7, snapshot.schema_nodes);
    assert_eq!(1, snapshot.utc_offset_changes);
    assert_eq!(
        u64::try_from(expected.len()).unwrap(),
        snapshot.serialized_bytes
    );
    assert_eq!(snapshot.serialized_bytes, snapshot.pending_bytes);
    assert_eq!(1, snapshot.is_finished);
    assert_eq!(0, snapshot.reserved);

    let mut total_bytes = u64::MAX;
    // SAFETY: The handle and caller-owned outputs remain live and disjoint.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_total_bytes(serializer.0, &raw mut total_bytes, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(snapshot.serialized_bytes, total_bytes);

    eof_bytes = u64::MAX;
    // SAFETY: This intentionally repeats finish on one live handle; outputs remain valid.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_finish(serializer.0, &raw mut eof_bytes, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_STATE, status);
    assert_eq!(0, eof_bytes, "fallible outputs are initialized before work");
    assert!(error_text(&text).contains("already finished"));
    assert_eq!(expected, pending_output(&serializer));

    let prefix = 17;
    // SAFETY: The live handle currently has at least `prefix` pending bytes.
    let status = unsafe { clp_s_v1_kv_ir_serializer_consume(serializer.0, prefix, &raw mut error) };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(&expected[prefix..], pending_output(&serializer));
    // SAFETY: Consume the exact remaining byte count from the live handle.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_consume(serializer.0, expected.len() - prefix, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(Vec::<u8>::new(), pending_output(&serializer));
    let drained = serializer_stats(&serializer);
    assert_eq!(snapshot.serialized_bytes, drained.serialized_bytes);
    assert_eq!(0, drained.pending_bytes);
}

#[test]
fn kv_ir_failed_event_rolls_back_schema_output_and_counts() {
    let serializer = new_serializer(None, None);
    let before = pending_output(&serializer);
    let before_stats = serializer_stats(&serializer);

    let (status, event_bytes, message) = append_maps(&serializer, &auto_map(), &[0x81]);
    assert_eq!(CLP_S_STATUS_KV_IR_INCOMPLETE, status);
    assert_eq!(0, event_bytes);
    assert!(message.contains("truncated MessagePack"));
    assert_eq!(before, pending_output(&serializer));
    assert_eq!(before_stats, serializer_stats(&serializer));

    let (status, event_bytes, message) = append_maps(&serializer, &auto_map(), &[0xc0]);
    assert_eq!(CLP_S_STATUS_KV_IR_ROOT_NOT_MAP, status);
    assert_eq!(0, event_bytes);
    assert!(message.contains("root MessagePack value is not a map"));
    assert_eq!(before, pending_output(&serializer));
    assert_eq!(before_stats, serializer_stats(&serializer));

    let invalid_user = [0x81, 0xa1, b'x', 0x81, 0x01, 0x02];
    let (status, event_bytes, message) = append_maps(&serializer, &auto_map(), &invalid_user);
    assert_eq!(CLP_S_STATUS_KV_IR_INVALID_DATA, status);
    assert_eq!(0, event_bytes);
    assert!(message.contains("map key"));
    assert_eq!(before, pending_output(&serializer));
    assert_eq!(before_stats, serializer_stats(&serializer));

    let (status, event_bytes, message) = append_maps(&serializer, &auto_map(), &user_map());
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    assert!(event_bytes > 0);
    assert_eq!(1, serializer_stats(&serializer).log_events);

    let fresh = new_serializer(None, None);
    let (status, fresh_event_bytes, message) = append_maps(&fresh, &auto_map(), &user_map());
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    assert_eq!(event_bytes, fresh_event_bytes);
    assert_eq!(pending_output(&fresh), pending_output(&serializer));

    let trailing = new_serializer(None, None);
    let mut auto_with_compatibility_suffix = auto_map();
    auto_with_compatibility_suffix.push(0xc1);
    let mut user_with_compatibility_suffix = user_map();
    user_with_compatibility_suffix.push(0xc1);
    let (status, trailing_event_bytes, message) = append_maps(
        &trailing,
        &auto_with_compatibility_suffix,
        &user_with_compatibility_suffix,
    );
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    assert_eq!(fresh_event_bytes, trailing_event_bytes);
    assert_eq!(pending_output(&fresh), pending_output(&trailing));
}

#[test]
fn kv_ir_serializer_validates_options_pointer_shapes_limits_and_short_errors() {
    let mut text = [0xff_u8; 12];
    let mut error = error_buffer(&mut text);
    let mut handle = ptr::dangling_mut::<ClpSKvIrSerializer>();
    let options = ClpSKvIrSerializerOptions {
        struct_size: 0,
        ..ClpSKvIrSerializerOptions::default()
    };
    // SAFETY: Every pointer names live storage; the bad size is the tested input.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(
            &raw const options,
            ptr::null(),
            0,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(
        handle.is_null(),
        "handle output is initialized before validation"
    );
    assert_eq!(0, text[text.len() - 1], "short error text is terminated");
    assert!(error.required > text.len());

    let options = ClpSKvIrSerializerOptions {
        reserved: [0, 0, 1, 0],
        ..ClpSKvIrSerializerOptions::default()
    };
    // SAFETY: Every pointer names live storage; the nonzero reserved field is tested.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(
            &raw const options,
            ptr::null(),
            0,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(handle.is_null());

    let options = ClpSKvIrSerializerOptions {
        encoding: 99,
        ..ClpSKvIrSerializerOptions::default()
    };
    // SAFETY: Every pointer names live storage; the unknown encoding is tested.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(
            &raw const options,
            ptr::null(),
            0,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(handle.is_null());

    let options = ClpSKvIrSerializerOptions {
        max_pending_output_bytes: 1,
        ..ClpSKvIrSerializerOptions::default()
    };
    // SAFETY: Every pointer names live storage; the deliberately tiny limit is tested.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(
            &raw const options,
            ptr::null(),
            0,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_LIMIT_EXCEEDED, status);
    assert!(handle.is_null());

    // SAFETY: Null metadata with nonzero length is the tested pointer shape; outputs are valid.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(ptr::null(), ptr::null(), 1, &raw mut handle, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(handle.is_null());

    let empty = [0_u8; 0];
    // SAFETY: A non-null empty slice deliberately means present-but-empty invalid metadata.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(
            ptr::null(),
            empty.as_ptr(),
            0,
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_KV_IR_INVALID_DATA, status);
    assert!(handle.is_null());

    // SAFETY: A null mandatory output is the tested input; diagnostics remain writable.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_new(ptr::null(), ptr::null(), 0, ptr::null_mut(), &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);

    let eight_byte_options = ClpSKvIrSerializerOptions {
        encoding: CLP_S_KV_IR_ENCODING_EIGHT_BYTE,
        ..ClpSKvIrSerializerOptions::default()
    };
    let eight_byte = new_serializer(Some(&eight_byte_options), None);
    assert_eq!(&[0xfd, 0x2f, 0xb5, 0x30], &pending_output(&eight_byte)[..4]);
}

#[test]
fn kv_ir_serializer_initializes_outputs_before_handle_and_map_validation() {
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    let empty_map = [0x80];
    let mut event_bytes = u64::MAX;
    // SAFETY: The null handle is tested; all byte regions and outputs remain live.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_append_msgpack_maps(
            ptr::null_mut(),
            empty_map.as_ptr(),
            empty_map.len(),
            empty_map.as_ptr(),
            empty_map.len(),
            &raw mut event_bytes,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(0, event_bytes);

    let serializer = new_serializer(None, None);
    event_bytes = u64::MAX;
    // SAFETY: Null with nonzero auto-map length is the tested shape; outputs remain live.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_append_msgpack_maps(
            serializer.0,
            ptr::null(),
            1,
            empty_map.as_ptr(),
            empty_map.len(),
            &raw mut event_bytes,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(0, event_bytes);

    let mut view = ClpSKvIrPendingView {
        data: ptr::dangling(),
        length: usize::MAX,
    };
    // SAFETY: The null handle is tested and `view` is writable.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_pending_view(ptr::null(), &raw mut view, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(view.data.is_null());
    assert_eq!(0, view.length);

    let mut zeroed_stats = ClpSKvIrSerializerStats {
        log_events: u64::MAX,
        schema_nodes: u64::MAX,
        utc_offset_changes: u64::MAX,
        serialized_bytes: u64::MAX,
        pending_bytes: u64::MAX,
        is_finished: u32::MAX,
        reserved: u32::MAX,
    };
    // SAFETY: The null handle is tested and `zeroed_stats` is writable.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_stats(ptr::null(), &raw mut zeroed_stats, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(ClpSKvIrSerializerStats::default(), zeroed_stats);

    let pending = pending_output(&serializer).len();
    // SAFETY: The live handle has fewer than `pending + 1` pending bytes by construction.
    let status =
        unsafe { clp_s_v1_kv_ir_serializer_consume(serializer.0, pending + 1, &raw mut error) };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(pending, pending_output(&serializer).len());

    // SAFETY: Null frees are explicitly documented no-ops.
    unsafe {
        clp_s_v1_kv_ir_serializer_free(ptr::null_mut());
    }
}

#[test]
fn kv_ir_serializer_rejects_null_mandatory_outputs_without_mutation() {
    let serializer = new_serializer(None, None);
    let original_pending = pending_output(&serializer);
    let empty_map = [0x80];
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);

    // SAFETY: A null mandatory event-byte output is the tested input; all other regions are live.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_append_msgpack_maps(
            serializer.0,
            empty_map.as_ptr(),
            empty_map.len(),
            empty_map.as_ptr(),
            empty_map.len(),
            ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);

    // SAFETY: Null mandatory outputs are tested one at a time on the live handle.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_pending_view(serializer.0, ptr::null_mut(), &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    // SAFETY: Null mandatory outputs are tested one at a time on the live handle.
    let status =
        unsafe { clp_s_v1_kv_ir_serializer_stats(serializer.0, ptr::null_mut(), &raw mut error) };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    // SAFETY: Null mandatory outputs are tested one at a time on the live handle.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_total_bytes(serializer.0, ptr::null_mut(), &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    // SAFETY: Null mandatory outputs are tested one at a time on the live handle.
    let status = unsafe {
        clp_s_v1_kv_ir_serializer_change_utc_offset(
            serializer.0,
            0,
            ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    // SAFETY: Null mandatory outputs are tested one at a time on the live handle.
    let status =
        unsafe { clp_s_v1_kv_ir_serializer_finish(serializer.0, ptr::null_mut(), &raw mut error) };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert_eq!(original_pending, pending_output(&serializer));
}

#[test]
fn kv_ir_deserializer_reads_cpp_four_and_eight_byte_fixtures_in_tiny_chunks() {
    for fixture_hex in [FOUR_BYTE_KV_IR_ORACLE_HEX, EIGHT_BYTE_KV_IR_ORACLE_HEX] {
        let mut state = InputState::new(decode_hex(fixture_hex));
        let expected_length = state.bytes.len();
        let mut deserializer = new_deserializer(&mut state, Some(&tiny_chunk_options()));
        assert_eq!(KV_IR_FIXTURE_METADATA, deserializer_metadata(&deserializer));

        // The metadata view owns a full copy rather than borrowing the callback input.
        state.bytes[7..7 + KV_IR_FIXTURE_METADATA.len()].fill(0);
        assert_eq!(KV_IR_FIXTURE_METADATA, deserializer_metadata(&deserializer));

        let (status, event, message) = next_event_with_view(&deserializer);
        assert_eq!(CLP_S_STATUS_OK, status, "{message}");
        let (event, view) = event.expect("one fixture event");
        assert_eq!(2, view.auto_node_count);
        assert_eq!(5, view.user_node_count);
        assert_eq!(3_600_000, view.utc_offset_millis);
        assert_eq!(0, view.stream_index);
        assert_eq!(0, view.event_index);
        assert!(0 < view.unit_index);
        assert!(0 < view.input_offset);
        assert_eq!([0; 4], view.reserved);

        let arena = arena_bytes(&view);
        let auto = event_nodes(&view, true);
        assert_eq!(b"level", span_bytes(arena, auto[0].key_span()));
        assert_eq!(CLP_S_KV_IR_VALUE_STRING, auto[0].value_kind() as u32);
        assert_eq!(b"info", span_bytes(arena, auto[0].value_span()));
        assert_eq!(b"seq", span_bytes(arena, auto[1].key_span()));
        assert_eq!(CLP_S_KV_IR_VALUE_INTEGER, auto[1].value_kind() as u32);
        assert_eq!(7, auto[1].scalar_bits());

        let user = event_nodes(&view, false);
        let keys = user
            .iter()
            .map(|node| span_bytes(arena, node.key_span()))
            .collect::<Vec<_>>();
        assert_eq!(
            [
                b"empty".as_slice(),
                b"message".as_slice(),
                b"none".as_slice(),
                b"ok".as_slice(),
                b"ratio".as_slice(),
            ],
            keys.as_slice()
        );
        assert_eq!(CLP_S_KV_IR_VALUE_EMPTY_OBJECT, user[0].value_kind() as u32);
        assert_eq!(CLP_S_KV_IR_VALUE_STRING, user[1].value_kind() as u32);
        assert_eq!(
            b"task 42 done",
            span_bytes(arena, user[1].value_span()),
            "encoded text is reconstructed into the owned arena"
        );
        assert_eq!(CLP_S_KV_IR_VALUE_NULL, user[2].value_kind() as u32);
        assert_eq!(CLP_S_KV_IR_VALUE_BOOLEAN, user[3].value_kind() as u32);
        assert_eq!(1, user[3].scalar_bits());
        assert_eq!(CLP_S_KV_IR_VALUE_FLOAT, user[4].value_kind() as u32);
        assert_eq!(1.25_f64.to_bits(), user[4].scalar_bits());

        let (status, no_event, message) = next_event(&deserializer);
        assert_eq!(CLP_S_STATUS_EOF, status, "{message}");
        assert!(no_event.is_none());
        let calls_at_eof = state.calls;
        let (status, no_event, message) = next_event(&deserializer);
        assert_eq!(CLP_S_STATUS_EOF, status, "{message}");
        assert!(no_event.is_none());
        assert_eq!(
            calls_at_eof, state.calls,
            "repeated EOF performs no callback"
        );
        assert_eq!(expected_length, state.offset);

        deserializer.free();
        let retained = event_view(&event);
        assert_eq!(view.arena, retained.arena);
        assert_eq!(view.user_nodes, retained.user_nodes);
        assert_eq!(
            b"task 42 done",
            span_bytes(arena_bytes(&retained), user[1].value_span())
        );
    }
}

#[test]
fn kv_ir_owned_event_exposes_every_value_kind_and_exact_scalar_bits() {
    let mut user = vec![
        0x87, 0xa3, b'o', b'b', b'j', 0x81, 0xa6, b'n', b'e', b's', b't', b'e', b'd', 0xfb, 0xa5,
        b'a', b'r', b'r', b'a', b'y', 0x95, 0x01, 0xa1, b'x', 0xc3, 0xc0, 0x80, 0xa4, b't', b'e',
        b'x', b't', 0xac, b't', b'a', b's', b'k', b' ', b'4', b'2', b' ', b'd', b'o', b'n', b'e',
        0xa5, b'f', b'l', b'o', b'a', b't', 0xcb,
    ];
    user.extend_from_slice(&1.25_f64.to_bits().to_be_bytes());
    user.extend_from_slice(&[
        0xa4, b'b', b'o', b'o', b'l', 0xc2, 0xa4, b'n', b'u', b'l', b'l', 0xc0, 0xa5, b'e', b'm',
        b'p', b't', b'y', 0x80,
    ]);
    let stream = serialize_event_stream(CLP_S_KV_IR_ENCODING_FOUR_BYTE, &user);
    let mut state = InputState::new(stream);
    let deserializer = new_deserializer(&mut state, None);
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    let event = event.expect("one generated event");
    let view = event_view(&event);
    assert_eq!(0, view.auto_node_count);
    let arena = arena_bytes(&view);
    let nodes = event_nodes(&view, false);
    assert_eq!(8, nodes.len());
    assert_eq!(
        [
            CLP_S_KV_IR_VALUE_OBJECT,
            CLP_S_KV_IR_VALUE_INTEGER,
            CLP_S_KV_IR_VALUE_ARRAY_JSON,
            CLP_S_KV_IR_VALUE_STRING,
            CLP_S_KV_IR_VALUE_FLOAT,
            CLP_S_KV_IR_VALUE_BOOLEAN,
            CLP_S_KV_IR_VALUE_NULL,
            CLP_S_KV_IR_VALUE_EMPTY_OBJECT,
        ],
        nodes
            .iter()
            .map(|node| node.value_kind() as u32)
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(
        [1, 2, 1, 1, 1, 1, 1, 1],
        nodes
            .iter()
            .map(|node| node.depth())
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(b"obj", span_bytes(arena, nodes[0].key_span()));
    assert_eq!(b"nested", span_bytes(arena, nodes[1].key_span()));
    assert_eq!((-5_i64).to_ne_bytes(), nodes[1].scalar_bits().to_ne_bytes());
    assert_eq!(
        b"[1,\"x\",true,null,{}]",
        span_bytes(arena, nodes[2].value_span())
    );
    assert_eq!(b"task 42 done", span_bytes(arena, nodes[3].value_span()));
    assert_eq!(1.25_f64.to_bits(), nodes[4].scalar_bits());
    assert_eq!(0, nodes[5].scalar_bits());
}

#[test]
fn kv_ir_deserializer_stops_at_first_stream_end_and_ignores_concatenation() {
    let first = decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX);
    let first_length = first.len();
    let mut concatenated = first;
    concatenated.extend_from_slice(&decode_hex(EIGHT_BYTE_KV_IR_ORACLE_HEX));
    let mut state = InputState::new(concatenated);
    let deserializer = new_deserializer(&mut state, Some(&tiny_chunk_options()));
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_OK, status, "{message}");
    drop(event.expect("first-stream event"));
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_EOF, status, "{message}");
    assert!(event.is_none());
    assert_eq!(first_length, state.offset);
    let calls_at_eof = state.calls;
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_EOF, status, "{message}");
    assert!(event.is_none());
    assert_eq!(first_length, state.offset);
    assert_eq!(calls_at_eof, state.calls);
}

#[test]
fn kv_ir_deserializer_distinguishes_truncation_callback_failure_and_terminal_state() {
    let tiny_options = tiny_chunk_options();
    for mut state in [
        InputState::new(Vec::new()),
        InputState::new(vec![0xfd, 0x2f, 0xb5]),
    ] {
        let mut handle = ptr::dangling_mut::<ClpSKvIrDeserializer>();
        let mut text = [0_u8; 128];
        let mut error = error_buffer(&mut text);
        // SAFETY: Callback state and outputs remain live; incomplete bytes are the tested input.
        let status = unsafe {
            clp_s_v1_kv_ir_deserializer_new(
                &raw const tiny_options,
                Some(read_input),
                (&raw mut state).cast(),
                &raw mut handle,
                &raw mut error,
            )
        };
        assert_eq!(CLP_S_STATUS_KV_IR_INCOMPLETE, status);
        assert!(handle.is_null());
        assert!(error.required > 1);
    }

    let mut callback_failure = InputState::new(decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX));
    callback_failure.fail_on_call = Some(1);
    let mut handle = ptr::dangling_mut::<ClpSKvIrDeserializer>();
    let mut text = [0_u8; 128];
    let mut error = error_buffer(&mut text);
    // SAFETY: Callback state and outputs remain live; callback failure is intentional.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            &raw const tiny_options,
            Some(read_input),
            (&raw mut callback_failure).cast(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR, status);
    assert!(handle.is_null());

    let mut over_report = InputState::new(decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX));
    over_report.over_report = true;
    // SAFETY: Callback state and outputs remain live; callback contract violation is intentional.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            &raw const tiny_options,
            Some(read_input),
            (&raw mut over_report).cast(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR, status);
    assert!(handle.is_null());

    let fixture = decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX);
    let mut truncated = InputState::new(fixture[..220].to_vec());
    let deserializer = new_deserializer(&mut truncated, Some(&tiny_chunk_options()));
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_KV_IR_INCOMPLETE, status, "{message}");
    assert!(event.is_none());
    let calls_at_failure = truncated.calls;
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_INVALID_STATE, status, "{message}");
    assert!(event.is_none());
    assert_eq!(calls_at_failure, truncated.calls);

    let mut later_callback_failure = InputState::new(fixture);
    let deserializer = new_deserializer(&mut later_callback_failure, Some(&tiny_chunk_options()));
    later_callback_failure.fail_on_call = Some(later_callback_failure.calls + 1);
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_KV_IR_INPUT_CALLBACK_ERROR, status, "{message}");
    assert!(event.is_none());
    let calls_at_failure = later_callback_failure.calls;
    let (status, event, message) = next_event(&deserializer);
    assert_eq!(CLP_S_STATUS_INVALID_STATE, status, "{message}");
    assert!(event.is_none());
    assert_eq!(calls_at_failure, later_callback_failure.calls);
}

#[test]
fn kv_ir_deserializer_constructor_rejects_an_invalid_stream_start() {
    let mut state = InputState::new(vec![0, 0, 0, 0]);
    let mut handle = ptr::dangling_mut::<ClpSKvIrDeserializer>();
    let mut text = [0_u8; 256];
    let mut error = error_buffer(&mut text);
    let options = tiny_chunk_options();
    // SAFETY: Callback state and outputs remain live; the malformed magic is intentional.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            &raw const options,
            Some(read_input),
            (&raw mut state).cast(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_KV_IR_INVALID_DATA, status);
    assert!(handle.is_null());
    assert!(error.required > 1);
}

#[test]
fn kv_ir_deserializer_validates_options_pointers_and_zeroes_outputs() {
    let mut state = InputState::new(decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX));
    let mut handle = ptr::dangling_mut::<ClpSKvIrDeserializer>();
    let mut text = [0xff_u8; 12];
    let mut error = error_buffer(&mut text);
    let bad_size = ClpSKvIrDeserializerOptions {
        struct_size: 0,
        ..ClpSKvIrDeserializerOptions::default()
    };
    // SAFETY: Inputs and outputs are live; the bad structure size is intentional.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            &raw const bad_size,
            Some(read_input),
            (&raw mut state).cast(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(handle.is_null());
    assert_eq!(0, text[text.len() - 1]);
    assert!(error.required > text.len());
    assert_eq!(0, state.calls, "invalid options precede callback work");

    let reserved = ClpSKvIrDeserializerOptions {
        reserved: [0, 1, 0, 0],
        ..ClpSKvIrDeserializerOptions::default()
    };
    // SAFETY: Inputs and outputs are live; the nonzero reserved field is intentional.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            &raw const reserved,
            Some(read_input),
            (&raw mut state).cast(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(handle.is_null());

    // SAFETY: Null callback and null mandatory output are independently tested inputs.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            ptr::null(),
            None,
            ptr::null_mut(),
            &raw mut handle,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(handle.is_null());
    // SAFETY: The mandatory output is intentionally null; no callback may occur.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_new(
            ptr::null(),
            Some(read_input),
            (&raw mut state).cast(),
            ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);

    let mut byte_view = ClpSKvIrByteView {
        data: ptr::dangling(),
        length: u64::MAX,
    };
    // SAFETY: Null handle is intentional and the output is writable.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_metadata_view(ptr::null(), &raw mut byte_view, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(byte_view.data.is_null());
    assert_eq!(0, byte_view.length);

    let mut event = ptr::dangling_mut::<ClpSKvIrEvent>();
    // SAFETY: Null handle is intentional and the event output is writable.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_next_event(ptr::null_mut(), &raw mut event, &raw mut error)
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(event.is_null());

    let mut view = ClpSKvIrEventView {
        arena: ptr::dangling(),
        arena_length: u64::MAX,
        auto_nodes: ptr::dangling(),
        auto_node_count: u64::MAX,
        user_nodes: ptr::dangling(),
        user_node_count: u64::MAX,
        utc_offset_millis: i64::MAX,
        stream_index: u64::MAX,
        unit_index: u64::MAX,
        event_index: u64::MAX,
        input_offset: u64::MAX,
        reserved: [u64::MAX; 4],
    };
    // SAFETY: Null event is intentional and the view output is writable.
    let status = unsafe { clp_s_v1_kv_ir_event_view(ptr::null(), &raw mut view, &raw mut error) };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(view.arena.is_null());
    assert!(view.auto_nodes.is_null());
    assert!(view.user_nodes.is_null());
    assert_eq!(0, view.arena_length);
    assert_eq!(0, view.auto_node_count);
    assert_eq!(0, view.user_node_count);
    assert_eq!([0; 4], view.reserved);
}

#[test]
fn kv_ir_combined_next_event_validates_and_zeroes_outputs() {
    let mut event = ptr::dangling_mut::<ClpSKvIrEvent>();
    let mut view = zero_event_view();
    let mut text = [0_u8; 128];
    let mut error = error_buffer(&mut text);

    // SAFETY: Null handle is intentional and both outputs are writable.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_next_event_with_view(
            ptr::null_mut(),
            &raw mut event,
            &raw mut view,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(event.is_null());
    assert!(view.arena.is_null());

    // SAFETY: Null mandatory outputs are tested individually; the other output is writable.
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_next_event_with_view(
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut view,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    let status = unsafe {
        clp_s_v1_kv_ir_deserializer_next_event_with_view(
            ptr::null_mut(),
            &raw mut event,
            ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(event.is_null());
}

#[test]
fn kv_ir_deserializer_enforces_owned_event_limits_and_becomes_terminal() {
    let limits = ClpSKvIrDeserializerOptions {
        max_read_chunk_bytes: 1,
        max_materialized_nodes: 1,
        ..ClpSKvIrDeserializerOptions::default()
    };
    let mut limited_state = InputState::new(decode_hex(FOUR_BYTE_KV_IR_ORACLE_HEX));
    let limited = new_deserializer(&mut limited_state, Some(&limits));
    let (status, event, message) = next_event(&limited);
    assert_eq!(CLP_S_STATUS_LIMIT_EXCEEDED, status, "{message}");
    assert!(event.is_none());
    let (status, event, message) = next_event(&limited);
    assert_eq!(CLP_S_STATUS_INVALID_STATE, status, "{message}");
    assert!(event.is_none());

    // SAFETY: Null frees are explicit no-ops.
    unsafe {
        clp_s_v1_kv_ir_deserializer_free(ptr::null_mut());
        clp_s_v1_kv_ir_event_free(ptr::null_mut());
    }
}

fn auto_map() -> Vec<u8> {
    vec![
        0x82, 0xa5, b'l', b'e', b'v', b'e', b'l', 0xa4, b'i', b'n', b'f', b'o', 0xa3, b's', b'e',
        b'q', 0x07,
    ]
}

fn user_map() -> Vec<u8> {
    let mut bytes = vec![
        0x85, 0xa5, b'e', b'm', b'p', b't', b'y', 0x80, 0xa7, b'm', b'e', b's', b's', b'a', b'g',
        b'e', 0xac, b't', b'a', b's', b'k', b' ', b'4', b'2', b' ', b'd', b'o', b'n', b'e', 0xa4,
        b'n', b'o', b'n', b'e', 0xc0, 0xa2, b'o', b'k', 0xc3, 0xa5, b'r', b'a', b't', b'i', b'o',
        0xcb,
    ];
    bytes.extend_from_slice(&1.25_f64.to_bits().to_be_bytes());
    bytes
}

fn decode_hex(source: &str) -> Vec<u8> {
    let digits: Vec<u8> = source.bytes().filter(u8::is_ascii_hexdigit).collect();
    let (pairs, remainder) = digits.as_chunks::<2>();
    assert_eq!(remainder, []);
    pairs
        .iter()
        .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
        .collect()
}

const fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        b'A'..=b'F' => value - b'A' + 10,
        _ => panic!("non-hex byte"),
    }
}
