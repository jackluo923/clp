use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::{self};
use std::ops::Range;

use super::format::ArchiveHeader;

/// Validated byte ranges for a CLP structured single-file archive.
///
/// This type validates only the outer SFA envelope. Metadata packets and the section offsets they
/// contain require their own validation after the metadata section has been decompressed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SingleFileArchiveLayout {
    header: ArchiveHeader,
    archive_size: u64,
}

impl SingleFileArchiveLayout {
    /// Validates the outer layout described by `header` against the actual source size.
    ///
    /// # Errors
    ///
    /// Returns an error when the declared archive size cannot contain the header and metadata, or
    /// when it differs from the actual source size.
    pub fn new(header: ArchiveHeader, archive_size: u64) -> Result<Self, LayoutError> {
        let files_section_offset = header.files_section_offset();
        let declared_size = header.compressed_size();
        if declared_size < files_section_offset {
            return Err(LayoutError::DeclaredSizeBeforeFilesSection {
                declared_size,
                files_section_offset,
            });
        }
        if declared_size != archive_size {
            return Err(LayoutError::ArchiveSizeMismatch {
                declared_size,
                actual_size: archive_size,
            });
        }

        Ok(Self {
            header,
            archive_size,
        })
    }

    /// Returns the decoded archive header.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        &self.header
    }

    /// Returns the actual, validated size of the archive source.
    #[must_use]
    pub const fn archive_size(&self) -> u64 {
        self.archive_size
    }

    /// Returns the compressed metadata section's byte range.
    #[must_use]
    pub fn metadata_range(&self) -> Range<u64> {
        64..self.header.files_section_offset()
    }

    /// Returns the concatenated archive-files section's byte range.
    #[must_use]
    pub fn files_range(&self) -> Range<u64> {
        self.header.files_section_offset()..self.archive_size
    }
}

/// Failure to validate the outer byte layout of a structured single-file archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LayoutError {
    /// The declared archive ends before its metadata section does.
    DeclaredSizeBeforeFilesSection {
        /// Total compressed archive size recorded in the header.
        declared_size: u64,
        /// First byte after the metadata section.
        files_section_offset: u64,
    },
    /// The total size recorded in the header differs from the source size.
    ArchiveSizeMismatch {
        /// Total compressed archive size recorded in the header.
        declared_size: u64,
        /// Actual size of the archive source.
        actual_size: u64,
    },
}

impl Display for LayoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeclaredSizeBeforeFilesSection {
                declared_size,
                files_section_offset,
            } => write!(
                formatter,
                "declared archive size {declared_size} ends before files-section offset \
                 {files_section_offset}"
            ),
            Self::ArchiveSizeMismatch {
                declared_size,
                actual_size,
            } => write!(
                formatter,
                "declared archive size {declared_size} differs from actual size {actual_size}"
            ),
        }
    }
}

impl Error for LayoutError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_validated_ranges() {
        let header = ArchiveHeader::new(1_000, 200, 36);
        let layout = SingleFileArchiveLayout::new(header, 200).expect("valid layout");

        assert_eq!(&header, layout.header());
        assert_eq!(200, layout.archive_size());
        assert_eq!(64..100, layout.metadata_range());
        assert_eq!(100..200, layout.files_range());
    }

    #[test]
    fn accepts_empty_metadata_and_files_sections() {
        let header = ArchiveHeader::new(0, 64, 0);
        let layout = SingleFileArchiveLayout::new(header, 64).expect("empty archive envelope");

        assert_eq!(64..64, layout.metadata_range());
        assert_eq!(64..64, layout.files_range());
    }

    #[test]
    fn rejects_declared_size_before_metadata_end() {
        let header = ArchiveHeader::new(0, 99, 36);

        assert_eq!(
            Err(LayoutError::DeclaredSizeBeforeFilesSection {
                declared_size: 99,
                files_section_offset: 100,
            }),
            SingleFileArchiveLayout::new(header, 99)
        );
    }

    #[test]
    fn rejects_truncated_or_trailing_sources() {
        let header = ArchiveHeader::new(0, 100, 0);

        for actual_size in [99, 101] {
            assert_eq!(
                Err(LayoutError::ArchiveSizeMismatch {
                    declared_size: 100,
                    actual_size,
                }),
                SingleFileArchiveLayout::new(header, actual_size)
            );
        }
    }
}
