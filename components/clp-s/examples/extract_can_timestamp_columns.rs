//! Research helper: extract physical CAN `ts` columns and their frame provenance.
//! Usage: extract_can_timestamp_columns OUTPUT_DIRECTORY FORMAT ARCHIVE...
use std::{env, error::Error, fs::{self, File}, io::{BufWriter, Write}, path::PathBuf};
use clp_s::archive::{ArchiveCatalogLimits, ColumnData, ColumnLimits, PackedStreamLimits, SingleFileArchiveReader};
use serde_json::{json, Value};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    let format = args.next().ok_or("missing format")?.to_string_lossy().into_owned();
    fs::create_dir_all(&output)?;
    let output = output.canonicalize()?;
    let mut columns = Vec::<Value>::new();
    let mut archives = Vec::<Value>::new();
    for archive in args.map(PathBuf::from) {
        let archive = archive.canonicalize()?;
        let name = archive.file_name().ok_or("archive filename")?.to_string_lossy();
        let mut reader = SingleFileArchiveReader::open(File::open(&archive)?)?;
        let catalog = reader.read_catalog(ArchiveCatalogLimits::default())?;
        let metadata = catalog.table_metadata();
        let mut decoded_rows = 0usize;
        let mut timestamp_rows = 0usize;
        let mut timestamp_frame_bytes = 0u64;
        let mut unallocated_rows = 0usize;
        for stream_index in 0..metadata.packed_streams().len() {
            let stream_id = stream_index as u64;
            let raw = reader.read_packed_stream(catalog.metadata(), metadata, stream_index, PackedStreamLimits::default())?;
            let raw_columns = raw.column_layout().map(|layout| {
                let mut offset = 0;
                layout.iter().map(|slot| {
                    let bytes = raw.as_bytes()[offset..offset + slot.stored].to_vec();
                    offset += slot.stored;
                    bytes
                }).collect::<Vec<_>>()
            });
            let decoded = catalog.materialize_adaptive_numerics(stream_id, raw, ColumnLimits::default())?;
            for table in catalog.schema_tables(stream_id, &decoded, ColumnLimits::default())? {
                let table = table?;
                let rows = table.table().message_count();
                decoded_rows += rows;
                let mut found_ts = false;
                for (column_index, column) in table.table().columns().iter().enumerate() {
                    let node = catalog.schema_tree().get(column.node_id() as usize).ok_or("column node")?;
                    if node.key_bytes() != b"ts" { continue; }
                    if found_ts { return Err("multiple ts columns per CAN schema".into()); }
                    found_ts = true;
                    let id = format!("{format}-{name}-t{}", table.table_index());
                    let path = output.join(format!("{id}.i64le"));
                    let mut file = BufWriter::new(File::create(&path)?);
                    let mut written = 0usize;
                    let mut min = i64::MAX;
                    let mut max = i64::MIN;
                    let mut sum = 0i128;
                    let mut write_value = |value: i64| -> Result<(), Box<dyn Error>> {
                        file.write_all(&value.to_le_bytes())?;
                        written += 1; min = min.min(value); max = max.max(value); sum += value as i128;
                        Ok(())
                    };
                    let mixed = match column.data() {
                        ColumnData::Integer(values) => { for value in values.iter() { write_value(value)?; } false },
                        ColumnData::MixedInteger(values) => {
                            if values.has_large_uint64() { return Err("CAN timestamp exceeds i64".into()); }
                            for value in values.iter() { write_value(value.as_i64().ok_or("unsigned timestamp")?)?; }
                            true
                        },
                        _ => return Err(format!("unexpected timestamp column type {:?}", column.node_type()).into()),
                    };
                    drop(write_value);
                    file.flush()?;
                    if written != rows || fs::metadata(&path)?.len() != rows as u64 * 8 { return Err("decoded row count mismatch".into()); }
                    timestamp_rows += rows;
                    let frame = metadata.separate_columns_for(stream_id).and_then(|s| s.columns().get(column_index));
                    let encoded = raw_columns.as_ref().and_then(|c| c.get(column_index));
                    let (encoding, encoded_path) = if let Some(bytes) = encoded {
                        let encoded_path = output.join(format!("{id}.encoded"));
                        fs::write(&encoded_path, bytes)?;
                        let offset = usize::from(mixed);
                        let scheme = bytes.get(offset).copied();
                        let label = match scheme {
                            Some(0) => "plain", Some(1) => "frame_of_reference", Some(2) => "legacy_delta",
                            Some(3) => "dictionary", Some(4) => "run_length", Some(5) => "constant",
                            Some(6) => match bytes.get(offset + 9) { Some(255) => "chunk_local_packed_delta", Some(64) => "fixed_width_delta", _ => "column_wide_packed_delta" },
                            Some(7) => "experimental_zero_run_delta", Some(8) => "experimental_center_patched4",
                            _ => "unknown",
                        };
                        (json!(label), json!(encoded_path))
                    } else { (Value::Null, Value::Null) };
                    let compressed = frame.map(|f| f.compressed_size());
                    if let Some(size) = compressed { timestamp_frame_bytes += size; } else { unallocated_rows += rows; }
                    columns.push(json!({"id":id,"path":path,"rows":rows,"row_count":rows,"format":format,
                        "archive":archive,"schema":table.metadata().schema_id(),"table":table.table_index(),
                        "stream":stream_id,"column":column_index,"node":column.node_id(),
                        "current_compressed_bytes":compressed,"current_encoding":encoding,
                        "encoded_path":encoded_path,"mixed_integer":mixed,
                        "shared_frame":frame.is_none(),"minimum":min,"maximum":max,"sum":sum.to_string()}));
                }
                if !found_ts { return Err(format!("CAN table lacks ts: {archive:?} table {}", table.table_index()).into()); }
            }
        }
        if decoded_rows as u64 != metadata.record_count() || timestamp_rows != decoded_rows { return Err("archive record count mismatch".into()); }
        archives.push(json!({"path":archive,"format":format,"bytes":fs::metadata(&archive)?.len(),"rows":decoded_rows,
            "timestamp_frame_bytes":timestamp_frame_bytes,"unallocated_timestamp_rows":unallocated_rows}));
        eprintln!("{}: {} rows, {} individually attributable timestamp frame bytes", archive.display(), decoded_rows, timestamp_frame_bytes);
    }
    let result = json!({"columns":columns,"archives":archives,"format":format,
        "notes":"Each stream preserves physical schema-table row order. Shared-frame compressed bytes are null and excluded from timestamp byte totals; do not attribute an entire shared frame to ts. Encoded sidecars are pre-Zstd payloads including the mixed-integer flag when applicable."});
    let manifest = output.join(format!("{format}-columns.json"));
    fs::write(manifest, serde_json::to_string_pretty(&result)? + "\n")?;
    Ok(())
}
