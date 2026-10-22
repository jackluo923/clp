//! Runtime hints selecting string fields whose templates partition physical tables.
use crate::ingest::KvIrTimestampResolver;
use std::sync::Arc;

/// Parsed, namespace-aware field paths, shared by every archive in a compression session.
/// No hints means no template-based partitioning. Paths use the timestamp-key grammar.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TemplateKeys {
    paths: Arc<[KvIrTimestampResolver]>,
}

impl TemplateKeys {
    /// Parses a comma-separated list of dotted paths. Whitespace around paths is ignored;
    /// duplicate decoded paths are removed. A literal comma can be written as `\u002c`.
    ///
    /// # Errors
    /// Returns an error for an empty entry, wildcard, invalid escape, or reserved namespace.
    pub fn parse_csv(csv: &str) -> Result<Self, String> {
        let mut paths: Vec<KvIrTimestampResolver> = Vec::new();
        for descriptor in csv.split(',').map(str::trim) {
            if descriptor.is_empty() {
                return Err("--template-keys contains an empty field path".into());
            }
            let path = KvIrTimestampResolver::parse(descriptor)
                .map_err(|error| format!("invalid --template-keys path `{descriptor}`: {error}"))?;
            if path.namespace().is_none() {
                return Err(format!(
                    "--template-keys path `{descriptor}` uses a reserved metadata namespace"
                ));
            }
            if !paths.iter().any(|p| {
                p.namespace() == path.namespace()
                    && p.path().components() == path.path().components()
            }) {
                paths.push(path);
            }
        }
        Ok(Self {
            paths: paths.into(),
        })
    }

    pub(crate) fn paths(&self) -> &[KvIrTimestampResolver] {
        &self.paths
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_uses_timestamp_path_grammar_and_deduplicates_decoded_paths() {
        let keys = TemplateKeys::parse_csv(r"body, event.text, literal\.key, body, @generated.text, literal\u002ekey, comma\u002ckey").unwrap();
        assert_eq!(keys.paths().len(), 5);
        assert_eq!(keys.paths()[2].path().components(), ["literal.key"]);
        assert_eq!(keys.paths()[4].path().components(), ["comma,key"]);
        for bad in ["", " ", "a,", ",a", "a,,b", "a..b", "a.*", r"a\q", "$field"] {
            assert!(TemplateKeys::parse_csv(bad).is_err(), "{bad}");
        }
    }
}
