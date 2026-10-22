//! Batched numeric summaries and fused filtered expression aggregation.
use super::*;

#[repr(C)]
pub struct Field {
    pub path: *const u8,
    pub len: usize,
    pub typ: u32,
}
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Node {
    pub op: u32,
    pub typ: u32,
    pub left: u32,
    pub right: u32,
    pub bits: [u64; 2],
}
#[derive(Clone, Copy, Debug)]
enum Scalar {
    Int(i128),
    Float(f64),
    Bool(bool),
}
type Nullable = Option<Scalar>;
fn range(typ: u32) -> Option<(i128, i128)> {
    if typ == 2 {
        Some((i128::MIN, i128::MAX))
    } else {
        bounds(typ)
    }
}
fn cast(v: Nullable, typ: u32) -> Result<Nullable, ApiError> {
    match v {
        None => Ok(None),
        Some(Scalar::Int(v)) if typ == 3 => Ok(Some(Scalar::Float(v as f64))),
        Some(Scalar::Int(v)) => {
            if range(typ).is_some_and(|(lo, hi)| lo <= v && v <= hi) {
                Ok(Some(Scalar::Int(v)))
            } else {
                Err(failure("integer expression overflow"))
            }
        }
        Some(Scalar::Float(v)) if typ == 3 => Ok(Some(Scalar::Float(v))),
        Some(Scalar::Bool(v)) if typ == 7 => Ok(Some(Scalar::Bool(v))),
        _ => Err(failure("unsupported expression cast")),
    }
}
fn compare(a: Scalar, b: Scalar) -> Result<std::cmp::Ordering, ApiError> {
    use std::cmp::Ordering;
    Ok(match (a, b) {
        (Scalar::Int(a), Scalar::Int(b)) => a.cmp(&b),
        (Scalar::Bool(a), Scalar::Bool(b)) => a.cmp(&b),
        (Scalar::Float(a), Scalar::Float(b)) => {
            if a.is_nan() {
                if b.is_nan() {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            } else if b.is_nan() {
                Ordering::Less
            } else {
                a.partial_cmp(&b).unwrap()
            }
        }
        _ => return Err(failure("comparison type mismatch")),
    })
}
fn evaluate(
    index: usize,
    nodes: &[Node],
    values: &[Nullable],
    cache: &mut [Option<Nullable>],
) -> Result<Nullable, ApiError> {
    if let Some(v) = cache[index] {
        return Ok(v);
    }
    let n = nodes[index];
    let result =
        match n.op {
            0 => values[n.left as usize],
            1 => {
                if n.right != 0 {
                    None
                } else {
                    Some(match n.typ {
                        3 => Scalar::Float(f64::from_bits(n.bits[0])),
                        7 => Scalar::Bool(n.bits[0] != 0),
                        _ => Scalar::Int(integer(n.bits)),
                    })
                }
            }
            _ => {
                let a = evaluate(n.left as usize, nodes, values, cache)?;
                match n.op {
                    2 => cast(a, n.typ)?,
                    15 => Some(Scalar::Bool(a.is_none())),
                    16 => Some(Scalar::Bool(a.is_some())),
                    17 => {
                        // SQL aggregate arguments are evaluated before FILTER.
                        let predicate = evaluate(n.right as usize, nodes, values, cache)?;
                        if matches!(predicate, Some(Scalar::Bool(true))) {
                            a
                        } else {
                            None
                        }
                    }
                    13 | 14 => {
                        let b = evaluate(n.right as usize, nodes, values, cache)?;
                        match (n.op, a, b) {
                            (13, Some(Scalar::Bool(false)), _)
                            | (13, _, Some(Scalar::Bool(false))) => Some(Scalar::Bool(false)),
                            (14, Some(Scalar::Bool(true)), _)
                            | (14, _, Some(Scalar::Bool(true))) => Some(Scalar::Bool(true)),
                            (_, None, _) | (_, _, None) => None,
                            (13, Some(Scalar::Bool(a)), Some(Scalar::Bool(b))) => {
                                Some(Scalar::Bool(a && b))
                            }
                            (14, Some(Scalar::Bool(a)), Some(Scalar::Bool(b))) => {
                                Some(Scalar::Bool(a || b))
                            }
                            _ => return Err(failure("non-boolean predicate")),
                        }
                    }
                    _ => {
                        let b = evaluate(n.right as usize, nodes, values, cache)?;
                        match (a, b) {
                            (Some(a), Some(b)) if (7..=12).contains(&n.op) => {
                                let c = compare(a, b)?;
                                Some(Scalar::Bool(match n.op {
                                    7 => c.is_eq(),
                                    8 => !c.is_eq(),
                                    9 => c.is_lt(),
                                    10 => !c.is_gt(),
                                    11 => c.is_gt(),
                                    _ => !c.is_lt(),
                                }))
                            }
                            (Some(Scalar::Int(a)), Some(Scalar::Int(b))) => {
                                let v = match n.op {
                                    3 => a.checked_add(b),
                                    4 => a.checked_sub(b),
                                    5 => a.checked_mul(b),
                                    _ => None,
                                }
                                .ok_or_else(|| failure("integer expression overflow"))?;
                                cast(Some(Scalar::Int(v)), n.typ)?
                            }
                            (Some(Scalar::Float(a)), Some(Scalar::Float(b))) => {
                                Some(Scalar::Float(match n.op {
                                    3 => a + b,
                                    4 => a - b,
                                    5 => a * b,
                                    6 => a / b,
                                    _ => return Err(failure("unsupported float operation")),
                                }))
                            }
                            (None, _) | (_, None) => None,
                            _ => return Err(failure("expression type mismatch")),
                        }
                    }
                }
            }
        };
    cache[index] = Some(result);
    Ok(result)
}
unsafe fn value(
    column: &columnar::ColumnView,
    row: usize,
    text: &[u8],
    typ: u32,
) -> Result<Nullable, ApiError> {
    unsafe {
        let mut kind = column.kind;
        let (int, real);
        match kind {
            columnar::kind::INTEGER
            | columnar::kind::FLOAT
            | columnar::kind::CONSTANT_INTEGER
            | columnar::kind::CONSTANT_FLOAT => {
                let constant = matches!(
                    kind,
                    columnar::kind::CONSTANT_INTEGER | columnar::kind::CONSTANT_FLOAT
                );
                let bits = column
                    .data
                    .add(if constant { 0 } else { row * 8 })
                    .cast::<u64>()
                    .read_unaligned();
                if kind == columnar::kind::CONSTANT_INTEGER {
                    kind = columnar::kind::INTEGER;
                }
                if kind == columnar::kind::CONSTANT_FLOAT {
                    kind = columnar::kind::FLOAT;
                }
                int = bits as i64;
                real = f64::from_bits(bits);
            }
            columnar::kind::ABSENT => return Ok(None),
            _ => {
                let cell = column
                    .data
                    .cast::<columnar::Cell>()
                    .add(row)
                    .read_unaligned();
                kind = cell.kind;
                int = cell.integer;
                real = cell.real;
                if kind == columnar::kind::TIMESTAMP {
                    let end = cell
                        .text_offset
                        .checked_add(cell.text_length)
                        .ok_or_else(|| failure("timestamp scalar bounds"))?;
                    let raw = str::from_utf8(
                        text.get(cell.text_offset..end)
                            .ok_or_else(|| failure("timestamp scalar bounds"))?,
                    )
                    .map_err(failure)?;
                    // Timestamp index units must never leak into SQL numeric expressions.
                    if let Ok(v) = raw.parse::<i128>() {
                        return Ok(bounds(typ)
                            .filter(|(lo, hi)| *lo <= v && v <= *hi)
                            .map(|_| Scalar::Int(v)));
                    }
                    if typ == 3 && !raw.starts_with('"') {
                        return Ok(raw.parse::<f64>().ok().map(Scalar::Float));
                    }
                    return Ok(None);
                }
            }
        }
        if typ == 3 {
            return Ok((kind == columnar::kind::FLOAT).then_some(Scalar::Float(real)));
        }
        let v = match kind {
            columnar::kind::INTEGER => i128::from(int),
            columnar::kind::UNSIGNED => i128::from(int as u64),
            _ => return Ok(None),
        };
        Ok(bounds(typ)
            .filter(|(lo, hi)| *lo <= v && v <= *hi)
            .map(|_| Scalar::Int(v)))
    }
}
fn summary_field(root: usize, nodes: &[Node]) -> Option<usize> {
    let n = nodes[root];
    if n.op == 0 {
        Some(n.left as usize)
    } else if n.op == 2 && n.typ != 3 {
        summary_field(n.left as usize, nodes)
    } else {
        None
    }
}

// Conservative SQL three-valued evaluation using schema-level eligibility.
// Unknown fields may be either null or non-null; impossible typed fields are null.
fn null_only(root: usize, nodes: &[Node], eligible: &[bool]) -> bool {
    let n = nodes[root];
    match n.op {
        0 => !eligible[n.left as usize],
        1 => n.right != 0,
        2 => null_only(n.left as usize, nodes, eligible),
        3..=6 => {
            null_only(n.left as usize, nodes, eligible)
                || null_only(n.right as usize, nodes, eligible)
        }
        _ => false,
    }
}
fn truths(root: usize, nodes: &[Node], eligible: &[bool]) -> u8 {
    // bits: true=1, false=2, null=4.
    let n = nodes[root];
    match n.op {
        1 => {
            if n.right != 0 {
                4
            } else if n.bits[0] != 0 {
                1
            } else {
                2
            }
        }
        7..=12 => {
            if null_only(n.left as usize, nodes, eligible)
                || null_only(n.right as usize, nodes, eligible)
            {
                4
            } else {
                7
            }
        }
        15 => {
            if null_only(n.left as usize, nodes, eligible) {
                1
            } else {
                3
            }
        }
        16 => {
            if null_only(n.left as usize, nodes, eligible) {
                2
            } else {
                3
            }
        }
        13 | 14 => {
            let a = truths(n.left as usize, nodes, eligible);
            let b = truths(n.right as usize, nodes, eligible);
            let mut result = 0;
            for x in [1, 2, 4] {
                for y in [1, 2, 4] {
                    if a & x == 0 || b & y == 0 {
                        continue;
                    }
                    result |= if n.op == 13 {
                        if x == 2 || y == 2 {
                            2
                        } else if x == 4 || y == 4 {
                            4
                        } else {
                            1
                        }
                    } else {
                        if x == 1 || y == 1 {
                            1
                        } else if x == 4 || y == 4 {
                            4
                        } else {
                            2
                        }
                    };
                }
            }
            result
        }
        _ => 7,
    }
}
fn schema_may_match(
    ctx: &SummaryContext,
    paths: &[String],
    fields: &[Field],
    nodes: &[Node],
    root: usize,
) -> bool {
    let eligible: Vec<_> = paths
        .iter()
        .zip(fields)
        .map(|(path, field)| {
            let parts: Vec<_> = path.split('.').collect();
            columnar::resolve_path(ctx.catalog.schema_tree(), &parts)
                .iter()
                .any(|id| {
                    let Some(node) = ctx.catalog.schema_tree().get(*id as usize) else {
                        return true;
                    };
                    match node.node_type() {
                        NodeType::Timestamp => true,
                        NodeType::Float | NodeType::FormattedFloat | NodeType::DictionaryFloat => {
                            field.typ == 3
                        }
                        NodeType::Integer | NodeType::MixedInteger => field.typ != 3,
                        _ => false,
                    }
                })
        })
        .collect();
    truths(root, nodes, &eligible) & 1 != 0
}

/// All pointer ranges must be valid for this synchronous call. Output has output_len entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clp_s_v6_numeric_aggregate(
    path: *const u8,
    path_len: usize,
    fields: *const Field,
    field_len: usize,
    nodes: *const Node,
    node_len: usize,
    outputs: *const u32,
    sum_required: *const u8,
    output_len: usize,
    predicate: i32,
    max_compressed: u64,
    max_decompressed: u64,
    force_scan: u32,
    cancel: Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> u32>,
    context: *mut std::ffi::c_void,
    out: *mut ClpNumericSummary,
    error: *mut ClpSErrorBuffer,
) -> ClpSStatus {
    unsafe {
        ffi_entry(error, || {
            if fields.is_null()
                || nodes.is_null()
                || sum_required.is_null()
                || outputs.is_null()
                || out.is_null()
                || field_len == 0
                || node_len == 0
                || output_len == 0
                || node_len > 4096
            {
                return Err(ApiError::InvalidArgument(
                    "invalid numeric program buffers".into(),
                ));
            }
            let fields = std::slice::from_raw_parts(fields, field_len);
            let nodes = std::slice::from_raw_parts(nodes, node_len);
            let outputs = std::slice::from_raw_parts(outputs, output_len);
            let sum_required = std::slice::from_raw_parts(sum_required, output_len);
            let out = std::slice::from_raw_parts_mut(out, output_len);
            out.fill(ClpNumericSummary::default());
            let mut depths = vec![0usize; node_len];
            for (i, n) in nodes.iter().enumerate() {
                if n.op > 17
                    || (n.typ != 3 && n.typ != 7 && range(n.typ).is_none())
                    || (n.op == 0 && n.left as usize >= field_len)
                    || (n.op > 1
                        && (n.left as usize >= i
                            || (((3..=14).contains(&n.op) || n.op == 17) && n.right as usize >= i)))
                {
                    return Err(ApiError::InvalidArgument(
                        "invalid numeric program node".into(),
                    ));
                }
                depths[i] = if n.op <= 1 {
                    1
                } else {
                    1 + depths[n.left as usize].max(if (3..=14).contains(&n.op) || n.op == 17 {
                        depths[n.right as usize]
                    } else {
                        0
                    })
                };
                if depths[i] > 64 {
                    return Err(ApiError::InvalidArgument("numeric program too deep".into()));
                }
            }
            if outputs.iter().any(|i| *i as usize >= node_len)
                || predicate < -1
                || predicate >= node_len as i32
            {
                return Err(ApiError::InvalidArgument(
                    "invalid numeric program roots".into(),
                ));
            }
            let paths: Vec<String> = fields
                .iter()
                .map(|f| {
                    if f.typ != 3 && bounds(f.typ).is_none() {
                        return Err(ApiError::InvalidArgument(
                            "invalid numeric field type".into(),
                        ));
                    }
                    Ok(str::from_utf8(borrowed_bytes(f.path, f.len, "field")?)
                        .map_err(failure)?
                        .to_owned())
                })
                .collect::<Result<_, ApiError>>()?;
            let check = || {
                if cancel.is_some_and(|f| f(context) != 0) {
                    Err(failure("aggregate interrupted"))
                } else {
                    Ok(())
                }
            };
            check()?;
            let archive = ClpSArchive::validate(PathBuf::from(
                str::from_utf8(borrowed_bytes(path, path_len, "path")?).map_err(failure)?,
            ))?;
            let metadata = SummaryContext::open(&archive)?;
            if let Some(metadata) = &metadata {
                if predicate >= 0
                    && !schema_may_match(metadata, &paths, fields, nodes, predicate as usize)
                {
                    for summary in out.iter_mut() {
                        summary.supported = 1;
                        summary.from_metadata = 1;
                    }
                    if std::env::var_os("CLP_SCAN_PROFILE").is_some() {
                        eprintln!(
                            "[clp aggregate] schema_tables=0 matched_rows=0 (no eligible input)"
                        );
                    }
                    check()?;
                    return Ok(());
                }
                if predicate == -1 && force_scan == 0 {
                    for (i, root) in outputs.iter().enumerate() {
                        if let Some(f) = summary_field(*root as usize, nodes) {
                            if let Some(summary) = metadata.field(&paths[f], fields[f].typ)? {
                                out[i] = summary;
                            }
                        }
                    }
                    if out.iter().all(|s| s.supported != 0) {
                        check()?;
                        return Ok(());
                    }
                }
            }
            drop(metadata);
            let mut options = ArchiveSearchOptions::default();
            let limits = options.packed_stream();
            options = options.with_packed_stream(PackedStreamLimits::new(
                if max_compressed == 0 {
                    limits.max_compressed_size()
                } else {
                    max_compressed
                },
                if max_decompressed == 0 {
                    limits.max_decompressed_size()
                } else {
                    max_decompressed
                },
            ));
            let parsed = parse_kql("*", KqlLimits::default()).map_err(failure)?;
            let mut scanner =
                columnar::ProjectedScanner::open(archive.open_reader()?, parsed, &options, &paths)
                    .map_err(failure)?;
            let mut ints = vec![IntegerSummary::default(); output_len];
            let mut floats = vec![FloatSummary::default(); output_len];
            let mut values = vec![None; field_len];
            let mut cache = vec![None; node_len];
            while let Some((columns, text, rows)) =
                scanner.next_columns_with_constants(true).map_err(failure)?
            {
                for row in 0..rows {
                    if row % 8192 == 0 {
                        check()?;
                    }
                    for (i, column) in columns.iter().enumerate() {
                        values[i] = value(column, row, text, fields[i].typ)?;
                    }
                    cache.fill(None);
                    if predicate >= 0
                        && !matches!(
                            evaluate(predicate as usize, nodes, &values, &mut cache)?,
                            Some(Scalar::Bool(true))
                        )
                    {
                        continue;
                    }
                    for (i, root) in outputs.iter().enumerate() {
                        if out[i].supported != 0 {
                            continue;
                        }
                        match evaluate(*root as usize, nodes, &values, &mut cache)? {
                            Some(Scalar::Int(v)) => {
                                if sum_required[i] == 0 {
                                    ints[i].sum = [0, 0];
                                }
                                ints[i]
                                    .add(v)
                                    .ok_or_else(|| failure("integer aggregate overflow"))?;
                                if sum_required[i] == 0 {
                                    ints[i].sum = [0, 0];
                                }
                            }
                            Some(Scalar::Float(v)) => floats[i]
                                .add(v)
                                .ok_or_else(|| failure("aggregate count overflow"))?,
                            None => (),
                            _ => return Err(failure("non-numeric aggregate input")),
                        }
                    }
                }
            }
            for (i, root) in outputs.iter().enumerate() {
                if out[i].supported == 0 {
                    out[i] = result(
                        ints[i].clone(),
                        floats[i].clone(),
                        nodes[*root as usize].typ,
                        false,
                    );
                }
            }
            check()?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_layout_and_integer_overflow() {
        assert_eq!(std::mem::size_of::<Node>(), 32);
        assert_eq!(std::mem::size_of::<ClpNumericSummary>(), 96);
        let nodes = [
            Node {
                op: 0,
                typ: 0,
                left: 0,
                right: 0,
                bits: [0, 0],
            },
            Node {
                op: 5,
                typ: 0,
                left: 0,
                right: 0,
                bits: [0, 0],
            },
        ];
        assert!(
            evaluate(
                1,
                &nodes,
                &[Some(Scalar::Int(i64::MAX as i128))],
                &mut [None; 2]
            )
            .is_err()
        );
        let mut widened = nodes;
        widened[1].typ = 2;
        assert!(matches!(
            evaluate(
                1,
                &widened,
                &[Some(Scalar::Int(i64::MAX as i128))],
                &mut [None; 2]
            )
            .unwrap(),
            Some(Scalar::Int(_))
        ));
    }
    #[test]
    fn sql_null_boolean_and_nan_ordering() {
        let nodes = [
            Node {
                op: 1,
                typ: 7,
                left: 0,
                right: 1,
                bits: [0, 0],
            },
            Node {
                op: 1,
                typ: 7,
                left: 0,
                right: 0,
                bits: [0, 0],
            },
            Node {
                op: 13,
                typ: 7,
                left: 0,
                right: 1,
                bits: [0, 0],
            },
            Node {
                op: 14,
                typ: 7,
                left: 0,
                right: 1,
                bits: [0, 0],
            },
        ];
        assert!(matches!(
            evaluate(2, &nodes, &[], &mut [None; 4]).unwrap(),
            Some(Scalar::Bool(false))
        ));
        assert!(evaluate(3, &nodes, &[], &mut [None; 4]).unwrap().is_none());
        assert!(
            compare(Scalar::Float(f64::NAN), Scalar::Float(f64::INFINITY))
                .unwrap()
                .is_gt()
        );
        assert!(
            compare(Scalar::Float(f64::NAN), Scalar::Float(f64::NAN))
                .unwrap()
                .is_eq()
        );
        assert!(
            compare(Scalar::Float(-0.0), Scalar::Float(0.0))
                .unwrap()
                .is_eq()
        );
    }
}
