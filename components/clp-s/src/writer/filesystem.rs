//! Thin transactional `std::fs` adapter for canonical directory archives.

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use super::directory::DirectoryArchiveSink;
use crate::archive::DirectoryArchiveMember;

const MEMBER_COUNT: usize = DirectoryArchiveMember::ALL.len();

/// Filesystem sink that stages all members in one caller-selected directory before publication.
///
/// Both paths are explicit so temporary naming, placement, permissions, and cleanup policy remain
/// outside the archive core. `staging_root` and `target_root` should have the same
/// parent/filesystem when atomic rename publication is required. Neither may exist when the first
/// member is written. A failed write leaves the staging directory available for diagnosis or
/// caller-controlled cleanup; the target path remains untouched.
#[derive(Debug)]
pub struct FsDirectoryArchiveSink {
    target_root: PathBuf,
    staging_root: PathBuf,
    started: bool,
    written: [bool; MEMBER_COUNT],
}

impl FsDirectoryArchiveSink {
    /// Creates a lazy filesystem sink without touching either path.
    #[must_use]
    pub fn new(target_root: impl Into<PathBuf>, staging_root: impl Into<PathBuf>) -> Self {
        Self {
            target_root: target_root.into(),
            staging_root: staging_root.into(),
            started: false,
            written: [false; MEMBER_COUNT],
        }
    }

    /// Returns the final directory path.
    #[must_use]
    pub fn target_root(&self) -> &Path {
        &self.target_root
    }

    /// Returns the caller-selected staging directory path.
    #[must_use]
    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    fn start(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        require_absent(&self.target_root, "target directory")?;
        require_absent(&self.staging_root, "staging directory")?;
        fs::create_dir(&self.staging_root)?;
        self.started = true;
        Ok(())
    }
}

impl DirectoryArchiveSink for FsDirectoryArchiveSink {
    type Error = io::Error;
    type Output = PathBuf;

    fn write_member(&mut self, member: DirectoryArchiveMember, contents: &[u8]) -> io::Result<()> {
        self.start()?;
        let index = member_index(member);
        if self.written[index] {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("directory archive member {member} was already written"),
            ));
        }
        let path = self.staging_root.join(member.file_name());
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(contents)?;
        file.flush()?;
        self.written[index] = true;
        Ok(())
    }

    fn commit(self) -> io::Result<Self::Output> {
        if !self.started || self.written.iter().any(|written| !written) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot commit an incomplete directory archive",
            ));
        }
        require_absent(&self.target_root, "target directory")?;
        fs::rename(&self.staging_root, &self.target_root)?;
        Ok(self.target_root)
    }
}

fn require_absent(path: &Path, label: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "directory archive {label} already exists: {}",
                path.display()
            ),
        )),
        Err(error) if io::ErrorKind::NotFound == error.kind() => Ok(()),
        Err(error) => Err(error),
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
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::archive::DirectoryArchiveReader;
    use crate::archive::FsDirectoryArchiveSource;
    use crate::archive::MetadataLimits;
    use crate::writer::OpenDirectoryArchive;
    use crate::writer::WriterOptions;

    static NEXT_PATH_ID: AtomicU64 = AtomicU64::new(0);

    struct TestPaths {
        target: PathBuf,
        staging: PathBuf,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            let id = NEXT_PATH_ID.fetch_add(1, Ordering::Relaxed);
            let stem = format!("clp-s-{label}-{}-{id}", std::process::id());
            let temporary = std::env::temp_dir();
            Self {
                target: temporary.join(format!("{stem}-target")),
                staging: temporary.join(format!("{stem}-staging")),
            }
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            if self.target.exists() {
                fs::remove_dir_all(&self.target).expect("remove test target directory");
            }
            if self.staging.exists() {
                fs::remove_dir_all(&self.staging).expect("remove test staging directory");
            }
        }
    }

    #[test]
    fn stages_then_atomically_publishes_all_members() {
        let paths = TestPaths::new("directory-writer-publish");
        let archive = OpenDirectoryArchive::new(WriterOptions::default().with_log_order(false));
        let output = archive
            .finish_to(FsDirectoryArchiveSink::new(&paths.target, &paths.staging))
            .expect("publish filesystem directory archive")
            .into_inner();
        assert_eq!(paths.target, output);
        assert!(!paths.staging.exists());
        for member in DirectoryArchiveMember::ALL {
            assert!(paths.target.join(member.file_name()).is_file());
        }
        let reader = DirectoryArchiveReader::open(
            FsDirectoryArchiveSource::new(&paths.target),
            MetadataLimits::default(),
        )
        .expect("open published directory archive");
        assert_eq!(280, reader.header().compressed_size());
    }

    #[test]
    fn existing_target_is_never_modified_and_staging_is_not_created() {
        let paths = TestPaths::new("directory-writer-existing");
        fs::create_dir(&paths.target).expect("create existing target");
        let marker = paths.target.join("marker");
        fs::write(&marker, b"keep").expect("write target marker");
        let archive = OpenDirectoryArchive::new(WriterOptions::default().with_log_order(false));
        let error = archive
            .finish_to(FsDirectoryArchiveSink::new(&paths.target, &paths.staging))
            .expect_err("existing target must reject publication");
        assert!(matches!(
            error,
            crate::writer::DirectoryWriterError::Member {
                member: DirectoryArchiveMember::Header,
                source,
            } if io::ErrorKind::AlreadyExists == source.kind()
        ));
        assert_eq!(
            b"keep".as_slice(),
            fs::read(marker).expect("read retained marker")
        );
        assert!(!paths.staging.exists());
    }
}
