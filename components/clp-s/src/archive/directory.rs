//! Checked, source-agnostic access to canonical CLP-S directory archives.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Take;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;

use super::catalog::ArchiveCatalog;
use super::catalog::ArchiveCatalogError;
use super::catalog::ArchiveCatalogLimits;
use super::catalog::CatalogSectionSource;
use super::catalog::load_catalog;
use super::dictionary::ArrayDictionary;
use super::dictionary::DictionaryError;
use super::dictionary::DictionaryLimits;
use super::dictionary::DictionarySection;
use super::dictionary::LogTypeDictionary;
use super::dictionary::VariableDictionary;
use super::dictionary::decode_array_dictionary;
use super::dictionary::decode_logtype_dictionary;
use super::dictionary::decode_variable_dictionary;
use super::format::ArchiveHeader;
use super::format::ArchiveVersion;
use super::format::HeaderDecodeError;
use super::format::SFA_HEADER_SIZE;
use super::layout::LayoutError;
use super::layout::SingleFileArchiveLayout;
use super::metadata::ArchiveMetadata;
use super::metadata::MetadataError;
use super::metadata::MetadataLimits;
use super::metadata::decode_metadata;
use super::packed_stream::DecodedPackedStream;
use super::packed_stream::PackedStreamError;
use super::packed_stream::PackedStreamLimits;
use super::packed_stream::decode_packed_stream;
use super::packed_stream::packed_stream_range;
use super::schema_map::SchemaMap;
use super::schema_map::SchemaMapError;
use super::schema_map::SchemaMapLimits;
use super::schema_map::decode_schema_map;
use super::schema_tree::SchemaTree;
use super::schema_tree::SchemaTreeError;
use super::schema_tree::SchemaTreeLimits;
use super::schema_tree::decode_schema_tree;
use super::table_metadata::TableMetadata;
use super::table_metadata::TableMetadataError;
use super::table_metadata::TableMetadataLimits;
use super::table_metadata::decode_table_metadata;

const MEMBER_COUNT: usize = 8;
const DATA_MEMBER_COUNT: usize = 7;

/// One canonical physical member of a CLP-S directory archive.
///
/// File names omit the leading slash used by the metadata packet. Keeping this a closed enum
/// prevents a decoder from accepting caller-controlled paths.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DirectoryArchiveMember {
    /// Fixed archive header followed by the one compressed metadata frame.
    Header,
    /// Compressed schema-tree member.
    SchemaTree,
    /// Compressed schema-map member.
    SchemaIds,
    /// Compressed packed-stream and schema-table metadata member.
    TableMetadata,
    /// Variable dictionary member.
    VariableDictionary,
    /// CLP logtype dictionary member.
    LogTypeDictionary,
    /// Unstructured-array logtype dictionary member.
    ArrayDictionary,
    /// Concatenated packed-stream member.
    PackedStreams,
}

impl DirectoryArchiveMember {
    /// Every canonical member in physical/logical archive order.
    pub const ALL: [Self; MEMBER_COUNT] = [
        Self::Header,
        Self::SchemaTree,
        Self::SchemaIds,
        Self::TableMetadata,
        Self::VariableDictionary,
        Self::LogTypeDictionary,
        Self::ArrayDictionary,
        Self::PackedStreams,
    ];
    /// The seven data members represented by `ArchiveFileInfo` metadata.
    pub const DATA: [Self; DATA_MEMBER_COUNT] = [
        Self::SchemaTree,
        Self::SchemaIds,
        Self::TableMetadata,
        Self::VariableDictionary,
        Self::LogTypeDictionary,
        Self::ArrayDictionary,
        Self::PackedStreams,
    ];

    /// Returns the exact basename used in a directory archive.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::SchemaTree => "schema_tree",
            Self::SchemaIds => "schema_ids",
            Self::TableMetadata => "table_metadata",
            Self::VariableDictionary => "var.dict",
            Self::LogTypeDictionary => "log.dict",
            Self::ArrayDictionary => "array.dict",
            Self::PackedStreams => "0",
        }
    }

    /// Returns the canonical leading-slash name used by archive metadata.
    #[must_use]
    pub const fn metadata_name(self) -> Option<&'static str> {
        match self {
            Self::Header => None,
            Self::SchemaTree => Some("/schema_tree"),
            Self::SchemaIds => Some("/schema_ids"),
            Self::TableMetadata => Some("/table_metadata"),
            Self::VariableDictionary => Some("/var.dict"),
            Self::LogTypeDictionary => Some("/log.dict"),
            Self::ArrayDictionary => Some("/array.dict"),
            Self::PackedStreams => Some("/0"),
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Header => 0,
            Self::SchemaTree => 1,
            Self::SchemaIds => 2,
            Self::TableMetadata => 3,
            Self::VariableDictionary => 4,
            Self::LogTypeDictionary => 5,
            Self::ArrayDictionary => 6,
            Self::PackedStreams => 7,
        }
    }
}

impl Display for DirectoryArchiveMember {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.file_name())
    }
}

/// Caller-owned factory for canonical directory-archive members.
///
/// Implementations may open local files, binding-owned buffers, object-store objects, or other
/// seekable resources. Discovery, authentication, retries, and caching remain outside the archive
/// decoder. Like the C++ reader, the decoder requests only its closed set of canonical names and
/// neither requires list permission nor rejects unrelated entries in the same container.
pub trait DirectoryArchiveSource {
    /// Seekable reader returned for every member.
    type Reader: Read + Seek;

    /// Opens one canonical member, or returns `None` when it is absent.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the member cannot be addressed or opened.
    fn open_member(&mut self, member: DirectoryArchiveMember) -> io::Result<Option<Self::Reader>>;
}

/// Thin `std::fs` adapter for a directory-archive path.
///
/// The format reader itself depends only on [`DirectoryArchiveSource`]. This adapter deliberately
/// performs no recursive discovery and accepts only regular, non-symlink canonical members.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsDirectoryArchiveSource {
    root: PathBuf,
}

impl FsDirectoryArchiveSource {
    /// Creates a filesystem source rooted at `path` without touching the filesystem.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    /// Returns the configured archive directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl DirectoryArchiveSource for FsDirectoryArchiveSource {
    type Reader = File;

    fn open_member(&mut self, member: DirectoryArchiveMember) -> io::Result<Option<Self::Reader>> {
        let path = self.root.join(member.file_name());
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if io::ErrorKind::NotFound == error.kind() => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "directory archive member {} is not a regular file",
                    path.display()
                ),
            ));
        }
        File::open(path).map(Some)
    }
}

/// Validated streaming reader for one canonical CLP-S directory archive.
///
/// Opening measures all eight members, validates the fixed header and metadata frame, checks the
/// header's aggregate compressed size, and compares all seven physical data sizes with the
/// `ArchiveFileInfo` offset deltas. Section decoders seek within already-open bounded members;
/// `/0` is never buffered as a whole.
#[derive(Debug)]
pub struct DirectoryArchiveReader<S: DirectoryArchiveSource> {
    source: S,
    members: Vec<S::Reader>,
    member_sizes: [u64; MEMBER_COUNT],
    layout: SingleFileArchiveLayout,
}

impl<S: DirectoryArchiveSource> DirectoryArchiveReader<S> {
    /// Opens and validates a canonical directory archive.
    ///
    /// Metadata is decoded during opening so malformed framing and member-size disagreements fail
    /// before a usable reader is returned. A subsequent [`Self::read_metadata`] applies that
    /// call's limits independently and avoids retaining duplicate metadata allocations.
    ///
    /// # Errors
    ///
    /// Returns a typed error for source/open failures, missing members, invalid header or metadata,
    /// checked-size overflow, aggregate-size mismatch, or any data member whose measured size
    /// differs from its metadata offset delta.
    pub fn open(
        mut source: S,
        metadata_limits: MetadataLimits,
    ) -> Result<Self, DirectoryArchiveOpenError> {
        let mut members = Vec::new();
        members.try_reserve_exact(MEMBER_COUNT).map_err(|_| {
            DirectoryArchiveOpenError::AllocationFailed {
                requested_members: MEMBER_COUNT,
            }
        })?;
        let mut member_sizes = [0_u64; MEMBER_COUNT];

        for member in DirectoryArchiveMember::ALL {
            let mut reader = source
                .open_member(member)
                .map_err(|source| DirectoryArchiveOpenError::MemberIo { member, source })?
                .ok_or(DirectoryArchiveOpenError::MissingMember { member })?;
            let size = reader
                .seek(SeekFrom::End(0))
                .map_err(|source| DirectoryArchiveOpenError::MemberIo { member, source })?;
            reader
                .seek(SeekFrom::Start(0))
                .map_err(|source| DirectoryArchiveOpenError::MemberIo { member, source })?;
            member_sizes[member.index()] = size;
            members.push(reader);
        }

        let header_reader = members
            .get_mut(DirectoryArchiveMember::Header.index())
            .ok_or(DirectoryArchiveOpenError::AllocationFailed {
                requested_members: MEMBER_COUNT,
            })?;
        let mut header_bytes = [0_u8; SFA_HEADER_SIZE];
        header_reader
            .read_exact(&mut header_bytes)
            .map_err(|source| DirectoryArchiveOpenError::MemberIo {
                member: DirectoryArchiveMember::Header,
                source,
            })?;
        let header =
            ArchiveHeader::decode(&header_bytes).map_err(DirectoryArchiveOpenError::Header)?;
        if ArchiveVersion::CURRENT != header.version() {
            return Err(DirectoryArchiveOpenError::UnsupportedVersion {
                actual: header.version(),
            });
        }

        let fixed_header_size =
            u64::try_from(SFA_HEADER_SIZE).map_err(|_| DirectoryArchiveOpenError::SizeOverflow)?;
        let expected_header_size = fixed_header_size
            .checked_add(u64::from(header.metadata_section_size()))
            .ok_or(DirectoryArchiveOpenError::SizeOverflow)?;
        let actual_header_size = member_sizes[DirectoryArchiveMember::Header.index()];
        if expected_header_size != actual_header_size {
            return Err(DirectoryArchiveOpenError::HeaderMemberSizeMismatch {
                expected: expected_header_size,
                actual: actual_header_size,
            });
        }

        let aggregate_size = member_sizes.iter().try_fold(0_u64, |total, &size| {
            total
                .checked_add(size)
                .ok_or(DirectoryArchiveOpenError::SizeOverflow)
        })?;
        if header.compressed_size() != aggregate_size {
            return Err(DirectoryArchiveOpenError::AggregateCompressedSizeMismatch {
                advertised: header.compressed_size(),
                actual: aggregate_size,
            });
        }
        let layout = SingleFileArchiveLayout::new(header, aggregate_size)
            .map_err(DirectoryArchiveOpenError::Layout)?;
        let mut reader = Self {
            source,
            members,
            member_sizes,
            layout,
        };
        let metadata = reader
            .read_metadata(metadata_limits)
            .map_err(DirectoryArchiveOpenError::Metadata)?;
        reader.validate_data_member_sizes(&metadata)?;
        Ok(reader)
    }

    /// Returns the decoded fixed archive header.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        self.layout.header()
    }

    /// Returns the virtual concatenated layout used by `ArchiveFileInfo` offsets.
    ///
    /// Its byte ranges describe the equivalent SFA concatenation, not offsets within individual
    /// directory files.
    #[must_use]
    pub const fn virtual_layout(&self) -> &SingleFileArchiveLayout {
        &self.layout
    }

    /// Returns the measured size of one physical member.
    #[must_use]
    pub const fn member_size(&self, member: DirectoryArchiveMember) -> u64 {
        self.member_sizes[member.index()]
    }

    /// Returns the caller-owned member factory.
    #[must_use]
    pub const fn source(&self) -> &S {
        &self.source
    }

    /// Consumes the reader and returns the caller-owned member factory.
    #[must_use]
    pub fn into_source(self) -> S {
        self.source
    }

    /// Decompresses and validates the header member's metadata frame.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O or zstd failures, malformed packet framing or `MessagePack`,
    /// missing or duplicate packets, resource limits, or invalid canonical section offsets.
    pub fn read_metadata(
        &mut self,
        limits: MetadataLimits,
    ) -> Result<ArchiveMetadata, MetadataError> {
        let start = u64::try_from(SFA_HEADER_SIZE).map_err(|_| MetadataError::SizeOverflow)?;
        let end = start
            .checked_add(u64::from(self.header().metadata_section_size()))
            .ok_or(MetadataError::SizeOverflow)?;
        let layout = self.layout;
        let compressed = self
            .reader_for_member_range(DirectoryArchiveMember::Header, start..end)
            .map_err(MetadataError::Io)?;
        decode_metadata(compressed, layout, limits)
    }

    /// Decompresses and validates the schema tree.
    ///
    /// # Errors
    ///
    /// Returns a section, I/O, zstd, resource, or schema-tree corruption error.
    pub fn read_schema_tree(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: SchemaTreeLimits,
    ) -> Result<SchemaTree, SchemaTreeError> {
        if !self.section_matches_member(metadata, DirectoryArchiveMember::SchemaTree) {
            return Err(SchemaTreeError::SectionOutsideArchive);
        }
        let compressed = self
            .complete_member_reader(DirectoryArchiveMember::SchemaTree)
            .map_err(SchemaTreeError::Io)?;
        decode_schema_tree(compressed, limits)
    }

    /// Decompresses and validates the schema map against `schema_tree`.
    ///
    /// # Errors
    ///
    /// Returns a section, I/O, zstd, resource, or schema-map corruption error.
    pub fn read_schema_map(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_tree: &SchemaTree,
        limits: SchemaMapLimits,
    ) -> Result<SchemaMap, SchemaMapError> {
        if !self.section_matches_member(metadata, DirectoryArchiveMember::SchemaIds) {
            return Err(SchemaMapError::SectionOutsideArchive);
        }
        let compressed = self
            .complete_member_reader(DirectoryArchiveMember::SchemaIds)
            .map_err(SchemaMapError::Io)?;
        decode_schema_map(compressed, schema_tree, limits)
    }

    /// Decompresses and validates packed-stream and schema-table metadata.
    ///
    /// # Errors
    ///
    /// Returns a section, I/O, zstd, resource, table-layout, schema-reference, or packed-range
    /// corruption error.
    pub fn read_table_metadata(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_map: &SchemaMap,
        limits: TableMetadataLimits,
    ) -> Result<TableMetadata, TableMetadataError> {
        if !self.section_matches_member(metadata, DirectoryArchiveMember::TableMetadata) {
            return Err(TableMetadataError::SectionOutsideArchive);
        }
        if !self.section_matches_member(metadata, DirectoryArchiveMember::PackedStreams) {
            return Err(TableMetadataError::TablesSectionOutsideArchive);
        }
        let tables_size = self.member_size(DirectoryArchiveMember::PackedStreams);
        let compressed = self
            .complete_member_reader(DirectoryArchiveMember::TableMetadata)
            .map_err(TableMetadataError::Io)?;
        decode_table_metadata(compressed, schema_map, tables_size, limits)
    }

    /// Decompresses one checked packed stream without buffering the complete `/0` member.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched `/0` metadata, an absent stream ID, an out-of-range stream,
    /// I/O or zstd failures, resource limits, or compressed/decompressed size disagreement.
    pub fn read_packed_stream(
        &mut self,
        metadata: &ArchiveMetadata,
        table_metadata: &TableMetadata,
        stream_id: usize,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        if metadata.directory().get("/0").is_none() {
            return Err(PackedStreamError::MissingTablesSection);
        }
        if !self.section_matches_member(metadata, DirectoryArchiveMember::PackedStreams) {
            return Err(PackedStreamError::TablesSectionOutsideArchive);
        }
        let tables_size = self.member_size(DirectoryArchiveMember::PackedStreams);
        let (stream, range) = packed_stream_range(table_metadata, stream_id, tables_size)?;
        let compressed = self
            .reader_for_member_range(DirectoryArchiveMember::PackedStreams, range)
            .map_err(PackedStreamError::Io)?;
        decode_packed_stream(compressed, stream, limits)
    }

    /// Decompresses `/var.dict` and preserves entries as arbitrary bytes.
    ///
    /// # Errors
    ///
    /// Returns a section, I/O, zstd, resource, framing, or trailing-data error.
    pub fn read_variable_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<VariableDictionary, DictionaryError> {
        let compressed = self.dictionary_reader(metadata, DictionarySection::Variable)?;
        decode_variable_dictionary(compressed, limits)
    }

    /// Decompresses and validates `/log.dict`.
    ///
    /// # Errors
    ///
    /// Returns a section, I/O, zstd, resource, framing, escape, or trailing-data error.
    pub fn read_log_type_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<LogTypeDictionary, DictionaryError> {
        let compressed = self.dictionary_reader(metadata, DictionarySection::LogType)?;
        decode_logtype_dictionary(compressed, limits)
    }

    /// Decompresses and validates `/array.dict`.
    ///
    /// # Errors
    ///
    /// Returns a section, I/O, zstd, resource, framing, escape, or trailing-data error.
    pub fn read_array_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<ArrayDictionary, DictionaryError> {
        let compressed = self.dictionary_reader(metadata, DictionarySection::Array)?;
        decode_array_dictionary(compressed, limits)
    }

    /// Loads the shared, cross-validated non-table archive catalog.
    ///
    /// `/0` remains seekable on this reader and is loaded one packed stream at a time through
    /// [`Self::read_packed_stream`].
    ///
    /// # Errors
    ///
    /// Returns a section-specific decoding or catalog cross-validation error.
    pub fn read_catalog(
        &mut self,
        limits: ArchiveCatalogLimits,
    ) -> Result<ArchiveCatalog, ArchiveCatalogError> {
        load_catalog(self, &limits)
    }

    fn validate_data_member_sizes(
        &self,
        metadata: &ArchiveMetadata,
    ) -> Result<(), DirectoryArchiveOpenError> {
        for member in DirectoryArchiveMember::DATA {
            let Some(metadata_name) = member.metadata_name() else {
                return Err(DirectoryArchiveOpenError::SizeOverflow);
            };
            let Some(section) = metadata.directory().get(metadata_name) else {
                return Err(DirectoryArchiveOpenError::MissingMember { member });
            };
            let advertised = section.compressed_size();
            let actual = self.member_size(member);
            if advertised != actual {
                return Err(DirectoryArchiveOpenError::MemberSizeMismatch {
                    member,
                    advertised,
                    actual,
                });
            }
        }
        Ok(())
    }

    fn section_matches_member(
        &self,
        metadata: &ArchiveMetadata,
        member: DirectoryArchiveMember,
    ) -> bool {
        let Some(name) = member.metadata_name() else {
            return false;
        };
        let Some(section) = metadata.directory().get(name) else {
            return false;
        };
        let range = section.range();
        let files = self.layout.files_range();
        range.start >= files.start
            && range.end >= range.start
            && range.end <= files.end
            && section.compressed_size() == self.member_size(member)
    }

    fn complete_member_reader(
        &mut self,
        member: DirectoryArchiveMember,
    ) -> io::Result<Take<&mut S::Reader>> {
        let size = self.member_size(member);
        self.reader_for_member_range(member, 0..size)
    }

    fn reader_for_member_range(
        &mut self,
        member: DirectoryArchiveMember,
        range: Range<u64>,
    ) -> io::Result<Take<&mut S::Reader>> {
        let size = self.member_size(member);
        if range.start > range.end || range.end > size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bounded directory member range is outside the measured member",
            ));
        }
        let reader = self
            .members
            .get_mut(member.index())
            .ok_or_else(|| io::Error::other("directory reader member invariant violated"))?;
        reader.seek(SeekFrom::Start(range.start))?;
        Ok(reader.take(range.end - range.start))
    }

    fn dictionary_reader(
        &mut self,
        metadata: &ArchiveMetadata,
        dictionary: DictionarySection,
    ) -> Result<Take<&mut S::Reader>, DictionaryError> {
        let member = match dictionary {
            DictionarySection::Variable => DirectoryArchiveMember::VariableDictionary,
            DictionarySection::LogType => DirectoryArchiveMember::LogTypeDictionary,
            DictionarySection::Array => DirectoryArchiveMember::ArrayDictionary,
        };
        if metadata.directory().get(dictionary.name()).is_none() {
            return Err(DictionaryError::MissingSection {
                section: dictionary,
            });
        }
        if !self.section_matches_member(metadata, member) {
            return Err(DictionaryError::SectionOutsideArchive {
                section: dictionary,
            });
        }
        self.complete_member_reader(member)
            .map_err(DictionaryError::Io)
    }
}

impl<S: DirectoryArchiveSource> CatalogSectionSource for DirectoryArchiveReader<S> {
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

/// Failure to open and cross-check a directory archive's physical envelope.
#[derive(Debug)]
#[non_exhaustive]
pub enum DirectoryArchiveOpenError {
    /// One canonical member could not be opened, measured, sought, or read.
    MemberIo {
        /// Member being accessed.
        member: DirectoryArchiveMember,
        /// Underlying source error.
        source: io::Error,
    },
    /// A canonical member was absent.
    MissingMember {
        /// Missing member.
        member: DirectoryArchiveMember,
    },
    /// The fixed bounded member-reader collection could not be reserved.
    AllocationFailed {
        /// Number of member handles requested.
        requested_members: usize,
    },
    /// The fixed archive header was invalid.
    Header(HeaderDecodeError),
    /// The archive version is unsupported.
    UnsupportedVersion {
        /// Version decoded from the header member.
        actual: ArchiveVersion,
    },
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// The `header` member was not exactly the fixed header plus advertised metadata bytes.
    HeaderMemberSizeMismatch {
        /// Size implied by the fixed header.
        expected: u64,
        /// Measured physical `header` member size.
        actual: u64,
    },
    /// The header's aggregate compressed size differed from all eight physical members.
    AggregateCompressedSizeMismatch {
        /// Aggregate bytes advertised by `ArchiveHeader`.
        advertised: u64,
        /// Checked sum of measured member sizes.
        actual: u64,
    },
    /// The virtual concatenated layout was invalid.
    Layout(LayoutError),
    /// The header member's metadata frame or canonical section directory was invalid.
    Metadata(MetadataError),
    /// A data member size differed from its `ArchiveFileInfo` offset delta.
    MemberSizeMismatch {
        /// Mismatched member.
        member: DirectoryArchiveMember,
        /// Compressed bytes implied by metadata offsets.
        advertised: u64,
        /// Measured physical member bytes.
        actual: u64,
    },
}

impl Display for DirectoryArchiveOpenError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MemberIo { member, source } => {
                write!(
                    formatter,
                    "failed to read directory member {member}: {source}"
                )
            }
            Self::MissingMember { member } => {
                write!(formatter, "directory archive member {member} is missing")
            }
            Self::AllocationFailed { requested_members } => write!(
                formatter,
                "could not reserve {requested_members} bounded directory member handles"
            ),
            Self::Header(error) => write!(formatter, "invalid directory archive header: {error}"),
            Self::UnsupportedVersion { actual } => write!(
                formatter,
                "unsupported structured archive version {actual}; expected {}",
                ArchiveVersion::CURRENT
            ),
            Self::SizeOverflow => formatter.write_str("directory archive size overflow"),
            Self::HeaderMemberSizeMismatch { expected, actual } => write!(
                formatter,
                "directory header member has {actual} bytes; header requires exactly {expected}"
            ),
            Self::AggregateCompressedSizeMismatch { advertised, actual } => write!(
                formatter,
                "directory members total {actual} bytes; header advertises {advertised}"
            ),
            Self::Layout(error) => write!(formatter, "invalid virtual archive layout: {error}"),
            Self::Metadata(error) => write!(formatter, "invalid archive metadata: {error}"),
            Self::MemberSizeMismatch {
                member,
                advertised,
                actual,
            } => write!(
                formatter,
                "directory member {member} has {actual} bytes; metadata advertises {advertised}"
            ),
        }
    }
}

impl Error for DirectoryArchiveOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MemberIo { source, .. } => Some(source),
            Self::Header(error) => Some(error),
            Self::Layout(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::MissingMember { .. }
            | Self::AllocationFailed { .. }
            | Self::UnsupportedVersion { .. }
            | Self::SizeOverflow
            | Self::HeaderMemberSizeMismatch { .. }
            | Self::AggregateCompressedSizeMismatch { .. }
            | Self::MemberSizeMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use super::*;

    const CPP_SFA: &[u8] = include_bytes!("../../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");
    const MEMBER_RANGES: [(DirectoryArchiveMember, Range<usize>); MEMBER_COUNT] = [
        (DirectoryArchiveMember::Header, 0..363),
        (DirectoryArchiveMember::SchemaTree, 363..471),
        (DirectoryArchiveMember::SchemaIds, 471..510),
        (DirectoryArchiveMember::TableMetadata, 510..541),
        (DirectoryArchiveMember::VariableDictionary, 541..570),
        (DirectoryArchiveMember::LogTypeDictionary, 570..609),
        (DirectoryArchiveMember::ArrayDictionary, 609..617),
        (DirectoryArchiveMember::PackedStreams, 617..654),
    ];

    #[derive(Clone, Debug)]
    struct MemorySource {
        members: BTreeMap<DirectoryArchiveMember, Vec<u8>>,
    }

    impl MemorySource {
        fn cpp_oracle() -> Self {
            let members = MEMBER_RANGES
                .iter()
                .map(|(member, range)| (*member, CPP_SFA[range.clone()].to_vec()))
                .collect();
            Self { members }
        }
    }

    impl DirectoryArchiveSource for MemorySource {
        type Reader = Cursor<Vec<u8>>;

        fn open_member(
            &mut self,
            member: DirectoryArchiveMember,
        ) -> io::Result<Option<Self::Reader>> {
            Ok(self.members.get(&member).cloned().map(Cursor::new))
        }
    }

    #[test]
    fn opens_cpp_sfa_bytes_as_the_equivalent_canonical_directory() {
        let mut reader =
            DirectoryArchiveReader::open(MemorySource::cpp_oracle(), MetadataLimits::default())
                .expect("open split committed C++ fixture");

        assert_eq!(654, reader.header().compressed_size());
        assert_eq!(363, reader.member_size(DirectoryArchiveMember::Header));
        assert_eq!(
            37,
            reader.member_size(DirectoryArchiveMember::PackedStreams)
        );
        let catalog = reader
            .read_catalog(ArchiveCatalogLimits::default())
            .expect("load shared catalog from directory members");
        let stream = reader
            .read_packed_stream(
                catalog.metadata(),
                catalog.table_metadata(),
                0,
                PackedStreamLimits::default(),
            )
            .expect("read one packed stream lazily");
        assert_eq!(57, stream.len());
    }

    #[test]
    fn rejects_a_missing_canonical_member() {
        let mut source = MemorySource::cpp_oracle();
        source
            .members
            .remove(&DirectoryArchiveMember::ArrayDictionary);

        let error = DirectoryArchiveReader::open(source, MetadataLimits::default())
            .expect_err("missing member must fail");
        assert!(matches!(
            error,
            DirectoryArchiveOpenError::MissingMember {
                member: DirectoryArchiveMember::ArrayDictionary
            }
        ));
    }

    #[test]
    fn rejects_header_member_size_mismatch() {
        let mut source = MemorySource::cpp_oracle();
        source
            .members
            .get_mut(&DirectoryArchiveMember::Header)
            .expect("header")
            .push(0);

        let error = DirectoryArchiveReader::open(source, MetadataLimits::default())
            .expect_err("trailing header byte must fail");
        assert!(matches!(
            error,
            DirectoryArchiveOpenError::HeaderMemberSizeMismatch {
                expected: 363,
                actual: 364
            }
        ));
    }

    #[test]
    fn rejects_aggregate_size_mismatch() {
        let mut source = MemorySource::cpp_oracle();
        source
            .members
            .get_mut(&DirectoryArchiveMember::PackedStreams)
            .expect("packed streams")
            .push(0);

        let error = DirectoryArchiveReader::open(source, MetadataLimits::default())
            .expect_err("aggregate mismatch must fail");
        assert!(matches!(
            error,
            DirectoryArchiveOpenError::AggregateCompressedSizeMismatch {
                advertised: 654,
                actual: 655
            }
        ));
    }

    #[test]
    fn rejects_member_size_mismatch_even_when_aggregate_is_unchanged() {
        let mut source = MemorySource::cpp_oracle();
        source
            .members
            .get_mut(&DirectoryArchiveMember::SchemaTree)
            .expect("schema tree")
            .pop();
        source
            .members
            .get_mut(&DirectoryArchiveMember::SchemaIds)
            .expect("schema IDs")
            .push(0);

        let error = DirectoryArchiveReader::open(source, MetadataLimits::default())
            .expect_err("offset-delta mismatch must fail");
        assert!(matches!(
            error,
            DirectoryArchiveOpenError::MemberSizeMismatch {
                member: DirectoryArchiveMember::SchemaTree,
                advertised: 108,
                actual: 107
            }
        ));
    }

    #[test]
    fn rejects_corrupt_metadata_during_open() {
        let mut source = MemorySource::cpp_oracle();
        source
            .members
            .get_mut(&DirectoryArchiveMember::Header)
            .expect("header")[100] ^= 0xff;

        let error = DirectoryArchiveReader::open(source, MetadataLimits::default())
            .expect_err("corrupt metadata must fail");
        assert!(matches!(error, DirectoryArchiveOpenError::Metadata(_)));
    }
}
