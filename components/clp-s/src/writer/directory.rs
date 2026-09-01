//! Canonical directory-archive finalization and caller-owned member sinks.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;

use super::EncodedEmptyArchive;
use super::RecordEventAppender;
use super::WriterError;
use super::WriterLimits;
use super::WriterOptions;
use super::WriterResource;
use super::append_packet;
use super::check_limit;
use super::compress;
use super::encode_records;
use super::len_u64;
use super::primitive::AppendError;
use super::primitive::PrimitiveArchive;
use super::primitive::RecordEventAppendError;
use super::primitive::RecordEventRef;
use super::primitive::RecordRef;
use super::primitive::ReplayableRecordEventSource;
use crate::archive::ArchiveHeader;
use crate::archive::DirectoryArchiveMember;
use crate::archive::SFA_HEADER_SIZE;

const DIRECTORY_MEMBER_COUNT: usize = DirectoryArchiveMember::ALL.len();
const RANGE_INDEX_PACKET_TYPE: u8 = 3;

/// Caller-owned transactional destination for the eight canonical directory members.
///
/// [`EncodedDirectoryArchive::write_to`] calls [`Self::write_member`] exactly once per member in
/// [`DirectoryArchiveMember::ALL`] order, then calls [`Self::commit`]. Implementations that can
/// stage writes should keep them invisible until `commit`; dropping a sink after a member error
/// should abort or leave only explicitly recoverable staging state. Archive encoding completes
/// before the first sink call.
pub trait DirectoryArchiveSink: Sized {
    /// Sink-specific write or publication error.
    type Error;
    /// Caller-owned result returned after publication.
    type Output;

    /// Stages one complete canonical member.
    ///
    /// # Errors
    ///
    /// Returns a sink-specific error without publishing a complete archive.
    fn write_member(
        &mut self,
        member: DirectoryArchiveMember,
        contents: &[u8],
    ) -> Result<(), Self::Error>;

    /// Publishes all staged members and returns the caller-owned result.
    ///
    /// # Errors
    ///
    /// Returns a sink-specific error if publication cannot complete.
    fn commit(self) -> Result<Self::Output, Self::Error>;
}

/// An archive whose borrowed records have not yet been encoded as directory members.
///
/// This core type owns no filesystem path or output sink. [`Self::finish`] returns all eight
/// canonical member buffers for bindings and custom storage, while [`Self::finish_to`] additionally
/// drives a caller-owned [`DirectoryArchiveSink`].
#[derive(Debug, Default)]
#[must_use = "an open directory archive must be finished or explicitly aborted"]
pub struct OpenDirectoryArchive {
    options: WriterOptions,
    records: PrimitiveArchive,
}

impl OpenDirectoryArchive {
    /// Creates an empty directory archive without touching external state.
    pub fn new(options: WriterOptions) -> Self {
        Self {
            options,
            records: PrimitiveArchive::default(),
        }
    }

    /// Validates and atomically appends one borrowed record.
    ///
    /// # Errors
    ///
    /// Returns a structured append error before schema, dictionary, table, or record-count state
    /// changes.
    pub fn append_record(&mut self, record: RecordRef<'_>) -> Result<(), AppendError> {
        self.records.append(
            record,
            self.options.limits(),
            self.options.records_log_order(),
        )
    }

    /// Validates and atomically appends one flat borrowed record traversal.
    ///
    /// The root object is implicit; balanced object events describe nested objects without a
    /// self-referential borrowed tree.
    ///
    /// # Errors
    ///
    /// Returns a structured append error before schema, dictionary, table, or record-count state
    /// changes.
    pub fn append_record_events<'record, I>(&mut self, events: I) -> Result<(), AppendError>
    where
        I: IntoIterator<Item = RecordEventRef<'record>>, {
        self.records.append_events(
            events,
            self.options.limits(),
            self.options.records_log_order(),
        )
    }

    /// Validates and atomically appends a fallible flat borrowed record traversal.
    ///
    /// # Errors
    ///
    /// Returns the source failure with its event index, or an archive validation/planning error.
    pub fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        self.records.try_append_events(
            events,
            self.options.limits(),
            self.options.records_log_order(),
        )
    }

    pub(crate) fn try_append_replayable_record_events<'record, S>(
        &mut self,
        source: S,
    ) -> Result<(), RecordEventAppendError<S::Error>>
    where
        S: ReplayableRecordEventSource<'record>, {
        self.records.try_append_replayable_events(
            source,
            self.options.limits(),
            self.options.records_log_order(),
        )
    }

    /// Returns the number of successfully appended records.
    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.records.record_count()
    }

    /// Returns the number of distinct accumulated schema shapes.
    #[must_use]
    pub const fn schema_count(&self) -> usize {
        self.records.schema_count()
    }

    /// Returns owned key, schema-entry, dictionary-value, and encoded-column payload bytes.
    #[must_use]
    pub const fn resident_bytes(&self) -> u64 {
        self.records.resident_bytes()
    }

    /// Returns the C++ `get_data_size` archive-rotation metric.
    ///
    /// This is dictionary entry data plus bytes appended by encoded messages. It intentionally
    /// excludes schema, column headers, archive headers, metadata, and container overhead.
    #[must_use]
    pub const fn encoded_data_size(&self) -> u64 {
        self.records.encoded_data_size()
    }

    /// Adds caller-known source bytes to the archive header's uncompressed-size statistic.
    ///
    /// # Errors
    ///
    /// Returns [`WriterError::SizeOverflow`] without changing the statistic if it exceeds `u64`.
    pub fn add_uncompressed_bytes(&mut self, bytes: u64) -> Result<(), WriterError> {
        self.options.uncompressed_size = self
            .options
            .uncompressed_size
            .checked_add(bytes)
            .ok_or(WriterError::SizeOverflow)?;
        Ok(())
    }

    /// Returns caller-accounted source bytes for this open archive.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.options.uncompressed_size
    }

    pub(super) fn timestamp_bounds(&self) -> (i64, i64) {
        self.records.timestamp_bounds()
    }

    /// Abandons the in-memory archive without encoding or touching a sink.
    pub fn abort(self) {}

    /// Consumes the writer and encodes the eight canonical directory members.
    ///
    /// # Errors
    ///
    /// Returns an encoding, limit, allocation, or checked-size error. No external sink is touched.
    pub fn finish(self) -> Result<EncodedDirectoryArchive, WriterError> {
        EncodedDirectoryArchive::from_encoded(encode_records(&self.records, self.options)?)
    }

    /// Encodes the complete archive before driving a caller-owned transactional sink.
    ///
    /// # Errors
    ///
    /// Returns [`DirectoryWriterError::Encoding`] before the first sink call, or a member/commit
    /// error reported by the sink.
    pub fn finish_to<S: DirectoryArchiveSink>(
        self,
        sink: S,
    ) -> Result<FinishedDirectoryArchive<S::Output>, DirectoryWriterError<S::Error>> {
        self.finish()
            .map_err(DirectoryWriterError::Encoding)?
            .write_to(sink)
    }
}

impl RecordEventAppender for OpenDirectoryArchive {
    fn try_append_record_events<'record, I, E>(
        &mut self,
        events: I,
    ) -> Result<(), RecordEventAppendError<E>>
    where
        I: IntoIterator<Item = Result<RecordEventRef<'record>, E>>, {
        Self::try_append_record_events(self, events)
    }
}

/// Fully encoded, contiguous canonical directory members.
///
/// Each data member owns the same `Vec<u8>` produced for SFA output; finalization only combines the
/// fixed header and metadata frame into the directory's `header` member. No complete concatenated
/// archive buffer is materialized.
#[derive(Debug)]
pub struct EncodedDirectoryArchive {
    header: ArchiveHeader,
    members: [Vec<u8>; DIRECTORY_MEMBER_COUNT],
}

impl EncodedDirectoryArchive {
    fn from_encoded(encoded: EncodedEmptyArchive) -> Result<Self, WriterError> {
        let EncodedEmptyArchive {
            header,
            metadata,
            sections,
            archive_size,
        } = encoded;
        let header_member_capacity = SFA_HEADER_SIZE
            .checked_add(metadata.len())
            .ok_or(WriterError::SizeOverflow)?;
        let mut header_member = Vec::new();
        header_member
            .try_reserve_exact(header_member_capacity)
            .map_err(|_| WriterError::AllocationFailed {
                requested: header_member_capacity,
            })?;
        header_member.extend_from_slice(&header.encode());
        header_member.extend_from_slice(&metadata);
        let [
            schema_tree,
            schema_ids,
            table_metadata,
            variable,
            log_type,
            array,
            packed_streams,
        ] = sections;
        let members = [
            header_member,
            schema_tree,
            schema_ids,
            table_metadata,
            variable,
            log_type,
            array,
            packed_streams,
        ];
        debug_assert_eq!(
            archive_size,
            members
                .iter()
                .map(|member| u64::try_from(member.len()).unwrap_or(u64::MAX))
                .sum::<u64>()
        );
        Ok(Self { header, members })
    }

    /// Adds an already serialized range-index payload to this writer-owned archive.
    ///
    /// The base encoder deliberately emits only the three required metadata packets. Archive-set
    /// source tracking is the layer that knows whether a range index exists, so it appends the
    /// optional packet after record finalization and replaces only the header member. This helper
    /// is crate-private to keep arbitrary metadata rewriting out of the public writer API.
    pub(super) fn with_range_index(
        mut self,
        payload: &[u8],
        compression_level: i32,
        limits: WriterLimits,
    ) -> Result<Self, WriterError> {
        let header_member = self.member(DirectoryArchiveMember::Header);
        let compressed_metadata = header_member
            .get(SFA_HEADER_SIZE..)
            .ok_or(WriterError::SizeOverflow)?;
        let mut metadata =
            zstd::stream::decode_all(compressed_metadata).map_err(WriterError::Io)?;
        let packet_count = metadata.first_mut().ok_or(WriterError::SizeOverflow)?;
        debug_assert_eq!(3, *packet_count);
        *packet_count = packet_count
            .checked_add(1)
            .ok_or(WriterError::SizeOverflow)?;
        let packet_size = 1_usize
            .checked_add(size_of::<u32>())
            .and_then(|size| size.checked_add(payload.len()))
            .ok_or(WriterError::SizeOverflow)?;
        metadata
            .try_reserve_exact(packet_size)
            .map_err(|_| WriterError::AllocationFailed {
                requested: packet_size,
            })?;
        append_packet(&mut metadata, RANGE_INDEX_PACKET_TYPE, payload)?;
        check_limit(
            WriterResource::DecompressedMetadata,
            len_u64(&metadata)?,
            limits.max_metadata_decompressed_size(),
        )?;

        let metadata = compress(&metadata, compression_level)?;
        let metadata_size = len_u64(&metadata)?;
        check_limit(
            WriterResource::CompressedMetadata,
            metadata_size,
            limits.max_metadata_compressed_size(),
        )?;
        let metadata_size_u32 =
            u32::try_from(metadata_size).map_err(|_| WriterError::SizeOverflow)?;
        let old_header_member_size = len_u64(header_member)?;
        let data_size = self
            .total_size()
            .checked_sub(old_header_member_size)
            .ok_or(WriterError::SizeOverflow)?;
        let archive_size = u64::try_from(SFA_HEADER_SIZE)
            .map_err(|_| WriterError::SizeOverflow)?
            .checked_add(metadata_size)
            .and_then(|size| size.checked_add(data_size))
            .ok_or(WriterError::SizeOverflow)?;
        check_limit(
            WriterResource::Archive,
            archive_size,
            limits.max_archive_size(),
        )?;
        let header = ArchiveHeader::new(
            self.header.uncompressed_size(),
            archive_size,
            metadata_size_u32,
        );
        let header_member_capacity = SFA_HEADER_SIZE
            .checked_add(metadata.len())
            .ok_or(WriterError::SizeOverflow)?;
        let mut new_header_member = Vec::new();
        new_header_member
            .try_reserve_exact(header_member_capacity)
            .map_err(|_| WriterError::AllocationFailed {
                requested: header_member_capacity,
            })?;
        new_header_member.extend_from_slice(&header.encode());
        new_header_member.extend_from_slice(&metadata);
        self.header = header;
        self.members[member_index(DirectoryArchiveMember::Header)] = new_header_member;
        Ok(self)
    }

    /// Returns the canonical archive header shared by both directory and SFA layouts.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        &self.header
    }

    /// Returns one complete canonical member buffer.
    #[must_use]
    pub fn member(&self, member: DirectoryArchiveMember) -> &[u8] {
        &self.members[member_index(member)]
    }

    /// Iterates all complete members in canonical order.
    pub fn members(
        &self,
    ) -> impl ExactSizeIterator<Item = (DirectoryArchiveMember, &[u8])> + DoubleEndedIterator {
        DirectoryArchiveMember::ALL
            .into_iter()
            .zip(self.members.iter().map(Vec::as_slice))
    }

    /// Returns the aggregate bytes across all eight physical members.
    #[must_use]
    pub const fn total_size(&self) -> u64 {
        self.header.compressed_size()
    }

    /// Writes every member and commits a caller-owned sink.
    ///
    /// The encoded member buffers remain available if the sink rejects a member or commit, allowing
    /// the caller to retry with another destination.
    ///
    /// # Errors
    ///
    /// Returns the failing member and sink error, or a commit error after all member writes.
    pub fn write_to<S: DirectoryArchiveSink>(
        &self,
        mut sink: S,
    ) -> Result<FinishedDirectoryArchive<S::Output>, DirectoryWriterError<S::Error>> {
        for (member, contents) in self.members() {
            sink.write_member(member, contents)
                .map_err(|source| DirectoryWriterError::Member { member, source })?;
        }
        let output = sink
            .commit()
            .map_err(|source| DirectoryWriterError::Commit { source })?;
        Ok(FinishedDirectoryArchive {
            output,
            header: self.header,
        })
    }
}

/// A successfully published directory archive and its caller-owned sink result.
#[derive(Debug)]
pub struct FinishedDirectoryArchive<O> {
    output: O,
    header: ArchiveHeader,
}

impl<O> FinishedDirectoryArchive<O> {
    /// Returns the published archive header.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        &self.header
    }

    /// Consumes the result and returns the sink-defined output.
    #[must_use]
    pub fn into_inner(self) -> O {
        self.output
    }
}

/// Failure to encode, stage, or publish a canonical directory archive.
#[derive(Debug)]
#[non_exhaustive]
pub enum DirectoryWriterError<E> {
    /// Archive encoding failed before the sink was touched.
    Encoding(WriterError),
    /// A complete canonical member could not be staged.
    Member {
        /// Member whose write failed.
        member: DirectoryArchiveMember,
        /// Sink-specific failure.
        source: E,
    },
    /// All members were staged, but publication failed.
    Commit {
        /// Sink-specific failure.
        source: E,
    },
}

impl<E: Display> Display for DirectoryWriterError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encoding(error) => {
                write!(formatter, "directory archive encoding failed: {error}")
            }
            Self::Member { member, source } => {
                write!(
                    formatter,
                    "failed to write directory member {member}: {source}"
                )
            }
            Self::Commit { source } => {
                write!(formatter, "failed to commit directory archive: {source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for DirectoryWriterError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encoding(error) => Some(error),
            Self::Member { source, .. } | Self::Commit { source } => Some(source),
        }
    }
}

const fn member_index(member: DirectoryArchiveMember) -> usize {
    match member {
        DirectoryArchiveMember::Header => 0,
        DirectoryArchiveMember::SchemaTree => 1,
        DirectoryArchiveMember::SchemaIds => 2,
        DirectoryArchiveMember::TableMetadata => 3,
        DirectoryArchiveMember::VariableDictionary => 4,
        DirectoryArchiveMember::LogTypeDictionary => 5,
        DirectoryArchiveMember::ArrayDictionary => 6,
        DirectoryArchiveMember::PackedStreams => 7,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::io::Cursor;
    use std::rc::Rc;

    use super::*;
    use crate::ExtractionMode;
    use crate::ExtractionOptions;
    use crate::archive::DirectoryArchiveReader;
    use crate::archive::DirectoryArchiveSource;
    use crate::archive::MetadataLimits;
    use crate::extract_jsonl;
    use crate::writer::FieldRef;
    use crate::writer::OpenArchive;
    use crate::writer::UnstructuredArrayRef;
    use crate::writer::ValueRef;
    use crate::writer::WriterLimits;
    use crate::writer::WriterResource;

    const SOURCE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-input.jsonl");
    const SFA_ORACLE: &[u8] =
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp.bin");
    const ARRAY_ROWS: &[&[u8]] = &[
        b"[]",
        br#"[1,true,null,"x",{"k":"v"},[2,3]]"#,
        br#"[2,false,null,"y",{"k":"w"},[4,5]]"#,
        br#"[ -7, 12.50 , "user=face", {"n": 9} ]"#,
        br#"["slash\\\\marker","\u0011\u0012\u0013"]"#,
        br#"[[],{},[{"x":[]}]]"#,
    ];
    const PINNED_MEMBERS: [&[u8]; DIRECTORY_MEMBER_COUNT] = [
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/header"),
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/schema_tree"),
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/schema_ids"),
        include_bytes!(
            "../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/table_metadata"
        ),
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/var.dict"),
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/log.dict"),
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/array.dict"),
        include_bytes!("../../tests/fixtures/sfa-v0.5.0-unstructured-arrays-cpp-dir/0"),
    ];

    fn options() -> WriterOptions {
        WriterOptions::default()
            .with_log_order(false)
            .with_uncompressed_size(u64::try_from(SOURCE.len()).expect("source size fits u64"))
    }

    fn append_corpus(archive: &mut OpenDirectoryArchive) {
        for (kind, raw_json) in (0_i64..).zip(ARRAY_ROWS.iter().copied()) {
            let fields = [
                FieldRef::new(
                    b"array",
                    ValueRef::UnstructuredArray(UnstructuredArrayRef::new(raw_json)),
                ),
                FieldRef::new(b"kind", ValueRef::I64(kind)),
            ];
            archive
                .append_record(RecordRef::new(&fields))
                .expect("append directory-oracle record");
        }
    }

    fn encoded_corpus() -> EncodedDirectoryArchive {
        let mut archive = OpenDirectoryArchive::new(options());
        append_corpus(&mut archive);
        archive.finish().expect("encode directory archive")
    }

    #[derive(Debug)]
    struct MemoryArchive {
        members: [Vec<u8>; DIRECTORY_MEMBER_COUNT],
    }

    impl DirectoryArchiveSource for MemoryArchive {
        type Reader = Cursor<Vec<u8>>;

        fn open_member(
            &mut self,
            member: DirectoryArchiveMember,
        ) -> io::Result<Option<Self::Reader>> {
            Ok(Some(Cursor::new(
                self.members[member_index(member)].clone(),
            )))
        }
    }

    #[derive(Debug, Default)]
    struct MemorySink {
        members: [Option<Vec<u8>>; DIRECTORY_MEMBER_COUNT],
    }

    impl DirectoryArchiveSink for MemorySink {
        type Error = io::Error;
        type Output = MemoryArchive;

        fn write_member(
            &mut self,
            member: DirectoryArchiveMember,
            contents: &[u8],
        ) -> io::Result<()> {
            let destination = &mut self.members[member_index(member)];
            if destination.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "duplicate member",
                ));
            }
            *destination = Some(contents.to_vec());
            Ok(())
        }

        fn commit(self) -> io::Result<Self::Output> {
            if self.members.iter().any(Option::is_none) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete memory archive",
                ));
            }
            Ok(MemoryArchive {
                members: self
                    .members
                    .map(|member| member.expect("all members were checked before commit")),
            })
        }
    }

    #[test]
    fn every_member_is_byte_identical_to_cpp_and_concatenates_to_the_sfa() {
        let encoded = encoded_corpus();
        assert_eq!(618, encoded.total_size());
        for (((actual_member, actual), expected_member), expected) in encoded
            .members()
            .zip(DirectoryArchiveMember::ALL)
            .zip(PINNED_MEMBERS)
        {
            assert_eq!(expected_member, actual_member);
            assert_eq!(expected, actual, "member {actual_member}");
        }
        let concatenated = encoded
            .members()
            .flat_map(|(_, contents)| contents.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(SFA_ORACLE, concatenated);
    }

    #[test]
    fn public_sink_and_directory_reader_extract_the_exact_cpp_corpus() {
        let encoded = encoded_corpus();
        let memory = encoded
            .write_to(MemorySink::default())
            .expect("write memory directory")
            .into_inner();
        let mut reader = DirectoryArchiveReader::open(memory, MetadataLimits::default())
            .expect("open generated directory archive");
        let mut extracted = Vec::new();
        extract_jsonl(
            &mut reader,
            &mut extracted,
            ExtractionOptions::new(ExtractionMode::Unordered),
        )
        .expect("extract generated directory archive");
        assert_eq!(SOURCE, extracted);
    }

    #[test]
    fn directory_and_sfa_finalizers_share_identical_encoded_bytes() {
        let encoded = encoded_corpus();
        let mut sfa = OpenArchive::new(Cursor::new(Vec::new()), options());
        for (kind, raw_json) in (0_i64..).zip(ARRAY_ROWS.iter().copied()) {
            let fields = [
                FieldRef::new(
                    b"array",
                    ValueRef::UnstructuredArray(UnstructuredArrayRef::new(raw_json)),
                ),
                FieldRef::new(b"kind", ValueRef::I64(kind)),
            ];
            sfa.append_record(RecordRef::new(&fields))
                .expect("append SFA comparison record");
        }
        let sfa = sfa
            .finish()
            .expect("finish comparison SFA")
            .into_inner()
            .into_inner();
        let directory_bytes = encoded
            .members()
            .flat_map(|(_, contents)| contents.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(sfa, directory_bytes);
        assert_eq!(SFA_ORACLE, directory_bytes);
    }

    #[derive(Clone)]
    struct ObservedSink {
        calls: Rc<Cell<usize>>,
        commits: Rc<Cell<usize>>,
        fail_at: Option<DirectoryArchiveMember>,
    }

    impl DirectoryArchiveSink for ObservedSink {
        type Error = io::Error;
        type Output = ();

        fn write_member(
            &mut self,
            member: DirectoryArchiveMember,
            _contents: &[u8],
        ) -> io::Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.fail_at == Some(member) {
                Err(io::Error::other("injected member failure"))
            } else {
                Ok(())
            }
        }

        fn commit(self) -> io::Result<Self::Output> {
            self.commits.set(self.commits.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn encoding_precedes_sink_calls_and_member_failure_never_commits() {
        let calls = Rc::new(Cell::new(0));
        let commits = Rc::new(Cell::new(0));
        let sink = ObservedSink {
            calls: Rc::clone(&calls),
            commits: Rc::clone(&commits),
            fail_at: None,
        };
        let limits = WriterLimits::new(u64::MAX, u64::MAX, u64::MAX, 0);
        let archive = OpenDirectoryArchive::new(
            WriterOptions::default()
                .with_log_order(false)
                .with_limits(limits),
        );
        assert!(matches!(
            archive.finish_to(sink),
            Err(DirectoryWriterError::Encoding(WriterError::LimitExceeded {
                resource: WriterResource::Archive,
                ..
            }))
        ));
        assert_eq!(0, calls.get());
        assert_eq!(0, commits.get());

        let encoded = encoded_corpus();
        let sink = ObservedSink {
            calls: Rc::clone(&calls),
            commits: Rc::clone(&commits),
            fail_at: Some(DirectoryArchiveMember::SchemaIds),
        };
        assert!(matches!(
            encoded.write_to(sink),
            Err(DirectoryWriterError::Member {
                member: DirectoryArchiveMember::SchemaIds,
                ..
            })
        ));
        assert_eq!(3, calls.get());
        assert_eq!(0, commits.get());

        encoded
            .write_to(MemorySink::default())
            .expect("encoded buffers remain retryable after sink failure");
    }
}
