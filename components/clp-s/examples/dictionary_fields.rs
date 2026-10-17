//! Reports which logtype-dictionary entries each field of an archive references.
//!
//! Prints one JSON object per line: `{"field": <dot path>, "logtypes": [<id>, ...]}`, one line per
//! field whose CLP-string columns reference `/log.dict`, then `{"entries": <count>}`. Used to
//! measure what per-field dictionaries would hold and how much fields share.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

use clp_s::archive::ArchiveCatalogLimits;
use clp_s::archive::ColumnData;
use clp_s::archive::ColumnLimits;
use clp_s::archive::DirectoryArchiveReader;
use clp_s::archive::FsDirectoryArchiveSource;
use clp_s::archive::MetadataLimits;
use clp_s::archive::PackedStreamLimits;
use clp_s::archive::SchemaTree;

fn field_path(tree: &SchemaTree, node_id: u32) -> String {
    let mut keys = Vec::new();
    let mut current = Some(node_id as usize);
    while let Some(id) = current {
        let Some(node) = tree.get(id) else { break };
        if node.parent_id().is_some() {
            keys.push(String::from_utf8_lossy(node.key_bytes()).into_owned());
        }
        current = node.parent_id();
    }
    keys.reverse();
    keys.join(".")
}

fn main() -> Result<(), Box<dyn Error>> {
    let path = env::args_os().nth(1).map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: dictionary_fields ARCHIVE_DIR",
        )
    })?;
    let source = FsDirectoryArchiveSource::new(&path);
    let mut reader = DirectoryArchiveReader::open(source, MetadataLimits::default())?;
    let catalog = reader.read_catalog(ArchiveCatalogLimits::default())?;
    let columns = ColumnLimits::default();
    let mut fields: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for stream_index in 0..catalog.table_metadata().packed_streams().len() {
        let stream_id = stream_index as u64;
        let stream = reader.read_packed_stream(
            catalog.metadata(),
            catalog.table_metadata(),
            stream_index,
            PackedStreamLimits::default(),
        )?;
        let stream = catalog.materialize_adaptive_numerics(stream_id, stream, columns)?;
        for table in catalog.schema_tables(stream_id, &stream, columns)? {
            let table = table?;
            for column in table.table().columns() {
                let ColumnData::ClpString(clp) = column.data() else {
                    continue;
                };
                let ids = fields
                    .entry(field_path(catalog.schema_tree(), column.node_id()))
                    .or_default();
                for row in 0..clp.len() {
                    if let Some(descriptor) = clp.descriptor(row) {
                        ids.insert(descriptor.logtype_id());
                    }
                }
            }
        }
    }
    for (field, ids) in &fields {
        let escaped = field.replace('\\', "\\\\").replace('"', "\\\"");
        println!(
            "{{\"field\":\"{escaped}\",\"logtypes\":{:?}}}",
            ids.iter().collect::<Vec<_>>()
        );
    }
    println!("{{\"entries\":{}}}", catalog.log_type_dictionary().len());
    Ok(())
}
