//! Git work for the Changes panel: the process runner, the repositories of a
//! project, the porcelain v2 status parser, the staging mutations, commit, the
//! diff a commit message is generated from, and the network calls. `diff.rs`
//! keeps rendering. Git is started with separate arguments, never through a
//! shell (`docs/APP_PRINCIPLES.md:34`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use crate::platform::ConsoleWindow as _;

/// Fetch, pull and push can wait on credentials or a slow host; a timeout bounds them.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(300);
/// How deep a project that is no repository itself is searched for ones.
const SEARCH_DEPTH: usize = 2;

/// Repositories found per project. Finding them takes two git runs per
/// repository, so they are looked for again only after `forget_repositories`
/// or when one of them is gone.
static REPOSITORIES: LazyLock<Mutex<HashMap<PathBuf, Vec<PathBuf>>>> =
    LazyLock::new(Mutex::default);
/// Held for a whole search, apart from `REPOSITORIES`, so a reader on the
/// UI thread never waits for git.
static SEARCH: Mutex<()> = Mutex::new(());

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
///
/// Every call reads paths literally (a file named `a[1].md` is never a
/// pattern) unless `environment` turns that off, and reads skip the optional
/// index refresh: a `git status` that rewrote `.git/index` would wake the
/// project watcher, whose refresh runs `git status` again.
fn git_with_env(
    directory: &Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Result<String, GitError> {
    let output = Command::new("git")
        .without_console_window()
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
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

/// The repositories of a project, as Zed finds them: the one holding
/// `workspace` first, then those nested inside it. A project that is no
/// repository itself is searched a few folders down instead. The panels and
/// the watcher ask at once; the lock makes them share one search.
pub fn repositories(workspace: &Path) -> Result<Vec<PathBuf>, GitError> {
    let _search = SEARCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let known = known_repositories(workspace);
    if !known.is_empty() && known.iter().all(|root| root.join(".git").exists()) {
        return Ok(known);
    }
    let found = find_repositories(workspace)?;
    REPOSITORIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(workspace.to_path_buf(), found.clone());
    Ok(found)
}

/// The repositories `repositories` found last, without running git.
pub fn known_repositories(workspace: &Path) -> Vec<PathBuf> {
    REPOSITORIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(workspace)
        .cloned()
        .unwrap_or_default()
}

/// Makes the next `repositories` call search `workspace` again.
pub fn forget_repositories(workspace: &Path) {
    REPOSITORIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(workspace);
}

/// Searches level by level, the repositories of one level at once.
fn find_repositories(workspace: &Path) -> Result<Vec<PathBuf>, GitError> {
    let mut found = match git(workspace, &["rev-parse", "--show-toplevel"]) {
        Ok(output) => vec![PathBuf::from(output.trim_end_matches(['\n', '\r']))],
        Err(GitError::Failed(_)) => repositories_below(workspace, SEARCH_DEPTH),
        Err(error) => return Err(error),
    };
    let mut level = found.clone();
    while !level.is_empty() {
        let mut next = Vec::new();
        for nested in for_each_repository(&level, nested_repositories) {
            next.extend(
                nested?
                    .into_iter()
                    .filter(|root| root.starts_with(workspace)),
            );
        }
        found.extend(next.iter().cloned());
        level = next;
    }
    Ok(found)
}

/// Runs `job` for every repository at once, each on its own thread: a git
/// run is mostly process start-up, so they overlap well. Results keep the
/// order of `roots`.
pub(crate) fn for_each_repository<R: Send>(
    roots: &[PathBuf],
    job: impl Fn(&Path) -> R + Sync,
) -> Vec<R> {
    let job = &job;
    thread::scope(|scope| {
        let handles: Vec<_> = roots
            .iter()
            .map(|root| scope.spawn(move || job(root)))
            .collect();
        handles.into_iter().map(joined).collect()
    })
}

/// The result of a scoped thread; its panic carries on in the caller.
pub(crate) fn joined<T>(handle: thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Repositories directly inside `root`'s work tree: untracked or ignored
/// folders with their own `.git`, and checked-out submodules. Git lists an
/// untracked one as a folder, since it never looks inside.
fn nested_repositories(root: &Path) -> Result<Vec<PathBuf>, GitError> {
    let (untracked, ignored) = thread::scope(|scope| {
        let ignored = scope.spawn(|| {
            git(
                root,
                &[
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                    "-z",
                ],
            )
        });
        let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]);
        (untracked, joined(ignored))
    });
    let (untracked, ignored) = (untracked?, ignored?);
    let folders = untracked
        .split('\0')
        .chain(ignored.split('\0'))
        .filter_map(|path| path.strip_suffix('/'))
        .map(|path| root.join(path));
    let submodules = submodule_paths(root)
        .into_iter()
        .map(|path| root.join(path));
    Ok(folders
        .chain(submodules)
        .filter(|folder| folder.join(".git").exists())
        .collect())
}

/// Paths `.gitmodules` declares; none when it is missing.
fn submodule_paths(root: &Path) -> Vec<String> {
    if !root.join(".gitmodules").is_file() {
        return Vec::new();
    }
    let Ok(output) = git(
        root,
        &[
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    ) else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(_, path)| path.to_string())
        .collect()
}

/// Folders with a `.git` under `folder`, `depth` levels down at most; a
/// repository's own nested ones are found through git instead.
fn repositories_below(folder: &Path, depth: usize) -> Vec<PathBuf> {
    if depth == 0 {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || entry.file_name() == ".git" {
            continue;
        }
        if path.join(".git").exists() {
            found.push(path);
        } else {
            found.extend(repositories_below(&path, depth - 1));
        }
    }
    found.sort();
    found
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
    /// Repositories inside the work tree, repo-relative: git lists each as
    /// an untracked folder, which is not a change of this repository.
    pub nested: Vec<String>,
    /// Names of the configured remotes, filled by `remotes`.
    pub remotes: Vec<String>,
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

    /// The remote of the upstream, e.g. `origin` for `origin/main`. Remote
    /// names may contain slashes, so the longest matching name wins.
    pub fn upstream_remote(&self) -> Option<&str> {
        let upstream = self.upstream.as_deref()?;
        self.remotes
            .iter()
            .filter(|remote| {
                upstream
                    .strip_prefix(remote.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|remote| remote.len())
            .map(String::as_str)
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

/// What a repository holds that is not on its remote yet, for the
/// repository menu: uncommitted changes and branches still to push.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoOverview {
    /// Changed and untracked files, as the Changes panel counts them.
    pub changes: usize,
    pub unpushed: Vec<UnpushedBranch>,
}

/// A local branch with commits its upstream lacks, or with no upstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnpushedBranch {
    pub name: String,
    /// Commits ahead of the upstream; `None` when the branch was never published.
    pub ahead: Option<u32>,
}

impl RepoOverview {
    pub fn is_clean(&self) -> bool {
        self.changes == 0 && self.unpushed.is_empty()
    }

    /// Commits to push on the branches that have an upstream.
    pub fn commits_ahead(&self) -> u32 {
        self.unpushed.iter().filter_map(|branch| branch.ahead).sum()
    }

    pub fn unpublished(&self) -> usize {
        self.unpushed
            .iter()
            .filter(|branch| branch.ahead.is_none())
            .count()
    }
}

/// The overview of one repository: a status, a ref listing and the remotes.
/// A repository without remotes has nowhere to publish a branch, so its
/// branches without upstream are not listed.
pub fn overview(root: &Path) -> Result<RepoOverview, GitError> {
    let (status, branches, remotes) = thread::scope(|scope| {
        let remotes = scope.spawn(|| remotes(root));
        let branches = scope.spawn(|| {
            git(
                root,
                &[
                    "for-each-ref",
                    "--format=%(refname)%00%(upstream)%00%(upstream:track,nobracket)",
                    "refs/heads",
                ],
            )
        });
        (status(root), joined(branches), joined(remotes))
    });
    let mut unpushed = parse_unpushed(&branches?);
    if remotes?.is_empty() {
        unpushed.retain(|branch| branch.ahead.is_some());
    }
    Ok(RepoOverview {
        changes: status?.entries.len(),
        unpushed,
    })
}

/// Branches of `for-each-ref` output that still need a push. A `gone`
/// upstream was deleted on the remote, usually after a merge, so it counts
/// as pushed; a branch only behind its upstream has nothing to push.
fn parse_unpushed(output: &str) -> Vec<UnpushedBranch> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let name = fields.next()?.strip_prefix("refs/heads/")?.to_string();
            let upstream = fields.next().unwrap_or_default();
            let track = fields.next().unwrap_or_default();
            if upstream.is_empty() {
                return Some(UnpushedBranch { name, ahead: None });
            }
            // "ahead 2", "behind 1", "ahead 2, behind 1" or "gone".
            let ahead = track
                .split(", ")
                .find_map(|part| part.strip_prefix("ahead "))
                .and_then(|count| count.parse().ok())
                .filter(|&count: &u32| count > 0)?;
            Some(UnpushedBranch {
                name,
                ahead: Some(ahead),
            })
        })
        .collect()
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
            "?" if rest.ends_with('/') => status.nested.push(rest.to_string()),
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

/// Stages every change in the work tree, untracked files included. The
/// `nested` repositories stay out: `add -A` would record each as an embedded
/// repository. Excluding them takes pathspec magic, so literal paths are off
/// and each is marked literal itself.
pub fn stage_all(root: &Path, nested: &[String]) -> Result<(), GitError> {
    if nested.is_empty() {
        return git(root, &["add", "-A"]).map(|_| ());
    }
    let exclusions: Vec<String> = nested
        .iter()
        .map(|path| format!(":(exclude,top,literal){}", path.trim_end_matches('/')))
        .collect();
    let mut arguments = vec!["add", "-A", "--", ":(top)"];
    arguments.extend(exclusions.iter().map(String::as_str));
    git_with_env(root, &arguments, &[("GIT_LITERAL_PATHSPECS", "0")]).map(|_| ())
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

/// Where Pull and Push go when the user picks a remote: the upstream branch
/// on the upstream's remote, the branch of the same name on any other.
pub struct RemoteTarget {
    pub remote: String,
    pub local_branch: String,
    pub remote_branch: String,
    /// The branch tracks nothing yet: Push records the target as its upstream,
    /// the way `push.autoSetupRemote` would, instead of failing.
    pub set_upstream: bool,
}

impl RemoteTarget {
    /// `None` while HEAD is detached.
    pub fn new(status: &RepoStatus, remote: String) -> Option<Self> {
        let local_branch = status.branch.clone()?;
        let remote_branch = status
            .upstream
            .as_deref()
            .filter(|_| status.upstream_remote() == Some(remote.as_str()))
            .and_then(|upstream| upstream.strip_prefix(&format!("{remote}/")))
            .unwrap_or(&local_branch)
            .to_string();
        Some(Self {
            remote,
            local_branch,
            remote_branch,
            set_upstream: status.upstream.is_none(),
        })
    }
}

/// Pulls the target branch into the current one, fast-forward only.
pub fn pull_from(root: &Path, target: &RemoteTarget) -> Result<String, String> {
    network(
        root,
        &["pull", "--ff-only", &target.remote, &target.remote_branch],
    )
}

pub fn push_to(root: &Path, target: &RemoteTarget) -> Result<String, String> {
    let refspec = format!("{}:{}", target.local_branch, target.remote_branch);
    let mut arguments = vec!["push"];
    if target.set_upstream {
        arguments.push("--set-upstream");
    }
    arguments.extend([target.remote.as_str(), refspec.as_str()]);
    network(root, &arguments)
}

/// Names of the configured remotes.
pub fn remotes(root: &Path) -> Result<Vec<String>, GitError> {
    let output = git(root, &["remote"])?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// A local or remote-tracking branch, as the branch picker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// `main` for a local branch, `origin/main` for a remote one.
    pub name: String,
    pub is_remote: bool,
    /// The branch HEAD is on.
    pub is_head: bool,
    /// A local branch that is on no remote: it has no upstream, or its
    /// upstream was deleted there (`gone`).
    pub local_only: bool,
    /// Unix time of the last commit; 0 when git printed none.
    pub committed_at: u64,
    /// Subject of the last commit.
    pub subject: String,
}

/// Local and remote branches the way Zed's branch picker lists them: a
/// remote branch some local branch already tracks is left out, because
/// checking it out would only detach HEAD; the current branch comes first,
/// then the most recently committed.
pub fn branches(root: &Path) -> Result<Vec<Branch>, GitError> {
    let output = git(
        root,
        &[
            "for-each-ref",
            "--format=%(HEAD)%00%(refname)%00%(upstream)%00%(upstream:track)%00%(committerdate:unix)%00%(symref)%00%(subject)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(parse_branches(&output))
}

fn parse_branches(output: &str) -> Vec<Branch> {
    let mut tracked = Vec::new();
    let mut branches = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.splitn(7, '\0').collect();
        let [
            head,
            reference,
            upstream,
            track,
            committed_at,
            symref,
            subject,
        ] = fields[..]
        else {
            continue;
        };
        // `origin/HEAD` points at another remote branch.
        if !symref.is_empty() {
            continue;
        }
        if !upstream.is_empty() {
            tracked.push(upstream.to_string());
        }
        let (name, is_remote) = if let Some(name) = reference.strip_prefix("refs/heads/") {
            (name, false)
        } else if let Some(name) = reference.strip_prefix("refs/remotes/") {
            (name, true)
        } else {
            continue;
        };
        branches.push((
            reference.to_string(),
            Branch {
                name: name.to_string(),
                is_remote,
                is_head: head == "*",
                local_only: !is_remote && (upstream.is_empty() || track == "[gone]"),
                committed_at: committed_at.parse().unwrap_or(0),
                subject: subject.to_string(),
            },
        ));
    }
    let mut branches: Vec<Branch> = branches
        .into_iter()
        .filter(|(reference, _)| !tracked.contains(reference))
        .map(|(_, branch)| branch)
        .collect();
    branches.sort_by_key(|branch| (!branch.is_head, std::cmp::Reverse(branch.committed_at)));
    branches
}

/// Checks out `branch`. A remote branch gets a local branch of the same short
/// name that tracks it, as in Zed.
pub fn switch_branch(root: &Path, branch: &Branch) -> Result<(), GitError> {
    let arguments: &[&str] = if branch.is_remote {
        &["switch", "--track", &branch.name]
    } else {
        &["switch", &branch.name]
    };
    git(root, arguments).map(|_| ())
}

/// Deletes the local branch `name`. Without `force`, git refuses a branch
/// whose commits are not merged into its upstream or HEAD; see
/// `is_not_merged`, which reads git's English message, so the locale is C.
pub fn delete_branch(root: &Path, name: &str, force: bool) -> Result<(), GitError> {
    let flag = if force { "-D" } else { "-d" };
    git_with_env(root, &["branch", flag, name], &[("LC_ALL", "C")]).map(|_| ())
}

/// Whether a `delete_branch` error is git refusing an unmerged branch,
/// which a forced delete would go through.
pub fn is_not_merged(error: &str) -> bool {
    error.contains("not fully merged")
}

/// Creates `name` at HEAD and checks it out.
pub fn create_branch(root: &Path, name: &str) -> Result<(), GitError> {
    git(root, &["switch", "-c", name]).map(|_| ())
}

/// A typed branch name made valid the way Zed does: surrounding space
/// trimmed, inner whitespace turned into dashes.
pub fn normalize_branch_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join("-")
}

/// What discarding a file's changes runs, decided from its status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Discard {
    /// Tracked in HEAD: index and work tree go back to HEAD's version.
    Restore(String),
    /// New since HEAD: out of the index, and the file goes to the Trash.
    Remove { path: String, staged: bool },
}

impl Discard {
    /// The steps that throw away every change of `entry`. A rename restores
    /// its source and removes the new path.
    pub fn plan(entry: &StatusEntry, unborn: bool) -> Vec<Discard> {
        let is_new =
            entry.untracked || unborn || entry.code[0] == 'A' || entry.original_path.is_some();
        let mut steps = Vec::new();
        // A copy leaves its source alone; a rename took it away. The source
        // comes back first, so a failure later never leaves neither path.
        if let Some(original) = &entry.original_path
            && entry.code[0] == 'R'
            && !unborn
        {
            steps.push(Discard::Restore(original.clone()));
        }
        if is_new {
            steps.push(Discard::Remove {
                path: entry.path.clone(),
                staged: !entry.untracked,
            });
        } else {
            steps.push(Discard::Restore(entry.path.clone()));
        }
        steps
    }
}

/// Index and work tree of `path` back to HEAD.
pub fn restore_from_head(root: &Path, path: &str) -> Result<(), GitError> {
    git(
        root,
        &[
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            path,
        ],
    )
    .map(|_| ())
}

/// Drops `path` from the index and keeps the file on disk.
pub fn remove_from_index(root: &Path, path: &str) -> Result<(), GitError> {
    git(root, &["rm", "--cached", "--quiet", "--force", "--", path]).map(|_| ())
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
    fn keeps_a_nested_repository_out_of_the_changes() {
        let status = parse_status(&porcelain(&[HEAD, BRANCH, "? code/app/", "? notes.md"]));
        assert_eq!(status.nested, vec!["code/app/".to_string()]);
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].path, "notes.md");
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
    fn lists_branches_without_tracked_remotes_newest_first() {
        let output = [
            " \0refs/heads/feature\0\0\x00300\0\0Add feature",
            " \0refs/heads/merged\0refs/remotes/origin/merged\0[gone]\x00250\0\0Merge",
            "*\0refs/heads/main\0refs/remotes/origin/main\0[ahead 1]\x00100\0\0Initial commit",
            " \0refs/remotes/origin/HEAD\0\0\x00100\0refs/remotes/origin/main\0Initial commit",
            " \0refs/remotes/origin/main\0\0\x00100\0\0Initial commit",
            " \0refs/remotes/origin/fix/crash\0\0\x00200\0\0Fix crash",
        ]
        .join("\n");
        let names: Vec<(String, bool, bool, bool)> = parse_branches(&output)
            .into_iter()
            .map(|branch| {
                (
                    branch.name,
                    branch.is_remote,
                    branch.is_head,
                    branch.local_only,
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("main".to_string(), false, true, false),
                ("feature".to_string(), false, false, true),
                ("merged".to_string(), false, false, true),
                ("origin/fix/crash".to_string(), true, false, false),
            ]
        );
    }

    #[test]
    fn finds_the_remote_of_the_upstream() {
        let mut status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "# branch.upstream team/origin/main",
        ]));
        status.remotes = vec!["team".to_string(), "team/origin".to_string()];
        assert_eq!(status.upstream_remote(), Some("team/origin"));
        status.upstream = None;
        assert_eq!(status.upstream_remote(), None);
    }

    #[test]
    fn targets_the_upstream_branch_on_its_own_remote() {
        let mut status = parse_status(&porcelain(&[
            HEAD,
            "# branch.head work",
            "# branch.upstream origin/main",
        ]));
        status.remotes = vec!["fork".to_string(), "origin".to_string()];
        let target = RemoteTarget::new(&status, "origin".to_string()).unwrap();
        assert_eq!(
            (target.local_branch.as_str(), target.remote_branch.as_str()),
            ("work", "main")
        );
        let target = RemoteTarget::new(&status, "fork".to_string()).unwrap();
        assert_eq!(target.remote_branch, "work");
        assert!(!target.set_upstream);
    }

    #[test]
    fn lists_branches_that_still_need_a_push() {
        let output = [
            "refs/heads/main\0refs/remotes/origin/main\0",
            "refs/heads/work\0refs/remotes/origin/work\0ahead 2, behind 1",
            "refs/heads/old\0refs/remotes/origin/old\0behind 3",
            "refs/heads/merged\0refs/remotes/origin/merged\0gone",
            "refs/heads/new\0\0",
        ]
        .join("\n");
        let overview = RepoOverview {
            changes: 0,
            unpushed: parse_unpushed(&output),
        };
        assert_eq!(
            overview.unpushed,
            vec![
                UnpushedBranch {
                    name: "work".to_string(),
                    ahead: Some(2),
                },
                UnpushedBranch {
                    name: "new".to_string(),
                    ahead: None,
                },
            ]
        );
        assert_eq!((overview.commits_ahead(), overview.unpublished()), (2, 1));
        assert!(!overview.is_clean());
    }

    #[test]
    fn sets_the_upstream_of_a_branch_that_tracks_nothing() {
        let mut status = parse_status(&porcelain(&[HEAD, "# branch.head work"]));
        status.remotes = vec!["origin".to_string()];
        let target = RemoteTarget::new(&status, "origin".to_string()).unwrap();
        assert_eq!(target.remote_branch, "work");
        assert!(target.set_upstream);
    }

    #[test]
    fn plans_a_discard_from_the_status() {
        let status = parse_status(&porcelain(&[
            HEAD,
            BRANCH,
            "1 .M N... 100644 100644 100644 bab081fdb7372d4e bab081fdb7372d4e changed.rs",
            "1 A. N... 000000 100644 100644 0000000000000000 eb0abdacb2006de4 added.rs",
            "2 R. N... 100644 100644 100644 bab081fdb7372d4e bab081fdb7372d4e R100 new.rs",
            "old.rs",
            "? untracked.rs",
        ]));
        let plans: Vec<Vec<Discard>> = status
            .entries
            .iter()
            .map(|entry| Discard::plan(entry, false))
            .collect();
        assert_eq!(plans[0], vec![Discard::Restore("changed.rs".to_string())]);
        assert_eq!(
            plans[1],
            vec![Discard::Remove {
                path: "added.rs".to_string(),
                staged: true
            }]
        );
        assert_eq!(
            plans[2],
            vec![
                Discard::Restore("old.rs".to_string()),
                Discard::Remove {
                    path: "new.rs".to_string(),
                    staged: true
                },
            ]
        );
        assert_eq!(
            plans[3],
            vec![Discard::Remove {
                path: "untracked.rs".to_string(),
                staged: false
            }]
        );
    }

    #[test]
    fn normalizes_typed_branch_names() {
        assert_eq!(normalize_branch_name("  fix  the crash "), "fix-the-crash");
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
