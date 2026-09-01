//! Pure source-filename transformations for archive range metadata.
//!
//! The C++ compression CLI performs filesystem canonicalization before these lexical operations
//! when `--normalize-paths` is selected. Canonicalization is intentionally not hidden here: callers
//! that need that behavior must canonicalize both the source path and any prefix first, then pass
//! those results to [`SourcePathTransform`]. Keeping the filesystem lookup outside this module
//! makes the transformation deterministic and reusable by bindings with their own path-resolution
//! policy.

use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::path::Path;
use std::path::PathBuf;

use crate::writer::ArchiveSourceContext;

/// Pure, C++-compatible transformations applied to one filesystem source filename.
///
/// Prefix removal compares whole lexical path components. A successful removal reconstructs the
/// remainder beneath `/`, matching the C++ compressor. Removing the leading slash happens last and
/// removes exactly one `/` byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePathTransform {
    prefix_to_remove: Option<PathBuf>,
    remove_leading_slash: bool,
}

impl SourcePathTransform {
    /// Creates a transform that preserves the supplied path exactly.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prefix_to_remove: None,
            remove_leading_slash: false,
        }
    }

    /// Selects a lexical path-component prefix to remove.
    ///
    /// A trailing separator on `prefix` does not affect matching. To reproduce the C++
    /// `--normalize-paths --remove-path-prefix` combination, canonicalize both `prefix` and each
    /// source path before constructing and applying this transform.
    #[must_use]
    pub fn with_prefix_to_remove(mut self, prefix: impl Into<PathBuf>) -> Self {
        self.prefix_to_remove = Some(prefix.into());
        self
    }

    /// Selects whether exactly one leading `/` is removed after prefix removal.
    #[must_use]
    pub const fn with_remove_leading_slash(mut self, remove: bool) -> Self {
        self.remove_leading_slash = remove;
        self
    }

    /// Returns the lexical prefix removed by this transform, if any.
    #[must_use]
    pub fn prefix_to_remove(&self) -> Option<&Path> {
        self.prefix_to_remove.as_deref()
    }

    /// Returns whether one leading slash is removed from the final filename.
    #[must_use]
    pub const fn removes_leading_slash(&self) -> bool {
        self.remove_leading_slash
    }

    /// Transforms `source_path` without accessing the filesystem.
    ///
    /// The returned [`PathBuf`] preserves non-UTF-8 names. UTF-8 validation belongs at the archive
    /// source-context boundary; [`Self::source_context`] provides that checked conversion.
    ///
    /// # Errors
    ///
    /// Returns [`SourcePathTransformError::PrefixMismatch`] when the configured prefix is not a
    /// complete lexical component prefix of `source_path`.
    pub fn transform(&self, source_path: &Path) -> Result<PathBuf, SourcePathTransformError> {
        let transformed = self.prefix_to_remove.as_deref().map_or_else(
            || Ok(source_path.to_path_buf()),
            |prefix| remove_prefix(source_path, prefix),
        )?;
        if self.remove_leading_slash {
            Ok(remove_one_leading_slash(transformed))
        } else {
            Ok(transformed)
        }
    }

    /// Transforms a path and converts it at the UTF-8 archive source-context boundary.
    ///
    /// # Errors
    ///
    /// Returns a transformation error or [`SourcePathContextError::NonUtf8`] when the transformed
    /// path cannot be represented by the archive's string-valued `_filename` field.
    pub fn source_context(
        &self,
        source_path: &Path,
        archive_creator_id: impl Into<String>,
    ) -> Result<ArchiveSourceContext, SourcePathContextError> {
        let transformed = self
            .transform(source_path)
            .map_err(SourcePathContextError::Transform)?;
        let canonical_filename = transformed.into_os_string().into_string().map_err(|path| {
            SourcePathContextError::NonUtf8 {
                path: PathBuf::from(path),
            }
        })?;
        Ok(ArchiveSourceContext::new(
            canonical_filename,
            archive_creator_id,
        ))
    }
}

impl Default for SourcePathTransform {
    fn default() -> Self {
        Self::new()
    }
}

/// Failure to apply a pure source-path transformation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SourcePathTransformError {
    /// The requested prefix did not match whole lexical path components.
    PrefixMismatch {
        /// Original source path.
        source_path: PathBuf,
        /// Prefix that failed to match.
        prefix: PathBuf,
    },
}

impl Display for SourcePathTransformError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixMismatch {
                source_path,
                prefix,
            } => write!(
                formatter,
                "source path '{}' does not begin with component prefix '{}'",
                source_path.display(),
                prefix.display()
            ),
        }
    }
}

impl Error for SourcePathTransformError {}

/// Failure at the string-valued archive source-context boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SourcePathContextError {
    /// The pure path transformation failed.
    Transform(SourcePathTransformError),
    /// The transformed native path is not valid UTF-8.
    NonUtf8 {
        /// Transformed path that could not become archive string metadata.
        path: PathBuf,
    },
}

impl Display for SourcePathContextError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transform(source) => Display::fmt(source, formatter),
            Self::NonUtf8 { path } => write!(
                formatter,
                "transformed source path '{}' is not valid UTF-8 archive metadata",
                path.display()
            ),
        }
    }
}

impl Error for SourcePathContextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transform(source) => Some(source),
            Self::NonUtf8 { .. } => None,
        }
    }
}

#[cfg(unix)]
fn remove_prefix(source_path: &Path, prefix: &Path) -> Result<PathBuf, SourcePathTransformError> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::ffi::OsStringExt;

    let source_bytes = source_path.as_os_str().as_bytes();
    let prefix_bytes = prefix.as_os_str().as_bytes();
    let source_components = lexical_components(source_bytes, true);
    let prefix_components = lexical_components(prefix_bytes, false);
    if !source_components.starts_with(&prefix_components) {
        return Err(prefix_mismatch(source_path, prefix));
    }

    let remainder = &source_components[prefix_components.len()..];
    let mut output = Vec::with_capacity(source_bytes.len().saturating_add(1));
    output.push(b'/');
    for component in remainder {
        match component {
            LexicalComponent::Root => output.truncate(1),
            LexicalComponent::Name(name) => {
                if output.last().copied() != Some(b'/') {
                    output.push(b'/');
                }
                output.extend_from_slice(name);
            }
            LexicalComponent::TrailingSeparator => {
                if output.last().copied() != Some(b'/') {
                    output.push(b'/');
                }
            }
        }
    }
    Ok(PathBuf::from(OsString::from_vec(output)))
}

#[cfg(not(unix))]
fn remove_prefix(source_path: &Path, prefix: &Path) -> Result<PathBuf, SourcePathTransformError> {
    let remainder = source_path
        .strip_prefix(prefix)
        .map_err(|_| prefix_mismatch(source_path, prefix))?;
    let mut output = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
    output.push(remainder);
    Ok(output)
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LexicalComponent<'path> {
    Root,
    Name(&'path [u8]),
    TrailingSeparator,
}

#[cfg(unix)]
fn lexical_components(path: &[u8], retain_trailing_separator: bool) -> Vec<LexicalComponent<'_>> {
    let mut components = Vec::new();
    let mut position = 0;
    if path.first().copied() == Some(b'/') {
        components.push(LexicalComponent::Root);
        position = skip_separators(path, position);
    }

    while position < path.len() {
        let start = position;
        while position < path.len() && path[position] != b'/' {
            position += 1;
        }
        components.push(LexicalComponent::Name(&path[start..position]));
        position = skip_separators(path, position);
    }
    if retain_trailing_separator && !path.is_empty() && path.last().copied() == Some(b'/') {
        components.push(LexicalComponent::TrailingSeparator);
    }
    components
}

#[cfg(unix)]
const fn skip_separators(path: &[u8], mut position: usize) -> usize {
    while position < path.len() && path[position] == b'/' {
        position += 1;
    }
    position
}

fn prefix_mismatch(source_path: &Path, prefix: &Path) -> SourcePathTransformError {
    SourcePathTransformError::PrefixMismatch {
        source_path: source_path.to_path_buf(),
        prefix: prefix.to_path_buf(),
    }
}

#[cfg(unix)]
fn remove_one_leading_slash(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut bytes = path.into_os_string().into_vec();
    if bytes.first().copied() == Some(b'/') {
        bytes.remove(0);
    }
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn remove_one_leading_slash(path: PathBuf) -> PathBuf {
    path.strip_prefix(Path::new("/"))
        .map_or(path.clone(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::SourcePathContextError;
    use super::SourcePathTransform;
    use super::SourcePathTransformError;

    #[test]
    fn no_options_preserve_lexical_path_spelling() {
        let transform = SourcePathTransform::new();

        assert_eq!(
            transform
                .transform(Path::new("./inputs//nested/../event.json/"))
                .expect("preserve path")
                .as_os_str(),
            "./inputs//nested/../event.json/"
        );
        assert_eq!(
            transform
                .transform(Path::new("//srv/input.json"))
                .expect("preserve double slash")
                .as_os_str(),
            "//srv/input.json"
        );
    }

    #[test]
    fn prefix_removal_is_component_wise_and_rooted() {
        let transform = SourcePathTransform::new().with_prefix_to_remove("inputs");

        assert_eq!(
            transform
                .transform(Path::new("inputs/event.json"))
                .expect("remove prefix")
                .as_os_str(),
            "/event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("inputs/nested/event.json"))
                .expect("remove nested prefix")
                .as_os_str(),
            "/nested/event.json"
        );
    }

    #[test]
    fn trailing_prefix_separator_does_not_change_matching() {
        let transform = SourcePathTransform::new().with_prefix_to_remove("inputs///");

        assert_eq!(
            transform
                .transform(Path::new("inputs/event.json"))
                .expect("remove trailing-separator prefix")
                .as_os_str(),
            "/event.json"
        );
    }

    #[test]
    fn prefix_removal_preserves_parent_dot_and_trailing_components() {
        let transform = SourcePathTransform::new().with_prefix_to_remove("inputs/");

        assert_eq!(
            transform
                .transform(Path::new("inputs/../event.json"))
                .expect("preserve parent component")
                .as_os_str(),
            "/../event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("inputs/./event.json"))
                .expect("preserve dot component")
                .as_os_str(),
            "/./event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("inputs/nested/"))
                .expect("preserve trailing separator")
                .as_os_str(),
            "/nested/"
        );
    }

    #[test]
    fn root_prefix_matches_absolute_paths() {
        let transform = SourcePathTransform::new().with_prefix_to_remove("/");

        assert_eq!(
            transform
                .transform(Path::new("/inputs/event.json"))
                .expect("remove root prefix")
                .as_os_str(),
            "/inputs/event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("//inputs/event.json"))
                .expect("remove root prefix from double slash")
                .as_os_str(),
            "/inputs/event.json"
        );
        assert!(matches!(
            transform.transform(Path::new("inputs/event.json")),
            Err(SourcePathTransformError::PrefixMismatch { .. })
        ));
    }

    #[test]
    fn explicit_empty_prefix_roots_relative_paths() {
        let transform = SourcePathTransform::new().with_prefix_to_remove("");

        assert_eq!(
            transform
                .transform(Path::new("inputs/event.json"))
                .expect("remove empty prefix from relative path")
                .as_os_str(),
            "/inputs/event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("/inputs/event.json"))
                .expect("remove empty prefix from absolute path")
                .as_os_str(),
            "/inputs/event.json"
        );
    }

    #[test]
    fn prefix_must_match_complete_components_and_path_kind() {
        for (path, prefix) in [
            ("inputs/event.json", "input"),
            ("inputs/event.json", "other"),
            ("inputs", "inputs/nested"),
            ("inputs/event.json", "/inputs"),
            ("/inputs/event.json", "inputs"),
        ] {
            let error = SourcePathTransform::new()
                .with_prefix_to_remove(prefix)
                .transform(Path::new(path))
                .expect_err("reject mismatched prefix");
            assert_eq!(
                error,
                SourcePathTransformError::PrefixMismatch {
                    source_path: path.into(),
                    prefix: prefix.into(),
                }
            );
        }
    }

    #[test]
    fn leading_slash_removal_removes_exactly_one_byte() {
        let transform = SourcePathTransform::new().with_remove_leading_slash(true);

        assert_eq!(
            transform
                .transform(Path::new("inputs/event.json"))
                .expect("relative path")
                .as_os_str(),
            "inputs/event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("/inputs/event.json"))
                .expect("absolute path")
                .as_os_str(),
            "inputs/event.json"
        );
        assert_eq!(
            transform
                .transform(Path::new("//inputs/event.json"))
                .expect("double-leading slash path")
                .as_os_str(),
            "/inputs/event.json"
        );
    }

    #[test]
    fn leading_slash_removal_runs_after_prefix_removal() {
        let transform = SourcePathTransform::new()
            .with_prefix_to_remove("inputs")
            .with_remove_leading_slash(true);

        assert_eq!(
            transform
                .transform(Path::new("inputs/nested/event.json"))
                .expect("apply ordered transforms")
                .as_os_str(),
            "nested/event.json"
        );
    }

    #[test]
    fn caller_supplied_canonical_paths_reproduce_normalize_then_transform_order() {
        let transform = SourcePathTransform::new()
            .with_prefix_to_remove("/work")
            .with_remove_leading_slash(true);

        assert_eq!(
            transform
                .transform(Path::new("/work/inputs/event.json"))
                .expect("transform canonical path")
                .as_os_str(),
            "inputs/event.json"
        );
    }

    #[test]
    fn source_context_is_the_explicit_utf8_boundary() {
        let context = SourcePathTransform::new()
            .with_prefix_to_remove("inputs")
            .source_context(Path::new("inputs/event.json"), "creator")
            .expect("create source context");

        assert_eq!(context.canonical_filename(), "/event.json");
        assert_eq!(context.archive_creator_id(), "creator");
    }

    #[cfg(unix)]
    #[test]
    fn native_non_utf8_path_survives_transform_but_not_source_context() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"inputs/event-\xff.json"));
        let transform = SourcePathTransform::new().with_prefix_to_remove("inputs");
        let transformed = transform.transform(path).expect("preserve native bytes");
        assert_eq!(transformed.as_os_str().as_bytes(), b"/event-\xff.json");

        assert!(matches!(
            transform.source_context(path, "creator"),
            Err(SourcePathContextError::NonUtf8 { .. })
        ));
    }
}
