//! Git work for the Changes panel: the process runner, the porcelain v2 status
//! parser, the staging mutations, commit, the diff a commit message is generated
//! from, and the network calls. `diff.rs` keeps rendering. Git is started with
//! separate arguments, never through a shell (`docs/APP_PRINCIPLES.md:34`).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::platform::ConsoleWindow as _;

/// Fetch, pull and push can wait on credentials or a slow host; a timeout bounds them.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a git call produced no output.
pub(crate) enum GitError {
    Spawn(String),
    Failed(String),
}

impl GitError {
    pub(crate) fn message(self) -> String {
        match self {
            GitError::Spawn(message) | GitError::Failed(message) => message,
        }
    }
}

/// Runs `git -C <directory> <arguments>` and returns stdout.
pub(crate) fn git(directory: &Path, arguments: &[&str]) -> Result<String, GitError> {
    git_with_env(directory, arguments, &[])
}

/// `git` with extra environment variables, for the calls that must never stop
/// and ask the user something (`GIT_EDITOR`, credentials).
fn git_with_env(
    directory: &Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Result<String, GitError> {
    let output = Command::new("git")
        .without_console_window()
        .envs(environment.iter().copied())
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| GitError::Spawn(format!("could not run git: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(GitError::Failed(if stderr.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Where the repository stands: branch, upstream and the changed files. The
/// index is the staging state, so the app keeps no parallel set of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    /// `None` while HEAD is detached.
    pub branch: Option<String>,
    /// The commit HEAD points at; `None` on a branch without commits.
    pub head: Option<String>,
    /// The tracking branch, when the branch has one.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<StatusEntry>,
}

/// One changed path, as porcelain v2 reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// Repo-relative, printed as it is on disk (`core.quotePath=false`).
    pub path: String,
    /// The source of a rename or copy.
    pub original_path: Option<String>,
    /// Porcelain XY: the index letter, then the work tree one (`.` is
    /// unchanged, `A` added, `D` deleted, `M` modified); `??` when untracked.
    pub code: [char; 2],
    /// The index column differs from HEAD.
    pub staged: bool,
    /// The worktree column differs from the index.
    pub unstaged: bool,
    pub untracked: bool,
    /// An unmerged entry: both sides changed the same lines.
    pub conflicted: bool,
}

/// What a commit takes, and what `generation_diff` shows, per the commit matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitMode {
    /// The index.
    Staged,
    /// Nothing is staged, so `commit -a` takes the tracked worktree changes.
    AllTracked,
    /// Replace HEAD; an empty message keeps HEAD's own.
    Amend,
}

impl RepoStatus {
    /// A branch whose first commit is still missing: git prints `(initial)`
    /// instead of a HEAD object id.
    pub fn is_unborn(&self) -> bool {
        self.head.is_none()
    }

    pub fn has_staged(&self) -> bool {
        self.entries.iter().any(|entry| entry.staged)
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|entry| entry.conflicted)
    }

    /// Tracked files with worktree changes, i.e. what `commit -a` would take.
    /// Untracked files never count: they are not committed automatically.
    pub fn has_unstaged_tracked(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.unstaged && !entry.untracked)
    }

    /// Zed's default message when a commit takes exactly one file: "Create
    /// a.rs", "Update a.rs" or "Delete a.rs". `None` for an amend (an empty
    /// message keeps the last one), several files, or a rename or copy.
    pub fn suggested_commit_message(&self, mode: CommitMode) -> Option<String> {
        let mut taken = self.entries.iter().filter_map(|entry| match mode {
            CommitMode::Staged => entry.staged.then_some((entry, entry.code[0])),
            CommitMode::AllTracked => {
                (entry.unstaged && !entry.untracked).then_some((entry, entry.code[1]))
            }
            CommitMode::Amend => None,
        });
        let (entry, letter) = taken.next()?;
        if taken.next().is_some() || entry.conflicted {
            return None;
        }
        let verb = match letter {
            'A' => "Create",
            'D' => "Delete",
            'M' | 'T' => "Update",
            _ => return None,
        };
        let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        Some(format!("{verb} {name}"))
    }

    /// Amend wins over the index, and the index wins over "everything tracked".
    pub fn commit_mode(&self, amend: bool) -> CommitMode {
        if amend {
            CommitMode::Amend
        } else if self.has_staged() {
            CommitMode::Staged
        } else {
            CommitMode::AllTracked
        }
    }
}

/// Branch header and entries of `git status --porcelain=v2 -z`.
pub fn status(root: &Path) -> Result<RepoStatus, GitError> {
    let output = git(
        root,
        &[
            "-c",
            "core.quotePath=false",
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
        ],
    )?;
    Ok(parse_status(&output))
}

/// Parses porcelain v2 output. With `-z` every record, the `# branch.*` headers
/// included, ends at a NUL, so the whole output splits on it. The rename or copy
/// source is the record that follows its entry.
fn parse_status(output: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    let mut records = output.split('\0').filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        // The leading code is one character (or "#" for a header), so the first
        // space never belongs to a path.
        let (code, rest) = match record.split_once(' ') {
            Some(split) => split,
            None => continue,
        };
        match code {
            "#" => parse_header(&mut status, rest),
            // The columns between the status letters and the path: sub, mH, mI,
            // mW, hH, hI; plus the score for a rename/copy; plus m1, m2, m3 and
            // h1, h2, h3 style fields for an unmerged entry.
            "1" | "2" | "u" => {
                let fields = match code {
                    "1" => 6,
                    "2" => 7,
                    _ => 8,
                };
                let Some((columns, path)) = entry_parts(rest, fields) else {
                    continue;
                };
                // Only a rename or copy carries a second path, as the next NUL
                // token; reading it here keeps it out of the record loop.
                let original_path = if code == "2" {
                    records.next().map(str::to_string)
                } else {
                    None
                };
                let mut letters = columns.chars();
                status.entries.push(StatusEntry {
                    path: path.to_string(),
                    original_path,
                    code: [letters.next().unwrap_or('.'), letters.next().unwrap_or('.')],
                    staged: !columns.starts_with('.'),
                    unstaged: !columns.ends_with('.'),
                    untracked: false,
                    conflicted: code == "u",
                });
            }
            // An untracked path is not in the index at all, so only the worktree
            // side has it. "!" is an ignored path, which the panel does not show.
            "?" => status.entries.push(StatusEntry {
                path: rest.to_string(),
                original_path: None,
                code: ['?', '?'],
                staged: false,
                unstaged: true,
                untracked: true,
                conflicted: false,
            }),
            _ => {}
        }
    }
    status
}

/// A `# branch.*` header line. Git prints a placeholder instead of a value where
/// there is none: `(initial)` for a branch without commits, `(detached)` for a
/// detached HEAD.
fn parse_header(status: &mut RepoStatus, rest: &str) {
    let (name, value) = match rest.split_once(' ') {
        Some(split) => split,
        None => return,
    };
    match name {
        "branch.oid" => status.head = header_value(value, "(initial)"),
        "branch.head" => status.branch = header_value(value, "(detached)"),
        "branch.upstream" => status.upstream = Some(value.to_string()),
        // "+2 -1": commits ahead of and behind the upstream.
        "branch.ab" => {
            let mut counts = value.split(' ');
            let ahead = counts
                .next()
                .and_then(|count| count.strip_prefix('+'))
                .and_then(|count| count.parse().ok());
            let behind = counts
                .next()
                .and_then(|count| count.strip_prefix('-'))
                .and_then(|count| count.parse().ok());
            if let (Some(ahead), Some(behind)) = (ahead, behind) {
                status.ahead = ahead;
                status.behind = behind;
            }
        }
        _ => {}
    }
}

/// A header value, or `None` where git printed `placeholder`.
fn header_value(value: &str, placeholder: &str) -> Option<String> {
    (value != placeholder).then(|| value.to_string())
}

/// Splits an entry record into its two status columns and its path: `fields`
/// git-owned columns sit between them. The remainder is the path, which may
/// contain spaces and is never quoted (`core.quotePath=false`), so it is taken
/// whole and never split.
fn entry_parts(record: &str, fields: usize) -> Option<(&str, &str)> {
    let columns = record.get(..2)?;
    let mut path = record.get(2..)?.strip_prefix(' ')?;
    for _ in 0..fields {
        path = path.get(path.find(' ')? + 1..)?;
    }
    Some((columns, path))
}

/// Paths of `entries`, rename sources included: staging the new path alone would
/// leave the old one behind, and unstaging it cannot work without both.
fn entry_paths<'a>(entries: &[&'a StatusEntry]) -> Vec<&'a str> {
    let mut paths = Vec::with_capacity(entries.len());
    for entry in entries {
        paths.push(entry.path.as_str());
        if let Some(original) = &entry.original_path {
            paths.push(original);
        }
    }
    paths
}

/// True when HEAD does not resolve yet. `git restore --staged` needs a HEAD to
/// restore from, so the unstaging calls fall back to `git rm --cached`.
fn head_is_unborn(root: &Path) -> bool {
    git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err()
}

/// Stages `entries`, untracked files included. An empty selection is not an
/// error, but git wants at least one path after `--`.
pub fn stage(root: &Path, entries: &[&StatusEntry]) -> Result<(), GitError> {
    let paths = entry_paths(entries);
    if paths.is_empty() {
        return Ok(());
    }
    let mut arguments: Vec<&str> = vec!["add", "-A", "--"];
    arguments.extend(paths);
    git(root, &arguments).map(|_| ())
}

/// Takes `entries` out of the index, leaving the files themselves alone.
pub fn unstage(root: &Path, entries: &[&StatusEntry]) -> Result<(), GitError> {
    let paths = entry_paths(entries);
    if paths.is_empty() {
        return Ok(());
    }
    // `git restore --staged` needs a HEAD to restore from; on an unborn branch
    // the index entries have to be dropped instead.
    let mut arguments: Vec<&str> = if head_is_unborn(root) {
        vec!["rm", "--cached", "-r", "--quiet", "--"]
    } else {
        vec!["restore", "--staged", "--"]
    };
    arguments.extend(paths);
    git(root, &arguments).map(|_| ())
}

/// Stages every change in the work tree, untracked files included.
pub fn stage_all(root: &Path) -> Result<(), GitError> {
    git(root, &["add", "-A"]).map(|_| ())
}

/// Empties the index back to HEAD (or to nothing, on an unborn branch).
pub fn unstage_all(root: &Path) -> Result<(), GitError> {
    let arguments: &[&str] = if head_is_unborn(root) {
        &[
            "rm",
            "-r",
            "--cached",
            "--quiet",
            "--ignore-unmatch",
            "--",
            ".",
        ]
    } else {
        &["restore", "--staged", "--", "."]
    };
    git(root, arguments).map(|_| ())
}

/// Commits according to `mode` and returns git's own summary. The message always
/// comes from `-m`/`--no-edit`, and `GIT_EDITOR=true` closes the one way git has
/// of opening an editor and hanging the app.
pub fn commit(root: &Path, mode: CommitMode, message: &str) -> Result<String, GitError> {
    let message = message.trim();
    if message.is_empty() && mode != CommitMode::Amend {
        return Err(GitError::Failed("write a commit message first".to_string()));
    }
    if status(root)?.has_conflicts() {
        return Err(GitError::Failed(
            "the index has unresolved conflicts; resolve them before committing".to_string(),
        ));
    }
    let mut arguments: Vec<&str> = match mode {
        CommitMode::Staged => vec!["commit"],
        CommitMode::AllTracked => vec!["commit", "-a"],
        CommitMode::Amend => vec!["commit", "--amend"],
    };
    if message.is_empty() {
        // An amend with an empty box only restages HEAD's message.
        arguments.push("--no-edit");
    } else {
        arguments.extend(["-m", message]);
    }
    git_with_env(root, &arguments, &[("GIT_EDITOR", "true")])
}

/// The raw diff of what `mode` would commit, as the input for a generated commit
/// message. Same argument set as `diff::load_changes`.
pub fn generation_diff(root: &Path, mode: CommitMode) -> Result<String, GitError> {
    let base = match mode {
        // The index against HEAD: what is staged right now.
        CommitMode::Staged => None,
        // `commit -a` re-records the tracked work tree files against HEAD.
        CommitMode::AllTracked => Some(revision_or_empty_tree(root, "HEAD")?),
        // An amend replaces HEAD, so its content is HEAD^ plus the index.
        CommitMode::Amend => Some(revision_or_empty_tree(root, "HEAD^")?),
    };
    let mut arguments = vec![
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--find-renames",
    ];
    if !matches!(mode, CommitMode::AllTracked) {
        arguments.push("--cached");
    }
    if let Some(base) = &base {
        arguments.push(base.as_str());
    }
    git(root, &arguments)
}

/// `revision`, or the empty tree when it does not exist yet: an unborn branch has
/// no HEAD, and amending a root commit has no HEAD^.
fn revision_or_empty_tree(root: &Path, revision: &str) -> Result<String, GitError> {
    if git(root, &["rev-parse", "--verify", "--quiet", revision]).is_ok() {
        return Ok(revision.to_string());
    }
    let empty_tree = git(root, &["hash-object", "-t", "tree", "/dev/null"])?;
    Ok(empty_tree.trim().to_string())
}

/// A network call. Credentials must fail fast instead of hanging the app:
/// no terminal prompt, no askpass, no editor. `run_with_timeout` adds the hidden
/// console window flag, collects stdout, reports the last stderr line on failure
/// and kills the process tree at the timeout.
fn network(root: &Path, arguments: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null());
    crate::process::run_with_timeout(&mut command, None, NETWORK_TIMEOUT)
}

pub fn fetch(root: &Path) -> Result<String, String> {
    network(root, &["fetch"])
}

/// `--ff-only`: a diverged branch fails with git's own message instead of
/// creating a merge commit the user did not ask for.
pub fn pull(root: &Path) -> Result<String, String> {
    network(root, &["pull", "--ff-only"])
}

pub fn push(root: &Path) -> Result<String, String> {
    network(root, &["push"])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records joined the way `git status --porcelain=v2 -z` prints them: every
    /// one NUL-terminated, including the `# branch.*` headers. Fixtures are real
    /// git 2.47 output with the object ids shortened.
    fn porcelain(records: &[&str]) -> String {
        let mut output = records.join("\0");
        output.push('\0');
        output
    }

    const HEAD: &str = "# branch.oid 3961999b226e73c96f9126d85367800d36400f75";
    const BRANCH: &str = "# branch.head main";

    #[test]
    fn reads_a_file_that_is_staged_and_modified_again() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "1 MM N... 100644 100644 100644 bab081fdb7372d4e eb0abdacb2006de4 src/main.rs",
        ]));
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(
            status.head.as_deref(),
            Some("3961999b226e73c96f9126d85367800d36400f75")
        );
        assert!(!status.is_unborn());
        assert_eq!(
            status.entries,
            vec![StatusEntry {
                path: "src/main.rs".to_string(),
                original_path: None,
                code: ['M', 'M'],
                staged: true,
                unstaged: true,
                untracked: false,
                conflicted: false,
            }]
        );
        assert!(status.has_staged());
        assert!(status.has_unstaged_tracked());
        assert!(!status.has_conflicts());
        assert_eq!(status.commit_mode(false), CommitMode::Staged);
        assert_eq!(status.commit_mode(true), CommitMode::Amend);
    }

    #[test]
    fn reads_a_staged_rename_with_its_source_path() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "2 RM N... 100644 100644 100644 cd9a37b579ba1eab cd9a37b579ba1eab R100 renamed dir/ren copy.txt",
            "ren.txt",
            "? untracked.txt",
        ]));
        // The source is the entry's own second path, not a record of its own.
        assert_eq!(status.entries.len(), 2);
        assert_eq!(
            status.entries[0],
            StatusEntry {
                path: "renamed dir/ren copy.txt".to_string(),
                original_path: Some("ren.txt".to_string()),
                code: ['R', 'M'],
                staged: true,
                unstaged: true,
                untracked: false,
                conflicted: false,
            }
        );
        assert_eq!(status.entries[1].path, "untracked.txt");
    }

    #[test]
    fn reads_a_staged_copy_with_its_source_path() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "2 C. N... 100644 100644 100644 3d1c4b35706cac3e 3d1c4b35706cac3e C100 cp-dest.txt",
            "cp-src.txt",
        ]));
        assert_eq!(
            status.entries[0],
            StatusEntry {
                path: "cp-dest.txt".to_string(),
                original_path: Some("cp-src.txt".to_string()),
                code: ['C', '.'],
                staged: true,
                // The 'C' column is the index side only; the work tree matches.
                unstaged: false,
                untracked: false,
                conflicted: false,
            }
        );
    }

    #[test]
    fn keeps_an_untracked_file_out_of_the_index() {
        let status = parse_status(&porcelain(&[HEAD, BRANCH, "? notes/un tracked 名.txt"]));
        assert_eq!(
            status.entries,
            vec![StatusEntry {
                path: "notes/un tracked 名.txt".to_string(),
                original_path: None,
                code: ['?', '?'],
                staged: false,
                unstaged: true,
                untracked: true,
                conflicted: false,
            }]
        );
        // Untracked files are never committed on their own, so a commit
        // would have nothing to take.
        assert!(!status.has_staged());
        assert!(!status.has_unstaged_tracked());
        assert_eq!(status.commit_mode(false), CommitMode::AllTracked);
    }

    #[test]
    fn marks_unmerged_entries_as_conflicts() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "u UU N... 100644 100644 100644 100644 dc51eb430d2552ae d0e41bab516bb42b ef7745affd2824e2 both.txt",
            "1 A. N... 000000 100644 100644 0000000000000000 63c0a67c05421f97 side-only.txt",
        ]));
        assert!(status.entries[0].conflicted);
        assert!(status.entries[0].staged);
        assert!(status.entries[0].unstaged);
        assert_eq!(status.entries[0].path, "both.txt");
        assert!(!status.entries[1].conflicted);
        assert!(status.has_conflicts());
    }

    #[test]
    fn counts_commits_ahead_of_and_behind_the_upstream() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert!(status.entries.is_empty());
    }

    #[test]
    fn reports_no_upstream_and_no_distance_for_a_local_branch() {
        let status = parse_status(&porcelain(&[HEAD, BRANCH]));
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.upstream, None);
        assert_eq!((status.ahead, status.behind), (0, 0));
    }

    #[test]
    fn treats_a_branch_without_commits_as_unborn() {
        let status = parse_status(&porcelain(&[
            "# branch.oid (initial)",
            BRANCH,
            "1 A. N... 000000 100644 100644 0000000000000000 587be6b4c3f93f93 file.txt",
            "? other.txt",
        ]));
        assert!(status.is_unborn());
        assert_eq!(status.head, None);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.has_staged());
    }

    #[test]
    fn has_no_branch_name_while_head_is_detached() {
        let status = parse_status(&porcelain(&[HEAD, "# branch.head (detached)"]));
        assert_eq!(status.branch, None);
        assert_eq!(
            status.head.as_deref(),
            Some("3961999b226e73c96f9126d85367800d36400f75")
        );
        assert!(!status.is_unborn());
    }

    #[test]
    fn keeps_spaces_and_non_ascii_in_paths() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "1 M. N... 100644 100644 100644 bab081fdb7372d4e 5f27dafbc2945baa dir with space/日本 語.txt",
            "2 R. N... 100644 100644 100644 bab081fdb7372d4e bab081fdb7372d4e R100 新 名 前.txt",
            "old 名.txt",
        ]));
        assert_eq!(status.entries[0].path, "dir with space/日本 語.txt");
        assert_eq!(status.entries[1].path, "新 名 前.txt");
        assert_eq!(
            status.entries[1].original_path.as_deref(),
            Some("old 名.txt")
        );
    }

    #[test]
    fn suggests_a_message_for_a_single_file() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "1 .M N... 100644 100644 100644 bab081fdb7372d4e bab081fdb7372d4e src/diff.rs",
            "? notes.txt",
        ]));
        // Untracked files stay out of `commit -a`, so one file is taken.
        assert_eq!(
            status
                .suggested_commit_message(CommitMode::AllTracked)
                .as_deref(),
            Some("Update diff.rs")
        );
        assert_eq!(status.suggested_commit_message(CommitMode::Amend), None);

        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "1 A. N... 000000 100644 100644 0000000000000000 eb0abdacb2006de4 new.rs",
            "1 .M N... 100644 100644 100644 bab081fdb7372d4e bab081fdb7372d4e old.rs",
        ]));
        assert_eq!(
            status
                .suggested_commit_message(CommitMode::Staged)
                .as_deref(),
            Some("Create new.rs")
        );

        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "1 D. N... 100644 000000 000000 bab081fdb7372d4e 0000000000000000 a.rs",
            "1 D. N... 100644 000000 000000 bab081fdb7372d4e 0000000000000000 b.rs",
        ]));
        assert_eq!(status.suggested_commit_message(CommitMode::Staged), None);
    }
}
