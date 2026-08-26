//! Every path that reaches the filesystem goes through [`Root`].
//!
//! Containment is enforced in two stages because either one alone is bypassable:
//! lexical normalisation kills `..` before it ever touches the disk, and
//! canonicalising the deepest existing ancestor kills symlinks that point out of
//! the sandbox. A path that is rejected here never becomes a syscall.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RootError {
    #[error("path escapes the sandbox root: {0}")]
    Escape(String),
    #[error("cannot resolve {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Debug)]
pub struct Root {
    root: PathBuf,
}

impl Root {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, RootError> {
        let root = root.as_ref();
        let canonical = root.canonicalize().map_err(|source| RootError::Io {
            path: root.display().to_string(),
            source,
        })?;
        Ok(Self { root: canonical })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Strip `.` and resolve `..` textually, then require the result to sit
    /// under the root. Absolute inputs are kept as-is so they fail this check
    /// rather than being silently reinterpreted as relative.
    fn lexical(&self, input: &str) -> Result<PathBuf, RootError> {
        let raw = Path::new(input);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };

        let mut out = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::Prefix(prefix) => out.push(prefix.as_os_str()),
                Component::RootDir => out.push(Component::RootDir.as_os_str()),
                Component::CurDir => {},
                Component::ParentDir => {
                    if !out.pop() {
                        return Err(RootError::Escape(input.to_string()));
                    }
                },
                Component::Normal(part) => out.push(part),
            }
        }

        if out.starts_with(&self.root) {
            Ok(out)
        } else {
            Err(RootError::Escape(input.to_string()))
        }
    }

    /// Resolve a caller-supplied path to a real path inside the root.
    ///
    /// Works for paths that do not exist yet, so it also covers file creation.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, RootError> {
        let lexical = self.lexical(input)?;

        // Walk up to the deepest component that exists on disk. `symlink_metadata`
        // rather than `exists` so a dangling symlink counts as present and gets
        // canonicalised (and rejected) instead of being treated as a new file we
        // may create — writing through it would land outside the root.
        let mut ancestor = lexical.as_path();
        let mut trailing: Vec<OsString> = Vec::new();
        while ancestor.symlink_metadata().is_err() {
            let (Some(name), Some(parent)) = (ancestor.file_name(), ancestor.parent()) else {
                return Err(RootError::Escape(input.to_string()));
            };
            trailing.push(name.to_os_string());
            ancestor = parent;
        }

        let mut resolved = ancestor.canonicalize().map_err(|source| RootError::Io {
            path: ancestor.display().to_string(),
            source,
        })?;
        if !resolved.starts_with(&self.root) {
            return Err(RootError::Escape(input.to_string()));
        }

        for name in trailing.iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }

    /// Render a resolved path for display, relative to the root.
    pub fn relativize<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), b"hi").unwrap();
        let root = Root::new(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn resolves_paths_inside_the_root() {
        let (_dir, root) = sandbox();
        let resolved = root.resolve("sub/file.txt").unwrap();
        assert!(resolved.starts_with(root.path()));
        assert!(resolved.ends_with("sub/file.txt"));
    }

    #[test]
    fn accepts_the_root_itself() {
        let (_dir, root) = sandbox();
        assert_eq!(root.resolve(".").unwrap(), root.path());
    }

    #[test]
    fn allows_paths_that_do_not_exist_yet() {
        let (_dir, root) = sandbox();
        let resolved = root.resolve("sub/new/deep/file.txt").unwrap();
        assert!(resolved.starts_with(root.path()));
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_dir, root) = sandbox();
        for input in ["..", "../..", "sub/../../etc/passwd", "sub/../.."] {
            assert!(
                matches!(root.resolve(input), Err(RootError::Escape(_))),
                "should have rejected {input}"
            );
        }
    }

    #[test]
    fn rejects_absolute_paths_outside_the_root() {
        let (_dir, root) = sandbox();
        assert!(matches!(
            root.resolve("/etc/passwd"),
            Err(RootError::Escape(_))
        ));
    }

    #[test]
    fn accepts_absolute_paths_inside_the_root() {
        let (_dir, root) = sandbox();
        let inside = root.path().join("sub/file.txt");
        let resolved = root.resolve(inside.to_str().unwrap()).unwrap();
        assert_eq!(resolved, inside);
    }

    #[test]
    fn rejects_symlinks_pointing_out_of_the_root() {
        let (dir, root) = sandbox();
        std::os::unix::fs::symlink("/etc", dir.path().join("escape")).unwrap();
        assert!(matches!(
            root.resolve("escape/passwd"),
            Err(RootError::Escape(_))
        ));
        assert!(matches!(root.resolve("escape"), Err(RootError::Escape(_))));
    }

    #[test]
    fn rejects_dangling_symlinks_pointing_out_of_the_root() {
        let (dir, root) = sandbox();
        std::os::unix::fs::symlink("/nonexistent/target", dir.path().join("dangling")).unwrap();
        // Must not be mistaken for a creatable new file: writing through it
        // would land at /nonexistent/target.
        assert!(root.resolve("dangling").is_err());
    }

    #[test]
    fn allows_symlinks_that_stay_inside_the_root() {
        let (dir, root) = sandbox();
        std::os::unix::fs::symlink(dir.path().join("sub"), dir.path().join("link")).unwrap();
        let resolved = root.resolve("link/file.txt").unwrap();
        assert!(resolved.starts_with(root.path()));
    }
}
