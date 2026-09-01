use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::mem::size_of;

/// Size, in bytes, of the header at the beginning of a single-file archive.
pub const SFA_HEADER_SIZE: usize = 64;

/// Magic bytes identifying a CLP structured single-file archive.
pub const SFA_MAGIC: [u8; 4] = [0xfd, 0x2f, 0xc5, 0x30];

const VERSION_OFFSET: usize = 4;
const UNCOMPRESSED_SIZE_OFFSET: usize = 8;
const COMPRESSED_SIZE_OFFSET: usize = 16;
const RESERVED_PADDING_OFFSET: usize = 24;
const METADATA_SIZE_OFFSET: usize = 56;
const COMPRESSION_OFFSET: usize = 60;
const FINAL_PADDING_OFFSET: usize = 62;
const DEPRECATED_DATE_STRING_FORMAT_VERSION_MARKER: ArchiveVersion = ArchiveVersion::new(0, 5, 0);

/// The version encoded in a structured archive header.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArchiveVersion {
    major: u8,
    minor: u8,
    patch: u16,
}

impl ArchiveVersion {
    /// Archive version emitted by the reference implementation when this crate was introduced.
    pub const CURRENT: Self = Self::new(0, 5, 0);

    /// Creates a version from its semantic components.
    #[must_use]
    pub const fn new(major: u8, minor: u8, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Decodes the packed integer used by the archive wire format.
    #[must_use]
    pub const fn from_wire(raw: u32) -> Self {
        let bytes = raw.to_be_bytes();
        Self::new(bytes[0], bytes[1], u16::from_be_bytes([bytes[2], bytes[3]]))
    }

    /// Encodes this version using the archive wire format.
    #[must_use]
    pub const fn to_wire(self) -> u32 {
        u32::from_be_bytes([
            self.major,
            self.minor,
            self.patch.to_be_bytes()[0],
            self.patch.to_be_bytes()[1],
        ])
    }

    /// Returns the major version component.
    #[must_use]
    pub const fn major(self) -> u8 {
        self.major
    }

    /// Returns the minor version component.
    #[must_use]
    pub const fn minor(self) -> u8 {
        self.minor
    }

    /// Returns the patch version component.
    #[must_use]
    pub const fn patch(self) -> u16 {
        self.patch
    }

    /// Whether archives at this version can contain the deprecated `DateString` column type.
    #[must_use]
    pub const fn can_contain_deprecated_date_string(self) -> bool {
        self.to_wire() < DEPRECATED_DATE_STRING_FORMAT_VERSION_MARKER.to_wire()
    }
}

impl Display for ArchiveVersion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// General-purpose compression used for archive sections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
#[non_exhaustive]
pub enum ArchiveCompression {
    /// Zstandard compression.
    Zstd = 0,
}

impl ArchiveCompression {
    const fn from_wire(raw: u16) -> Option<Self> {
        match raw {
            0 => Some(Self::Zstd),
            _ => None,
        }
    }
}

impl From<ArchiveCompression> for u16 {
    fn from(compression: ArchiveCompression) -> Self {
        match compression {
            ArchiveCompression::Zstd => 0,
        }
    }
}

/// Decoded form of the fixed 64-byte structured single-file archive header.
///
/// Reserved and padding fields are retained so a decode/encode cycle does not silently discard
/// future-compatible header data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveHeader {
    version: ArchiveVersion,
    uncompressed_size: u64,
    compressed_size: u64,
    reserved_padding: [u64; 4],
    metadata_section_size: u32,
    compression: ArchiveCompression,
    final_padding: u16,
}

impl ArchiveHeader {
    /// Decodes a header from the first 64 bytes of `input` using explicit little-endian reads.
    ///
    /// Trailing bytes are intentionally ignored, allowing callers to pass a complete SFA buffer.
    /// Version acceptance is a reader-policy decision and is therefore not enforced here.
    ///
    /// # Errors
    ///
    /// Returns an error if `input` is shorter than a header, the magic bytes do not match, or the
    /// compression value is unsupported.
    pub fn decode(input: &[u8]) -> Result<Self, HeaderDecodeError> {
        let Some(header_bytes) = input.get(..SFA_HEADER_SIZE) else {
            return Err(HeaderDecodeError::Truncated {
                actual_size: input.len(),
            });
        };

        let mut magic = [0_u8; SFA_MAGIC.len()];
        magic.copy_from_slice(&header_bytes[..SFA_MAGIC.len()]);
        if magic != SFA_MAGIC {
            return Err(HeaderDecodeError::InvalidMagic { actual: magic });
        }

        let raw_compression = read_u16(header_bytes, COMPRESSION_OFFSET);
        let Some(compression) = ArchiveCompression::from_wire(raw_compression) else {
            return Err(HeaderDecodeError::UnsupportedCompression {
                value: raw_compression,
            });
        };

        Ok(Self {
            version: ArchiveVersion::from_wire(read_u32(header_bytes, VERSION_OFFSET)),
            uncompressed_size: read_u64(header_bytes, UNCOMPRESSED_SIZE_OFFSET),
            compressed_size: read_u64(header_bytes, COMPRESSED_SIZE_OFFSET),
            reserved_padding: std::array::from_fn(|index| {
                read_u64(
                    header_bytes,
                    RESERVED_PADDING_OFFSET + index * size_of::<u64>(),
                )
            }),
            metadata_section_size: read_u32(header_bytes, METADATA_SIZE_OFFSET),
            compression,
            final_padding: read_u16(header_bytes, FINAL_PADDING_OFFSET),
        })
    }

    /// Encodes the header exactly as the little-endian C++ reference layout.
    #[must_use]
    pub fn encode(&self) -> [u8; SFA_HEADER_SIZE] {
        let mut output = [0_u8; SFA_HEADER_SIZE];
        output[..SFA_MAGIC.len()].copy_from_slice(&SFA_MAGIC);
        write_u32(&mut output, VERSION_OFFSET, self.version.to_wire());
        write_u64(
            &mut output,
            UNCOMPRESSED_SIZE_OFFSET,
            self.uncompressed_size,
        );
        write_u64(&mut output, COMPRESSED_SIZE_OFFSET, self.compressed_size);
        for (index, value) in self.reserved_padding.iter().copied().enumerate() {
            write_u64(
                &mut output,
                RESERVED_PADDING_OFFSET + index * size_of::<u64>(),
                value,
            );
        }
        write_u32(
            &mut output,
            METADATA_SIZE_OFFSET,
            self.metadata_section_size,
        );
        write_u16(&mut output, COMPRESSION_OFFSET, u16::from(self.compression));
        write_u16(&mut output, FINAL_PADDING_OFFSET, self.final_padding);
        output
    }

    /// Creates a header for the current archive version using zeroed reserved fields.
    #[must_use]
    pub const fn new(
        uncompressed_size: u64,
        compressed_size: u64,
        metadata_section_size: u32,
    ) -> Self {
        Self {
            version: ArchiveVersion::CURRENT,
            uncompressed_size,
            compressed_size,
            reserved_padding: [0; 4],
            metadata_section_size,
            compression: ArchiveCompression::Zstd,
            final_padding: 0,
        }
    }

    /// Returns the archive format version.
    #[must_use]
    pub const fn version(&self) -> ArchiveVersion {
        self.version
    }

    /// Returns the uncompressed input size recorded by the writer.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Returns the total compressed archive size recorded by the writer.
    #[must_use]
    pub const fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Returns the compressed metadata-section size following the header.
    #[must_use]
    pub const fn metadata_section_size(&self) -> u32 {
        self.metadata_section_size
    }

    /// Returns the byte offset at which archive sections begin in an SFA.
    #[must_use]
    pub fn files_section_offset(&self) -> u64 {
        64_u64 + u64::from(self.metadata_section_size)
    }

    /// Returns the archive's general-purpose compression type.
    #[must_use]
    pub const fn compression(&self) -> ArchiveCompression {
        self.compression
    }
}

/// Failure to decode the fixed structured archive header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HeaderDecodeError {
    /// Fewer than 64 bytes were available.
    Truncated {
        /// Number of bytes that were available.
        actual_size: usize,
    },
    /// The four-byte SFA identifier was incorrect.
    InvalidMagic {
        /// Bytes found at the beginning of the input.
        actual: [u8; 4],
    },
    /// The compression discriminant is not implemented.
    UnsupportedCompression {
        /// Raw discriminant found in the header.
        value: u16,
    },
}

impl Display for HeaderDecodeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { actual_size } => write!(
                formatter,
                "structured archive header requires {SFA_HEADER_SIZE} bytes, found {actual_size}"
            ),
            Self::InvalidMagic { actual } => {
                write!(formatter, "invalid structured archive magic: {actual:02X?}")
            }
            Self::UnsupportedCompression { value } => {
                write!(
                    formatter,
                    "unsupported structured archive compression type {value}"
                )
            }
        }
    }
}

impl Error for HeaderDecodeError {}

const fn read_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([input[offset], input[offset + 1]])
}

const fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ])
}

const fn read_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
        input[offset + 4],
        input[offset + 5],
        input[offset + 6],
        input[offset + 7],
    ])
}

fn write_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + size_of::<u16>()].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + size_of::<u32>()].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + size_of::<u64>()].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPP_REFERENCE_HEADER: &[u8; SFA_HEADER_SIZE] =
        include_bytes!("../../tests/fixtures/sfa-header-v0.5.0-x86_64.bin");

    #[test]
    fn version_wire_encoding_matches_reference() {
        let version = ArchiveVersion::new(0x12, 0x34, 0x5678);
        assert_eq!(0x1234_5678, version.to_wire());
        assert_eq!(version, ArchiveVersion::from_wire(0x1234_5678));
        assert_eq!("18.52.22136", version.to_string());
    }

    #[test]
    fn current_version_matches_reference() {
        assert_eq!(0x0005_0000, ArchiveVersion::CURRENT.to_wire());
        assert_eq!("0.5.0", ArchiveVersion::CURRENT.to_string());
        assert!(ArchiveVersion::new(0, 4, u16::MAX).can_contain_deprecated_date_string());
        assert!(!ArchiveVersion::CURRENT.can_contain_deprecated_date_string());
    }

    #[test]
    fn decodes_cpp_emitted_header_fixture() {
        let header = ArchiveHeader::decode(CPP_REFERENCE_HEADER).expect("valid C++ header");
        assert_eq!(ArchiveVersion::CURRENT, header.version());
        assert_eq!(0x0102_0304_0506_0708, header.uncompressed_size());
        assert_eq!(0x1112_1314_1516_1718, header.compressed_size());
        assert_eq!(0x2122_2324, header.metadata_section_size());
        assert_eq!(0x2122_2364, header.files_section_offset());
        assert_eq!(ArchiveCompression::Zstd, header.compression());
        assert_eq!([0; 4], header.reserved_padding);
        assert_eq!(0, header.final_padding);
        assert_eq!(*CPP_REFERENCE_HEADER, header.encode());
    }

    #[test]
    fn decodes_synthetic_reserved_fields() {
        let mut bytes = [0_u8; SFA_HEADER_SIZE];
        bytes[..4].copy_from_slice(&SFA_MAGIC);
        bytes[4..8].copy_from_slice(&0x0005_0000_u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&1234_u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&987_u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&1_u64.to_le_bytes());
        bytes[48..56].copy_from_slice(&4_u64.to_le_bytes());
        bytes[56..60].copy_from_slice(&91_u32.to_le_bytes());
        bytes[60..62].copy_from_slice(&0_u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&7_u16.to_le_bytes());

        let header = ArchiveHeader::decode(&bytes).expect("valid reference header");
        assert_eq!(ArchiveVersion::CURRENT, header.version());
        assert_eq!(1234, header.uncompressed_size());
        assert_eq!(987, header.compressed_size());
        assert_eq!(91, header.metadata_section_size());
        assert_eq!(155, header.files_section_offset());
        assert_eq!(ArchiveCompression::Zstd, header.compression());
        assert_eq!(bytes, header.encode());
    }

    #[test]
    fn new_header_round_trips() {
        let header = ArchiveHeader::new(1_000_000, 400_000, 1_024);
        assert_eq!(
            header,
            ArchiveHeader::decode(&header.encode()).expect("encoded header")
        );
    }

    #[test]
    fn reports_truncated_header() {
        assert_eq!(
            Err(HeaderDecodeError::Truncated { actual_size: 63 }),
            ArchiveHeader::decode(&[0; SFA_HEADER_SIZE - 1])
        );
    }

    #[test]
    fn reports_invalid_magic() {
        let mut bytes = ArchiveHeader::new(0, 64, 0).encode();
        bytes[..4].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            Err(HeaderDecodeError::InvalidMagic {
                actual: [1, 2, 3, 4]
            }),
            ArchiveHeader::decode(&bytes)
        );
    }

    #[test]
    fn reports_unknown_compression() {
        let mut bytes = ArchiveHeader::new(0, 64, 0).encode();
        bytes[COMPRESSION_OFFSET..FINAL_PADDING_OFFSET].copy_from_slice(&123_u16.to_le_bytes());
        assert_eq!(
            Err(HeaderDecodeError::UnsupportedCompression { value: 123 }),
            ArchiveHeader::decode(&bytes)
        );
    }
}
