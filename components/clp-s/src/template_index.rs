//! Per-table hinted-field template index, decoded once with archive metadata.
use serde::{Deserialize, Serialize};
pub const PACKET_TYPE: u8 = 0x82;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateIndex {
    pub version: u32,
    pub columns: Vec<TemplateColumn>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateColumn {
    pub schema: i32,
    pub node: u32,
    pub template: Vec<u8>,
}
impl TemplateIndex {
    /// Reject unsupported, ambiguous or trailing metadata; callers conservatively ignore it.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut decoder = rmp_serde::Deserializer::new(std::io::Cursor::new(bytes));
        let index = Self::deserialize(&mut decoder).ok()?;
        if index.version != 1
            || decoder.position() != bytes.len() as u64
            || index.columns.iter().any(|c| c.schema < 0)
            || index
                .columns
                .windows(2)
                .any(|w| (w[0].schema, w[0].node) >= (w[1].schema, w[1].node))
        {
            return None;
        }
        Some(index)
    }
    pub fn template(&self, schema: i32, node: u32) -> Option<&[u8]> {
        let i = self
            .columns
            .binary_search_by_key(&(schema, node), |c| (c.schema, c.node))
            .ok()?;
        Some(&self.columns[i].template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{ArchiveCatalogLimits, SingleFileArchiveReader};
    use crate::extract::{ExtractionMode, ExtractionOptions, extract_jsonl};
    use crate::search::{KqlLimits, SearchOptions, parse_kql};
    use crate::writer::{FieldRef, OpenArchive, RecordRef, ValueRef, WriterOptions};
    use std::io::Cursor;

    fn archive(enabled: bool) -> Vec<u8> {
        let mut writer = OpenArchive::new_with_template_keys(
            Cursor::new(Vec::new()),
            WriterOptions::default().with_adaptive_numeric_columns(true),
            if enabled {
                crate::writer::TemplateKeys::parse_csv("message").unwrap()
            } else {
                crate::writer::TemplateKeys::default()
            },
        );
        for message in [
            "alpha value 1",
            "beta value 2",
            "alpha value 3",
            "beta value 4",
        ] {
            writer
                .append_record(RecordRef::new(&[FieldRef::new(
                    b"message",
                    ValueRef::String(message.as_bytes()),
                )]))
                .unwrap();
        }
        writer.finish().unwrap().into_inner().into_inner()
    }

    #[test]
    fn split_templates_preserve_order_and_prune_only_positive_matches() {
        let bytes = archive(true);
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone())).unwrap();
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .unwrap();
        let index = catalog
            .metadata()
            .template_index()
            .expect("template metadata");
        assert_eq!(index.columns.len(), 2);
        assert_eq!(catalog.table_metadata().schema_tables().len(), 2);
        assert_eq!(catalog.table_metadata().packed_streams().len(), 2);
        for (query, expected) in [
            ("message:alpha*", 1),
            ("message:beta*", 1),
            ("message:absent*", 0),
            ("message:*value*", 2),
            ("NOT message:alpha*", 2),
            ("message:alpha* OR message:beta*", 2),
        ] {
            let parsed = parse_kql(query, KqlLimits::default()).unwrap();
            let compiled = parsed
                .compile_for_archive(&catalog, SearchOptions::default())
                .unwrap();
            let possible = catalog
                .schema_map()
                .schemas()
                .iter()
                .filter(|schema| compiled.may_match_schema(schema).unwrap())
                .count();
            assert_eq!(possible, expected, "{query}");
        }
        let mut extracted = Vec::new();
        let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
        extract_jsonl(
            &mut reader,
            &mut extracted,
            ExtractionOptions::new(ExtractionMode::LogOrder),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(extracted).unwrap(),
            "{\"message\":\"alpha value 1\"}\n{\"message\":\"beta value 2\"}\n{\"message\":\"alpha value 3\"}\n{\"message\":\"beta value 4\"}\n"
        );
        let mut reader = SingleFileArchiveReader::open(Cursor::new(archive(false))).unwrap();
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .unwrap();
        assert!(catalog.metadata().template_index().is_none());
        assert_eq!(catalog.table_metadata().schema_tables().len(), 1);
    }

    #[test]
    fn hints_select_nested_and_escaped_fields_and_partition_by_combination() {
        use crate::writer::TemplateKeys;
        for (hints, tables, columns) in [
            ("", 1, 0),
            ("event.body", 2, 2),
            (r"event.body,literal\.key", 4, 8),
        ] {
            let keys = if hints.is_empty() {
                TemplateKeys::default()
            } else {
                TemplateKeys::parse_csv(hints).unwrap()
            };
            let mut writer = OpenArchive::new_with_template_keys(
                Cursor::new(Vec::new()),
                WriterOptions::default(),
                keys,
            );
            let mut expected = Vec::new();
            for i in 0..16 {
                let body = format!("{} value {i}", if i % 2 == 0 { "alpha" } else { "beta" });
                let literal = format!("{} value {i}", if i % 4 < 2 { "left" } else { "right" });
                // A different unhinted message template for each record must not split tables.
                let message = format!("unhinted{} value {i}", (b'a' + i as u8) as char);
                let nested = [FieldRef::new(b"body", ValueRef::String(body.as_bytes()))];
                writer
                    .append_record(RecordRef::new(&[
                        FieldRef::new(b"event", ValueRef::Object(&nested)),
                        FieldRef::new(b"literal.key", ValueRef::String(literal.as_bytes())),
                        FieldRef::new(b"message", ValueRef::String(message.as_bytes())),
                    ]))
                    .unwrap();
                expected.push(serde_json::json!({"event":{"body":body},"literal.key":literal,"message":message}));
            }
            let bytes = writer.finish().unwrap().into_inner().into_inner();
            let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes.clone())).unwrap();
            let catalog = reader
                .read_catalog(ArchiveCatalogLimits::default())
                .unwrap();
            assert_eq!(
                catalog.table_metadata().schema_tables().len(),
                tables,
                "{hints}"
            );
            assert_eq!(
                catalog
                    .metadata()
                    .template_index()
                    .map_or(0, |i| i.columns.len()),
                columns,
                "{hints}"
            );
            if columns == 8 {
                for (query, possible) in [
                    ("event.body:alpha*", 2),
                    (r"event.body:alpha* AND literal\.key:left*", 1),
                    (r"literal\.key:absent*", 0),
                ] {
                    let parsed = parse_kql(query, KqlLimits::default()).unwrap();
                    let compiled = parsed
                        .compile_for_archive(&catalog, SearchOptions::default())
                        .unwrap();
                    assert_eq!(
                        catalog
                            .schema_map()
                            .schemas()
                            .iter()
                            .filter(|s| compiled.may_match_schema(s).unwrap())
                            .count(),
                        possible,
                        "{query}"
                    );
                }
            }
            let mut extracted = Vec::new();
            let mut reader = SingleFileArchiveReader::open(Cursor::new(bytes)).unwrap();
            extract_jsonl(
                &mut reader,
                &mut extracted,
                ExtractionOptions::new(ExtractionMode::LogOrder),
            )
            .unwrap();
            let actual: Vec<serde_json::Value> = std::str::from_utf8(&extracted)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn unusable_template_metadata_is_not_used_for_pruning() {
        let index = TemplateIndex {
            version: 1,
            columns: vec![TemplateColumn {
                schema: 0,
                node: 2,
                template: b"hello".to_vec(),
            }],
        };
        let mut encoded = rmp_serde::to_vec(&index).unwrap();
        assert!(TemplateIndex::decode(&encoded).is_some());
        encoded.push(0);
        assert!(TemplateIndex::decode(&encoded).is_none());
        let mut bad = index.clone();
        bad.version = 2;
        assert!(TemplateIndex::decode(&rmp_serde::to_vec(&bad).unwrap()).is_none());
        bad = index.clone();
        bad.columns.push(index.columns[0].clone());
        assert!(TemplateIndex::decode(&rmp_serde::to_vec(&bad).unwrap()).is_none());
    }
}
