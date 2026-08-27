//! File-manager operation identity and clipboard state.
//!
//! Recursive filesystem work stays in `mt-project` / `remote_ssh`; this module owns the
//! application-layer contract that prevents a path copied from one project or SSH host from being
//! pasted into another project that happens to use the same textual path.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileBackendIdentity {
    Local,
    Remote {
        connection_id: String,
        connection_fingerprint: u64,
    },
    BrokenRemote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperationContext {
    pub project_id: String,
    pub root: PathBuf,
    pub backend: FileBackendIdentity,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileClipboardEntry {
    pub project_id: String,
    pub root: PathBuf,
    pub backend: FileBackendIdentity,
    pub generation: u64,
    pub source: PathBuf,
    pub is_dir: bool,
}

impl FileClipboardEntry {
    pub fn can_paste_into(&self, context: &FileOperationContext) -> bool {
        self.project_id == context.project_id
            && self.root == context.root
            && self.backend == context.backend
            && self.generation == context.generation
            && !matches!(&context.backend, FileBackendIdentity::BrokenRemote)
    }

    pub fn would_copy_into_itself(&self, target_dir: &Path) -> bool {
        if !self.is_dir {
            return false;
        }
        match &self.backend {
            FileBackendIdentity::Remote { .. } => {
                let source = self.source.to_string_lossy();
                let target = target_dir.to_string_lossy();
                target == source
                    || target
                        .strip_prefix(source.as_ref())
                        .is_some_and(|rest| rest.starts_with('/'))
            }
            FileBackendIdentity::Local => target_dir.starts_with(&self.source),
            FileBackendIdentity::BrokenRemote => true,
        }
    }
}

pub fn entry_target_directory(path: &Path, is_dir: bool, project_root: &Path) -> PathBuf {
    if is_dir {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project_root.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(project: &str, backend: FileBackendIdentity) -> FileOperationContext {
        FileOperationContext {
            project_id: project.into(),
            root: PathBuf::from("/work"),
            backend,
            generation: 3,
        }
    }

    #[test]
    fn clipboard_is_bound_to_project_and_remote_connection() {
        let clip = FileClipboardEntry {
            project_id: "p".into(),
            root: PathBuf::from("/work"),
            backend: FileBackendIdentity::Remote {
                connection_id: "a".into(),
                connection_fingerprint: 1,
            },
            generation: 3,
            source: PathBuf::from("/work/file"),
            is_dir: false,
        };
        assert!(clip.can_paste_into(&context(
            "p",
            FileBackendIdentity::Remote {
                connection_id: "a".into(),
                connection_fingerprint: 1,
            }
        )));
        assert!(!clip.can_paste_into(&context(
            "p",
            FileBackendIdentity::Remote {
                connection_id: "b".into(),
                connection_fingerprint: 1,
            }
        )));
        assert!(!clip.can_paste_into(&context(
            "p",
            FileBackendIdentity::Remote {
                connection_id: "a".into(),
                connection_fingerprint: 2,
            }
        )));
        let mut changed_generation = context(
            "p",
            FileBackendIdentity::Remote {
                connection_id: "a".into(),
                connection_fingerprint: 1,
            },
        );
        changed_generation.generation += 1;
        assert!(!clip.can_paste_into(&changed_generation));
        let mut changed_root = context(
            "p",
            FileBackendIdentity::Remote {
                connection_id: "a".into(),
                connection_fingerprint: 1,
            },
        );
        changed_root.root = PathBuf::from("/other");
        assert!(!clip.can_paste_into(&changed_root));
        assert!(!clip.can_paste_into(&context("other", FileBackendIdentity::Local)));
    }

    #[test]
    fn directory_clipboard_rejects_self_and_descendants() {
        let clip = FileClipboardEntry {
            project_id: "p".into(),
            root: PathBuf::from("/work"),
            backend: FileBackendIdentity::Remote {
                connection_id: "a".into(),
                connection_fingerprint: 1,
            },
            generation: 3,
            source: PathBuf::from("/work/src"),
            is_dir: true,
        };
        assert!(clip.would_copy_into_itself(Path::new("/work/src")));
        assert!(clip.would_copy_into_itself(Path::new("/work/src/nested")));
        assert!(!clip.would_copy_into_itself(Path::new("/work")));
        assert!(!clip.would_copy_into_itself(Path::new("/work/src-other")));
    }

    #[test]
    fn file_targets_parent_and_directory_targets_itself() {
        assert_eq!(
            entry_target_directory(Path::new("/work/src"), true, Path::new("/work")),
            PathBuf::from("/work/src")
        );
        assert_eq!(
            entry_target_directory(Path::new("/work/src/main.rs"), false, Path::new("/work")),
            PathBuf::from("/work/src")
        );
    }
}
