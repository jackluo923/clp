use super::*;
use clp_s::archive::SingleFileArchiveReader;
use clp_s::writer::{FieldRef, OpenArchive, RecordRef, TimestampRef, ValueRef, WriterOptions};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

fn node(op: u32, typ: u32, left: u32, right: u32) -> Node {
    Node {
        op,
        typ,
        left,
        right,
        bits: [0; 2],
    }
}
fn constant(v: i128) -> Node {
    Node {
        bits: clp_s::numeric_summary::words(v),
        ..node(1, 0, 0, 0)
    }
}
fn fields(ts_type: u32) -> [Field; 2] {
    [
        Field {
            path: b"ts".as_ptr(),
            len: 2,
            typ: ts_type,
        },
        Field {
            path: b"a".as_ptr(),
            len: 1,
            typ: 2,
        },
    ]
}
fn program(lo: i128, hi: i128, ts_type: u32) -> Vec<Node> {
    vec![
        node(0, ts_type, 0, 0),
        node(0, 2, 1, 0),
        constant(lo),
        constant(hi),
        node(12, 7, 0, 2),
        node(9, 7, 0, 3),
        node(13, 7, 4, 5),
    ]
}
fn catalog(bytes: &[u8]) -> clp_s::archive::ArchiveCatalog {
    SingleFileArchiveReader::open(Cursor::new(bytes.to_vec()))
        .unwrap()
        .read_catalog(ArchiveCatalogLimits::default())
        .unwrap()
}

fn run(
    bytes: &[u8],
    fields: &[Field],
    nodes: &[Node],
    outputs: &[u32],
    predicate: i32,
    force: u32,
) -> Vec<ClpNumericSummary> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "clp-window-{}-{}.clps",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).unwrap();
    let path_str = path.to_str().unwrap();
    let mut out = vec![ClpNumericSummary::default(); outputs.len()];
    let mut message = [0; 1024];
    let mut error = ClpSErrorBuffer {
        data: message.as_mut_ptr(),
        capacity: message.len(),
        required: 0,
    };
    // SAFETY: The input/output buffers remain live throughout this synchronous call.
    let status = unsafe {
        clp_s_v6_numeric_aggregate(
            path_str.as_ptr(),
            path_str.len(),
            fields.as_ptr(),
            fields.len(),
            nodes.as_ptr(),
            nodes.len(),
            outputs.as_ptr(),
            vec![1; outputs.len()].as_ptr(),
            outputs.len(),
            predicate,
            0,
            0,
            force,
            None,
            std::ptr::null_mut(),
            out.as_mut_ptr(),
            &mut error,
        )
    };
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        CLP_S_STATUS_OK,
        status,
        "{}",
        String::from_utf8_lossy(&message)
    );
    out
}

fn tables(separate: bool) -> Vec<u8> {
    let mut writer = OpenArchive::new(
        Cursor::new(Vec::new()),
        WriterOptions::default().with_separate_columns_min_size(u64::from(separate)),
    );
    for (group, timestamps) in [
        (b"covered".as_slice(), [1000, 1100]),
        (b"boundary", [1200, 1400]),
        (b"excluded", [1500, 1600]),
    ] {
        for ts in timestamps {
            let value = match ts {
                1000 => 10,
                1100 => 20,
                1200 => 30,
                1400 => 40,
                1500 => 50,
                _ => 60,
            };
            let raw = ts.to_string();
            writer
                .append_record(RecordRef::new(&[
                    FieldRef::new(
                        b"ts",
                        ValueRef::Timestamp(TimestampRef::new(ts * 1_000_000, &raw, r"\L", "ts")),
                    ),
                    FieldRef::new(b"a", ValueRef::I64(value)),
                    FieldRef::new(group, ValueRef::I64(1)),
                ]))
                .unwrap();
        }
    }
    writer
        .append_record(RecordRef::new(&[
            FieldRef::new(b"ts", ValueRef::Null),
            FieldRef::new(b"a", ValueRef::I64(100)),
        ]))
        .unwrap();
    writer
        .append_record(RecordRef::new(&[FieldRef::new(b"a", ValueRef::I64(200))]))
        .unwrap();
    writer.finish().unwrap().into_inner().into_inner()
}

#[test]
fn covered_tables_and_scanned_boundaries_contribute_once_in_both_layouts() {
    for separate in [false, true] {
        let bytes = tables(separate);
        let cat = catalog(&bytes);
        let ctx = SummaryContext::from_catalog(&cat).unwrap().unwrap();
        let fields = fields(0);
        let paths = vec!["ts".to_owned(), "a".to_owned()];
        for (lo, hi, count, sum, covered, rejected, scanned) in [
            (1000, 1300, 3, 60, 1, 3, 1),
            (1000, 1601, 6, 210, 3, 2, 0),
            (1101, 1401, 2, 70, 1, 4, 0),
            (2000, 3000, 0, 0, 0, 5, 0),
        ] {
            let nodes = program(lo, hi, 0);
            let partial = partials(&ctx, &paths, &fields, &nodes, &[1], &[1], 6)
                .unwrap()
                .unwrap();
            assert_eq!(partial.covered_tables, covered);
            assert_eq!(partial.rejected_tables, rejected);
            assert_eq!(partial.scan_tables.iter().filter(|v| **v).count(), scanned);
            let actual = run(&bytes, &fields, &nodes, &[1], 6, 0)[0];
            let reference = run(&bytes, &fields, &nodes, &[1], 6, 1)[0];
            assert_eq!((actual.count, integer(actual.sum)), (count, sum));
            assert_eq!(
                (actual.count, actual.sum, actual.min, actual.max),
                (reference.count, reference.sum, reference.min, reference.max)
            );
            assert_eq!(actual.from_metadata != 0, scanned == 0);
        }
    }
}

#[test]
fn source_unit_extrema_certify_negative_and_subunit_epochs() {
    let epochs = [
        -2_000_000_001,
        -1_000_000_001,
        -1,
        0,
        1,
        1_000_000_001,
        2_000_000_001,
    ];
    for (pattern, scale) in [
        (r"\E", 1_000_000_000),
        (r"\L", 1_000_000),
        (r"\C", 1_000),
        (r"\N", 1),
    ] {
        let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for (i, epoch) in epochs.iter().enumerate() {
            let raw = (epoch / scale).to_string();
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
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        for ts_type in [0, 4] {
            for (lo, hi) in [(-3_000_000_000, 3_000_000_000), (0, 3_000_000_000), (-1, 1)] {
                let nodes = program(lo, hi, ts_type);
                let actual = run(&bytes, &fields(ts_type), &nodes, &[1], 6, 0)[0];
                let reference = run(&bytes, &fields(ts_type), &nodes, &[1], 6, 1)[0];
                let (gate_lo, gate_hi) = bounds(ts_type).unwrap();
                let expected: Vec<i128> = epochs
                    .iter()
                    .enumerate()
                    .filter_map(|(i, epoch)| {
                        let value = i128::from(epoch / scale);
                        (value >= lo && value < hi && value >= gate_lo && value <= gate_hi)
                            .then_some(i as i128 + 1)
                    })
                    .collect();
                assert_eq!(
                    (actual.count, integer(actual.sum)),
                    (expected.len() as u64, expected.iter().sum())
                );
                assert_eq!((actual.count, actual.sum), (reference.count, reference.sum));
                if ts_type == 0 && lo == -3_000_000_000 {
                    assert_eq!(actual.from_metadata, 1);
                }
            }
        }
    }
}

#[test]
fn uncertain_summaries_and_extra_conditions_never_certify_coverage() {
    let bytes = tables(false);
    let cat = catalog(&bytes);
    let mut ctx = SummaryContext::from_catalog(&cat).unwrap().unwrap();
    let paths = vec!["ts".to_owned(), "a".to_owned()];
    let fields = fields(0);
    let nodes = program(0, 2000, 0);
    let ts = columnar::resolve_descriptor_path(cat.schema_tree(), "ts");
    for column in &mut ctx.summaries.columns {
        if ts.contains(&column.node) {
            column.nonnegative.count += 1;
        }
    }
    let invalid = partials(&ctx, &paths, &fields, &nodes, &[1], &[1], 6)
        .unwrap()
        .unwrap();
    assert_eq!(invalid.covered_tables, 0);
    assert_eq!(invalid.scan_tables.iter().filter(|v| **v).count(), 3);
    let ctx = SummaryContext::from_catalog(&cat).unwrap().unwrap();
    let mut extra = nodes.clone();
    extra.extend([node(11, 7, 1, 2), node(13, 7, 6, 7)]); // AND a > 0
    assert_eq!(
        partials(&ctx, &paths, &fields, &extra, &[1], &[1], 8)
            .unwrap()
            .unwrap()
            .covered_tables,
        0
    );
    extra[8].op = 14; // OR must not turn necessary bounds into all-row proof.
    assert!(
        partials(&ctx, &paths, &fields, &extra, &[1], &[1], 8)
            .unwrap()
            .is_none()
    );
    extra = nodes.clone();
    extra.push(node(5, 2, 1, 1));
    assert!(
        partials(&ctx, &paths, &fields, &extra, &[7], &[1], 6)
            .unwrap()
            .is_none()
    );
    extra[7] = node(17, 2, 1, 4);
    assert!(
        partials(&ctx, &paths, &fields, &extra, &[7], &[1], 6)
            .unwrap()
            .is_none()
    );
    extra = nodes.clone();
    extra.push(node(2, 4, 1, 0));
    assert!(
        partials(&ctx, &paths, &fields, &extra, &[7], &[1], 6)
            .unwrap()
            .is_none()
    );
}

#[test]
fn mixed_integer_units_use_source_extrema_and_string_patterns_require_scanning() {
    for second_pattern in [r"\N", r#""\Y-\m-\dT\H:\M:\S""#] {
        let mut writer = OpenArchive::new(Cursor::new(Vec::new()), WriterOptions::default());
        for (i, pattern) in [r"\L", second_pattern].iter().enumerate() {
            let raw = clp_s::timestamp::TimestampPattern::compile(pattern, Default::default())
                .unwrap()
                .format_epoch_nanoseconds(1_000_000_000)
                .unwrap();
            writer
                .append_record(RecordRef::new(&[
                    FieldRef::new(
                        b"ts",
                        ValueRef::Timestamp(TimestampRef::new(1_000_000_000, &raw, pattern, "ts")),
                    ),
                    FieldRef::new(b"a", ValueRef::I64(i as i64 + 1)),
                ]))
                .unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner().into_inner();
        for hi in [2000, 2_000_000_000] {
            let nodes = program(0, hi, 0);
            let actual = run(&bytes, &fields(0), &nodes, &[1], 6, 0)[0];
            let forced = run(&bytes, &fields(0), &nodes, &[1], 6, 1)[0];
            let all = second_pattern == r"\N" && hi == 2_000_000_000;
            assert_eq!(
                (actual.count, integer(actual.sum)),
                if all { (2, 3) } else { (1, 1) }
            );
            assert_eq!((actual.count, actual.sum), (forced.count, forced.sum));
            assert_eq!(actual.from_metadata != 0, all);
        }
    }
}
