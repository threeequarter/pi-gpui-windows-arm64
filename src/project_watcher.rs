//! Watches the active project folder, as Zed watches its worktrees, so the
//! Files and Changes panels follow edits made by the agent, an editor or git
//! without a refresh button. Events arrive on the watcher's thread; changes
//! under git-ignored folders (build output, dependencies) are dropped there,
//! and a burst of changes becomes one notification once it settles.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{Context, Task};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::platform::ConsoleWindow as _;

/// Quiet time after a change before the panels read the disk again.
const SETTLE_DELAY: Duration = Duration::from_millis(300);
/// Longest wait during a steady stream of changes (a checkout, an install),
/// so the panels never stay stale while it lasts.
const MAX_DELAY: Duration = Duration::from_secs(2);

pub struct ProjectWatcher {
    root: PathBuf,
    _watcher: RecommendedWatcher,
    _task: Task<()>,
}

impl ProjectWatcher {
    /// Calls `on_change` on the foreground after files under `root` change.
    pub fn new<T: 'static>(
        root: PathBuf,
        on_change: impl Fn(&mut T, &mut Context<T>) + 'static,
        cx: &mut Context<T>,
    ) -> Result<Self, String> {
        let ignored = Arc::new(RwLock::new(Vec::new()));
        let initial = (ignored.clone(), root.clone());
        cx.background_executor()
            .spawn(async move {
                let (ignored, root) = initial;
                let folders = ignored_folders(&root);
                if let Ok(mut ignored) = ignored.write() {
                    *ignored = folders;
                }
            })
            .detach();
        let (sender, receiver) = mpsc::unbounded::<()>();
        let watch_root = root.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let event = match result {
                    Ok(event) => event,
                    Err(error) => {
                        eprintln!("pi-gpui: watching {} failed: {error}", watch_root.display());
                        return;
                    }
                };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                if event.paths.iter().any(|path| is_gitignore(path)) {
                    let folders = ignored_folders(&watch_root);
                    if let Ok(mut ignored) = ignored.write() {
                        *ignored = folders;
                    }
                }
                let relevant = match ignored.read() {
                    Ok(ignored) => event
                        .paths
                        .iter()
                        .any(|path| is_relevant(path, &watch_root, &ignored)),
                    Err(_) => true,
                };
                if relevant {
                    sender.unbounded_send(()).ok();
                }
            })
            .map_err(|error| error.to_string())?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            root,
            _watcher: watcher,
            _task: when_settled(receiver, on_change, cx),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Calls `on_change` on the foreground once a burst of signals from a
/// watcher settles.
pub fn when_settled<T: 'static>(
    mut receiver: mpsc::UnboundedReceiver<()>,
    on_change: impl Fn(&mut T, &mut Context<T>) + 'static,
    cx: &mut Context<T>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        while receiver.next().await.is_some() {
            // Let the burst settle, then take everything that queued up.
            let started = Instant::now();
            loop {
                cx.background_executor().timer(SETTLE_DELAY).await;
                let mut more = false;
                while let Ok(Some(())) = receiver.try_next() {
                    more = true;
                }
                if !more || started.elapsed() >= MAX_DELAY {
                    break;
                }
            }
            if this.update(cx, |this, cx| on_change(this, cx)).is_err() {
                break;
            }
        }
    })
}

fn is_gitignore(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

/// Worth a refresh: a project file outside ignored folders, or the git state
/// the panels show (index, HEAD, branches). Object writes and lock files
/// inside `.git` come with every git command and are noise.
fn is_relevant(path: &Path, root: &Path, ignored: &[PathBuf]) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut components = relative.components();
    let first = components.next();
    if first.is_some_and(|component| component.as_os_str() == ".git") {
        let rest = components.as_path();
        if rest
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            return false;
        }
        return rest == Path::new("index")
            || rest == Path::new("HEAD")
            || rest == Path::new("packed-refs")
            || rest.starts_with("refs");
    }
    !ignored.iter().any(|folder| path.starts_with(folder))
}

/// Ignored folders and files under `root`, as git reports them (whole
/// folders where it can). Empty outside a git work tree.
fn ignored_folders(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .without_console_window()
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(|path| root.join(path.trim_end_matches('/')))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_git_noise_and_ignored_folders() {
        let root = Path::new("project");
        let ignored = vec![root.join("target")];
        let relevant = |path: &str| is_relevant(&root.join(path), root, &ignored);
        assert!(relevant("src/main.rs"));
        assert!(relevant(".git/index"));
        assert!(relevant(".git/HEAD"));
        assert!(relevant(".git/refs/heads/main"));
        assert!(!relevant(".git/index.lock"));
        assert!(!relevant(".git/objects/ab/cdef"));
        assert!(!relevant("target/debug/pi-gpui.exe"));
    }
}
