use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::git::{self, Discard, GitError, StatusEntry};

const CHANGED: &str = "the file or repository changed; nothing was discarded";

pub(super) struct PreparedDiscard {
    root: PathBuf,
    head: Option<String>,
    entry: StatusEntry,
    steps: Vec<Discard>,
    snapshot: Snapshot,
}

impl PreparedDiscard {
    pub(super) fn prepare(
        root: PathBuf,
        head: Option<String>,
        entry: StatusEntry,
        steps: Vec<Discard>,
    ) -> Result<Self, String> {
        check_status(&root, head.as_deref(), &entry)?;
        let snapshot = Snapshot::read(&root, &steps)?;
        Ok(Self {
            root,
            head,
            entry,
            steps,
            snapshot,
        })
    }

    pub(super) fn run(self, root: &Path) -> Result<(), String> {
        if root != self.root.as_path() {
            return Err(CHANGED.into());
        }
        check_status(root, self.head.as_deref(), &self.entry)?;
        if Snapshot::read(root, &self.steps)? != self.snapshot {
            return Err(CHANGED.into());
        }
        for step in self.steps {
            match step {
                Discard::Restore(path) => {
                    let commit = self.head.as_deref().ok_or(CHANGED)?;
                    git::restore_from_commit(root, commit, &path).map_err(GitError::message)?;
                }
                Discard::Remove { path, staged } => {
                    if staged {
                        git::remove_from_index(root, &path).map_err(GitError::message)?;
                    }
                    let full_path = root.join(path);
                    match fs::symlink_metadata(&full_path) {
                        Ok(metadata) if !metadata.is_dir() => {
                            crate::platform::move_to_trash(&full_path)?;
                        }
                        Err(error) if error.kind() == ErrorKind::NotFound => {}
                        Ok(_) => return Err("a file was replaced by a directory".into()),
                        Err(error) => return Err(format!("could not check the file: {error}")),
                    }
                }
            }
        }
        Ok(())
    }
}

fn check_status(root: &Path, head: Option<&str>, entry: &StatusEntry) -> Result<(), String> {
    let status = git::status(root).map_err(GitError::message)?;
    if status.head.as_deref() != head || !status.entries.contains(entry) {
        return Err(CHANGED.into());
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    index: String,
    worktree: Vec<WorktreeVersion>,
}

impl Snapshot {
    fn read(root: &Path, steps: &[Discard]) -> Result<Self, String> {
        let paths: Vec<&str> = steps
            .iter()
            .map(|step| match step {
                Discard::Restore(path) | Discard::Remove { path, .. } => path.as_str(),
            })
            .collect();
        let mut arguments = vec!["ls-files", "--stage", "-z", "--"];
        arguments.extend(paths.iter().copied());
        let index = git::git(root, &arguments).map_err(GitError::message)?;
        let worktree = paths
            .into_iter()
            .map(|path| WorktreeVersion::read(root, path))
            .collect::<Result<_, _>>()?;
        Ok(Self { index, worktree })
    }
}

#[derive(PartialEq, Eq)]
enum WorktreeVersion {
    Missing,
    File(String),
    Symlink(PathBuf),
}

impl WorktreeVersion {
    fn read(root: &Path, path: &str) -> Result<Self, String> {
        let full_path = root.join(path);
        let metadata = match fs::symlink_metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(format!("could not check the file: {error}")),
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return fs::read_link(full_path)
                .map(Self::Symlink)
                .map_err(|error| format!("could not read the link: {error}"));
        }
        if !kind.is_file() {
            return Err("the path is not a regular file or symlink".into());
        }
        git::git(root, &["hash-object", "--no-filters", "--", path])
            .map(Self::File)
            .map_err(GitError::message)
    }
}
