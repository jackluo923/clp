//! Validates complete CLP-S single-file archives through the public library API.

use std::env;
use std::error::Error;
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use clp_s::ExtractionPlan;
use clp_s::ExtractionPlanLimits;
use clp_s::LogOrderLocator;
use clp_s::OrderedMergeLimits;
use clp_s::OrderedMergeTable;
use clp_s::OrderedRowMerge;
use clp_s::archive::ArchiveCatalogLimits;
use clp_s::archive::ColumnLimits;
use clp_s::archive::PackedStreamLimits;
use clp_s::archive::SingleFileArchiveReader;

fn main() -> Result<(), Box<dyn Error>> {
    let paths = env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: cargo run -p clp-s --example validate_sfa -- ARCHIVE...",
        )
        .into());
    }

    for path in paths {
        validate(&path)?;
    }
    Ok(())
}

fn validate(path: &Path) -> Result<(), Box<dyn Error>> {
    let source = File::open(path)?;
    let mut reader = SingleFileArchiveReader::open(source)?;
    let catalog = reader.read_catalog(ArchiveCatalogLimits::default())?;
    let log_order = LogOrderLocator::discover(catalog.schema_tree())?;
    let mut decoded_tables = 0_usize;
    let mut decoded_records = 0_u64;

    for stream_id in 0..catalog.table_metadata().packed_streams().len() {
        let validate_global_order =
            1 == catalog.table_metadata().packed_streams().len() && log_order.is_some();
        let mut ordered_tables = Vec::new();
        let mut stream_records = 0_u64;
        let stream = reader.read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            stream_id,
            PackedStreamLimits::default(),
        )?;
        let stream_id = u64::try_from(stream_id)?;
        for table in catalog.schema_tables(stream_id, &stream, ColumnLimits::default())? {
            let table = table?;
            let plan = ExtractionPlan::compile(
                table.schema(),
                catalog.schema_tree(),
                ExtractionPlanLimits::default(),
            )?;
            if plan.column_count() != table.table().len() {
                return Err(io::Error::other(
                    "extraction-plan column count disagrees with decoded table",
                )
                .into());
            }
            let order_column = log_order
                .map(|locator| locator.locate(table.schema(), table.table()))
                .transpose()?
                .flatten();
            if let Some(column) = order_column {
                let consumed = column.cursor().count();
                if consumed != table.table().message_count() {
                    return Err(io::Error::other(
                        "log-order cursor length disagrees with decoded table",
                    )
                    .into());
                }
                if validate_global_order {
                    ordered_tables.push(OrderedMergeTable::new(table.table_index(), column));
                }
            } else if validate_global_order {
                return Err(io::Error::other(
                    "archive advertises log order but a decoded table omits it",
                )
                .into());
            }
            decoded_tables = decoded_tables
                .checked_add(1)
                .ok_or_else(|| io::Error::other("decoded table count overflow"))?;
            decoded_records = decoded_records
                .checked_add(table.metadata().message_count())
                .ok_or_else(|| io::Error::other("decoded record count overflow"))?;
            stream_records = stream_records
                .checked_add(table.metadata().message_count())
                .ok_or_else(|| io::Error::other("decoded stream record count overflow"))?;
        }

        if validate_global_order {
            let mut merge = OrderedRowMerge::new(&ordered_tables, OrderedMergeLimits::default())?;
            let mut merged_records = 0_u64;
            for row in &mut merge {
                let row = row?;
                let expected = i64::try_from(merged_records)?;
                if row.log_event_idx() != expected {
                    return Err(io::Error::other(
                        "ordered merge produced a noncanonical log-event index",
                    )
                    .into());
                }
                merged_records = merged_records
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("merged record count overflow"))?;
            }
            if merged_records != stream_records {
                return Err(io::Error::other(
                    "ordered merge count disagrees with decoded stream tables",
                )
                .into());
            }
        }
    }

    if decoded_tables != catalog.table_metadata().schema_tables().len()
        || decoded_records != catalog.table_metadata().record_count()
    {
        return Err(io::Error::other("decoded totals disagree with table metadata").into());
    }

    println!(
        "{}: {} streams, {decoded_tables} tables, {decoded_records} records",
        path.display(),
        catalog.table_metadata().packed_streams().len(),
    );
    Ok(())
}
