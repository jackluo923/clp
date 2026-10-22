//! Reuse per-table summaries only when the complete WHERE clause admits every row.
use super::*;

struct TimeCondition {
    field: usize,
    op: u32,
    value: i128,
}

fn conditions(nodes: &[Node], fields: &[Field], predicate: i32) -> Option<Vec<TimeCondition>> {
    if predicate < 0 || !nonthrowing_predicate(predicate as usize, nodes, fields) {
        return None;
    }
    let mut result = Vec::new();
    let mut pending = vec![predicate as usize];
    while let Some(root) = pending.pop() {
        let n = nodes[root];
        if n.op == 13 {
            pending.extend([n.left as usize, n.right as usize]);
            continue;
        }
        // Require the entire predicate, not just a necessary condition. An OR,
        // non-time field, aggregate FILTER or arithmetic never certifies coverage.
        if !matches!(n.op, 7 | 9..=12) {
            return None;
        }
        let (mut left, mut right) = (
            integer_operand(n.left as usize, nodes)?,
            integer_operand(n.right as usize, nodes)?,
        );
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
            return None;
        }
        result.push(TimeCondition {
            field: left.left as usize,
            op,
            value: integer(right.bits),
        });
    }
    Some(result)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coverage {
    All,
    None,
    Unknown,
}

fn coverage(
    ctx: &SummaryContext<'_>,
    table: &SchemaTableMetadata,
    nodes: &[Vec<u32>],
    fields: &[Field],
    conditions: &[TimeCondition],
) -> Result<Coverage, ApiError> {
    if table.message_count() == 0 {
        return Ok(Coverage::None);
    }
    let schema = ctx
        .catalog
        .schema_map()
        .get(table.schema_id())
        .ok_or_else(|| failure("missing schema"))?;
    let mut result = Coverage::All;
    for condition in conditions {
        let matching: Vec<_> = schema
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                SchemaEntry::Node(id) if nodes[condition.field].contains(id) => Some(*id),
                _ => None,
            })
            .collect();
        if matching.len() > 1 {
            result = Coverage::Unknown;
            continue;
        }
        let Some(node) = matching.first() else {
            // NULL compared with a constant cannot make this conjunction true.
            return Ok(Coverage::None);
        };
        let kind = ctx
            .catalog
            .schema_tree()
            .get(*node as usize)
            .ok_or_else(|| failure("missing node"))?
            .node_type();
        if kind == NodeType::Null {
            return Ok(Coverage::None);
        }
        if kind != NodeType::Timestamp {
            result = Coverage::Unknown;
            continue;
        }
        let Some(position) = ctx.index.get(&(table.schema_id(), *node)) else {
            result = Coverage::Unknown;
            continue;
        };
        let summary = &ctx.summaries.columns[*position];
        // Timestamp numeric summaries contain exact reconstructed source integers,
        // unlike the outward-rounded timestamp dictionary pruning bounds. Writers
        // omit this entry if any value uses a string/floating timestamp format.
        if summary
            .negative
            .count
            .checked_add(summary.nonnegative.count)
            != Some(table.message_count())
            || summary.unsigned.count != 0
            || summary.float.count != 0
        {
            result = Coverage::Unknown;
            continue;
        }
        let buckets = [&summary.negative, &summary.nonnegative];
        let mut min = i128::MAX;
        let mut max = i128::MIN;
        let mut valid = true;
        for bucket in buckets {
            if bucket.count == 0 {
                continue;
            }
            let (lo, hi) = (integer(bucket.min), integer(bucket.max));
            valid &= lo <= hi && lo >= i128::from(i64::MIN) && hi <= i128::from(i64::MAX);
            min = min.min(lo);
            max = max.max(hi);
        }
        if !valid || min > max {
            result = Coverage::Unknown;
            continue;
        }
        let (lo, hi) = bounds(fields[condition.field].typ).expect("integer condition field");
        if max < lo || min > hi {
            return Ok(Coverage::None);
        }
        // A partly eligible mapping introduces NULLs, so it cannot certify that
        // every row passes, even if the remaining values satisfy the comparison.
        if min < lo || max > hi {
            result = Coverage::Unknown;
            continue;
        }
        let value = condition.value;
        let (all, none) = match condition.op {
            7 => (min == value && max == value, value < min || value > max),
            9 => (max < value, min >= value),
            10 => (max <= value, min > value),
            11 => (min > value, max <= value),
            12 => (min >= value, max < value),
            _ => unreachable!(),
        };
        if none {
            return Ok(Coverage::None);
        }
        if !all {
            result = Coverage::Unknown;
        }
    }
    Ok(result)
}

enum Input {
    Field(usize),
    Constant(Nullable),
}

fn inputs(nodes: &[Node], fields: &[Field], outputs: &[u32]) -> Option<Vec<Input>> {
    outputs
        .iter()
        .map(|&root| {
            if !nonthrowing_predicate(root as usize, nodes, fields) {
                return None;
            }
            if let Some(field) = summary_field(root as usize, nodes) {
                Some(Input::Field(field))
            } else if nodes[root as usize].op == 1 && nodes[root as usize].typ != 7 {
                Some(Input::Constant(
                    evaluate(root as usize, nodes, &[], &mut vec![None; nodes.len()]).ok()?,
                ))
            } else {
                None
            }
        })
        .collect()
}

/// Simple window statistics need no per-row expression DAG. This also keeps
/// archives without timestamp indexes/summaries on a cheap scan fallback.
pub(super) struct ScanProgram {
    ranges: Vec<(usize, i128, i128)>,
    inputs: Vec<Input>,
    empty: bool,
}

impl ScanProgram {
    pub(super) fn new(
        nodes: &[Node],
        fields: &[Field],
        outputs: &[u32],
        predicate: i32,
    ) -> Option<Self> {
        let conditions = if predicate < 0 {
            Vec::new()
        } else {
            conditions(nodes, fields, predicate)?
        };
        let inputs = inputs(nodes, fields, outputs)?;
        let mut ranges: Vec<(usize, i128, i128)> = Vec::new();
        let mut empty = false;
        for condition in conditions {
            let (lo, hi) = match condition.op {
                7 => (condition.value, condition.value),
                9 => match condition.value.checked_sub(1) {
                    Some(hi) => (i128::MIN, hi),
                    None => {
                        empty = true;
                        continue;
                    }
                },
                10 => (i128::MIN, condition.value),
                11 => match condition.value.checked_add(1) {
                    Some(lo) => (lo, i128::MAX),
                    None => {
                        empty = true;
                        continue;
                    }
                },
                12 => (condition.value, i128::MAX),
                _ => unreachable!(),
            };
            let index = ranges
                .iter()
                .position(|r| r.0 == condition.field)
                .unwrap_or_else(|| {
                    let (lo, hi) =
                        bounds(fields[condition.field].typ).expect("integer condition field");
                    ranges.push((condition.field, lo, hi));
                    ranges.len() - 1
                });
            ranges[index].1 = ranges[index].1.max(lo);
            ranges[index].2 = ranges[index].2.min(hi);
            empty |= ranges[index].1 > ranges[index].2;
        }
        Some(Self {
            ranges,
            inputs,
            empty,
        })
    }

    pub(super) unsafe fn consume(
        &self,
        columns: &[columnar::ColumnView],
        text: &[u8],
        rows: usize,
        fields: &[Field],
        sum_required: &[u8],
        scan_outputs: &[bool],
        ints: &mut [IntegerSummary],
        floats: &mut [FloatSummary],
        check: &impl Fn() -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        if self.empty {
            return Ok(());
        }
        // Compact column views can span a whole table; bound the selection
        // scratch independently of its row count.
        let mut selected = Vec::with_capacity(rows.min(8192));
        for start in (0..rows).step_by(8192) {
            selected.clear();
            check()?;
            for row in start..start + (rows - start).min(8192) {
                let mut matches = true;
                for &(field, lo, hi) in &self.ranges {
                    if !matches!(unsafe { value(&columns[field], row, text, fields[field].typ)? },
                    Some(Scalar::Int(v)) if lo <= v && v <= hi)
                    {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    selected.push(row);
                }
            }
            for (i, input) in self.inputs.iter().enumerate() {
                if !scan_outputs[i] {
                    continue;
                }
                check()?;
                for &row in &selected {
                    let v = match input {
                        Input::Field(field) => unsafe {
                            value(&columns[*field], row, text, fields[*field].typ)?
                        },
                        Input::Constant(v) => *v,
                    };
                    match v {
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
                        Some(Scalar::Float(v)) => {
                            floats[i]
                                .add(v)
                                .ok_or_else(|| failure("aggregate count overflow"))?;
                        }
                        None => (),
                        _ => return Err(failure("non-numeric aggregate input")),
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) struct Partials {
    pub scan_tables: Vec<bool>,
    pub scan_outputs: Vec<Vec<bool>>,
    pub all_rows: Vec<bool>,
    pub metadata_outputs: Vec<bool>,
    pub ints: Vec<IntegerSummary>,
    pub floats: Vec<FloatSummary>,
    pub covered_tables: usize,
    pub rejected_tables: usize,
    pub predicate_covered_tables: usize,
    pub summary_partials: usize,
}

pub(super) fn partials(
    ctx: &SummaryContext<'_>,
    paths: &[String],
    fields: &[Field],
    nodes: &[Node],
    outputs: &[u32],
    sum_required: &[u8],
    predicate: i32,
) -> Result<Option<Partials>, ApiError> {
    let conditions = if predicate < 0 {
        Vec::new()
    } else {
        let Some(conditions) = conditions(nodes, fields, predicate) else {
            return Ok(None);
        };
        conditions
    };
    // Coverage is independent of aggregate arguments. Arithmetic/FILTER outputs
    // still evaluate normally; independently eligible outputs can use summaries.
    let inputs: Vec<_> = outputs
        .iter()
        .map(|&root| inputs(nodes, fields, &[root]).and_then(|mut values| values.pop()))
        .collect();
    let field_nodes: Vec<_> = paths
        .iter()
        .map(|p| columnar::resolve_descriptor_path(ctx.catalog.schema_tree(), p))
        .collect();
    let tables = ctx.catalog.table_metadata().schema_tables();
    let mut result = Partials {
        scan_tables: vec![true; tables.len()],
        scan_outputs: vec![vec![true; outputs.len()]; tables.len()],
        all_rows: vec![false; tables.len()],
        metadata_outputs: vec![true; outputs.len()],
        ints: vec![IntegerSummary::default(); outputs.len()],
        floats: vec![FloatSummary::default(); outputs.len()],
        covered_tables: 0,
        rejected_tables: 0,
        predicate_covered_tables: 0,
        summary_partials: 0,
    };
    for (index, table) in tables.iter().enumerate() {
        match coverage(ctx, table, &field_nodes, fields, &conditions)? {
            Coverage::None => {
                result.scan_tables[index] = false;
                result.scan_outputs[index].fill(false);
                result.rejected_tables += 1;
                continue;
            }
            Coverage::Unknown => continue,
            Coverage::All => (),
        }
        result.all_rows[index] = true;
        result.predicate_covered_tables += 1;
        for (i, input) in inputs.iter().enumerate() {
            let summary = match input {
                Some(Input::Field(field)) => {
                    ctx.field_tables(&field_nodes[*field], fields[*field].typ, [table])?
                }
                Some(Input::Constant(value)) => {
                    let mut summary = ClpNumericSummary {
                        supported: 1,
                        from_metadata: 1,
                        ..Default::default()
                    };
                    match value {
                        Some(Scalar::Int(value)) => {
                            summary.count = table.message_count();
                            summary.min = clp_s::numeric_summary::words(*value);
                            summary.max = summary.min;
                            if sum_required[i] != 0 {
                                summary.sum = clp_s::numeric_summary::words(
                                    value
                                        .checked_mul(i128::from(summary.count))
                                        .ok_or_else(|| failure("integer aggregate overflow"))?,
                                );
                            }
                        }
                        // Preserve repeated addition for a floating constant.
                        Some(_) => continue,
                        None => (),
                    }
                    Some(summary)
                }
                None => continue,
            };
            let Some(summary) = summary.filter(|s| s.supported != 0) else {
                continue;
            };
            if nodes[outputs[i] as usize].typ == 3 {
                result.floats[i]
                    .merge(&FloatSummary {
                        count: summary.count,
                        sum: summary.float_sum,
                        min: summary.float_min,
                        max: summary.float_max,
                    })
                    .ok_or_else(|| failure("aggregate count overflow"))?;
            } else {
                result.ints[i]
                    .merge(&IntegerSummary {
                        count: summary.count,
                        sum: if sum_required[i] != 0 {
                            summary.sum
                        } else {
                            [0; 2]
                        },
                        min: summary.min,
                        max: summary.max,
                    })
                    .ok_or_else(|| failure("integer aggregate overflow"))?;
            }
            result.scan_outputs[index][i] = false;
            result.summary_partials += 1;
        }
        if result.scan_outputs[index].iter().all(|scan| !scan) {
            result.scan_tables[index] = false;
            result.covered_tables += 1;
        }
    }
    for scan in &result.scan_outputs {
        for (i, scan) in scan.iter().enumerate() {
            result.metadata_outputs[i] &= !scan;
        }
    }
    Ok(Some(result))
}

#[cfg(test)]
#[path = "numeric_window_tests.rs"]
mod tests;
