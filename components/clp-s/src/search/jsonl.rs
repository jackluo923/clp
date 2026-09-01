//! Streaming JSONL projection over archive-wide physical search matches.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;

use super::ArchiveMatchSink;
use super::ArchiveTableMatches;
use super::Projection;
use super::ProjectionError;
use super::projection::ResolvedProjection;
use crate::ExtractionPlan;
use crate::ExtractionPlanError;
use crate::ExtractionPlanLimits;
use crate::JsonlRecord;
use crate::JsonlRecordSink;
use crate::RecordBindError;
use crate::RecordCompileError;
use crate::RecordError;
use crate::RecordLimits;
use crate::RecordProgram;
use crate::RecordScratch;
use crate::json::JsonBytePolicy;

/// Projection and record-formatting configuration for [`SearchJsonlAdapter`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchJsonlOptions {
    projection: Projection,
    plan: ExtractionPlanLimits,
    record: RecordLimits,
    byte_policy: JsonBytePolicy,
}

impl SearchJsonlOptions {
    /// Creates JSONL output options for the given projection.
    #[must_use]
    pub fn new(projection: Projection) -> Self {
        Self {
            projection,
            plan: ExtractionPlanLimits::default(),
            record: RecordLimits::default(),
            byte_policy: JsonBytePolicy::StrictUtf8,
        }
    }

    /// Replaces extraction-plan compilation limits.
    #[must_use]
    pub const fn with_plan_limits(mut self, limits: ExtractionPlanLimits) -> Self {
        self.plan = limits;
        self
    }

    /// Replaces record-program and per-record limits.
    #[must_use]
    pub const fn with_record_limits(mut self, limits: RecordLimits) -> Self {
        self.record = limits;
        self
    }

    /// Selects strict UTF-8 or explicit C++ byte-preserving output.
    #[must_use]
    pub const fn with_byte_policy(mut self, byte_policy: JsonBytePolicy) -> Self {
        self.byte_policy = byte_policy;
        self
    }

    /// Returns the archive-independent result projection.
    #[must_use]
    pub const fn projection(&self) -> &Projection {
        &self.projection
    }

    /// Returns extraction-plan compilation limits.
    #[must_use]
    pub const fn plan_limits(&self) -> ExtractionPlanLimits {
        self.plan
    }

    /// Returns record-program and per-record limits.
    #[must_use]
    pub const fn record_limits(&self) -> RecordLimits {
        self.record
    }

    /// Returns the archive-byte output policy.
    #[must_use]
    pub const fn byte_policy(&self) -> JsonBytePolicy {
        self.byte_policy
    }
}

impl Default for SearchJsonlOptions {
    fn default() -> Self {
        Self::new(Projection::all())
    }
}

/// Adapts typed physical search batches into borrowed JSONL records.
///
/// Create one adapter per [`super::search_archive`] call. Projection paths are resolved once for
/// that archive, record programs are compiled once per matching schema, and only one reusable
/// record buffer plus one table-bound cursor set is retained. Unmatched rows advance stateful
/// columns without formatting JSON.
pub struct SearchJsonlAdapter<'sink, 'options, S: ?Sized> {
    sink: &'sink mut S,
    options: &'options SearchJsonlOptions,
    projection: Option<ResolvedProjection>,
    programs: HashMap<i32, RecordProgram>,
    scratch: RecordScratch,
    record: Vec<u8>,
}

impl<'sink, 'options, S: JsonlRecordSink + ?Sized> SearchJsonlAdapter<'sink, 'options, S> {
    /// Creates an adapter borrowing a synchronous record sink and immutable configuration.
    #[must_use]
    pub fn new(sink: &'sink mut S, options: &'options SearchJsonlOptions) -> Self {
        Self {
            sink,
            options,
            projection: None,
            programs: HashMap::new(),
            scratch: RecordScratch::new(),
            record: Vec::new(),
        }
    }

    /// Returns the caller-owned sink.
    #[must_use]
    pub const fn sink_mut(&mut self) -> &mut S {
        self.sink
    }

    fn prepare_projection(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchJsonlAdapterError> {
        if self.projection.is_none() {
            self.projection = Some(
                self.options
                    .projection()
                    .resolve(matches.catalog().schema_tree())
                    .map_err(SearchJsonlAdapterError::Projection)?,
            );
        }
        Ok(())
    }

    fn compile_program(
        &self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<RecordProgram, SearchJsonlAdapterError> {
        let schema_id = matches.schema_id();
        let plan = ExtractionPlan::compile(
            matches.table().schema(),
            matches.catalog().schema_tree(),
            self.options.plan_limits(),
        )
        .map_err(|source| SearchJsonlAdapterError::Plan { schema_id, source })?;
        let plan = match self
            .projection
            .as_ref()
            .and_then(ResolvedProjection::selected_node_ids)
        {
            Some(node_ids) => plan
                .project_selected_nodes(node_ids)
                .map_err(|source| SearchJsonlAdapterError::Plan { schema_id, source })?,
            None => plan,
        };
        RecordProgram::compile_with_byte_policy(
            &plan,
            matches.catalog().schema_tree(),
            self.options.byte_policy(),
            self.options.record_limits(),
        )
        .map_err(|source| SearchJsonlAdapterError::Program { schema_id, source })
    }

    fn prepare_program(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchJsonlAdapterError> {
        let schema_id = matches.schema_id();
        if self.programs.contains_key(&schema_id) {
            return Ok(());
        }
        let program = self.compile_program(matches)?;
        self.programs
            .try_reserve(1)
            .map_err(|_| SearchJsonlAdapterError::AllocationFailed {
                requested: self.programs.len().saturating_add(1),
            })?;
        self.programs.insert(schema_id, program);
        Ok(())
    }

    fn write_jsonl(
        &mut self,
        matches: ArchiveTableMatches<'_, '_, '_>,
    ) -> Result<(), SearchJsonlAdapterError> {
        self.prepare_projection(matches)?;
        self.prepare_program(matches)?;
        let schema_id = matches.schema_id();
        let program = self
            .programs
            .get(&schema_id)
            .ok_or(SearchJsonlAdapterError::SizeOverflow)?;
        let scratch = std::mem::take(&mut self.scratch);
        let mut writer = program
            .writer_with_scratch(
                matches.table().table(),
                matches.catalog().timestamp_patterns(),
                scratch,
            )
            .map_err(|source| SearchJsonlAdapterError::Bind { schema_id, source })?;

        let mut pending_skips = 0_usize;
        for (row_index, matched) in matches.bitmap().as_bytes().iter().copied().enumerate() {
            if 0 == matched {
                pending_skips = pending_skips
                    .checked_add(1)
                    .ok_or(SearchJsonlAdapterError::SizeOverflow)?;
                continue;
            }

            let skip_start = row_index
                .checked_sub(pending_skips)
                .ok_or(SearchJsonlAdapterError::SizeOverflow)?;
            if !writer.skip_records(pending_skips).map_err(|source| {
                SearchJsonlAdapterError::Record {
                    schema_id,
                    row_index: skip_start,
                    source,
                }
            })? {
                return Err(SearchJsonlAdapterError::RowCountMismatch {
                    schema_id,
                    expected: matches.bitmap().len(),
                    actual: writer.next_row_index(),
                });
            }
            pending_skips = 0;

            self.record.clear();
            if !writer
                .append_next_record(&mut self.record)
                .map_err(|source| SearchJsonlAdapterError::Record {
                    schema_id,
                    row_index,
                    source,
                })?
            {
                return Err(SearchJsonlAdapterError::RowCountMismatch {
                    schema_id,
                    expected: matches.bitmap().len(),
                    actual: row_index,
                });
            }
            self.record
                .try_reserve(1)
                .map_err(|_| SearchJsonlAdapterError::AllocationFailed {
                    requested: self.record.len().saturating_add(1),
                })?;
            self.record.push(b'\n');
            self.sink
                .write_record(JsonlRecord::new(
                    &self.record,
                    matches.table_index(),
                    row_index,
                    None,
                ))
                .map_err(|source| SearchJsonlAdapterError::Output {
                    schema_id,
                    row_index,
                    source,
                })?;
        }
        if !writer.skip_records(pending_skips).map_err(|source| {
            SearchJsonlAdapterError::Record {
                schema_id,
                row_index: writer.next_row_index(),
                source,
            }
        })? {
            return Err(SearchJsonlAdapterError::RowCountMismatch {
                schema_id,
                expected: matches.bitmap().len(),
                actual: writer.next_row_index(),
            });
        }
        if 0 != writer.remaining() {
            return Err(SearchJsonlAdapterError::RowCountMismatch {
                schema_id,
                expected: matches.bitmap().len(),
                actual: matches.bitmap().len() - writer.remaining(),
            });
        }
        self.scratch = writer.into_scratch();
        Ok(())
    }
}

impl<S: JsonlRecordSink + ?Sized> ArchiveMatchSink for SearchJsonlAdapter<'_, '_, S> {
    fn write_matches(&mut self, matches: ArchiveTableMatches<'_, '_, '_>) -> io::Result<()> {
        self.write_jsonl(matches).map_err(io::Error::other)
    }
}

/// Failure while projecting or formatting an accepted search batch as JSONL.
#[derive(Debug)]
#[non_exhaustive]
pub enum SearchJsonlAdapterError {
    /// Resolving selected descriptors against the archive failed.
    Projection(ProjectionError),
    /// Compiling or pruning one schema extraction plan failed.
    Plan {
        /// Opaque schema ID.
        schema_id: i32,
        /// Plan failure.
        source: ExtractionPlanError,
    },
    /// Compiling a reusable record program failed.
    Program {
        /// Opaque schema ID.
        schema_id: i32,
        /// Program failure.
        source: RecordCompileError,
    },
    /// Binding a compiled program to one decoded table failed.
    Bind {
        /// Opaque schema ID.
        schema_id: i32,
        /// Bind failure.
        source: RecordBindError,
    },
    /// Formatting or advancing one physical row failed.
    Record {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
        /// Record failure.
        source: RecordError,
    },
    /// The decoded table and bound record writer disagreed on row count.
    RowCountMismatch {
        /// Opaque schema ID.
        schema_id: i32,
        /// Match-bitmap row count.
        expected: usize,
        /// Rows accepted or skipped by the writer.
        actual: usize,
    },
    /// The caller's JSONL sink rejected one complete record.
    Output {
        /// Opaque schema ID.
        schema_id: i32,
        /// Physical table-local row index.
        row_index: usize,
        /// Sink failure.
        source: io::Error,
    },
    /// A bounded adapter allocation failed.
    AllocationFailed {
        /// Requested element or byte count.
        requested: usize,
    },
    /// Checked size arithmetic overflowed.
    SizeOverflow,
}

impl Display for SearchJsonlAdapterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projection(source) => write!(formatter, "failed to resolve projection: {source}"),
            Self::Plan { schema_id, source } => {
                write!(
                    formatter,
                    "failed to compile projection for schema {schema_id}: {source}"
                )
            }
            Self::Program { schema_id, source } => {
                write!(
                    formatter,
                    "failed to compile JSON for schema {schema_id}: {source}"
                )
            }
            Self::Bind { schema_id, source } => {
                write!(
                    formatter,
                    "failed to bind JSON writer for schema {schema_id}: {source}"
                )
            }
            Self::Record {
                schema_id,
                row_index,
                source,
            } => write!(
                formatter,
                "failed to format schema {schema_id}, row {row_index}: {source}"
            ),
            Self::RowCountMismatch {
                schema_id,
                expected,
                actual,
            } => write!(
                formatter,
                "schema {schema_id} advertised {expected} rows but JSON consumed {actual}"
            ),
            Self::Output {
                schema_id,
                row_index,
                source,
            } => write!(
                formatter,
                "JSONL sink failed for schema {schema_id}, row {row_index}: {source}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "failed to allocate {requested} element(s) for search JSONL output"
            ),
            Self::SizeOverflow => formatter.write_str("search JSONL size arithmetic overflow"),
        }
    }
}

impl Error for SearchJsonlAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(source) => Some(source),
            Self::Plan { source, .. } => Some(source),
            Self::Program { source, .. } => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::Record { source, .. } => Some(source),
            Self::Output { source, .. } => Some(source),
            Self::RowCountMismatch { .. } | Self::AllocationFailed { .. } | Self::SizeOverflow => {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::SingleFileArchiveReader;
    use crate::search::ArchiveSearchOptions;
    use crate::search::KqlLimits;
    use crate::search::ProjectionLimits;
    use crate::search::parse_kql;
    use crate::search::search_archive;

    const CPP_MULTI_SCHEMA_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-log-order-cpp.bin");
    const CPP_MINIMAL_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");
    const STRUCTURED_ARRAY_FIXTURE_HEX: &str =
        include_str!("../../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp.hex");
    const STRUCTURED_ARRAY_EXPECTED: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-structured-arrays-cpp-search.jsonl");

    fn decode_hex(source: &str) -> Vec<u8> {
        let digits: Vec<u8> = source
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        let (pairs, remainder) = digits.as_chunks::<2>();
        let bytes = pairs
            .iter()
            .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
            .collect();
        assert!(remainder.is_empty(), "hex fixture has even length");
        bytes
    }

    fn hex_nibble(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => panic!("invalid hex fixture byte"),
        }
    }

    #[derive(Default)]
    struct ByteSink(Vec<u8>);

    impl JsonlRecordSink for ByteSink {
        fn write_record(&mut self, record: JsonlRecord<'_>) -> io::Result<()> {
            self.0.extend_from_slice(record.jsonl_bytes());
            Ok(())
        }
    }

    fn search_fixture(
        fixture: &[u8],
        query: &str,
        options: &SearchJsonlOptions,
    ) -> (crate::search::ArchiveSearchStats, Vec<u8>) {
        let query = parse_kql(query, KqlLimits::default()).expect("parse query");
        let mut reader = SingleFileArchiveReader::open(Cursor::new(fixture)).expect("open fixture");
        let mut sink = ByteSink::default();
        let mut adapter = SearchJsonlAdapter::new(&mut sink, options);
        let stats = search_archive(
            &mut reader,
            &query,
            &mut adapter,
            &ArchiveSearchOptions::default(),
        )
        .expect("search and project");
        (stats, sink.0)
    }

    #[test]
    fn full_records_follow_cpp_physical_schema_order() {
        let (stats, output) = search_fixture(
            CPP_MULTI_SCHEMA_FIXTURE,
            "a:* OR b:* OR c:*",
            &SearchJsonlOptions::default(),
        );
        assert_eq!(6, stats.matching_rows());
        assert_eq!(
            b"{\"a\":10}\n{\"a\":20}\n{\"a\":30}\n{\"b\":true}\n{\"b\":false}\n{\"c\":\"x\"}\n",
            output.as_slice()
        );
    }

    #[test]
    fn selected_fields_keep_schema_order_and_missing_fields_emit_empty_objects() {
        let columns = ["message", "ts", "missing"];
        let projection =
            Projection::selected(&columns, ProjectionLimits::default()).expect("projection");
        let options = SearchJsonlOptions::new(projection);
        let (stats, output) = search_fixture(CPP_MINIMAL_FIXTURE, "level:INFO", &options);
        assert_eq!(1, stats.matching_rows());
        assert_eq!(
            b"{\"ts\":1700000000123,\"message\":\"oracle fixture\"}\n",
            output.as_slice()
        );

        let projection = Projection::selected(&["missing"], ProjectionLimits::default())
            .expect("missing projection");
        let (_, output) = search_fixture(
            CPP_MULTI_SCHEMA_FIXTURE,
            "a:* OR b:* OR c:*",
            &SearchJsonlOptions::new(projection),
        );
        assert_eq!(b"{}\n{}\n{}\n{}\n{}\n{}\n", output.as_slice());
    }

    #[test]
    fn structured_array_matches_emit_exact_cpp_records_and_projection_no_ops() {
        let fixture = decode_hex(STRUCTURED_ARRAY_FIXTURE_HEX);
        let (stats, output) = search_fixture(&fixture, "*:*", &SearchJsonlOptions::default());
        assert_eq!(9, stats.matching_rows());
        assert_eq!(STRUCTURED_ARRAY_EXPECTED, output);

        let columns = ["id", "items", "items.x", "obj", "obj.items.x"];
        let projection =
            Projection::selected(&columns, ProjectionLimits::default()).expect("projection");
        let (_, projected) = search_fixture(&fixture, "*:*", &SearchJsonlOptions::new(projection));
        assert_eq!(
            b"{\"id\":0}\n{\"id\":1}\n{\"id\":2}\n{\"id\":7}\n{\"id\":6}\n\
              {\"id\":8}\n{\"id\":3}\n{\"id\":4}\n{\"id\":5}\n",
            projected.as_slice()
        );
    }

    struct FailSecondRecord(usize);

    impl JsonlRecordSink for FailSecondRecord {
        fn write_record(&mut self, _record: JsonlRecord<'_>) -> io::Result<()> {
            if 1 == self.0 {
                return Err(io::Error::other("destination closed"));
            }
            self.0 += 1;
            Ok(())
        }
    }

    #[test]
    fn output_errors_retain_archive_table_schema_and_row_context() {
        let query = parse_kql("a:*", KqlLimits::default()).expect("parse query");
        let mut reader = SingleFileArchiveReader::open(Cursor::new(CPP_MULTI_SCHEMA_FIXTURE))
            .expect("open fixture");
        let mut sink = FailSecondRecord(0);
        let options = SearchJsonlOptions::default();
        let mut adapter = SearchJsonlAdapter::new(&mut sink, &options);
        let error = search_archive(
            &mut reader,
            &query,
            &mut adapter,
            &ArchiveSearchOptions::default(),
        )
        .expect_err("second matching record must fail");
        let (stream_id, table_index, schema_id, source) = match error {
            crate::search::ArchiveSearchError::Sink {
                stream_id,
                table_index,
                schema_id,
                source,
            } => (stream_id, table_index, schema_id, source),
            other => panic!("unexpected search failure: {other}"),
        };
        assert_eq!(0, stream_id);
        assert_eq!(0, table_index);
        let adapter = source
            .get_ref()
            .and_then(|source| source.downcast_ref::<SearchJsonlAdapterError>())
            .expect("typed adapter source");
        assert!(matches!(
            adapter,
            SearchJsonlAdapterError::Output {
                schema_id: output_schema,
                row_index: 1,
                source,
            } if schema_id == *output_schema && "destination closed" == source.to_string()
        ));
    }
}
