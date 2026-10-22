//! Batched numeric summaries and fused filtered expression aggregation.
use super::*;

#[path = "numeric_window.rs"]
mod window;

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

/// Whether node `index` is NULL whenever every field it reads is absent.
///
/// Field reads, casts, arithmetic and comparisons propagate NULL. Constants, `IS [NOT] NULL` and
/// Boolean connectives can yield a value with no input, and a conditional is NULL-propagating only
/// when both its operands are; all of those are treated as yielding a value.
fn null_without_fields(index: usize, nodes: &[Node]) -> bool {
    let Some(node) = nodes.get(index) else { return false };
    let left = node.left as usize;
    let right = node.right as usize;
    match node.op {
        0 => true,
        2 => left < index && null_without_fields(left, nodes),
        3..=12 => {
            (left < index && null_without_fields(left, nodes))
                || (right < index && null_without_fields(right, nodes))
        }
        17 => {
            left < index && right < index
                && null_without_fields(left, nodes) && null_without_fields(right, nodes)
        }
        _ => false,
    }
}

/// Whether the predicate rooted at `index` can only be true on a row holding a field it reads.
fn untrue_without_fields(index: usize, nodes: &[Node]) -> bool {
    if null_without_fields(index, nodes) {
        return true;
    }
    let Some(node) = nodes.get(index) else { return false };
    let (left, right) = (node.left as usize, node.right as usize);
    match node.op {
        // AND is untrue if either side is; OR only if both are. IS NOT NULL of nothing is false.
        13 => (left < index && untrue_without_fields(left, nodes)) || (right < index && untrue_without_fields(right, nodes)),
        14 => left < index && right < index && untrue_without_fields(left, nodes) && untrue_without_fields(right, nodes),
        16 => true,
        _ => false,
    }
}

/// The fields node `index` reads.
fn fields_of(index: usize, nodes: &[Node], fields: &mut Vec<usize>) {
    let Some(node) = nodes.get(index) else { return };
    let (left, right) = (node.left as usize, node.right as usize);
    match node.op {
        0 => fields.push(left),
        1 => {}
        2 | 15 | 16 => {
            if left < index { fields_of(left, nodes, fields) }
        }
        _ => {
            if left < index { fields_of(left, nodes, fields) }
            if right < index { fields_of(right, nodes, fields) }
        }
    }
}

/// A KQL query for the rows that can contribute, or `None` when every row can.
///
/// A row contributes only if it holds a field some aggregate reads, when every aggregate is NULL
/// without its fields, and only if it holds a field the predicate reads, when the predicate cannot
/// be true without one. The engine skips, unread, every table holding no such field.
fn field_existence_kql(paths: &[String], nodes: &[Node], outputs: &[u32], predicate: i32) -> Option<String> {
    let any_of = |roots: &[usize]| {
        let mut fields = Vec::new();
        for root in roots {
            fields_of(*root, nodes, &mut fields);
        }
        let mut names: Vec<&String> = fields.iter().filter_map(|field| paths.get(*field)).collect();
        names.sort();
        names.dedup();
        (!names.is_empty()).then(|| {
            names
                .iter()
                .map(|path| format!("\"{}\": *", path.replace('\\', "\\\\").replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(" OR ")
        })
    };
    let mut conjuncts = Vec::new();
    let roots: Vec<usize> = outputs.iter().map(|root| *root as usize).collect();
    if roots.iter().all(|root| null_without_fields(*root, nodes)) {
        conjuncts.extend(any_of(&roots));
    }
    if predicate >= 0 && untrue_without_fields(predicate as usize, nodes) {
        conjuncts.extend(any_of(&[predicate as usize]));
    }
    (!conjuncts.is_empty()).then(|| {
        conjuncts.iter().map(|kql| format!("({kql})")).collect::<Vec<_>>().join(" AND ")
    })
}

// A prefilter must not hide errors from another predicate operand. The SQL
// compiler already rejects throwing predicates; retain that guard for C callers.
fn nonthrowing_predicate(root: usize, nodes: &[Node], fields: &[Field]) -> bool {
    let n = nodes[root];
    match n.op {
        0 => n.typ == fields[n.left as usize].typ,
        1 => {
            n.right != 0
                || n.typ == 3
                || n.typ == 7
                || range(n.typ).is_some_and(|(lo, hi)| (lo..=hi).contains(&integer(n.bits)))
        }
        2 => {
            let child = nodes[n.left as usize];
            let widening = match (range(child.typ), range(n.typ)) {
                (Some((lo, hi)), Some((dest_lo, dest_hi))) => dest_lo <= lo && hi <= dest_hi,
                _ => n.typ == child.typ || (n.typ == 3 && range(child.typ).is_some()),
            };
            widening && nonthrowing_predicate(n.left as usize, nodes, fields)
        }
        7..=12 => {
            let a = nodes[n.left as usize].typ;
            let b = nodes[n.right as usize].typ;
            n.typ == 7
                && (a == b || (range(a).is_some() && range(b).is_some()))
                && nonthrowing_predicate(n.left as usize, nodes, fields)
                && nonthrowing_predicate(n.right as usize, nodes, fields)
        }
        13 | 14 => {
            n.typ == 7
                && nodes[n.left as usize].typ == 7
                && nodes[n.right as usize].typ == 7
                && nonthrowing_predicate(n.left as usize, nodes, fields)
                && nonthrowing_predicate(n.right as usize, nodes, fields)
        }
        15 | 16 => n.typ == 7 && nonthrowing_predicate(n.left as usize, nodes, fields),
        _ => false,
    }
}

fn integer_operand(mut root: usize, nodes: &[Node]) -> Option<Node> {
    loop {
        let n = nodes[root];
        range(n.typ)?;
        if n.op != 2 {
            return Some(n);
        }
        root = n.left as usize;
    }
}

// Integer timestamp formatting truncates epoch_ns / scale toward zero. Expand
// the entire source-unit bucket, including sub-unit epochs and negative/zero
// timestamps, just as the SQL bridge's SourceUnitTimestampKql does.
fn source_time_kql(path: &str, op: u32, value: i64, scale: i64) -> String {
    let (v, scale) = (i128::from(value), i128::from(scale));
    let (min, max) = (i128::from(i64::MIN), i128::from(i64::MAX));
    let first = |x: i128| x * scale - if x <= 0 { scale - 1 } else { 0 };
    let last = |x: i128| x * scale + if x >= 0 { scale - 1 } else { 0 };
    let (lo, hi) = match op {
        7 => (first(v), last(v)),
        9 => (min, last(v - 1)),
        10 => (min, last(v)),
        11 => (first(v + 1), max),
        12 => (first(v), max),
        _ => unreachable!("only equality and ordered comparisons are extracted"),
    };
    let descriptor = format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""));
    if lo > max || hi < min || lo > hi {
        format!("{descriptor} > {max}")
    } else {
        let (lo, hi) = (lo.max(min), hi.min(max));
        if lo == min && hi == max {
            format!("{descriptor}: *")
        } else if lo == min {
            format!("{descriptor} <= {hi}")
        } else if hi == max {
            format!("{descriptor} >= {lo}")
        } else {
            format!("{descriptor} >= {lo} AND {descriptor} <= {hi}")
        }
    }
}

/// Safe necessary time conditions from the WHERE predicate. Per-aggregate FILTER,
/// OR branches, arithmetic, floating conversions, and uncertified source units
/// remain with the numeric evaluator. Its original predicate is always retained.
fn timestamp_prefilters(
    catalog: &clp_s::archive::ArchiveCatalog,
    paths: &[String],
    fields: &[Field],
    nodes: &[Node],
    predicate: i32,
) -> Vec<String> {
    if predicate < 0 || !nonthrowing_predicate(predicate as usize, nodes, fields) {
        return Vec::new();
    }
    let scale = timestamp_integer_scale(catalog.metadata().timestamp_dictionary().patterns());
    if scale <= 0 {
        return Vec::new();
    }
    let types = columnar::archive_field_types(catalog.schema_tree());
    let mut filters = Vec::new();
    let mut pending = vec![predicate as usize];
    while let Some(root) = pending.pop() {
        let n = nodes[root];
        if n.op == 13 {
            pending.extend([n.left as usize, n.right as usize]);
            continue;
        }
        if !matches!(n.op, 7 | 9..=12) {
            continue;
        }
        let (Some(mut left), Some(mut right)) = (
            integer_operand(n.left as usize, nodes),
            integer_operand(n.right as usize, nodes),
        ) else {
            continue;
        };
        let mut op = n.op;
        if left.op == 1 && right.op == 0 {
            std::mem::swap(&mut left, &mut right);
            op = match op {
                9 => 11,
                10 => 12,
                11 => 9,
                12 => 10,
                other => other,
            };
        }
        if left.op != 0 || right.op != 1 || right.right != 0 {
            continue;
        }
        let Ok(value) = i64::try_from(integer(right.bits)) else {
            continue;
        };
        let path = &paths[left.left as usize];
        let mut timestamp = false;
        let certified = types.iter().filter(|t| t.path == path.as_bytes()).all(|t| {
            timestamp |= t.node_type == NodeType::Timestamp;
            matches!(t.node_type, NodeType::Timestamp | NodeType::Null)
        });
        if certified && timestamp {
            filters.push(source_time_kql(path, op, value, scale));
        }
    }
    filters
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
            let (reader, catalog) = open_numeric_archive(
                str::from_utf8(borrowed_bytes(path, path_len, "path")?).map_err(failure)?,
            )?;
            check()?;
            let metadata = SummaryContext::from_catalog(&catalog)?;
            let mut window_partials = None;
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
                if force_scan == 0 {
                    window_partials = window::partials(metadata, &paths, fields, nodes, outputs, sum_required, predicate)?;
                    if let Some(partials) = &window_partials {
                        let scan_tables = partials.scan_tables.iter().filter(|scan| **scan).count();
                        if std::env::var_os("CLP_SCAN_PROFILE").is_some() {
                            eprintln!("[clp aggregate] window_summary_tables={} window_rejected_tables={} window_scan_tables={scan_tables} predicate_covered_tables={} summary_partials={}",
                                partials.covered_tables, partials.rejected_tables, partials.predicate_covered_tables, partials.summary_partials);
                        }
                        if scan_tables == 0 {
                            for (i, root) in outputs.iter().enumerate() {
                                out[i] = result(partials.ints[i].clone(), partials.floats[i].clone(), nodes[*root as usize].typ, true);
                            }
                            check()?;
                            return Ok(());
                        }
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
            let table_count = catalog.table_metadata().schema_tables().len();
            let (scan_tables, scan_outputs, all_rows, metadata_outputs, mut ints, mut floats) = match window_partials {
                Some(p) => (p.scan_tables, p.scan_outputs, p.all_rows, p.metadata_outputs, p.ints, p.floats),
                None => (vec![true; table_count], vec![vec![true; output_len]; table_count],
                    vec![false; table_count], vec![false; output_len],
                    vec![IntegerSummary::default(); output_len], vec![FloatSummary::default(); output_len]),
            };
            let dependencies: Vec<Vec<usize>> = outputs.iter().map(|root| {
                let mut fields = Vec::new(); fields_of(*root as usize, nodes, &mut fields); fields
            }).collect();
            let mut predicate_fields = Vec::new();
            if predicate >= 0 { fields_of(predicate as usize, nodes, &mut predicate_fields); }
            let table_fields: Vec<Vec<bool>> = (0..table_count).map(|table| {
                let mut needed = vec![force_scan != 0; field_len];
                if scan_tables[table] {
                    for (i, deps) in dependencies.iter().enumerate() {
                        if scan_outputs[table][i] { for &field in deps { needed[field] = true; } }
                    }
                    if !all_rows[table] { for &field in &predicate_fields { needed[field] = true; } }
                } else { needed.fill(false); }
                needed
            }).collect();
            let active: Vec<_> = (0..field_len).filter(|field| table_fields.iter().any(|needed| needed[*field])).collect();
            let scan_paths: Vec<_> = active.iter().map(|&field| paths[field].clone()).collect();
            let plans = table_fields.iter().enumerate().map(|(table, needed)| columnar::NumericColumnPlan {
                fields: active.iter().map(|&field| needed[field]).collect(), all_rows: all_rows[table],
            }).collect();
            let residual = if scan_tables.iter().enumerate().any(|(i, scan)| *scan && !all_rows[i]) { predicate } else { -1 };
            let scan_roots: Vec<_> = outputs.iter().enumerate().filter_map(|(i, root)|
                scan_outputs.iter().any(|scan| scan[i]).then_some(*root)).collect();
            // Necessary conditions apply only to unresolved tables. Covered tables
            // bypass this query altogether and retain their exact SQL coverage proof.
            let mut prefilters: Vec<_> = field_existence_kql(&paths, nodes, &scan_roots, residual).into_iter().collect();
            let time_filters = timestamp_prefilters(&catalog, &paths, fields, nodes, residual);
            let time_filter_count = time_filters.len();
            prefilters.extend(time_filters);
            let kql = if prefilters.is_empty() {
                "*".to_owned()
            } else {
                prefilters.iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(" AND ")
            };
            let parsed = match parse_kql(&kql, KqlLimits::default()) {
                Ok(parsed) => {
                    if std::env::var_os("CLP_SCAN_PROFILE").is_some() && time_filter_count > 0 {
                        eprintln!("[clp aggregate] timestamp_prefilters={time_filter_count}");
                    }
                    parsed
                }
                Err(_) => parse_kql("*", KqlLimits::default()).map_err(failure)?,
            };
            let mut scanner = columnar::ProjectedScanner::open_with_catalog(reader, catalog, parsed, &options, &scan_paths)
                .map_err(failure)?;
            scanner.set_numeric_plans(plans).map_err(failure)?;
            scanner.restrict_tables(&scan_tables).map_err(failure)?;
            let mut values = vec![None; field_len];
            let mut cache = vec![None; node_len];
            let window_scan = if force_scan == 0 {
                window::ScanProgram::new(nodes, fields, outputs, predicate)
            } else { None };
            let covered_scan = if force_scan == 0 { window::ScanProgram::new(nodes, fields, outputs, -1) } else { None };
            let mut projected = vec![columnar::ColumnView { kind: columnar::kind::ABSENT, reserved: 0, data: std::ptr::null() }; field_len];
            let profile = std::env::var_os("CLP_SCAN_PROFILE").is_some();
            let mut evaluated_rows = 0u64;
            let mut predicate_rows = 0u64;
            if profile { eprintln!("[clp aggregate] projected_fields={} input_fields={field_len}", active.len()); }
            while let Some((table, columns, text, rows)) = scanner.next_numeric_columns().map_err(failure)? {
                for (&field, column) in active.iter().zip(columns) { projected[field] = *column; }
                let columns = &projected;
                let table_predicate = if all_rows[table] { -1 } else { predicate };
                let table_window_scan = if all_rows[table] { &covered_scan } else { &window_scan };
                if profile {
                    evaluated_rows = evaluated_rows.saturating_add(rows as u64);
                    if table_predicate >= 0 { predicate_rows = predicate_rows.saturating_add(rows as u64); }
                }
                if let Some(window_scan) = table_window_scan {
                    window_scan.consume(columns, text, rows, fields, sum_required, &scan_outputs[table],
                        &mut ints, &mut floats, &check)?;
                    continue;
                }
                for row in 0..rows {
                    if row % 8192 == 0 {
                        check()?;
                    }
                    for &i in &active {
                        values[i] = value(&columns[i], row, text, fields[i].typ)?;
                    }
                    cache.fill(None);
                    if table_predicate >= 0
                        && !matches!(
                            evaluate(table_predicate as usize, nodes, &values, &mut cache)?,
                            Some(Scalar::Bool(true))
                        )
                    {
                        continue;
                    }
                    for (i, root) in outputs.iter().enumerate() {
                        if !scan_outputs[table][i] { continue; }
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
            if profile {
                eprintln!("[clp aggregate] evaluated_rows={evaluated_rows} predicate_rows={predicate_rows}");
            }
            for (i, root) in outputs.iter().enumerate() {
                if out[i].supported == 0 {
                    out[i] = result(
                        ints[i].clone(),
                        floats[i].clone(),
                        nodes[*root as usize].typ,
                        metadata_outputs[i],
                    );
                }
            }
            check()?;
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "numeric_program_tests.rs"]
mod pushdown_tests;

#[cfg(test)]
mod tests {

    fn node(op: u32, left: u32, right: u32) -> Node {
        Node { op, typ: 0, left, right, bits: [0, 0] }
    }

    #[test]
    fn aggregate_scans_read_only_rows_that_can_contribute() {
        let paths = vec!["A".to_owned(), "B".to_owned(), "ts".to_owned()];
        // 0: A, 1: B, 2: A*B, 3: constant, 4: ts, 5: ts >= c, 6: A IS NULL, 7: A >= c
        let nodes = vec![
            node(0, 0, 0), node(0, 1, 0), node(5, 0, 1), node(1, 0, 0),
            node(0, 2, 0), node(12, 4, 3), node(15, 0, 0), node(12, 0, 3),
        ];
        // sum(A*B): rows holding A or B.
        assert_eq!(Some(r#"("A": * OR "B": *)"#.to_owned()), field_existence_kql(&paths, &nodes, &[2], -1));
        // sum(A*B) WHERE ts >= c: also rows holding ts.
        assert_eq!(Some(r#"("A": * OR "B": *) AND ("ts": *)"#.to_owned()), field_existence_kql(&paths, &nodes, &[2], 5));
        // count(*) WHERE A >= c: only the predicate restricts.
        assert_eq!(Some(r#"("A": *)"#.to_owned()), field_existence_kql(&paths, &nodes, &[3], 7));
        // count(*) alone, or a predicate true without its field (A IS NULL): every row counts.
        assert_eq!(None, field_existence_kql(&paths, &nodes, &[3], -1));
        assert_eq!(None, field_existence_kql(&paths, &nodes, &[3], 6));
        // One aggregate over a constant keeps every row for the aggregates.
        assert_eq!(None, field_existence_kql(&paths, &nodes, &[2, 3], -1));
    }

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
