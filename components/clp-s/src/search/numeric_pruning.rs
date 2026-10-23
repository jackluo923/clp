//! Conservative integer min/max pruning from compression-time schema summaries.

use std::collections::HashMap;

use super::{ComparisonOperator, Literal};
use crate::archive::{ArchiveCatalog, NodeType};
use crate::numeric_summary::{ColumnSummary, IntegerSummary, NumericSummaries, integer};

#[derive(Debug, Default)]
pub(super) struct NumericPruning {
    columns: HashMap<(i32, u32), Option<ColumnSummary>>,
}

impl NumericPruning {
    pub(super) fn new(catalog: &ArchiveCatalog) -> Self {
        let Some(summaries) = catalog
            .metadata()
            .numeric_summaries()
            .and_then(|bytes| NumericSummaries::decode(bytes).ok())
        else {
            return Self::default();
        };
        // Only use a summary when its schema identifies one physical table. Repeated schema
        // IDs or repeated array-column IDs cannot silently overwrite an earlier summary.
        let mut rows = HashMap::new();
        for table in catalog.table_metadata().schema_tables() {
            rows.entry(table.schema_id())
                .and_modify(|count| *count = None)
                .or_insert(Some(table.message_count()));
        }
        let mut columns = HashMap::new();
        for summary in summaries.columns {
            let key = (summary.schema, summary.node);
            let node = catalog.schema_tree().get(summary.node as usize);
            let usable = node.is_some_and(|node| {
                node.node_type() == NodeType::MixedInteger
                    || (node.node_type() == NodeType::Integer && summary.unsigned.count == 0)
            }) && rows
                .get(&summary.schema)
                .copied()
                .flatten()
                .is_some_and(|rows| valid(&summary, rows));
            columns
                .entry(key)
                .and_modify(|value| *value = None)
                .or_insert_with(|| usable.then_some(summary));
        }
        Self { columns }
    }

    /// A true result proves a positive integer predicate cannot match this column. Float and
    /// timestamp comparisons retain their existing evaluation; absent or dubious stats do too.
    pub(super) fn excludes(
        &self,
        schema: i32,
        node: u32,
        operator: ComparisonOperator,
        literal: &Literal,
    ) -> bool {
        let Some(Some(summary)) = self.columns.get(&(schema, node)) else {
            return false;
        };
        let (operand, buckets) = match literal {
            Literal::Integer { value, .. } => (
                i128::from(*value),
                [&summary.negative, &summary.nonnegative],
            ),
            Literal::Unsigned { value, .. } => (
                i128::from(*value),
                [&summary.nonnegative, &summary.unsigned],
            ),
            _ => return false,
        };
        !buckets
            .into_iter()
            .any(|bucket| may_match(bucket, operator, operand))
    }
}

fn valid(summary: &ColumnSummary, rows: u64) -> bool {
    summary.float.count == 0
        && summary
            .negative
            .count
            .checked_add(summary.nonnegative.count)
            .and_then(|count| count.checked_add(summary.unsigned.count))
            == Some(rows)
        && valid_bucket(&summary.negative, i128::from(i64::MIN), -1)
        && valid_bucket(&summary.nonnegative, 0, i128::from(i64::MAX))
        && valid_bucket(
            &summary.unsigned,
            i128::from(i64::MAX) + 1,
            i128::from(u64::MAX),
        )
}

fn valid_bucket(bucket: &IntegerSummary, low: i128, high: i128) -> bool {
    bucket.count == 0
        || (low <= integer(bucket.min)
            && integer(bucket.min) <= integer(bucket.max)
            && integer(bucket.max) <= high)
}

fn may_match(bucket: &IntegerSummary, operator: ComparisonOperator, operand: i128) -> bool {
    if bucket.count == 0 {
        return false;
    }
    let (min, max) = (integer(bucket.min), integer(bucket.max));
    match operator {
        ComparisonOperator::Equal => min <= operand && operand <= max,
        ComparisonOperator::Less => min < operand,
        ComparisonOperator::LessOrEqual => min <= operand,
        ComparisonOperator::Greater => max > operand,
        ComparisonOperator::GreaterOrEqual => max >= operand,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numeric_summary::words;

    #[test]
    fn integer_bounds_preserve_typed_eligibility_and_extreme_values() {
        let mut summary = ColumnSummary::default();
        for value in [i128::from(i64::MIN), -5, 0, 7, i128::from(u64::MAX)] {
            summary.add_integer(value).unwrap();
        }
        assert!(valid(&summary, 5));
        let pruning = NumericPruning {
            columns: [((0, 0), Some(summary))].into(),
        };
        let signed = |value| Literal::Integer {
            value,
            source: String::new(),
        };
        let unsigned = |value| Literal::Unsigned {
            value,
            source: String::new(),
        };
        assert!(pruning.excludes(0, 0, ComparisonOperator::Greater, &signed(7)));
        assert!(!pruning.excludes(0, 0, ComparisonOperator::Greater, &unsigned(7)));
        assert!(pruning.excludes(0, 0, ComparisonOperator::Less, &unsigned(0)));
        assert!(!pruning.excludes(0, 0, ComparisonOperator::Less, &signed(0)));
        assert!(!pruning.excludes(0, 0, ComparisonOperator::Equal, &unsigned(u64::MAX)));
        assert!(!pruning.excludes(0, 0, ComparisonOperator::Equal, &signed(i64::MIN)));
        assert!(pruning.excludes(0, 0, ComparisonOperator::Less, &signed(i64::MIN)));
        assert!(pruning.excludes(0, 0, ComparisonOperator::Greater, &unsigned(u64::MAX)));
        assert!(!pruning.excludes(
            0,
            0,
            ComparisonOperator::Equal,
            &Literal::Float {
                value: 9.5,
                source: String::new()
            }
        ));
    }

    #[test]
    fn incomplete_counts_invalid_ranges_and_missing_summaries_cannot_prune() {
        let mut summary = ColumnSummary::default();
        summary.add_integer(3).unwrap();
        assert!(!valid(&summary, 2));
        summary.nonnegative.min = words(4);
        assert!(!valid(&summary, 1));
        summary.nonnegative.min = words(-1);
        assert!(!valid(&summary, 1));
        summary.nonnegative.min = words(3);
        summary.float.count = 1;
        assert!(!valid(&summary, 1));
        assert!(!NumericPruning::default().excludes(
            0,
            0,
            ComparisonOperator::Equal,
            &Literal::Integer {
                value: 999,
                source: String::new()
            }
        ));
    }
}
