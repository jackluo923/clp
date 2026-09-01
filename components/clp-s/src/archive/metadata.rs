use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::io::Cursor;
use std::io::Read;
use std::io::Take;
use std::io::{self};
use std::ops::Range;

use serde::Deserialize;
use serde::Serialize;

use super::layout::SingleFileArchiveLayout;
use super::range_index::RangeIndex;
use super::range_index::RangeIndexError;
use super::range_index::RangeIndexLimits;
use super::timestamp_dictionary::TimestampDictionary;
use super::timestamp_dictionary::TimestampDictionaryError;
use super::timestamp_dictionary::TimestampDictionaryLimits;

/// Canonical names and physical order of the seven sections concatenated after SFA metadata.
pub const SFA_SECTION_NAMES: [&str; 7] = [
    "/schema_tree",
    "/schema_ids",
    "/table_metadata",
    "/var.dict",
    "/log.dict",
    "/array.dict",
    "/0",
];

const ARCHIVE_INFO_PACKET_TYPE: u8 = 0;
const ARCHIVE_FILE_INFO_PACKET_TYPE: u8 = 1;
const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;
const RANGE_INDEX_PACKET_TYPE: u8 = 3;
const PACKET_HEADER_SIZE: u64 = 5;

/// Resource limits applied while decoding the archive metadata frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataLimits {
    compressed: u64,
    decompressed_total: u64,
    packet_payload: u32,
    timestamp_dictionary: TimestampDictionaryLimits,
    range_index: RangeIndexLimits,
}

impl MetadataLimits {
    /// Creates explicit metadata resource limits.
    #[must_use]
    pub const fn new(
        max_compressed_size: u64,
        max_decompressed_size: u64,
        max_packet_size: u32,
    ) -> Self {
        Self {
            compressed: max_compressed_size,
            decompressed_total: max_decompressed_size,
            packet_payload: max_packet_size,
            timestamp_dictionary: TimestampDictionaryLimits::DEFAULT,
            range_index: RangeIndexLimits::DEFAULT,
        }
    }

    /// Replaces the limits used for the typed timestamp-dictionary packet.
    #[must_use]
    pub const fn with_timestamp_dictionary_limits(
        mut self,
        limits: TimestampDictionaryLimits,
    ) -> Self {
        self.timestamp_dictionary = limits;
        self
    }

    /// Replaces the limits used for the typed range-index packet.
    #[must_use]
    pub const fn with_range_index_limits(mut self, limits: RangeIndexLimits) -> Self {
        self.range_index = limits;
        self
    }

    /// Maximum compressed metadata bytes accepted from the SFA header.
    #[must_use]
    pub const fn max_compressed_size(self) -> u64 {
        self.compressed
    }

    /// Maximum bytes accepted after metadata decompression.
    #[must_use]
    pub const fn max_decompressed_size(self) -> u64 {
        self.decompressed_total
    }

    /// Maximum decompressed payload size accepted for any one packet.
    #[must_use]
    pub const fn max_packet_size(self) -> u32 {
        self.packet_payload
    }

    /// Limits used for the typed timestamp-dictionary packet.
    #[must_use]
    pub const fn timestamp_dictionary_limits(self) -> TimestampDictionaryLimits {
        self.timestamp_dictionary
    }

    /// Limits used for the typed range-index packet.
    #[must_use]
    pub const fn range_index_limits(self) -> RangeIndexLimits {
        self.range_index
    }
}

impl Default for MetadataLimits {
    fn default() -> Self {
        const MEBIBYTE: u64 = 1024 * 1024;
        Self::new(64 * MEBIBYTE, 256 * MEBIBYTE, 64 * 1024 * 1024)
    }
}

/// A validated named section in an SFA source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSection {
    name: String,
    range: Range<u64>,
}

impl ArchiveSection {
    /// Returns the canonical metadata name, including its leading slash.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the section's absolute byte range in the SFA source.
    #[must_use]
    pub fn range(&self) -> Range<u64> {
        self.range.clone()
    }

    /// Returns the compressed section size.
    #[must_use]
    pub const fn compressed_size(&self) -> u64 {
        self.range.end - self.range.start
    }
}

/// Validated directory of the seven sections concatenated in an SFA.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SectionDirectory {
    sections: Vec<ArchiveSection>,
}

impl SectionDirectory {
    /// Returns all sections in physical archive order.
    #[must_use]
    pub fn sections(&self) -> &[ArchiveSection] {
        &self.sections
    }

    /// Finds a section by its canonical metadata name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ArchiveSection> {
        self.sections.iter().find(|section| section.name == name)
    }

    fn validate(
        files: &[ArchiveFileInfo],
        layout: SingleFileArchiveLayout,
    ) -> Result<Self, MetadataError> {
        if files.len() != SFA_SECTION_NAMES.len() {
            return Err(MetadataError::UnexpectedSectionCount {
                actual: files.len(),
                expected: SFA_SECTION_NAMES.len(),
            });
        }

        let files_range = layout.files_range();
        let files_size = files_range.end - files_range.start;
        let mut sections = Vec::with_capacity(files.len());

        for (index, (file, expected_name)) in files.iter().zip(SFA_SECTION_NAMES).enumerate() {
            if file.name != expected_name {
                return Err(MetadataError::UnexpectedSectionName {
                    index,
                    expected: expected_name,
                    actual: file.name.clone(),
                });
            }
            if 0 == index && 0 != file.offset {
                return Err(MetadataError::InvalidFirstSectionOffset {
                    actual: file.offset,
                });
            }
            let is_final_section = index + 1 == files.len();
            if file.offset > files_size || (!is_final_section && file.offset == files_size) {
                return Err(MetadataError::SectionOffsetOutOfBounds {
                    index,
                    offset: file.offset,
                    files_size,
                });
            }
            if index > 0 {
                let previous = files[index - 1].offset;
                if file.offset <= previous {
                    return Err(MetadataError::NonIncreasingSectionOffset {
                        index,
                        previous,
                        actual: file.offset,
                    });
                }
            }

            let relative_end = files.get(index + 1).map_or(files_size, |next| next.offset);
            sections.push(ArchiveSection {
                name: file.name.clone(),
                range: (files_range.start + file.offset)..(files_range.start + relative_end),
            });
        }

        Ok(Self { sections })
    }
}

/// Metadata needed to address the physical sections of an SFA.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveMetadata {
    directory: SectionDirectory,
    timestamp_dictionary: TimestampDictionary,
    range_index: Option<RangeIndex>,
    unknown_packet_count: u8,
}

impl ArchiveMetadata {
    /// Returns the validated section directory.
    #[must_use]
    pub const fn directory(&self) -> &SectionDirectory {
        &self.directory
    }

    /// Returns the structurally validated timestamp dictionary.
    #[must_use]
    pub const fn timestamp_dictionary(&self) -> &TimestampDictionary {
        &self.timestamp_dictionary
    }

    /// Returns the exact bounded timestamp-dictionary packet bytes.
    #[must_use]
    pub fn timestamp_dictionary_bytes(&self) -> &[u8] {
        self.timestamp_dictionary.encoded_bytes()
    }

    /// Returns the structurally validated optional range index.
    #[must_use]
    pub const fn range_index(&self) -> Option<&RangeIndex> {
        self.range_index.as_ref()
    }

    /// Returns the exact bounded optional range-index packet bytes.
    #[must_use]
    pub fn range_index_bytes(&self) -> Option<&[u8]> {
        self.range_index.as_ref().map(RangeIndex::encoded_bytes)
    }

    /// Returns the number of forward-compatible packet types skipped while decoding.
    #[must_use]
    pub const fn unknown_packet_count(&self) -> u8 {
        self.unknown_packet_count
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ArchiveInfo {
    num_segments: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ArchiveFileInfoPacket {
    files: Vec<ArchiveFileInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArchiveFileInfo {
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "o")]
    offset: u64,
}

pub(super) fn decode_metadata<R: Read>(
    compressed: Take<R>,
    layout: SingleFileArchiveLayout,
    limits: MetadataLimits,
) -> Result<ArchiveMetadata, MetadataError> {
    let compressed_size = compressed.limit();
    if compressed_size > limits.compressed {
        return Err(MetadataError::CompressedMetadataTooLarge {
            actual: compressed_size,
            limit: limits.compressed,
        });
    }

    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(MetadataError::Io)?
        .single_frame();
    let metadata = decode_packets(&mut decoder, layout, limits)?;

    let mut trailing = [0_u8; 1];
    if 0 != decoder.read(&mut trailing).map_err(MetadataError::Io)? {
        return Err(MetadataError::TrailingDecompressedData);
    }

    let compressed = decoder.finish();
    let remaining_compressed = u64::try_from(compressed.buffer().len())
        .map_err(|_| MetadataError::SizeOverflow)?
        .checked_add(compressed.get_ref().limit())
        .ok_or(MetadataError::SizeOverflow)?;
    if 0 != remaining_compressed {
        return Err(MetadataError::TrailingCompressedData {
            remaining: remaining_compressed,
        });
    }

    Ok(metadata)
}

fn decode_packets<R: Read>(
    reader: &mut R,
    layout: SingleFileArchiveLayout,
    limits: MetadataLimits,
) -> Result<ArchiveMetadata, MetadataError> {
    let packet_count = read_u8(reader)?;
    let mut decompressed_size = 1_u64;
    let mut archive_info_seen = false;
    let mut file_info = None;
    let mut timestamp_dictionary = None;
    let mut range_index = None;
    let mut unknown_packet_count = 0_u8;

    for packet_index in 0..packet_count {
        let packet_type = read_u8(reader)?;
        let packet_size = read_u32(reader)?;
        decompressed_size = decompressed_size
            .checked_add(PACKET_HEADER_SIZE)
            .and_then(|size| size.checked_add(u64::from(packet_size)))
            .ok_or(MetadataError::SizeOverflow)?;
        if decompressed_size > limits.decompressed_total {
            return Err(MetadataError::DecompressedMetadataTooLarge {
                actual: decompressed_size,
                limit: limits.decompressed_total,
            });
        }
        if packet_size > limits.packet_payload {
            return Err(MetadataError::PacketTooLarge {
                packet_index,
                actual: packet_size,
                limit: limits.packet_payload,
            });
        }

        match packet_type {
            ARCHIVE_INFO_PACKET_TYPE => {
                reject_duplicate(archive_info_seen, packet_type)?;
                let payload = read_payload(reader, packet_size)?;
                let info: ArchiveInfo = decode_msgpack(packet_type, &payload)?;
                if 1 != info.num_segments {
                    return Err(MetadataError::UnsupportedSegmentCount {
                        actual: info.num_segments,
                    });
                }
                archive_info_seen = true;
            }
            ARCHIVE_FILE_INFO_PACKET_TYPE => {
                reject_duplicate(file_info.is_some(), packet_type)?;
                let payload = read_payload(reader, packet_size)?;
                let packet: ArchiveFileInfoPacket = decode_msgpack(packet_type, &payload)?;
                file_info = Some(packet.files);
            }
            TIMESTAMP_DICTIONARY_PACKET_TYPE => {
                reject_duplicate(timestamp_dictionary.is_some(), packet_type)?;
                let payload = read_payload(reader, packet_size)?;
                timestamp_dictionary = Some(
                    TimestampDictionary::decode(payload, limits.timestamp_dictionary)
                        .map_err(|source| MetadataError::InvalidTimestampDictionary { source })?,
                );
            }
            RANGE_INDEX_PACKET_TYPE => {
                reject_duplicate(range_index.is_some(), packet_type)?;
                let payload = read_payload(reader, packet_size)?;
                range_index = Some(
                    RangeIndex::decode(payload, limits.range_index)
                        .map_err(|source| MetadataError::InvalidRangeIndex { source })?,
                );
            }
            _ => {
                skip_payload(reader, packet_size)?;
                unknown_packet_count = unknown_packet_count
                    .checked_add(1)
                    .ok_or(MetadataError::SizeOverflow)?;
            }
        }
    }

    if !archive_info_seen {
        return Err(MetadataError::MissingPacket {
            packet_type: ARCHIVE_INFO_PACKET_TYPE,
        });
    }
    let files = file_info.ok_or(MetadataError::MissingPacket {
        packet_type: ARCHIVE_FILE_INFO_PACKET_TYPE,
    })?;
    let timestamp_dictionary = timestamp_dictionary.ok_or(MetadataError::MissingPacket {
        packet_type: TIMESTAMP_DICTIONARY_PACKET_TYPE,
    })?;

    Ok(ArchiveMetadata {
        directory: SectionDirectory::validate(&files, layout)?,
        timestamp_dictionary,
        range_index,
        unknown_packet_count,
    })
}

const fn reject_duplicate(already_seen: bool, packet_type: u8) -> Result<(), MetadataError> {
    if already_seen {
        Err(MetadataError::DuplicatePacket { packet_type })
    } else {
        Ok(())
    }
}

fn decode_msgpack<T>(packet_type: u8, payload: &[u8]) -> Result<T, MetadataError>
where
    T: for<'de> Deserialize<'de>, {
    let mut deserializer = rmp_serde::Deserializer::new(Cursor::new(payload));
    let value =
        T::deserialize(&mut deserializer).map_err(|source| MetadataError::InvalidMessagePack {
            packet_type,
            source,
        })?;
    let consumed =
        usize::try_from(deserializer.position()).map_err(|_| MetadataError::SizeOverflow)?;
    if consumed != payload.len() {
        return Err(MetadataError::TrailingPacketData {
            packet_type,
            remaining: payload.len() - consumed,
        });
    }
    Ok(value)
}

fn read_payload<R: Read>(reader: &mut R, size: u32) -> Result<Vec<u8>, MetadataError> {
    let size = usize::try_from(size).map_err(|_| MetadataError::SizeOverflow)?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(size)
        .map_err(|_| MetadataError::AllocationFailed { requested: size })?;
    payload.resize(size, 0);
    reader.read_exact(&mut payload).map_err(MetadataError::Io)?;
    Ok(payload)
}

fn skip_payload<R: Read>(reader: &mut R, size: u32) -> Result<(), MetadataError> {
    let mut bounded = reader.take(u64::from(size));
    let copied = io::copy(&mut bounded, &mut io::sink()).map_err(MetadataError::Io)?;
    if copied != u64::from(size) {
        return Err(MetadataError::TruncatedPacket {
            expected: size,
            actual: u32::try_from(copied).unwrap_or(u32::MAX),
        });
    }
    Ok(())
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8, MetadataError> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes).map_err(MetadataError::Io)?;
    Ok(bytes[0])
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, MetadataError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(MetadataError::Io)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Failure to decompress or validate the SFA metadata frame and section directory.
#[derive(Debug)]
#[non_exhaustive]
pub enum MetadataError {
    /// The compressed metadata section exceeds the configured limit.
    CompressedMetadataTooLarge {
        /// Compressed metadata bytes declared by the header.
        actual: u64,
        /// Configured maximum compressed metadata bytes.
        limit: u64,
    },
    /// The decompressed metadata framing exceeds the configured limit.
    DecompressedMetadataTooLarge {
        /// Decompressed bytes implied by packet framing.
        actual: u64,
        /// Configured maximum decompressed metadata bytes.
        limit: u64,
    },
    /// A packet payload exceeds the configured per-packet limit.
    PacketTooLarge {
        /// Zero-based packet index.
        packet_index: u8,
        /// Declared decompressed payload bytes.
        actual: u32,
        /// Configured maximum packet bytes.
        limit: u32,
    },
    /// Metadata input, decompression, or seeking failed.
    Io(io::Error),
    /// Checked size arithmetic or conversion overflowed.
    SizeOverflow,
    /// A bounded packet allocation could not be reserved.
    AllocationFailed {
        /// Packet bytes requested by the failed reservation.
        requested: usize,
    },
    /// A mandatory packet was absent.
    MissingPacket {
        /// Missing wire packet type.
        packet_type: u8,
    },
    /// A singleton known packet appeared more than once.
    DuplicatePacket {
        /// Duplicated wire packet type.
        packet_type: u8,
    },
    /// The archive uses an unsupported number of segments.
    UnsupportedSegmentCount {
        /// Segment count decoded from `ArchiveInfo`.
        actual: u64,
    },
    /// A known `MessagePack` payload was invalid.
    InvalidMessagePack {
        /// Wire packet type being decoded.
        packet_type: u8,
        /// `MessagePack` decoder failure.
        source: rmp_serde::decode::Error,
    },
    /// The raw timestamp-dictionary packet was structurally invalid.
    InvalidTimestampDictionary {
        /// Timestamp-dictionary decoding failure.
        source: TimestampDictionaryError,
    },
    /// The range-index packet was structurally invalid.
    InvalidRangeIndex {
        /// Range-index decoding failure.
        source: RangeIndexError,
    },
    /// A known `MessagePack` packet had bytes after its one object.
    TrailingPacketData {
        /// Wire packet type being decoded.
        packet_type: u8,
        /// Unconsumed payload bytes.
        remaining: usize,
    },
    /// A skipped packet ended before its declared payload size.
    TruncatedPacket {
        /// Declared payload bytes.
        expected: u32,
        /// Bytes that were available.
        actual: u32,
    },
    /// Decompressed bytes followed the declared packet sequence.
    TrailingDecompressedData,
    /// Compressed bytes followed the one metadata zstd frame.
    TrailingCompressedData {
        /// Bytes remaining inside the bounded metadata section.
        remaining: u64,
    },
    /// The file-info packet did not contain exactly the canonical section set.
    UnexpectedSectionCount {
        /// Number of section entries found.
        actual: usize,
        /// Number of section entries required.
        expected: usize,
    },
    /// A section name or its physical list position was not canonical.
    UnexpectedSectionName {
        /// Zero-based section index.
        index: usize,
        /// Canonical name at this index.
        expected: &'static str,
        /// Name decoded from the archive.
        actual: String,
    },
    /// The first section did not begin at offset zero.
    InvalidFirstSectionOffset {
        /// First relative section offset.
        actual: u64,
    },
    /// Section offsets did not increase in physical order.
    NonIncreasingSectionOffset {
        /// Zero-based section index.
        index: usize,
        /// Previous relative section offset.
        previous: u64,
        /// Current relative section offset.
        actual: u64,
    },
    /// A section offset was outside the SFA files region.
    SectionOffsetOutOfBounds {
        /// Zero-based section index.
        index: usize,
        /// Relative section offset.
        offset: u64,
        /// Total files-region bytes.
        files_size: u64,
    },
}

impl Display for MetadataError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedMetadataTooLarge { actual, limit } => write!(
                formatter,
                "compressed archive metadata size {actual} exceeds limit {limit}"
            ),
            Self::DecompressedMetadataTooLarge { actual, limit } => write!(
                formatter,
                "decompressed archive metadata size {actual} exceeds limit {limit}"
            ),
            Self::PacketTooLarge {
                packet_index,
                actual,
                limit,
            } => write!(
                formatter,
                "archive metadata packet {packet_index} size {actual} exceeds limit {limit}"
            ),
            Self::Io(error) => write!(formatter, "archive metadata I/O failed: {error}"),
            Self::SizeOverflow => formatter.write_str("archive metadata size overflow"),
            Self::AllocationFailed { requested } => format_allocation_error(*requested, formatter),
            Self::MissingPacket { packet_type } => {
                write!(
                    formatter,
                    "required archive metadata packet {packet_type} is missing"
                )
            }
            Self::DuplicatePacket { packet_type } => {
                write!(
                    formatter,
                    "archive metadata packet {packet_type} is duplicated"
                )
            }
            Self::UnsupportedSegmentCount { actual } => {
                write!(formatter, "unsupported archive segment count {actual}")
            }
            Self::InvalidMessagePack {
                packet_type,
                source,
            } => write!(
                formatter,
                "invalid MessagePack in archive metadata packet {packet_type}: {source}"
            ),
            Self::InvalidTimestampDictionary { source } => {
                write!(formatter, "invalid archive timestamp dictionary: {source}")
            }
            Self::InvalidRangeIndex { source } => {
                write!(formatter, "invalid archive range index: {source}")
            }
            Self::TrailingPacketData {
                packet_type,
                remaining,
            } => write!(
                formatter,
                "archive metadata packet {packet_type} has {remaining} trailing bytes"
            ),
            Self::TruncatedPacket { expected, actual } => write!(
                formatter,
                "archive metadata packet declared {expected} bytes but only {actual} were \
                 available"
            ),
            Self::TrailingDecompressedData => {
                formatter.write_str("data follows the declared archive metadata packets")
            }
            Self::TrailingCompressedData { remaining } => write!(
                formatter,
                "{remaining} compressed bytes follow the archive metadata zstd frame"
            ),
            Self::UnexpectedSectionCount { actual, expected } => write!(
                formatter,
                "archive file-info contains {actual} sections; expected {expected}"
            ),
            Self::UnexpectedSectionName {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "archive section {index} is {actual:?}; expected {expected:?}"
            ),
            Self::InvalidFirstSectionOffset { actual } => write!(
                formatter,
                "first archive section begins at relative offset {actual}; expected zero"
            ),
            Self::NonIncreasingSectionOffset {
                index,
                previous,
                actual,
            } => write!(
                formatter,
                "archive section {index} offset {actual} does not follow {previous}"
            ),
            Self::SectionOffsetOutOfBounds {
                index,
                offset,
                files_size,
            } => write!(
                formatter,
                "archive section {index} offset {offset} is outside files size {files_size}"
            ),
        }
    }
}

fn format_allocation_error(requested: usize, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(
        formatter,
        "could not reserve bounded archive metadata packet of {requested} bytes"
    )
}

impl Error for MetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidMessagePack { source, .. } => Some(source),
            Self::InvalidTimestampDictionary { source } => Some(source),
            Self::InvalidRangeIndex { source } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::archive::ArchiveHeader;
    use crate::archive::SFA_HEADER_SIZE;
    use crate::archive::SingleFileArchiveReader;

    fn append_packet(packet_type: u8, payload: &[u8], output: &mut Vec<u8>) {
        output.push(packet_type);
        output.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("test packet size fits u32")
                .to_le_bytes(),
        );
        output.extend_from_slice(payload);
    }

    fn canonical_files() -> Vec<ArchiveFileInfo> {
        SFA_SECTION_NAMES
            .into_iter()
            .zip(0_u64..)
            .map(|(name, offset)| ArchiveFileInfo {
                name: name.to_owned(),
                offset,
            })
            .collect()
    }

    fn metadata_with_files(files: Vec<ArchiveFileInfo>) -> Vec<u8> {
        let info =
            rmp_serde::to_vec_named(&ArchiveInfo { num_segments: 1 }).expect("encode archive info");
        let files =
            rmp_serde::to_vec_named(&ArchiveFileInfoPacket { files }).expect("encode file info");

        let mut metadata = vec![3];
        append_packet(ARCHIVE_INFO_PACKET_TYPE, &info, &mut metadata);
        append_packet(ARCHIVE_FILE_INFO_PACKET_TYPE, &files, &mut metadata);
        append_packet(TIMESTAMP_DICTIONARY_PACKET_TYPE, &[0_u8; 16], &mut metadata);
        metadata
    }

    fn valid_metadata_packets() -> Vec<u8> {
        metadata_with_files(canonical_files())
    }

    fn archive_with_metadata(metadata: &[u8], files: &[u8]) -> Vec<u8> {
        let compressed = zstd::stream::encode_all(metadata, 3).expect("compress metadata");
        let archive_size = SFA_HEADER_SIZE + compressed.len() + files.len();
        let header = ArchiveHeader::new(
            0,
            u64::try_from(archive_size).expect("archive size fits u64"),
            u32::try_from(compressed.len()).expect("metadata size fits u32"),
        );
        let mut archive = Vec::with_capacity(archive_size);
        archive.extend_from_slice(&header.encode());
        archive.extend_from_slice(&compressed);
        archive.extend_from_slice(files);
        archive
    }

    #[test]
    fn decodes_and_validates_section_directory() {
        let bytes = archive_with_metadata(&valid_metadata_packets(), &[0_u8; 7]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("valid archive metadata");

        assert_eq!(
            SFA_SECTION_NAMES.len(),
            metadata.directory().sections().len()
        );
        assert_eq!(16, metadata.timestamp_dictionary_bytes().len());
        assert_eq!(0, metadata.timestamp_dictionary().ranges().len());
        assert_eq!(0, metadata.timestamp_dictionary().patterns().len());
        assert!(metadata.range_index_bytes().is_none());
        assert_eq!(0, metadata.unknown_packet_count());
        for (index, expected_name) in SFA_SECTION_NAMES.into_iter().enumerate() {
            let section = &metadata.directory().sections()[index];
            assert_eq!(expected_name, section.name());
            assert_eq!(1, section.compressed_size());
            assert_eq!(Some(section), metadata.directory().get(expected_name));
        }
    }

    #[test]
    fn accepts_an_empty_final_tables_section_at_end_of_archive() {
        let bytes = archive_with_metadata(&valid_metadata_packets(), &[0_u8; 6]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");
        let metadata = reader
            .read_metadata(MetadataLimits::default())
            .expect("empty final /0 section is valid");

        let tables = metadata
            .directory()
            .get("/0")
            .expect("canonical directory includes /0");
        assert_eq!(0, tables.compressed_size());
        assert_eq!(tables.range().start, tables.range().end);
        assert_eq!(reader.layout().files_range().end, tables.range().end);
    }

    #[test]
    fn rejects_duplicate_required_packets() {
        let mut metadata = valid_metadata_packets();
        metadata[0] += 1;
        append_packet(TIMESTAMP_DICTIONARY_PACKET_TYPE, &[0_u8; 16], &mut metadata);
        let bytes = archive_with_metadata(&metadata, &[0_u8; 7]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");

        assert!(matches!(
            reader.read_metadata(MetadataLimits::default()),
            Err(MetadataError::DuplicatePacket {
                packet_type: TIMESTAMP_DICTIONARY_PACKET_TYPE
            })
        ));
    }

    #[test]
    fn rejects_noncanonical_section_order() {
        let mut files = canonical_files();
        files.swap(0, 1);
        let bytes = archive_with_metadata(&metadata_with_files(files), &[0_u8; 7]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");

        assert!(matches!(
            reader.read_metadata(MetadataLimits::default()),
            Err(MetadataError::UnexpectedSectionName { index: 0, .. })
        ));
    }

    #[test]
    fn rejects_nonincreasing_section_offsets() {
        let mut files = canonical_files();
        files[2].offset = files[1].offset;
        let bytes = archive_with_metadata(&metadata_with_files(files), &[0_u8; 7]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");

        assert!(matches!(
            reader.read_metadata(MetadataLimits::default()),
            Err(MetadataError::NonIncreasingSectionOffset { index: 2, .. })
        ));
    }

    #[test]
    fn skips_unknown_packets_without_retaining_them() {
        let mut metadata = valid_metadata_packets();
        metadata[0] += 1;
        append_packet(99, &[1, 2, 3], &mut metadata);
        let bytes = archive_with_metadata(&metadata, &[0_u8; 7]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");

        assert_eq!(
            1,
            reader
                .read_metadata(MetadataLimits::default())
                .expect("metadata with unknown extension")
                .unknown_packet_count()
        );
    }

    #[test]
    fn applies_packet_resource_limit_before_allocation() {
        let bytes = archive_with_metadata(&valid_metadata_packets(), &[0_u8; 7]);
        let mut reader =
            SingleFileArchiveReader::open(Cursor::new(bytes)).expect("valid SFA envelope");
        let limits = MetadataLimits::new(u64::MAX, u64::MAX, 1);

        assert!(matches!(
            reader.read_metadata(limits),
            Err(MetadataError::PacketTooLarge {
                packet_index: 0,
                ..
            })
        ));
    }
}
