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
const AGGREGATIONS_ARCHIVE: &str = "sfa-v0.5.0-aggregations-cpp.bin";
const LOG_ORDER_ARCHIVE: &str = "sfa-v0.5.0-log-order-cpp.bin";
const ARRAY_DIRECTORY: &str = "sfa-v0.5.0-unstructured-arrays-cpp-dir";
const UNSTRUCTURED_ARRAY_ARCHIVE: &str = "sfa-v0.5.0-unstructured-arrays-cpp.bin";
const UNSTRUCTURED_ARRAY_JSONL: &str = "sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl";
const STRUCTURED_ARRAY_JSONL: &str = "sfa-v0.5.0-structured-arrays-cpp-input.jsonl";
const STRUCTURED_ARRAY_ARCHIVE_HEX: &str =
    include_str!("../../clp-s/tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");
const FOUR_BYTE_KV_IR_ORACLE_HEX: &str =
    include_str!("../../clp-s/tests/fixtures/kv-ir-v0.1.0-four-byte-cpp.hex");
const EIGHT_BYTE_KV_IR_ORACLE_HEX: &str =
    include_str!("../../clp-s/tests/fixtures/kv-ir-v0.1.0-eight-byte-cpp.hex");
const NESTED_KV_IR_SEARCH_HEX: &str =
    include_str!("../../clp-s/tests/fixtures/kv-ir-search-v0.1.0-nested-cpp.hex");
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
    assert_eq!(24, std::mem::size_of::<ClpSV2ArchiveFieldType>());

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

struct ArchiveFieldTypesHandle(*mut ClpSV2ArchiveFieldTypeList);

impl Drop for ArchiveFieldTypesHandle {
    fn drop(&mut self) {
        // SAFETY: The wrapper owns exactly one live listing.
        unsafe {
            clp_s_v2_archive_field_type_list_free(self.0);
        }
    }
}

/// Lists an archive's field types as owned `(escaped path, node code)` pairs.
fn archive_field_types(archive: &ArchiveHandle) -> Vec<(Vec<u8>, u8)> {
    let mut types = ptr::dangling_mut::<ClpSV2ArchiveFieldTypeList>();
    let mut entries = ptr::dangling::<ClpSV2ArchiveFieldType>();
    let mut count = usize::MAX;
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: The archive is live and every output is writable and disjoint.
    let status = unsafe {
        clp_s_v2_archive_field_types(
            archive.0,
            &raw mut types,
            &raw mut entries,
            &raw mut count,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert!(!types.is_null());
    let owner = ArchiveFieldTypesHandle(types);
    // SAFETY: The listing owns `count` entries and their paths until `owner` is dropped.
    let entries = unsafe { slice::from_raw_parts(entries, count) };
    let listed = entries
        .iter()
        .map(|entry| {
            // SAFETY: Each path is `path_length` bytes owned by the listing.
            let path = unsafe { slice::from_raw_parts(entry.path, entry.path_length) };
            (path.to_vec(), entry.node_type)
        })
        .collect();
    drop(owner);
    listed
}

#[test]
fn archive_field_types_list_each_addressable_node_once_per_type() {
    let archive = open_archive(&fixture(AGGREGATIONS_ARCHIVE));
    let types = archive_field_types(&archive);
    let type_of = |name: &[u8]| -> Vec<u8> {
        let mut codes: Vec<u8> = types
            .iter()
            .filter(|(path, _)| path.as_slice() == name)
            .map(|(_, node_type)| *node_type)
            .collect();
        codes.sort_unstable();
        codes
    };

    // A nested path is dot-joined, and a key holding a literal dot is escaped, so `outer` ->
    // `a.b` -> `value` cannot be confused with `outer` -> `a` -> `b` -> `value`.
    assert_eq!(vec![archive_node::OBJECT], type_of(b"outer"), "{types:?}");
    assert_eq!(vec![archive_node::OBJECT], type_of(br"outer.a\.b"), "{types:?}");
    assert_eq!(vec![archive_node::INTEGER], type_of(br"outer.a\.b.value"), "{types:?}");
    assert!(type_of(b"outer.a.b.value").is_empty(), "{types:?}");

    // A key stored as an integer in one record and a float in another is listed under both, which
    // is how a caller learns the path is mixed.
    let mixed = type_of(b"mixed");
    assert_eq!(2, mixed.len(), "{types:?}");
    assert!(mixed.contains(&archive_node::INTEGER), "{types:?}");
    assert!(!mixed.iter().all(|code| *code == archive_node::INTEGER), "{types:?}");

    // `unique` holds an integer, a float, strings, both Booleans, null, an object and an array.
    let unique = type_of(b"unique");
    for expected in [
        archive_node::INTEGER,
        archive_node::BOOLEAN,
        archive_node::NULL,
        archive_node::OBJECT,
    ] {
        assert!(unique.contains(&expected), "{expected} missing from {types:?}");
    }
    assert!(
        unique.contains(&archive_node::VAR_STRING) || unique.contains(&archive_node::CLP_STRING),
        "{types:?}"
    );
    // An object's members are listed under its path, whichever other types the key also takes.
    assert_eq!(vec![archive_node::INTEGER], type_of(b"unique.nested"), "{types:?}");

    assert_eq!(1, type_of(b"ts").len(), "{types:?}");
    assert!(type_of(b"no-such-key").is_empty(), "{types:?}");
    assert!(types.iter().all(|(path, _)| !path.is_empty()), "{types:?}");
}

#[test]
fn archive_field_types_reject_null_arguments_after_resetting_outputs() {
    let archive = open_archive(&fixture(MINIMAL_ARCHIVE));
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);

    let mut types = ptr::dangling_mut::<ClpSV2ArchiveFieldTypeList>();
    let mut entries = ptr::dangling::<ClpSV2ArchiveFieldType>();
    let mut count = usize::MAX;
    // SAFETY: The outputs are writable; the archive handle is deliberately null.
    let status = unsafe {
        clp_s_v2_archive_field_types(
            ptr::null(),
            &raw mut types,
            &raw mut entries,
            &raw mut count,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(types.is_null());
    assert!(entries.is_null());
    assert_eq!(0, count);

    // SAFETY: The archive is live; the entry output is deliberately null.
    let status = unsafe {
        clp_s_v2_archive_field_types(
            archive.0,
            &raw mut types,
            ptr::null_mut(),
            &raw mut count,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, status);
    assert!(types.is_null());

    // SAFETY: Freeing null is a documented no-op.
    unsafe {
        clp_s_v2_archive_field_type_list_free(ptr::null_mut());
    }

    // The minimal fixture's flat record lists each key exactly once.
    let listed = archive_field_types(&archive);
    let mut paths: Vec<&[u8]> = listed.iter().map(|(path, _)| path.as_slice()).collect();
    paths.sort_unstable();
    assert_eq!(
        vec![&b"active"[..], b"level", b"message", b"ts", b"value"],
        paths,
        "{listed:?}"
    );
    assert!(
        listed.contains(&(b"value".to_vec(), archive_node::INTEGER)),
        "{listed:?}"
    );
    assert!(
        listed.contains(&(b"active".to_vec(), archive_node::BOOLEAN)),
        "{listed:?}"
    );
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

/// Counts the rows a projected scan matches when it projects no fields.
fn projected_scan_count(archive: &Path, query: &str) -> Result<u64, String> {
    let query = compile_query(query, 0);
    let path = archive.to_str().expect("archive path is UTF-8").as_bytes();
    let mut scanner = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Every pointer names live storage for the duration of the call.
    let status = unsafe {
        clp_s_v2_scanner_open(
            path.as_ptr(),
            path.len(),
            query.0,
            ptr::null(),
            0,
            &raw mut scanner,
            &raw mut error,
        )
    };
    if CLP_S_STATUS_OK != status {
        return Err(error_text(&text).to_owned());
    }
    let mut rows = 0;
    let result = loop {
        let mut has_row = 0_u32;
        let mut text = [0_u8; 512];
        let mut error = error_buffer(&mut text);
        // SAFETY: The scanner is live, and with no projected fields it writes no values.
        let status = unsafe {
            clp_s_v2_scanner_next_row(scanner, ptr::null_mut(), &raw mut has_row, &raw mut error)
        };
        if CLP_S_STATUS_OK != status {
            break Err(error_text(&text).to_owned());
        }
        if 0 == has_row {
            break Ok(rows);
        }
        rows += 1;
    };
    // SAFETY: The scanner is live and freed exactly once.
    unsafe { clp_s_v2_scanner_free(scanner) };
    result
}

/// Writes `source` into a single-file archive with the Rust writer's adaptive numeric columns, so
/// every table holding a number is stored one zstd frame per column and a scan inflates only the
/// columns it reads.
fn adaptive_archive(source: &[u8], structurize_arrays: bool) -> Vec<u8> {
    let mut archive = clp_s::writer::OpenArchive::new(
        std::io::Cursor::new(Vec::new()),
        clp_s::writer::WriterOptions::default().with_adaptive_numeric_columns(true),
    );
    {
        let mut reader =
            clp_s::ingest::ParseManyReader::new(source, clp_s::ingest::ParseManyOptions::default());
        let mut sink = clp_s::ingest::JsonArchiveSink::new(
            &mut archive,
            clp_s::ingest::JsonArchiveOptions::new().with_structurize_arrays(structurize_arrays),
        );
        reader.read_to_end(&mut sink).expect("ingest array fixture");
    }
    archive
        .finish()
        .expect("finish array archive")
        .into_inner()
        .into_inner()
}

#[test]
fn projected_scan_matches_predicates_inside_arrays() {
    let temporary = |name: &str, bytes: Vec<u8>| {
        let path =
            std::env::temp_dir().join(format!("clp-s-ffi-{name}-{}.clps", std::process::id()));
        std::fs::write(&path, bytes).expect("write array archive");
        path
    };
    let unstructured_source =
        std::fs::read(fixture(UNSTRUCTURED_ARRAY_JSONL)).expect("read unstructured-array source");
    let structured_source =
        std::fs::read(fixture(STRUCTURED_ARRAY_JSONL)).expect("read structured-array source");
    let cpp_structured = temporary("cpp-structured", decode_hex(STRUCTURED_ARRAY_ARCHIVE_HEX));
    let rust_unstructured = temporary(
        "rust-unstructured",
        adaptive_archive(&unstructured_source, false),
    );
    let rust_structured = temporary(
        "rust-structured",
        adaptive_archive(&structured_source, true),
    );
    let cases = [
        (fixture(UNSTRUCTURED_ARRAY_ARCHIVE), "array.k: \"v\""),
        (fixture(UNSTRUCTURED_ARRAY_ARCHIVE), "array.n: 9"),
        (cpp_structured.clone(), "items.x: 3"),
        (cpp_structured.clone(), "items.nested.z: \"yes\""),
        (rust_unstructured.clone(), "array.k: \"v\""),
        (rust_unstructured.clone(), "array.n: 9"),
        (rust_structured.clone(), "items.x: 3"),
        (rust_structured.clone(), "items.nested.z: \"yes\""),
    ];
    for (archive, query) in &cases {
        // A predicate on an array element names a path the schema tree has no node for, so the
        // scan must still read the array's columns and dictionaries. The full search reads
        // everything, so it is the reference.
        let mut state = CallbackState::default();
        let (status, expected, error) =
            search(&open_archive(archive), &compile_query(query, 0), &mut state);
        assert_eq!(CLP_S_STATUS_OK, status, "{query}: {error}");
        assert!(0 < expected, "{query} matches a fixture record");
        assert_eq!(
            Ok(expected),
            projected_scan_count(archive, query),
            "{query} on {}",
            archive.display()
        );
    }
    for path in [cpp_structured, rust_unstructured, rust_structured] {
        std::fs::remove_file(path).expect("remove array archive");
    }
}

/// The integers a projected scan returns for one field, in scan order.
fn projected_scan_integers(archive: &Path, query: &str, field: &str) -> Vec<i64> {
    let query = compile_query(query, 0);
    let path = archive.to_str().expect("archive path is UTF-8").as_bytes();
    let fields = [ClpSV2ProjectedField {
        path: field.as_ptr(),
        path_length: field.len(),
    }];
    let mut scanner = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Every pointer names live storage for the duration of the call.
    let status = unsafe {
        clp_s_v2_scanner_open(
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
    let mut values = Vec::new();
    loop {
        let mut value = ClpSV2Value {
            kind: 0,
            reserved: 0,
            integer: 0,
            real: 0.0,
            text: ptr::null(),
            text_length: 0,
        };
        let mut has_row = 0_u32;
        let mut text = [0_u8; 512];
        let mut error = error_buffer(&mut text);
        // SAFETY: The scanner is live and `value` holds one cell per projected field.
        let status = unsafe {
            clp_s_v2_scanner_next_row(scanner, &raw mut value, &raw mut has_row, &raw mut error)
        };
        assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
        if 0 == has_row {
            break;
        }
        assert_eq!(CLP_S_V2_VALUE_INTEGER, value.kind);
        values.push(value.integer);
    }
    // SAFETY: The scanner is live and freed exactly once.
    unsafe { clp_s_v2_scanner_free(scanner) };
    values
}

/// The strings a projected scan returns for one field, in scan order, `None` for any other kind.
fn projected_scan_strings(archive: &Path, query: &str, field: &str) -> Vec<Option<String>> {
    let query = compile_query(query, 0);
    let path = archive.to_str().expect("archive path is UTF-8").as_bytes();
    let fields = [ClpSV2ProjectedField {
        path: field.as_ptr(),
        path_length: field.len(),
    }];
    let mut scanner = ptr::null_mut();
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: Every pointer names live storage for the duration of the call.
    let status = unsafe {
        clp_s_v2_scanner_open(
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
    let mut values = Vec::new();
    loop {
        let mut value = ClpSV2Value {
            kind: 0,
            reserved: 0,
            integer: 0,
            real: 0.0,
            text: ptr::null(),
            text_length: 0,
        };
        let mut has_row = 0_u32;
        let mut text = [0_u8; 512];
        let mut error = error_buffer(&mut text);
        // SAFETY: The scanner is live and `value` holds one cell per projected field.
        let status = unsafe {
            clp_s_v2_scanner_next_row(scanner, &raw mut value, &raw mut has_row, &raw mut error)
        };
        assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
        if 0 == has_row {
            break;
        }
        values.push((CLP_S_V2_VALUE_STRING == value.kind).then(|| {
            // SAFETY: A string cell's span stays valid until the scanner's next call.
            let bytes = unsafe { std::slice::from_raw_parts(value.text, value.text_length) };
            String::from_utf8(bytes.to_vec()).expect("projected text is UTF-8")
        }));
    }
    // SAFETY: The scanner is live and freed exactly once.
    unsafe { clp_s_v2_scanner_free(scanner) };
    values
}

#[test]
fn projected_scan_writes_shape_fields_as_logtype_templates() {
    let source = concat!(
        r#"{"n":1,"msg":"took 15 ms for user alice42","content":[{"is_error":true,"text":"a 1"}]}"#,
        "\n",
        r#"{"n":2,"msg":"ratio 0.5 at 100% of 10.0.0.1","content":[{"is_error":false,"text":"b 2"}]}"#,
        "\n",
        r#"{"n":3,"msg":"no variables here at all","content":[{"is_error":true,"text":"c 3"}]}"#,
        "\n",
    );
    let path = std::env::temp_dir().join(format!("clp-s-ffi-shape-{}.clps", std::process::id()));
    std::fs::write(&path, adaptive_archive(source.as_bytes(), true)).expect("write shape archive");
    let strings = |values: &[&str]| -> Vec<Option<String>> {
        values
            .iter()
            .map(|value| Some((*value).to_owned()))
            .collect()
    };
    assert_eq!(
        strings(&[
            "took %int% ms for user %str%",
            "ratio %float% at %int%%% of %str%",
            "no variables here at all",
        ]),
        projected_scan_strings(&path, "*", "shape(msg)")
    );
    assert_eq!(
        strings(&[
            "took 15 ms for user alice42",
            "ratio 0.5 at 100% of 10.0.0.1",
            "no variables here at all",
        ]),
        projected_scan_strings(&path, "*", "msg")
    );
    // A filter through the structured array reads its Boolean column alone before the
    // projected field; the rows must be exactly those a whole read matches.
    assert_eq!(
        strings(&["took 15 ms for user alice42", "no variables here at all"]),
        projected_scan_strings(&path, "content.is_error: true", "msg")
    );
    assert_eq!(
        strings(&["took %int% ms for user %str%", "no variables here at all"]),
        projected_scan_strings(&path, "content.is_error: true", "shape(msg)")
    );
    std::fs::remove_file(path).expect("remove shape archive");
}

#[test]
fn projected_scan_skips_streams_no_schema_can_match_and_resumes_after_them() {
    // Adaptive numeric columns give each table holding a number its own stream, so the nine
    // records of this fixture spread over several streams with different schemas. A query on a
    // column only some schemas hold skips the others' streams unread; the rows of every stream
    // after a skipped one must still be found.
    let source =
        std::fs::read(fixture(STRUCTURED_ARRAY_JSONL)).expect("read structured-array source");
    let archive =
        std::env::temp_dir().join(format!("clp-s-ffi-stream-skip-{}.clps", std::process::id()));
    std::fs::write(&archive, adaptive_archive(&source, true)).expect("write archive");
    for (query, expected) in [
        ("obj.items.x: 5", vec![6]),
        ("items.nested.z: *", vec![7, 8]),
        ("id >= 0", vec![0, 1, 2, 3, 4, 5, 6, 7, 8]),
    ] {
        let mut ids = projected_scan_integers(&archive, query, "id");
        ids.sort_unstable();
        assert_eq!(expected, ids, "{query}");
    }
    std::fs::remove_file(&archive).expect("remove archive");
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
        reserved: [0, 1, 0],
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

/// The KV-IR search filters a stream by the compiled query and delivers each matching event's
/// user namespace alone, so a caller asking for a whole record receives a bare document rather
/// than the two-namespace JSONL envelope `clp_s_v1_search`'s KV-IR twin would otherwise carry.
#[test]
fn kv_ir_search_delivers_bare_user_documents() {
    let stream = TemporaryKvIrFile::from_raw_stream(&decode_hex(NESTED_KV_IR_SEARCH_HEX));
    let query = compile_query("*: *", 0);
    let path = stream.path.to_str().expect("temporary path is UTF-8");
    let mut state = CallbackState::default();
    let mut delivered = u64::MAX;
    let mut text = [0_u8; 512];
    let mut error = error_buffer(&mut text);
    // SAFETY: The path bytes and both handles stay live for this synchronous call, and the
    // callback state is a live local that the ABI only borrows during the call.
    let status = unsafe {
        clp_s_v1_kv_ir_search(
            path.as_ptr(),
            path.len(),
            query.0,
            Some(collect_record),
            (&raw mut state).cast(),
            &raw mut delivered,
            &raw mut error,
        )
    };
    assert_eq!(CLP_S_STATUS_OK, status, "{}", error_text(&text));
    assert_eq!(0, error.required);
    assert_eq!(
        u64::try_from(state.records.len()).expect("the delivered count fits"),
        delivered
    );
    assert!(
        !state.records.is_empty(),
        "a match-all query delivers the fixture's events"
    );
    for record in &state.records {
        assert!(
            !record.json.starts_with(b"{\"auto_generated_kv_pairs\""),
            "a whole-record search must not carry the JSONL envelope: {}",
            String::from_utf8_lossy(&record.json)
        );
        assert!(
            record.json.starts_with(b"{") && record.json.ends_with(b"}"),
            "the document is one bare object: {}",
            String::from_utf8_lossy(&record.json)
        );
    }
    assert_eq!(
        br#"{"a":1,"empty":{},"none":null,"obj":{"a":true,"b":"bee"},"z":9}"#,
        state.records[0].json.as_slice()
    );
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

// Copy every borrowed value before advancing the scanner. This compares the old row ABI
// with the compact ABI across stream/table boundaries and sparse selections.
type OwnedProjectedCell = (u32, i64, u64, Vec<u8>);
fn projected_rows(path: &Path, query: &str, names: &[&str], columns: bool) -> Vec<Vec<OwnedProjectedCell>> {
    projected_rows_version(path, query, names, if columns { 3 } else { 2 })
}

fn projected_rows_version(path: &Path, query: &str, names: &[&str], version: u32) -> Vec<Vec<OwnedProjectedCell>> {
    let query = compile_query(query, 0);
    let path = path.to_str().unwrap().as_bytes();
    let fields: Vec<_> = names.iter().map(|name| ClpSV2ProjectedField {
        path: name.as_ptr(), path_length: name.len(),
    }).collect();
    let mut scanner = ptr::null_mut();
    let mut message = [0u8; 512];
    let mut error = error_buffer(&mut message);
    let mut result = Vec::new();
    let next_columns = if version == 4 { clp_s_v4_scanner_next_columns } else { clp_s_v3_scanner_next_columns };
    // SAFETY: All inputs and output storage live through each call; returned views are
    // copied before the next call, and the scanner is freed exactly once.
    unsafe {
        assert_eq!(CLP_S_STATUS_OK, clp_s_v2_scanner_open(path.as_ptr(), path.len(), query.0,
            fields.as_ptr(), fields.len(), &raw mut scanner, &raw mut error), "{}", error_text(&message));
        loop {
            if version == 2 {
                let mut values: Vec<ClpSV2Value> = (0..names.len()).map(|_| std::mem::zeroed()).collect();
                let mut has_row = 0;
                assert_eq!(CLP_S_STATUS_OK, clp_s_v2_scanner_next_row(scanner, values.as_mut_ptr(),
                    &raw mut has_row, &raw mut error), "{}", error_text(&message));
                if has_row == 0 { break; }
                result.push(values.iter().map(|v| (v.kind, v.integer, v.real.to_bits(),
                    if v.text_length == 0 { Vec::new() } else { slice::from_raw_parts(v.text, v.text_length).to_vec() })).collect());
            } else {
                let mut views = ptr::null();
                let mut text = ptr::null();
                let mut text_len = 0;
                let mut rows = 0;
                assert_eq!(CLP_S_STATUS_OK, next_columns(scanner, &raw mut views,
                    &raw mut text, &raw mut text_len, &raw mut rows, &raw mut error), "{}", error_text(&message));
                if rows == 0 {
                    assert!(views.is_null());
                    assert!(text.is_null());
                    assert_eq!(0, text_len);
                    // EOF is stable and clears all outputs.
                    assert_eq!(CLP_S_STATUS_OK, next_columns(scanner, &raw mut views,
                        &raw mut text, &raw mut text_len, &raw mut rows, &raw mut error));
                    assert_eq!(0, rows);
                    break;
                }
                let views = slice::from_raw_parts(views, names.len());
                for row in 0..rows {
                    result.push(views.iter().map(|v| {
                        assert_eq!(0, v.reserved);
                        match v.kind {
                            CLP_S_V2_VALUE_ABSENT => (v.kind, 0, 0, Vec::new()),
                            CLP_S_V2_VALUE_INTEGER => (v.kind, v.data.add(row * 8).cast::<i64>().read_unaligned(), 0, Vec::new()),
                            CLP_S_V2_VALUE_FLOAT => (v.kind, 0, v.data.add(row * 8).cast::<u64>().read_unaligned(), Vec::new()),
                            CLP_S_V4_COLUMN_CONSTANT_INTEGER if version == 4 =>
                                (CLP_S_V2_VALUE_INTEGER, v.data.cast::<i64>().read_unaligned(), 0, Vec::new()),
                            CLP_S_V4_COLUMN_CONSTANT_FLOAT if version == 4 =>
                                (CLP_S_V2_VALUE_FLOAT, 0, v.data.cast::<u64>().read_unaligned(), Vec::new()),
                            CLP_S_V2_VALUE_UNSUPPORTED => {
                                let c = v.data.cast::<ClpSV2Cell>().add(row).read_unaligned();
                                assert!(c.text_offset + c.text_length <= text_len);
                                (c.kind, c.integer, c.real.to_bits(), if c.text_length == 0 { Vec::new() }
                                    else { slice::from_raw_parts(text.add(c.text_offset), c.text_length).to_vec() })
                            }
                            other => panic!("unexpected column layout {other}"),
                        }
                    }).collect());
                }
            }
        }
        clp_s_v2_scanner_free(scanner);
    }
    result
}

#[test]
fn compact_columns_match_row_abi() {
    let mut source = String::new();
    for i in 0..5003 {
        use std::fmt::Write;
        // Several schemas: present, absent and mixed integer/float/string. Include
        // strings and booleans to exercise the cell fallback beside numeric views.
        let extra = match i % 4 { 0 => ",\"mixed\":42", 1 => ",\"mixed\":2.5", 2 => ",\"mixed\":\"hello\"", _ => "" };
        writeln!(source, "{{\"i\":{i},\"f\":{}.25,\"flag\":{},\"text\":\"row {i}\"{extra}}}", i % 101, i % 2 == 0).unwrap();
    }
    let path = std::env::temp_dir().join(format!("clp-compact-columns-{}.clps", std::process::id()));
    std::fs::write(&path, adaptive_archive(source.as_bytes(), false)).unwrap();
    for query in ["*", "i >= 0", "i >= 2047 AND i < 4099", "flag: true", "i > 99999"] {
        for fields in [&["i", "f", "flag", "text", "mixed", "absent"][..], &[][..]] {
            let old = projected_rows(&path, query, fields, false);
            let new = projected_rows(&path, query, fields, true);
            assert_eq!(old, new, "query {query}, fields {fields:?}");
            assert_eq!(old, projected_rows_version(&path, query, fields, 4));
            if query == "*" { assert_eq!(5003, new.len()); }
        }
    }
    std::fs::remove_file(path).unwrap();
    // Also retain compatibility with C++ 0.5 archives (shared frames, delta/timestamps).
    for name in [MINIMAL_ARCHIVE, AGGREGATIONS_ARCHIVE, LOG_ORDER_ARCHIVE] {
        let path = fixture(name);
        let fields = ["timestamp", "level", "message", "value", "absent"];
        assert_eq!(projected_rows(&path, "*", &fields, false), projected_rows(&path, "*", &fields, true));
    }
}

#[test]
fn compact_columns_reject_null_arguments() {
    let mut views = ptr::dangling();
    let mut text = ptr::dangling();
    let mut text_len = 123;
    let mut rows = 123;
    // SAFETY: Null handles are explicitly rejected; output pointers name live storage.
    unsafe {
        assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, clp_s_v3_scanner_next_columns(ptr::null_mut(),
            &raw mut views, &raw mut text, &raw mut text_len, &raw mut rows, ptr::null_mut()));
        assert!(views.is_null());
        assert!(text.is_null());
        assert_eq!((0, 0), (text_len, rows));
        assert_eq!(CLP_S_STATUS_INVALID_ARGUMENT, clp_s_v3_scanner_next_columns(ptr::null_mut(),
            ptr::null_mut(), &raw mut text, &raw mut text_len, &raw mut rows, ptr::null_mut()));
    }
}

#[test]
fn wildcard_projection_preserves_full_search_semantics() {
    // Empty objects, explicit nulls, empty/nested arrays, and mixed schemas must
    // keep the full search engine's semantics; `*` is not replaced with true.
    let source = concat!(
        "{}\n", "{\"obj\":{}}\n", "{\"a\":null}\n", "{\"a\":[]}\n",
        "{\"a\":[{},null]}\n", "{\"a\":[{\"x\":1},{\"nested\":{\"x\":2}}]}\n",
        "{\"n\":1,\"text\":\"alpha\"}\n", "{\"n\":2,\"text\":\"beta 12\"}\n",
        "{\"n\":3,\"a\":[{\"x\":3}]}\n"
    );
    for structured in [false, true] {
        let path = std::env::temp_dir().join(format!("clp-wildcard-{structured}-{}.clps", std::process::id()));
        std::fs::write(&path, adaptive_archive(source.as_bytes(), structured)).unwrap();
        for query in ["*", "NOT *", "* AND n >= 2", "* OR n:1", "NOT (* AND n:1)",
            "*:alpha", "a.*:*", "a.x:*", "NOT a.x:*", "* AND a.x:3",
            "* AND $_filename:*", "* AND $_filename:missing", "*:(*)"] {
            let mut expected = CallbackState::default();
            let (status, count, error) = search(&open_archive(&path), &compile_query(query, 0), &mut expected);
            assert_eq!(CLP_S_STATUS_OK, status, "{query}: {error}");
            for fields in [&[][..], &["n", "text"][..]] {
                let rows = projected_rows(&path, query, fields, false);
                let columns = projected_rows(&path, query, fields, true);
                assert_eq!(count as usize, rows.len(), "{structured}: {query}");
                assert_eq!(rows, columns, "{structured}: {query}");
                assert_eq!(rows, projected_rows_version(&path, query, fields, 4));
            }
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn timestamp_source_scalars_and_search_surfaces_agree() {
    let path = fixture("sfa-v0.5.0-timestamps-cpp.bin");
    let epochs = [1422752523004000000_i64, 1700000000123000000,
                  1422752524004000000, 1700000001123000000];
    let raw = [r#""2015-02-01T01:02:03.004""#, "1700000000123",
               r#""2015-02-01T01:02:04.004""#, "1700000001123"];
    for (query, ids) in [
        ("*", vec![1, 2, 3, 4]),
        ("ts >= 1700000000123000000 AND ts <= 1700000001123000000", vec![2, 4]),
        ("NOT (ts >= 1700000000123000000 AND ts <= 1700000001123000000)", vec![1, 3]),
        ("ts:1700000000123000000", vec![2]),
        ("ts < 1422752523004000001", vec![1]),
        ("ts > 1700000001123000000", vec![]),
    ] {
        let mut docs = CallbackState::default();
        let (status, count, error) = search(&open_archive(&path), &compile_query(query, 0), &mut docs);
        assert_eq!(CLP_S_STATUS_OK, status, "{query}: {error}");
        assert_eq!(ids.len() as u64, count, "{query}");
        for version in [2, 3, 4] {
            let rows = projected_rows_version(&path, query, &["ts", "kind"], version);
            assert_eq!(ids, rows.iter().map(|r| r[1].1).collect::<Vec<_>>(), "{query}");
            for row in rows {
                let i = row[1].1 as usize - 1;
                assert_eq!(CLP_S_V2_VALUE_TIMESTAMP, row[0].0);
                assert_eq!(epochs[i], row[0].1);
                assert_eq!(raw[i].as_bytes(), row[0].3);
            }
        }
    }
}

#[test]
fn timestamp_integer_unit_metadata_is_conservative() {
    use clp_s::writer::{FieldRef, OpenArchive, RecordRef, TimestampRef, ValueRef, WriterOptions};
    for (patterns, expected) in [
        (vec![r"\E"], 1_000_000_000), (vec![r"\L"], 1_000_000),
        (vec![r"\C"], 1_000), (vec![r"\N"], 1),
        (vec![r"\L", r"\N"], 0), (vec![r#""\L""#], 0),
        (vec![r#""\Y-\m-\dT\H:\M:\S""#], 0), (vec![], 0),
    ] {
        let mut writer = OpenArchive::new(std::io::Cursor::new(Vec::new()), WriterOptions::default());
        for pattern in patterns {
            let raw = clp_s::timestamp::TimestampPattern::compile(pattern, Default::default())
                .unwrap().format_epoch_nanoseconds(1_000_000_000).unwrap();
            writer.append_record(RecordRef::new(&[FieldRef::new(b"ts",
                ValueRef::Timestamp(TimestampRef::new(1_000_000_000, &raw, pattern, "ts")))]))
                .unwrap();
        }
        let path = std::env::temp_dir().join(format!("clp-timestamp-unit-{}.clps", std::process::id()));
        std::fs::write(&path, writer.finish().unwrap().into_inner().into_inner()).unwrap();
        let archive = open_archive(&path);
        let mut list = ptr::null_mut();
        let mut entries = ptr::null();
        let mut count = 0;
        let mut message = [0; 512];
        let mut error = error_buffer(&mut message);
        // SAFETY: The archive and outputs remain live; free the returned owner once.
        unsafe {
            assert_eq!(CLP_S_STATUS_OK, clp_s_v2_archive_field_types(archive.0,
                &raw mut list, &raw mut entries, &raw mut count, &raw mut error));
            assert_eq!(expected, clp_s_v3_archive_timestamp_integer_scale(list));
            assert_eq!(0, clp_s_v3_archive_timestamp_integer_scale(ptr::null()));
            clp_s_v2_archive_field_type_list_free(list);
        }
        std::fs::remove_file(path).unwrap();
    }
}
