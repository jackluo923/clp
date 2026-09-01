use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Read;
use std::io::Seek;

use super::column::ColumnLimits;
use super::dictionary::ArrayDictionary;
use super::dictionary::DictionaryError;
use super::dictionary::DictionaryLimits;
use super::dictionary::LogTypeDictionary;
use super::dictionary::VariableDictionary;
use super::metadata::ArchiveMetadata;
use super::metadata::MetadataError;
use super::metadata::MetadataLimits;
use super::packed_stream::DecodedPackedStream;
use super::range_index::RangeIndexError;
use super::reader::SingleFileArchiveReader;
use super::schema::NodeType;
use super::schema_map::SchemaMap;
use super::schema_map::SchemaMapError;
use super::schema_map::SchemaMapLimits;
use super::schema_tree::SchemaTree;
use super::schema_tree::SchemaTreeError;
use super::schema_tree::SchemaTreeLimits;
use super::table_metadata::TableMetadata;
use super::table_metadata::TableMetadataError;
use super::table_metadata::TableMetadataLimits;
use super::table_stream::SchemaTableStream;
use super::table_stream::TableStreamError;
use crate::timestamp_catalog::TimestampPatternCatalog;
use crate::timestamp_catalog::TimestampPatternCatalogError;
use crate::timestamp_catalog::TimestampPatternCatalogLimits;

/// Resource limits used to load the validated non-table contents of an archive.
///
/// Each section retains an independent policy so a binding or service can tune large variable
/// values without also weakening limits for logtypes, schemas, or metadata.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveCatalogLimits {
    metadata: MetadataLimits,
    schema_tree: SchemaTreeLimits,
    schema_map: SchemaMapLimits,
    table_metadata: TableMetadataLimits,
    variable_dictionary: DictionaryLimits,
    log_type_dictionary: DictionaryLimits,
    array_dictionary: DictionaryLimits,
    timestamp_patterns: TimestampPatternCatalogLimits,
}

impl ArchiveCatalogLimits {
    /// Replaces the archive-metadata limits.
    #[must_use]
    pub const fn with_metadata(mut self, limits: MetadataLimits) -> Self {
        self.metadata = limits;
        self
    }

    /// Replaces the schema-tree limits.
    #[must_use]
    pub const fn with_schema_tree(mut self, limits: SchemaTreeLimits) -> Self {
        self.schema_tree = limits;
        self
    }

    /// Replaces the schema-map limits.
    #[must_use]
    pub const fn with_schema_map(mut self, limits: SchemaMapLimits) -> Self {
        self.schema_map = limits;
        self
    }

    /// Replaces the table-metadata limits.
    #[must_use]
    pub const fn with_table_metadata(mut self, limits: TableMetadataLimits) -> Self {
        self.table_metadata = limits;
        self
    }

    /// Replaces the `/var.dict` limits.
    #[must_use]
    pub const fn with_variable_dictionary(mut self, limits: DictionaryLimits) -> Self {
        self.variable_dictionary = limits;
        self
    }

    /// Replaces the `/log.dict` limits.
    #[must_use]
    pub const fn with_log_type_dictionary(mut self, limits: DictionaryLimits) -> Self {
        self.log_type_dictionary = limits;
        self
    }

    /// Replaces the `/array.dict` limits.
    #[must_use]
    pub const fn with_array_dictionary(mut self, limits: DictionaryLimits) -> Self {
        self.array_dictionary = limits;
        self
    }

    /// Replaces the limits for precompiling timestamp patterns used during extraction.
    #[must_use]
    pub const fn with_timestamp_patterns(mut self, limits: TimestampPatternCatalogLimits) -> Self {
        self.timestamp_patterns = limits;
        self
    }

    /// Returns the archive-metadata limits.
    #[must_use]
    pub const fn metadata(self) -> MetadataLimits {
        self.metadata
    }

    /// Returns the schema-tree limits.
    #[must_use]
    pub const fn schema_tree(self) -> SchemaTreeLimits {
        self.schema_tree
    }

    /// Returns the schema-map limits.
    #[must_use]
    pub const fn schema_map(self) -> SchemaMapLimits {
        self.schema_map
    }

    /// Returns the table-metadata limits.
    #[must_use]
    pub const fn table_metadata(self) -> TableMetadataLimits {
        self.table_metadata
    }

    /// Returns the `/var.dict` limits.
    #[must_use]
    pub const fn variable_dictionary(self) -> DictionaryLimits {
        self.variable_dictionary
    }

    /// Returns the `/log.dict` limits.
    #[must_use]
    pub const fn log_type_dictionary(self) -> DictionaryLimits {
        self.log_type_dictionary
    }

    /// Returns the `/array.dict` limits.
    #[must_use]
    pub const fn array_dictionary(self) -> DictionaryLimits {
        self.array_dictionary
    }

    /// Returns the limits for precompiling timestamp patterns.
    #[must_use]
    pub const fn timestamp_patterns(self) -> TimestampPatternCatalogLimits {
        self.timestamp_patterns
    }
}

/// Fully validated archive metadata, schemas, table layout, and dictionaries.
///
/// Packed table bytes are deliberately not retained here. Call
/// [`SingleFileArchiveReader::read_packed_stream`] as streams are needed so extraction and search
/// can reuse one bounded buffer instead of materializing the complete archive.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveCatalog {
    metadata: ArchiveMetadata,
    schema_tree: SchemaTree,
    schema_map: SchemaMap,
    table_metadata: TableMetadata,
    variable_dictionary: VariableDictionary,
    log_type_dictionary: LogTypeDictionary,
    array_dictionary: ArrayDictionary,
    timestamp_patterns: TimestampPatternCatalog,
}

impl ArchiveCatalog {
    /// Returns the archive metadata and validated physical section directory.
    #[must_use]
    pub const fn metadata(&self) -> &ArchiveMetadata {
        &self.metadata
    }

    /// Returns the schema tree.
    #[must_use]
    pub const fn schema_tree(&self) -> &SchemaTree {
        &self.schema_tree
    }

    /// Returns the schema map.
    #[must_use]
    pub const fn schema_map(&self) -> &SchemaMap {
        &self.schema_map
    }

    /// Returns packed-stream and schema-table metadata.
    #[must_use]
    pub const fn table_metadata(&self) -> &TableMetadata {
        &self.table_metadata
    }

    /// Returns the variable dictionary.
    #[must_use]
    pub const fn variable_dictionary(&self) -> &VariableDictionary {
        &self.variable_dictionary
    }

    /// Returns the CLP string logtype dictionary.
    #[must_use]
    pub const fn log_type_dictionary(&self) -> &LogTypeDictionary {
        &self.log_type_dictionary
    }

    /// Returns the unstructured-array logtype dictionary.
    #[must_use]
    pub const fn array_dictionary(&self) -> &ArrayDictionary {
        &self.array_dictionary
    }

    /// Returns all archive timestamp patterns compiled for repeated record formatting.
    #[must_use]
    pub const fn timestamp_patterns(&self) -> &TimestampPatternCatalog {
        &self.timestamp_patterns
    }

    /// Creates a lazy zero-copy table decoder for one already-loaded packed stream.
    ///
    /// This is the high-level bridge between [`SingleFileArchiveReader::read_packed_stream`] and
    /// typed table values. The returned iterator borrows both the packed-stream buffer and this
    /// catalog; it decodes only the next table requested by the caller.
    ///
    /// # Errors
    ///
    /// Returns an error when `stream_id` is absent, the decoded stream size disagrees with table
    /// metadata, table spans are invalid, or a requested table is corrupt.
    pub fn schema_tables<'stream, 'archive>(
        &'archive self,
        stream_id: u64,
        stream: &'stream DecodedPackedStream,
        limits: ColumnLimits,
    ) -> Result<SchemaTableStream<'stream, 'archive>, TableStreamError> {
        SchemaTableStream::new(
            stream_id,
            stream.as_bytes(),
            &self.table_metadata,
            &self.schema_map,
            &self.schema_tree,
            &self.variable_dictionary,
            &self.log_type_dictionary,
            &self.array_dictionary,
            self.metadata.timestamp_dictionary(),
            limits,
        )
    }
}

/// Layout-independent inputs needed to assemble an [`ArchiveCatalog`].
///
/// The outer readers retain responsibility for selecting bounded physical members. All semantic
/// cross-validation is intentionally shared here so the SFA and directory paths cannot drift.
pub(super) trait CatalogSectionSource {
    fn catalog_metadata(
        &mut self,
        limits: MetadataLimits,
    ) -> Result<ArchiveMetadata, MetadataError>;

    fn catalog_schema_tree(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: SchemaTreeLimits,
    ) -> Result<SchemaTree, SchemaTreeError>;

    fn catalog_schema_map(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_tree: &SchemaTree,
        limits: SchemaMapLimits,
    ) -> Result<SchemaMap, SchemaMapError>;

    fn catalog_table_metadata(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_map: &SchemaMap,
        limits: TableMetadataLimits,
    ) -> Result<TableMetadata, TableMetadataError>;

    fn catalog_variable_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<VariableDictionary, DictionaryError>;

    fn catalog_log_type_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<LogTypeDictionary, DictionaryError>;

    fn catalog_array_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<ArrayDictionary, DictionaryError>;
}

pub(super) fn load_catalog<S: CatalogSectionSource>(
    source: &mut S,
    limits: &ArchiveCatalogLimits,
) -> Result<ArchiveCatalog, ArchiveCatalogError> {
    let metadata = source
        .catalog_metadata(limits.metadata)
        .map_err(ArchiveCatalogError::Metadata)?;
    let schema_tree = source
        .catalog_schema_tree(&metadata, limits.schema_tree)
        .map_err(ArchiveCatalogError::SchemaTree)?;
    validate_timestamp_columns(&metadata, &schema_tree)?;
    let schema_map = source
        .catalog_schema_map(&metadata, &schema_tree, limits.schema_map)
        .map_err(ArchiveCatalogError::SchemaMap)?;
    let table_metadata = source
        .catalog_table_metadata(&metadata, &schema_map, limits.table_metadata)
        .map_err(ArchiveCatalogError::TableMetadata)?;
    if let Some(range_index) = metadata.range_index() {
        range_index
            .validate_record_domain(table_metadata.record_count())
            .map_err(ArchiveCatalogError::RangeIndex)?;
    }
    let variable_dictionary = source
        .catalog_variable_dictionary(&metadata, limits.variable_dictionary)
        .map_err(ArchiveCatalogError::VariableDictionary)?;
    let log_type_dictionary = source
        .catalog_log_type_dictionary(&metadata, limits.log_type_dictionary)
        .map_err(ArchiveCatalogError::LogTypeDictionary)?;
    let array_dictionary = source
        .catalog_array_dictionary(&metadata, limits.array_dictionary)
        .map_err(ArchiveCatalogError::ArrayDictionary)?;
    let timestamp_patterns = TimestampPatternCatalog::compile(
        metadata.timestamp_dictionary(),
        limits.timestamp_patterns,
    )
    .map_err(ArchiveCatalogError::TimestampPatterns)?;

    Ok(ArchiveCatalog {
        metadata,
        schema_tree,
        schema_map,
        table_metadata,
        variable_dictionary,
        log_type_dictionary,
        array_dictionary,
        timestamp_patterns,
    })
}

impl<R: Read + Seek> CatalogSectionSource for SingleFileArchiveReader<R> {
    fn catalog_metadata(
        &mut self,
        limits: MetadataLimits,
    ) -> Result<ArchiveMetadata, MetadataError> {
        Self::read_metadata(self, limits)
    }

    fn catalog_schema_tree(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: SchemaTreeLimits,
    ) -> Result<SchemaTree, SchemaTreeError> {
        Self::read_schema_tree(self, metadata, limits)
    }

    fn catalog_schema_map(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_tree: &SchemaTree,
        limits: SchemaMapLimits,
    ) -> Result<SchemaMap, SchemaMapError> {
        Self::read_schema_map(self, metadata, schema_tree, limits)
    }

    fn catalog_table_metadata(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_map: &SchemaMap,
        limits: TableMetadataLimits,
    ) -> Result<TableMetadata, TableMetadataError> {
        Self::read_table_metadata(self, metadata, schema_map, limits)
    }

    fn catalog_variable_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<VariableDictionary, DictionaryError> {
        Self::read_variable_dictionary(self, metadata, limits)
    }

    fn catalog_log_type_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<LogTypeDictionary, DictionaryError> {
        Self::read_log_type_dictionary(self, metadata, limits)
    }

    fn catalog_array_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<ArrayDictionary, DictionaryError> {
        Self::read_array_dictionary(self, metadata, limits)
    }
}

impl<R: Read + Seek> SingleFileArchiveReader<R> {
    /// Loads and cross-validates all non-table archive sections.
    ///
    /// This is the high-level library entry point for extraction and search. It leaves `/0`
    /// packed streams on the underlying seekable source and loads them only on demand.
    ///
    /// # Errors
    ///
    /// Returns a section-specific decoding error, an invalid timestamp-column reference, or a
    /// range-index coordinate outside the archive's validated record domain.
    pub fn read_catalog(
        &mut self,
        limits: ArchiveCatalogLimits,
    ) -> Result<ArchiveCatalog, ArchiveCatalogError> {
        load_catalog(self, &limits)
    }
}

fn validate_timestamp_columns(
    metadata: &ArchiveMetadata,
    schema_tree: &SchemaTree,
) -> Result<(), ArchiveCatalogError> {
    for (range_index, range) in metadata.timestamp_dictionary().ranges().iter().enumerate() {
        for &column_id in range.column_ids() {
            let node_id = usize::try_from(column_id).map_err(|_| {
                ArchiveCatalogError::InvalidTimestampColumnId {
                    range_index,
                    column_id,
                }
            })?;
            let node = schema_tree.get(node_id).ok_or_else(|| {
                ArchiveCatalogError::UnknownTimestampColumn {
                    range_index,
                    column_id,
                    node_count: schema_tree.len(),
                }
            })?;
            let node_type = node.node_type();
            if !matches!(
                node_type,
                NodeType::Timestamp
                    | NodeType::DeprecatedDateString
                    | NodeType::Integer
                    | NodeType::DeltaInteger
                    | NodeType::Float
            ) {
                return Err(ArchiveCatalogError::InvalidTimestampColumnType {
                    range_index,
                    column_id,
                    node_type,
                });
            }
        }
    }
    Ok(())
}

/// Failure to load or cross-validate an [`ArchiveCatalog`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ArchiveCatalogError {
    /// Archive metadata or its section directory was invalid.
    Metadata(MetadataError),
    /// The schema tree was invalid.
    SchemaTree(SchemaTreeError),
    /// The schema map was invalid.
    SchemaMap(SchemaMapError),
    /// Packed-stream or schema-table metadata was invalid.
    TableMetadata(TableMetadataError),
    /// The variable dictionary was invalid.
    VariableDictionary(DictionaryError),
    /// The CLP string logtype dictionary was invalid.
    LogTypeDictionary(DictionaryError),
    /// The unstructured-array logtype dictionary was invalid.
    ArrayDictionary(DictionaryError),
    /// A resolved timestamp pattern could not be precompiled within configured limits.
    TimestampPatterns(TimestampPatternCatalogError),
    /// The range index did not fit the table metadata's record domain.
    RangeIndex(RangeIndexError),
    /// A timestamp range used a negative schema-tree node ID.
    InvalidTimestampColumnId {
        /// Zero-based timestamp-range index.
        range_index: usize,
        /// Invalid signed wire ID.
        column_id: i32,
    },
    /// A timestamp range referenced a node absent from the schema tree.
    UnknownTimestampColumn {
        /// Zero-based timestamp-range index.
        range_index: usize,
        /// Referenced signed wire ID.
        column_id: i32,
        /// Number of nodes in the schema tree.
        node_count: usize,
    },
    /// A timestamp range referenced a node type the C++ extractor cannot interpret as time.
    InvalidTimestampColumnType {
        /// Zero-based timestamp-range index.
        range_index: usize,
        /// Referenced signed wire ID.
        column_id: i32,
        /// Referenced node type.
        node_type: NodeType,
    },
}

impl Display for ArchiveCatalogError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => write!(formatter, "invalid archive metadata: {error}"),
            Self::SchemaTree(error) => write!(formatter, "invalid schema tree: {error}"),
            Self::SchemaMap(error) => write!(formatter, "invalid schema map: {error}"),
            Self::TableMetadata(error) => write!(formatter, "invalid table metadata: {error}"),
            Self::VariableDictionary(error) => {
                write!(formatter, "invalid variable dictionary: {error}")
            }
            Self::LogTypeDictionary(error) => {
                write!(formatter, "invalid logtype dictionary: {error}")
            }
            Self::ArrayDictionary(error) => {
                write!(formatter, "invalid array dictionary: {error}")
            }
            Self::TimestampPatterns(error) => {
                write!(formatter, "invalid timestamp patterns: {error}")
            }
            Self::RangeIndex(error) => write!(formatter, "invalid range index: {error}"),
            Self::InvalidTimestampColumnId {
                range_index,
                column_id,
            } => write!(
                formatter,
                "timestamp range {range_index} has negative column ID {column_id}"
            ),
            Self::UnknownTimestampColumn {
                range_index,
                column_id,
                node_count,
            } => write!(
                formatter,
                "timestamp range {range_index} references column {column_id}, but the schema tree \
                 has {node_count} nodes"
            ),
            Self::InvalidTimestampColumnType {
                range_index,
                column_id,
                node_type,
            } => write!(
                formatter,
                "timestamp range {range_index} column {column_id} has unsupported type \
                 {node_type:?}"
            ),
        }
    }
}

impl Error for ArchiveCatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::SchemaTree(error) => Some(error),
            Self::SchemaMap(error) => Some(error),
            Self::TableMetadata(error) => Some(error),
            Self::VariableDictionary(error)
            | Self::LogTypeDictionary(error)
            | Self::ArrayDictionary(error) => Some(error),
            Self::TimestampPatterns(error) => Some(error),
            Self::RangeIndex(error) => Some(error),
            Self::InvalidTimestampColumnId { .. }
            | Self::UnknownTimestampColumn { .. }
            | Self::InvalidTimestampColumnType { .. } => None,
        }
    }
}
