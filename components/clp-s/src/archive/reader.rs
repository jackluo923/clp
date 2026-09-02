use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Take;
use std::io::{self};
use std::ops::Range;

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
use super::table_metadata::PackedStreamMetadata;
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

/// Streaming reader for the outer envelope of a CLP structured single-file archive.
///
/// The generic source keeps the library usable with files, in-memory buffers, and binding-owned
/// streams. Section methods seek directly to bounded ranges and do not buffer the full archive.
#[derive(Debug)]
pub struct SingleFileArchiveReader<R> {
    source: R,
    layout: SingleFileArchiveLayout,
}

impl<R: Read + Seek> SingleFileArchiveReader<R> {
    /// Opens and validates a seekable SFA source.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the source cannot be measured or read, a header decode error for
    /// an invalid fixed header, an unsupported-version error, or a layout error when the declared
    /// and actual lengths disagree.
    pub fn open(mut source: R) -> Result<Self, OpenError> {
        let archive_size = source.seek(SeekFrom::End(0)).map_err(OpenError::Io)?;
        source.seek(SeekFrom::Start(0)).map_err(OpenError::Io)?;

        Self::open_streaming(source, Some(archive_size))
    }

    /// Opens an SFA source whose fixed header begins at logical byte zero without seeking to its
    /// end.
    ///
    /// This entry point supports one-pass sources wrapped by a forward-only [`Seek`] adapter. When
    /// `source_size` is known (for example from HTTP `Content-Length`), it retains the same exact
    /// declared-versus-actual size validation as [`Self::open`]. When it is unknown, the header's
    /// declared total is used as the outer boundary. In that mode, bounded section reads still
    /// reject truncation, but trailing bytes after the declared archive cannot be detected without
    /// consuming or buffering the complete source.
    ///
    /// The returned reader may subsequently request sections out of physical order. A caller using
    /// a forward-only source must use the ordinary one-pass catalog and packed-stream access order;
    /// a backward request is reported by that source's [`Seek`] implementation.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the header cannot be read, a header decode or unsupported-version
    /// error, or a layout error when a known source size differs from the header declaration.
    pub fn open_streaming(mut source: R, source_size: Option<u64>) -> Result<Self, OpenError> {
        source.seek(SeekFrom::Start(0)).map_err(OpenError::Io)?;
        let mut header_bytes = [0_u8; SFA_HEADER_SIZE];
        source
            .read_exact(&mut header_bytes)
            .map_err(OpenError::Io)?;
        let header = ArchiveHeader::decode(&header_bytes).map_err(OpenError::Header)?;
        if !header.version().is_readable() {
            return Err(OpenError::UnsupportedVersion {
                actual: header.version(),
            });
        }
        let archive_size = source_size.unwrap_or_else(|| header.compressed_size());
        let layout =
            SingleFileArchiveLayout::new(header, archive_size).map_err(OpenError::Layout)?;

        Ok(Self { source, layout })
    }

    /// Returns the validated outer archive layout.
    #[must_use]
    pub const fn layout(&self) -> &SingleFileArchiveLayout {
        &self.layout
    }

    /// Returns the decoded archive header.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        self.layout.header()
    }

    /// Returns a bounded streaming reader for the compressed metadata section.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when seeking to the metadata section fails.
    pub fn metadata_reader(&mut self) -> io::Result<Take<&mut R>> {
        self.reader_for_range(self.layout.metadata_range())
    }

    /// Returns a bounded streaming reader for the complete concatenated-files section.
    ///
    /// Metadata must be decoded before individual named files can be safely addressed.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when seeking to the files section fails.
    pub fn files_reader(&mut self) -> io::Result<Take<&mut R>> {
        self.reader_for_range(self.layout.files_range())
    }

    /// Decompresses and validates the SFA metadata frame and physical section directory.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O or zstd failures, malformed packet framing or `MessagePack`,
    /// missing or duplicate required packets, resource-limit violations, or invalid section
    /// boundaries.
    pub fn read_metadata(
        &mut self,
        limits: MetadataLimits,
    ) -> Result<ArchiveMetadata, MetadataError> {
        let layout = self.layout;
        let compressed = self.metadata_reader().map_err(MetadataError::Io)?;
        decode_metadata(compressed, layout, limits)
    }

    /// Decompresses and validates the schema tree addressed by previously decoded metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds section, I/O or zstd failures, resource
    /// limit violations, invalid parent/type/key records, duplicate node identities, or trailing
    /// data.
    pub fn read_schema_tree(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: SchemaTreeLimits,
    ) -> Result<SchemaTree, SchemaTreeError> {
        let section = metadata
            .directory()
            .get("/schema_tree")
            .ok_or(SchemaTreeError::MissingSection)?;
        let range = section.range();
        let files_range = self.layout.files_range();
        if range.start < files_range.start || range.end > files_range.end || range.start > range.end
        {
            return Err(SchemaTreeError::SectionOutsideArchive);
        }
        let compressed = self.reader_for_range(range).map_err(SchemaTreeError::Io)?;
        decode_schema_tree(compressed, limits)
    }

    /// Decompresses and validates the schema map against a previously decoded schema tree.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds section, I/O or zstd failures, resource
    /// limit violations, duplicate schema IDs, invalid node references or ordered regions,
    /// malformed unordered-container delimiters, or trailing data.
    pub fn read_schema_map(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_tree: &SchemaTree,
        limits: SchemaMapLimits,
    ) -> Result<SchemaMap, SchemaMapError> {
        let section = metadata
            .directory()
            .get("/schema_ids")
            .ok_or(SchemaMapError::MissingSection)?;
        let range = section.range();
        let files_range = self.layout.files_range();
        if range.start < files_range.start || range.end > files_range.end || range.start > range.end
        {
            return Err(SchemaMapError::SectionOutsideArchive);
        }
        let compressed = self.reader_for_range(range).map_err(SchemaMapError::Io)?;
        decode_schema_map(compressed, schema_tree, limits)
    }

    /// Decompresses and validates packed-stream and schema-table metadata.
    ///
    /// Stream ranges are checked against `/0`, and every table must cover exactly one schema from
    /// the supplied schema map. This method validates metadata only; it does not decompress `/0`.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds section, I/O or zstd failures,
    /// resource-limit violations, unsupported separate columns, invalid stream/table ordering,
    /// unknown or duplicate schema IDs, or trailing data.
    pub fn read_table_metadata(
        &mut self,
        metadata: &ArchiveMetadata,
        schema_map: &SchemaMap,
        limits: TableMetadataLimits,
    ) -> Result<TableMetadata, TableMetadataError> {
        let section = metadata
            .directory()
            .get("/table_metadata")
            .ok_or(TableMetadataError::MissingSection)?;
        let range = section.range();
        let tables_section = metadata
            .directory()
            .get("/0")
            .ok_or(TableMetadataError::MissingTablesSection)?;
        let tables_range = tables_section.range();
        let files_range = self.layout.files_range();
        if range.start < files_range.start || range.end > files_range.end || range.start > range.end
        {
            return Err(TableMetadataError::SectionOutsideArchive);
        }
        if tables_range.start < files_range.start
            || tables_range.end > files_range.end
            || tables_range.start > tables_range.end
        {
            return Err(TableMetadataError::TablesSectionOutsideArchive);
        }
        let tables_compressed_size = tables_range.end - tables_range.start;
        let compressed = self
            .reader_for_range(range)
            .map_err(TableMetadataError::Io)?;
        decode_table_metadata(compressed, schema_map, tables_compressed_size, limits)
    }

    /// Decompresses and validates one packed stream from `/0`.
    ///
    /// The stream's relative range is checked against this reader's archive metadata before any
    /// seek or allocation. This makes passing table metadata decoded from a different archive a
    /// deterministic error rather than an out-of-bounds read.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds `/0` section, an unknown stream ID,
    /// inconsistent stream ranges, I/O or zstd failures, resource-limit violations, truncated or
    /// trailing compressed data, or a decompressed-size mismatch.
    pub fn read_packed_stream(
        &mut self,
        metadata: &ArchiveMetadata,
        table_metadata: &TableMetadata,
        stream_id: usize,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        // A separate-column stream is several frames; inflate them in order.
        if table_metadata.separate_columns_for(stream_id as u64).is_some() {
            return self.read_packed_stream_frames(metadata, table_metadata, stream_id, None, limits);
        }
        let tables_section = metadata
            .directory()
            .get("/0")
            .ok_or(PackedStreamError::MissingTablesSection)?;
        let tables_range = tables_section.range();
        let files_range = self.layout.files_range();
        if tables_range.start < files_range.start
            || tables_range.end > files_range.end
            || tables_range.start > tables_range.end
        {
            return Err(PackedStreamError::TablesSectionOutsideArchive);
        }

        let tables_size = tables_range.end - tables_range.start;
        let (stream, relative_range) = packed_stream_range(table_metadata, stream_id, tables_size)?;

        let absolute_start = tables_range
            .start
            .checked_add(relative_range.start)
            .ok_or(PackedStreamError::SizeOverflow)?;
        let absolute_end = tables_range
            .start
            .checked_add(relative_range.end)
            .ok_or(PackedStreamError::SizeOverflow)?;
        let compressed = self
            .reader_for_range(absolute_start..absolute_end)
            .map_err(PackedStreamError::Io)?;
        decode_packed_stream(compressed, stream, limits)
    }

    /// Reads one packed stream, inflating every column frame of a separate-column stream in
    /// order so the bytes match a shared-frame stream exactly.
    pub(crate) fn read_packed_stream_frames(
        &mut self,
        metadata: &ArchiveMetadata,
        table_metadata: &TableMetadata,
        stream_id: usize,
        wanted: Option<&[bool]>,
        limits: PackedStreamLimits,
    ) -> Result<DecodedPackedStream, PackedStreamError> {
        let Some(separate) = table_metadata.separate_columns_for(stream_id as u64) else {
            return self.read_packed_stream(metadata, table_metadata, stream_id, limits);
        };
        let tables_section = metadata
            .directory()
            .get("/0")
            .ok_or(PackedStreamError::MissingTablesSection)?;
        let tables_range = tables_section.range();
        let tables_size = tables_range.end - tables_range.start;
        let (_, relative_range) = packed_stream_range(table_metadata, stream_id, tables_size)?;
        let mut columns = Vec::with_capacity(separate.columns().len());
        let mut sizes = Vec::with_capacity(separate.columns().len());
        let mut offset = tables_range.start + relative_range.start;
        for (index, frame) in separate.columns().iter().enumerate() {
            let size = usize::try_from(frame.uncompressed_size())
                .map_err(|_| PackedStreamError::SizeOverflow)?;
            sizes.push(size);
            let load = wanted.is_none_or(|mask| mask.get(index).copied().unwrap_or(true));
            let end = offset
                .checked_add(frame.compressed_size())
                .ok_or(PackedStreamError::SizeOverflow)?;
            if load && 0 != size {
                let frame_meta = PackedStreamMetadata::for_frame(frame.compressed_size(), frame.uncompressed_size());
                let compressed = self.reader_for_range(offset..end).map_err(PackedStreamError::Io)?;
                let decoded = decode_packed_stream(compressed, &frame_meta, limits)?;
                columns.push(Some(decoded.into_bytes()));
            } else if load {
                columns.push(Some(Vec::new()));
            } else {
                columns.push(None);
            }
            offset = end;
        }
        Ok(DecodedPackedStream::from_columns(columns, &sizes))
    }


    /// Decompresses `/var.dict` and preserves every value as arbitrary bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds section, I/O or zstd failures,
    /// resource-limit violations, malformed entry framing, or trailing data.
    pub fn read_variable_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<VariableDictionary, DictionaryError> {
        let compressed = self.dictionary_reader(metadata, DictionarySection::Variable)?;
        decode_variable_dictionary(compressed, limits)
    }

    /// Decompresses `/log.dict`, validates escaping, and counts unescaped placeholders.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds section, I/O or zstd failures,
    /// resource-limit violations, malformed entry framing or escaping, or trailing data.
    pub fn read_log_type_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<LogTypeDictionary, DictionaryError> {
        let compressed = self.dictionary_reader(metadata, DictionarySection::LogType)?;
        decode_logtype_dictionary(compressed, limits)
    }

    /// Decompresses `/array.dict`, validates escaping, and counts unescaped placeholders.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or out-of-bounds section, I/O or zstd failures,
    /// resource-limit violations, malformed entry framing or escaping, or trailing data.
    pub fn read_array_dictionary(
        &mut self,
        metadata: &ArchiveMetadata,
        limits: DictionaryLimits,
    ) -> Result<ArrayDictionary, DictionaryError> {
        let compressed = self.dictionary_reader(metadata, DictionarySection::Array)?;
        decode_array_dictionary(compressed, limits)
    }

    /// Consumes the archive reader and returns its underlying source.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.source
    }

    fn reader_for_range(&mut self, range: Range<u64>) -> io::Result<Take<&mut R>> {
        self.source.seek(SeekFrom::Start(range.start))?;
        Ok(self.source.by_ref().take(range.end - range.start))
    }

    fn dictionary_reader(
        &mut self,
        metadata: &ArchiveMetadata,
        dictionary: DictionarySection,
    ) -> Result<Take<&mut R>, DictionaryError> {
        let section =
            metadata
                .directory()
                .get(dictionary.name())
                .ok_or(DictionaryError::MissingSection {
                    section: dictionary,
                })?;
        let range = section.range();
        let files_range = self.layout.files_range();
        if range.start < files_range.start || range.end > files_range.end || range.start > range.end
        {
            return Err(DictionaryError::SectionOutsideArchive {
                section: dictionary,
            });
        }
        self.reader_for_range(range).map_err(DictionaryError::Io)
    }
}

/// Failure to open the outer envelope of a structured single-file archive.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// The source could not be measured, sought, or read.
    Io(io::Error),
    /// The fixed archive header was invalid.
    Header(HeaderDecodeError),
    /// The archive version is not supported by this reader implementation.
    UnsupportedVersion {
        /// Version decoded from the archive header.
        actual: ArchiveVersion,
    },
    /// Header sizes were inconsistent with the source.
    Layout(LayoutError),
}

impl Display for OpenError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "failed to read structured archive: {error}"),
            Self::Header(error) => write!(formatter, "invalid structured archive header: {error}"),
            Self::UnsupportedVersion { actual } => write!(
                formatter,
                "unsupported structured archive version {actual}; expected {}.{}.x",
                ArchiveVersion::CURRENT.major(),
                ArchiveVersion::CURRENT.minor()
            ),
            Self::Layout(error) => write!(formatter, "invalid structured archive layout: {error}"),
        }
    }
}

impl Error for OpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Header(error) => Some(error),
            Self::UnsupportedVersion { .. } => None,
            Self::Layout(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    struct EndRejectingSource(Cursor<Vec<u8>>);

    impl Read for EndRejectingSource {
        fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
            self.0.read(destination)
        }
    }

    impl Seek for EndRejectingSource {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            if matches!(position, SeekFrom::End(_)) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "end-relative seek is unsupported",
                ));
            }
            self.0.seek(position)
        }
    }

    fn archive_bytes(metadata: &[u8], files: &[u8]) -> Vec<u8> {
        let archive_size = u64::try_from(SFA_HEADER_SIZE + metadata.len() + files.len())
            .expect("test archive size fits u64");
        let metadata_size = u32::try_from(metadata.len()).expect("metadata size fits u32");
        let header = ArchiveHeader::new(123, archive_size, metadata_size);

        let mut bytes = Vec::with_capacity(
            usize::try_from(archive_size).expect("test archive size fits usize"),
        );
        bytes.extend_from_slice(&header.encode());
        bytes.extend_from_slice(metadata);
        bytes.extend_from_slice(files);
        bytes
    }

    #[test]
    fn streams_bounded_outer_sections() {
        let bytes = archive_bytes(b"metadata", b"files");
        let mut archive =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid archive envelope");

        assert_eq!(123, archive.header().uncompressed_size());

        let mut metadata = Vec::new();
        archive
            .metadata_reader()
            .expect("metadata seek")
            .read_to_end(&mut metadata)
            .expect("metadata read");
        assert_eq!(b"metadata", metadata.as_slice());

        let mut files = Vec::new();
        archive
            .files_reader()
            .expect("files seek")
            .read_to_end(&mut files)
            .expect("files read");
        assert_eq!(b"files", files.as_slice());
    }

    #[test]
    fn streaming_open_does_not_require_an_end_seek() {
        let bytes = archive_bytes(b"metadata", b"files");
        let source_size = u64::try_from(bytes.len()).expect("test source size fits u64");
        let mut archive = SingleFileArchiveReader::open_streaming(
            EndRejectingSource(Cursor::new(bytes)),
            Some(source_size),
        )
        .expect("open forward source");

        let mut metadata = Vec::new();
        archive
            .metadata_reader()
            .expect("metadata forward seek")
            .read_to_end(&mut metadata)
            .expect("metadata read");
        assert_eq!(b"metadata", metadata.as_slice());
    }

    #[test]
    fn streaming_open_uses_logical_byte_zero_for_all_absolute_ranges() {
        let archive = archive_bytes(&[], &[]);
        let mut prefixed = b"prefix".to_vec();
        let archive_offset = u64::try_from(prefixed.len()).expect("prefix size fits u64");
        prefixed.extend_from_slice(&archive);
        let mut source = Cursor::new(prefixed);
        source.set_position(archive_offset);

        let error = SingleFileArchiveReader::open_streaming(source, None)
            .expect_err("a physical prefix cannot redefine logical byte zero");
        assert!(matches!(error, OpenError::Header(_)));
    }

    #[test]
    fn unknown_stream_size_uses_declared_boundary_but_known_size_rejects_trailing_data() {
        let mut bytes = archive_bytes(&[], &[]);
        bytes.push(0);
        let actual_size = u64::try_from(bytes.len()).expect("test source size fits u64");

        let archive = SingleFileArchiveReader::open_streaming(Cursor::new(bytes.clone()), None)
            .expect("unknown source size uses header declaration");
        assert_eq!(actual_size - 1, archive.layout().archive_size());

        let error = SingleFileArchiveReader::open_streaming(Cursor::new(bytes), Some(actual_size))
            .expect_err("known trailing byte must fail");
        assert!(matches!(
            error,
            OpenError::Layout(LayoutError::ArchiveSizeMismatch { .. })
        ));
    }

    #[test]
    fn rejects_a_short_header_as_io() {
        let error = SingleFileArchiveReader::open(Cursor::new(vec![0_u8; SFA_HEADER_SIZE - 1]))
            .expect_err("short source must fail");
        assert!(matches!(error, OpenError::Io(_)));
    }

    #[test]
    fn rejects_an_invalid_header() {
        let mut bytes = archive_bytes(&[], &[]);
        bytes[0] = 0;
        let error =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect_err("invalid magic must fail");
        assert!(matches!(error, OpenError::Header(_)));
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let mut bytes = archive_bytes(&[], &[]);
        bytes[4..8].copy_from_slice(&ArchiveVersion::new(1, 0, 0).to_wire().to_le_bytes());
        let error = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect_err("unsupported version must fail");
        assert!(matches!(
            error,
            OpenError::UnsupportedVersion {
                actual
            } if actual == ArchiveVersion::new(1, 0, 0)
        ));
    }

    #[test]
    fn rejects_a_declared_size_mismatch() {
        let mut bytes = archive_bytes(&[], &[]);
        bytes.push(0);
        let error = SingleFileArchiveReader::open(Cursor::new(bytes))
            .expect_err("trailing source byte must fail");
        assert!(matches!(
            error,
            OpenError::Layout(LayoutError::ArchiveSizeMismatch { .. })
        ));
    }
}
