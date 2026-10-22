//! Regression coverage for metadata reuse and necessary timestamp prefilters.
use super::*;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::writer::{FieldRef, OpenArchive, RecordRef, TimestampRef, ValueRef, WriterOptions};
use std::{ffi::c_void, io::Cursor, path::PathBuf};

fn catalog(bytes: &[u8]) -> clp_s::archive::ArchiveCatalog {
    SingleFileArchiveReader::open(Cursor::new(bytes.to_vec()))
        .unwrap()
        .read_catalog(ArchiveCatalogLimits::default())
        .unwrap()
}

fn field(path: &str, typ: u32) -> Field {
    Field {
        path: path.as_ptr(),
        len: path.len(),
        typ,
    }
}
fn node(op: u32, typ: u32, left: u32, right: u32) -> Node {
    Node {
        op,
        typ,
        left,
        right,
        bits: [0; 2],
    }
}
fn constant(value: i128) -> Node {
    Node {
        bits: [value as u64, (value >> 64) as u64],
        ..node(1, 0, 0, 0)
    }
}
fn timestamp_bytes(patterns: &[&str], epochs: &[i64], mixed: bool) -> Vec<u8> {
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
    for (i, epoch) in epochs.iter().enumerate() {
        let pattern = patterns[i % patterns.len()];
        let raw = clp_s::timestamp::TimestampPattern::compile(pattern, Default::default())
            .unwrap()
            .format_epoch_nanoseconds(*epoch)
            .unwrap();
        writer
            .append_record(RecordRef::new(&[
                FieldRef::new(
                    b"ts",
                    ValueRef::Timestamp(TimestampRef::new(*epoch, &raw, pattern, "ts")),
                ),
                FieldRef::new(b"a", ValueRef::I64(i as i64 + 1)),
            ]))
            .unwrap();
    }
    if mixed {
        writer
            .append_record(RecordRef::new(&[FieldRef::new(b"ts", ValueRef::I64(42))]))
            .unwrap();
    }
    writer.finish().unwrap().into_inner().into_inner()
}

#[test]
fn timestamp_prefilters_match_source_integers_in_every_unit() {
    // Includes sub-unit epochs, truncation around zero, and physical domain edges.
    let epochs = [
        i64::MIN,
        -2_000_000_001,
        -1_000_000_001,
        -1,
        0,
        1,
        1_000_000_001,
        2_000_000_001,
        i64::MAX,
    ];
    let paths = vec!["ts".to_owned(), "a".to_owned()];
    let fields = [field("ts", 0), field("a", 2)];
    for (pattern, scale) in [
        (r"\E", 1_000_000_000),
        (r"\L", 1_000_000),
        (r"\C", 1_000),
        (r"\N", 1),
    ] {
        let bytes = timestamp_bytes(&[pattern], &epochs, false);
        let cat = catalog(&bytes);
        for bound in [i64::MIN, -2, -1, 0, 1, 2, i64::MAX] {
            for op in [7, 9, 10, 11, 12] {
                for reverse in [false, true] {
                    // Test widening casts on both operands as emitted by SQL binding.
                    let nodes = [
                        node(0, 0, 0, 0),
                        constant(bound.into()),
                        node(2, 2, 0, 0),
                        node(2, 2, 1, 0),
                        node(
                            op,
                            7,
                            if reverse { 3 } else { 2 },
                            if reverse { 2 } else { 3 },
                        ),
                    ];
                    let filters = timestamp_prefilters(&cat, &paths, &fields, &nodes, 4);
                    assert_eq!(filters.len(), 1, "{pattern} {op} {bound}");
                    let parsed = parse_kql(&filters[0], KqlLimits::default()).unwrap();
                    let reader = Box::new(
                        SingleFileArchiveReader::open(Cursor::new(bytes.clone())).unwrap(),
                    );
                    let mut scan = columnar::ProjectedScanner::open_with_catalog(
                        reader,
                        catalog(&bytes),
                        parsed,
                        &ArchiveSearchOptions::default(),
                        &paths,
                    )
                    .unwrap();
                    let mut actual = Vec::new();
                    while let Some((columns, text, rows)) =
                        scan.next_columns_with_constants(true).unwrap()
                    {
                        for row in 0..rows {
                            // SAFETY: The borrowed column/text buffers are live for this batch.
                            let Some(Scalar::Int(id)) =
                                (unsafe { value(&columns[1], row, text, 2) }).unwrap()
                            else {
                                panic!("missing identity")
                            };
                            actual.push(id);
                        }
                    }
                    let expected: Vec<_> = epochs
                        .iter()
                        .enumerate()
                        .filter_map(|(i, epoch)| {
                            let yes = evaluate(
                                4,
                                &nodes,
                                &[Some(Scalar::Int((epoch / scale).into()))],
                                &mut [None; 5],
                            )
                            .unwrap();
                            matches!(yes, Some(Scalar::Bool(true))).then_some(i as i128 + 1)
                        })
                        .collect();
                    actual.sort();
                    assert_eq!(
                        actual, expected,
                        "pattern={pattern} op={op} bound={bound} reverse={reverse}"
                    );
                }
            }
        }
    }
}

#[test]
fn only_certified_top_level_nonthrowing_time_comparisons_are_prefiltered() {
    let paths = vec!["ts".to_owned()];
    let fields = [field("ts", 0)];
    let cat = catalog(&timestamp_bytes(&[r"\L"], &[0, 1_000_000], false));
    let base = vec![node(0, 0, 0, 0), constant(0), node(12, 7, 0, 1)];
    assert_eq!(
        timestamp_prefilters(&cat, &paths, &fields, &base, 2).len(),
        1
    );
    assert!(timestamp_prefilters(&cat, &paths, &fields, &base, -1).is_empty());
    for (op, expected) in [(13, 2), (14, 0)] {
        let mut nodes = base.clone();
        nodes.push(node(op, 7, 2, 2));
        assert_eq!(
            timestamp_prefilters(&cat, &paths, &fields, &nodes, 3).len(),
            expected
        );
    }
    let mut nodes = base.clone();
    nodes[2].op = 8; // != is not an interval.
    assert!(timestamp_prefilters(&cat, &paths, &fields, &nodes, 2).is_empty());
    nodes = base.clone();
    nodes[1].right = 1; // SQL NULL constant.
    assert!(timestamp_prefilters(&cat, &paths, &fields, &nodes, 2).is_empty());
    nodes = vec![
        node(0, 0, 0, 0),
        node(2, 3, 0, 0),
        node(1, 3, 0, 0),
        node(12, 7, 1, 2),
    ];
    assert!(timestamp_prefilters(&cat, &paths, &fields, &nodes, 3).is_empty());
    // A second WHERE operand that may throw must prevent early row elimination.
    nodes = base.clone();
    nodes.extend([node(5, 0, 0, 0), node(12, 7, 3, 1), node(13, 7, 2, 4)]);
    assert!(timestamp_prefilters(&cat, &paths, &fields, &nodes, 5).is_empty());
    nodes = base.clone();
    nodes.extend([node(2, 4, 0, 0), node(12, 7, 3, 1), node(13, 7, 2, 4)]);
    assert!(timestamp_prefilters(&cat, &paths, &fields, &nodes, 5).is_empty());
    for patterns in [
        vec![r"\L", r"\N"],
        vec![r#""\L""#],
        vec![r#""\Y-\m-\dT\H:\M:\S""#],
    ] {
        let other = catalog(&timestamp_bytes(&patterns, &[0, 1_000_000], false));
        assert!(timestamp_prefilters(&other, &paths, &fields, &base, 2).is_empty());
    }
    let mixed = catalog(&timestamp_bytes(&[r"\L"], &[0], true));
    assert!(timestamp_prefilters(&mixed, &paths, &fields, &base, 2).is_empty());
}

struct ReplaceOnCheck {
    calls: usize,
    path: PathBuf,
    replacement: PathBuf,
    error: Option<std::io::Error>,
}
unsafe extern "C" fn replace_on_second_check(context: *mut c_void) -> u32 {
    // SAFETY: The caller retains this state for the duration of the synchronous FFI call.
    let state = unsafe { &mut *context.cast::<ReplaceOnCheck>() };
    state.calls += 1;
    if state.calls == 2 {
        state.error = std::fs::rename(&state.replacement, &state.path).err();
    }
    u32::from(state.error.is_some())
}
fn numeric_bytes(rows: &[(i64, i64)]) -> Vec<u8> {
    let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
    for &(a, b) in rows {
        writer
            .append_record(RecordRef::new(&[
                FieldRef::new(b"a", ValueRef::I64(a)),
                FieldRef::new(b"b", ValueRef::I64(b)),
            ]))
            .unwrap();
    }
    writer.finish().unwrap().into_inner().into_inner()
}

#[test]
fn numeric_summary_and_fallback_share_the_opened_file() {
    for mode in 0..3 {
        let path = std::env::temp_dir().join(format!(
            "clp-numeric-reader-{}-{mode}.clps",
            std::process::id()
        ));
        let replacement = path.with_extension("replacement");
        std::fs::write(&path, numeric_bytes(&[(1, 4), (2, 8)])).unwrap();
        std::fs::write(&replacement, numeric_bytes(&[(100, 200)])).unwrap();
        let name = path.to_str().unwrap().to_owned();
        let mut state = ReplaceOnCheck {
            calls: 0,
            path: path.clone(),
            replacement,
            error: None,
        };
        let context = (&mut state as *mut ReplaceOnCheck).cast::<c_void>();
        let mut output = [ClpNumericSummary::default(); 2];
        let mut message = [0u8; 1024];
        let mut error = ClpSErrorBuffer {
            data: message.as_mut_ptr(),
            capacity: message.len(),
            required: 0,
        };
        // SAFETY: All buffers and callback state remain live for each synchronous call.
        let status = unsafe {
            if mode < 2 {
                clp_s_v5_numeric_summary(
                    name.as_ptr(),
                    name.len(),
                    b"a".as_ptr(),
                    1,
                    2,
                    0,
                    0,
                    mode,
                    Some(replace_on_second_check),
                    context,
                    output.as_mut_ptr(),
                    &mut error,
                )
            } else {
                let fields = [field("a", 2), field("b", 2)];
                let nodes = [node(0, 2, 0, 0), node(0, 2, 1, 0), node(5, 2, 0, 1)];
                clp_s_v6_numeric_aggregate(
                    name.as_ptr(),
                    name.len(),
                    fields.as_ptr(),
                    fields.len(),
                    nodes.as_ptr(),
                    nodes.len(),
                    [0, 2].as_ptr(),
                    [1, 1].as_ptr(),
                    2,
                    -1,
                    0,
                    0,
                    0,
                    Some(replace_on_second_check),
                    context,
                    output.as_mut_ptr(),
                    &mut error,
                )
            }
        };
        std::fs::remove_file(&path).unwrap();
        assert!(state.error.is_none(), "{:?}", state.error);
        assert!(state.calls >= 2);
        assert_eq!(
            status,
            CLP_S_STATUS_OK,
            "{}",
            String::from_utf8_lossy(&message)
        );
        assert_eq!(output[0].count, 2);
        assert_eq!(integer(output[0].sum), 3);
        assert_eq!(output[0].from_metadata, u64::from(mode != 1));
        if mode == 2 {
            assert_eq!(output[1].count, 2);
            assert_eq!(integer(output[1].sum), 20);
            assert_eq!(output[1].from_metadata, 0);
        }
    }
}
