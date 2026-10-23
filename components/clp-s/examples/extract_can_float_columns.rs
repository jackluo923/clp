//! Research helper: extract every physical binary64 column and its frame provenance.
//! Usage: extract_can_float_columns OUTPUT_DIRECTORY FORMAT ARCHIVE...
use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
};

use clp_s::archive::{
    ArchiveCatalogLimits, ColumnData, ColumnLimits, PackedStreamLimits, SingleFileArchiveReader,
};
use serde_json::{Value, json};

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
        let mut float_values = 0usize;
        let mut float_frame_bytes = 0u64;
        let mut unallocated_values = 0usize;
        let first_column = columns.len();
        for stream_index in 0..metadata.packed_streams().len() {
            let stream_id = stream_index as u64;
            let separate = metadata.separate_columns_for(stream_id);
            let raw = reader.read_packed_stream(catalog.metadata(), metadata, stream_index, PackedStreamLimits::default())?;
            // Grouped streams also have a column_layout before materialization, but their
            // slots describe shared nodes, not individual tables' physical columns. Only
            // separate-column streams have a one-to-one slot/frame/table-column mapping.
            let raw_columns = if separate.is_some() {
                let layout = raw.column_layout().ok_or("separate stream lacks layout")?;
                if layout.len() != separate.unwrap().columns().len() {
                    return Err("separate frame/layout count mismatch".into());
                }
                let mut offset = 0usize;
                let mut bytes = Vec::with_capacity(layout.len());
                for slot in layout {
                    let end = offset.checked_add(slot.stored).ok_or("column size overflow")?;
                    bytes.push(raw.as_bytes().get(offset..end).ok_or("column bounds")?.to_vec());
                    offset = end;
                }
                if offset != raw.len() { return Err("column layout does not consume stream".into()); }
                Some(bytes)
            } else { None };
            let decoded = catalog.materialize_adaptive_numerics(stream_id, raw, ColumnLimits::default())?;
            let mut tables_in_stream = 0usize;
            for table in catalog.schema_tables(stream_id, &decoded, ColumnLimits::default())? {
                let table = table?;
                tables_in_stream += 1;
                if let Some(frames) = separate {
                    if tables_in_stream != 1 || table.table().columns().len() != frames.columns().len() {
                        return Err("separate stream is not one table with matching column frames".into());
                    }
                }
                let rows = table.table().message_count();
                decoded_rows += rows;
                for (column_index, column) in table.table().columns().iter().enumerate() {
                    let values = match column.data() {
                        ColumnData::Float(values) => values,
                        ColumnData::FormattedFloat(_) | ColumnData::DictionaryFloat(_) => {
                            return Err("noncanonical float column requires a separate lexeme-preserving experiment".into());
                        }
                        _ => continue,
                    };
                    let node = catalog.schema_tree().get(column.node_id() as usize).ok_or("column node")?;
                    let id = format!("{format}-{name}-t{}-c{column_index}", table.table_index());
                    let path = output.join(format!("{id}.u64le"));
                    let mut file = BufWriter::new(File::create(&path)?);
                    let mut written = 0usize;
                    for value in values.iter() {
                        file.write_all(&value.to_bits().to_le_bytes())?;
                        written += 1;
                    }
                    file.flush()?;
                    if written != rows || fs::metadata(&path)?.len() != rows as u64 * 8 {
                        return Err("decoded float row count mismatch".into());
                    }
                    float_values += rows;
                    let frame = separate.and_then(|s| s.columns().get(column_index));
                    let encoded = raw_columns.as_ref().and_then(|c| c.get(column_index));
                    let (encoding, encoded_path, encoded_bytes) = if let Some(bytes) = encoded {
                        let encoded_path = output.join(format!("{id}.encoded"));
                        fs::write(&encoded_path, bytes)?;
                        let label = match bytes.first() {
                            Some(0) => "plain", Some(1) => "byte_stream_split", Some(2) => "dictionary",
                            Some(3) => "constant", Some(4) => "run_length", Some(5) => "decimal",
                            _ => return Err("unknown adaptive float encoding".into()),
                        };
                        (json!(label), json!(encoded_path), json!(bytes.len()))
                    } else { (Value::Null, Value::Null, Value::Null) };
                    let compressed = frame.map(|f| f.compressed_size());
                    if let Some(size) = compressed { float_frame_bytes += size; } else { unallocated_values += rows; }
                    columns.push(json!({"id":id,"path":path,"rows":rows,"row_count":rows,"format":format,
                        "archive":archive,"schema":table.metadata().schema_id(),"table":table.table_index(),
                        "stream":stream_id,"column":column_index,"node":column.node_id(),
                        "key":std::str::from_utf8(node.key_bytes())?,"node_type":"Float",
                        "current_compressed_bytes":compressed,"current_encoding":encoding,
                        "encoded_path":encoded_path,"encoded_bytes":encoded_bytes,"raw_bytes":rows as u64 * 8,
                        "shared_frame":frame.is_none()}));
                    eprintln!("extracted {id}: {rows} float values");
                }
            }
        }
        if decoded_rows as u64 != metadata.record_count() { return Err("archive record count mismatch".into()); }
        archives.push(json!({"path":archive,"format":format,"bytes":fs::metadata(&archive)?.len(),"rows":decoded_rows,
            "float_columns":columns.len()-first_column,"float_values":float_values,
            "float_frame_bytes":float_frame_bytes,"unallocated_float_values":unallocated_values}));
        eprintln!("{}: {} rows, {} float values, {} attributable float frame bytes", archive.display(), decoded_rows, float_values, float_frame_bytes);
    }
    let result = json!({"columns":columns,"archives":archives,"format":format,
        "notes":"Values retain physical schema-table order and original f64 bits. Separate-column slots are matched by physical column index only after checking one table per stream and equal layout/frame/column counts. Shared-frame compressed bytes and encoded sidecars are null; their full frames are never attributed to one column. Encoded sidecars contain exact decompressed adaptive payloads before Zstd."});
    fs::write(output.join(format!("{format}-columns.json")), serde_json::to_string_pretty(&result)? + "\n")?;
    Ok(())
}
