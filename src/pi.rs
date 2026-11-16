//! Installed pi: discovery, version gating and starting `pi --mode rpc`.
//!
//! Everything here blocks; call it from a background task.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use futures::channel::mpsc;

use crate::jsonl::{JsonlMessage, JsonlProcess};
use crate::process::run_with_timeout;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// One-shot print mode answers a prompt with a model round-trip, so it gets
/// far more time than the quick `pi` commands above.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(120);

/// Longest diff (in characters) sent to pi for a commit message; the rest is
/// cut, with a note, to keep the prompt in a sane size.
const MAX_COMMIT_DIFF_CHARS: usize = 120_000;

/// Replaces pi's coding-agent system prompt, which describes tools: with
/// `--no-tools` some models (DeepSeek) then write tool calls as plain text.
const COMMIT_SYSTEM_PROMPT: &str = "You write git commit messages. You have no tools and \
need none: the user message holds the instructions and the complete diff. Answer with \
the commit message only, as plain text.";

/// Tool-call markup that models print as text when they reach for a tool
/// they do not have.
const TOOL_CALL_MARKERS: [&str; 4] = ["DSML", "<tool_call>", "<function_calls>", "<invoke "];

/// Field order makes the derived ordering compare major, then minor, then patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Finds the first `x.y.z` in `text`, ignoring prefixes such as `v` or `pi `.
    pub fn find_in(text: &str) -> Option<Self> {
        text.split(|character: char| !(character.is_ascii_digit() || character == '.'))
            .find_map(|candidate| {
                let mut parts = candidate.split('.');
                let major = parts.next()?.parse().ok()?;
                let minor = parts.next()?.parse().ok()?;
                let patch = parts.next()?.parse().ok()?;
                Some(Self {
                    major,
                    minor,
                    patch,
                })
            })
    }

    pub fn app() -> Self {
        Self {
            major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
            minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
            patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The oldest pi the app supports. Newer pi versions with the same major are
/// supported too; a new major is not (see docs/APP_PRINCIPLES.md).
pub const MIN_PI: Version = Version {
    major: 1,
    minor: 4,
    patch: 0,
};

/// The newest pi minor the app was tested with. Newer minors of the same
/// major still run, with a note that they are untested.
pub const TESTED_PI: Version = Version {
    major: 1,
    minor: 5,
    patch: 0,
};

/// Whether this app supports `pi`: at least `MIN_PI`, same major.
pub fn supports(pi: Version) -> bool {
    pi >= MIN_PI && pi.major == MIN_PI.major
}

/// Supported, but newer than anything the app was tested with.
pub fn is_untested(pi: Version) -> bool {
    (pi.major, pi.minor) > (TESTED_PI.major, TESTED_PI.minor)
}

/// The app's pi support as JSON, published next to each release so the
/// updater can tell which release fits the installed pi.
pub fn compatibility_json() -> String {
    serde_json::json!({
        "app": env!("CARGO_PKG_VERSION"),
        "pi": {
            "min": MIN_PI.to_string(),
            "major": MIN_PI.major,
            "tested": format!("{}.{}", TESTED_PI.major, TESTED_PI.minor),
        },
    })
    .to_string()
}

#[derive(Clone, Debug)]
pub struct Installation {
    pub executable: PathBuf,
    pub version: Version,
}

#[derive(Clone, Debug)]
pub enum CheckError {
    NotFound,
    VersionUnreadable {
        executable: PathBuf,
        details: String,
    },
    Unsupported {
        installation: Installation,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::NotFound => write!(
                formatter,
                "pi was not found. Install pi or set its path in PI_EXECUTABLE."
            ),
            CheckError::VersionUnreadable {
                executable,
                details,
            } => write!(
                formatter,
                "Could not read the version of {}: {details}",
                executable.display()
            ),
            CheckError::Unsupported { installation } if installation.version < MIN_PI => write!(
                formatter,
                "Installed pi {} ({}) is too old: this app needs pi {MIN_PI} or newer. Update pi, then reopen the app.",
                installation.version,
                installation.executable.display(),
            ),
            CheckError::Unsupported { installation } => write!(
                formatter,
                "Installed pi {} ({}) is a new major version: this app supports pi {}.x. Check for app updates.",
                installation.version,
                installation.executable.display(),
                MIN_PI.major,
            ),
        }
    }
}

/// Finds pi, reads its version and checks that the app supports it.
pub fn check_installation() -> Result<Installation, CheckError> {
    let executable = discover().ok_or(CheckError::NotFound)?;
    let output = run_with_timeout(
        Command::new(&executable)
            .arg("--version")
            .env("PATH", path_with_executable_dir(&executable)),
        None,
        COMMAND_TIMEOUT,
    )
    .map_err(|details| CheckError::VersionUnreadable {
        executable: executable.clone(),
        details,
    })?;

    let version = Version::find_in(&output).ok_or_else(|| CheckError::VersionUnreadable {
        executable: executable.clone(),
        details: format!("unexpected output `{}`", output.trim()),
    })?;

    let installation = Installation {
        executable,
        version,
    };
    if supports(installation.version) {
        Ok(installation)
    } else {
        Err(CheckError::Unsupported { installation })
    }
}

/// Explicit `PI_EXECUTABLE` first, then the general lookup.
fn discover() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PI_EXECUTABLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    find_executable("pi")
}

/// The inherited `PATH`, then the user's login shell. Apps started from Finder
/// do not inherit PATH entries added by shell rc files (nvm, fnm, volta), so
/// the shell lookup is what usually finds `pi` and `node`. Blocking.
///
/// Windows apps inherit the user's PATH, so there is no shell lookup. npm
/// installs `pi` there as an extensionless shell script next to `pi.cmd`, so
/// only files `Command` can start are considered.
#[cfg(windows)]
pub fn find_executable(name: &str) -> Option<PathBuf> {
    const STARTABLE: [&str; 4] = ["com", "exe", "bat", "cmd"];

    let has_startable_extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| STARTABLE.contains(&extension.to_ascii_lowercase().as_str()));
    let file_names: Vec<String> = if has_startable_extension {
        vec![name.to_string()]
    } else {
        STARTABLE
            .iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|directory| file_names.iter().map(move |file| directory.join(file)))
        .find(|candidate| candidate.is_file())
}

#[cfg(not(windows))]
pub fn find_executable(name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    {
        return Some(found);
    }

    // `name` comes from our code or a plugin manifest: only plain names are looked up.
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        return None;
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| {
        OsString::from(if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/bash"
        })
    });
    let output = run_with_timeout(
        Command::new(shell).args(["-ilc", &format!("command -v {name}")]),
        None,
        COMMAND_TIMEOUT,
    )
    .ok()?;

    // Interactive shells may print banners; the answer is the last absolute path.
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('/'))
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

/// pi is usually a `#!/usr/bin/env node` script, so the directory holding the
/// executable (where nvm also puts `node`) has to be on PATH for the child.
pub fn path_with_executable_dir(executable: &Path) -> OsString {
    let mut directories: Vec<PathBuf> = executable
        .parent()
        .map(|directory| vec![directory.to_path_buf()])
        .unwrap_or_default();
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(directories).unwrap_or_default()
}

/// Whether this pi has the one-shot print mode commit message generation
/// needs. The flags are probed, not assumed: the oldest supported pi may lack
/// one of them. Blocking; the caller runs this in the background and caches
/// the answer.
pub fn supports_print_mode(installation: &Installation) -> bool {
    const FLAGS: [&str; 3] = ["--print", "--no-session", "--no-tools"];

    let mut command = Command::new(&installation.executable);
    command
        .arg("--help")
        .env("PATH", path_with_executable_dir(&installation.executable));
    let Ok(help) = run_with_timeout(&mut command, None, COMMAND_TIMEOUT) else {
        return false;
    };
    FLAGS.iter().all(|flag| help.contains(*flag))
}

/// A commit message for `diff`, written by pi in one-shot print mode: no
/// session file, no tools, no extensions, so only the prompt reaches the
/// model. The prompt goes in over stdin, so a large diff cannot hit the
/// Windows command-line limit, and `root` is the working directory, so pi sees
/// the repository the diff came from. Blocking.
pub fn generate_commit_message(
    installation: &Installation,
    root: &Path,
    instructions: &str,
    diff: &str,
    model: Option<&str>,
) -> Result<String, String> {
    let prompt = commit_prompt(instructions, diff);
    let mut command = Command::new(&installation.executable);
    command
        .args([
            "-p",
            "--no-session",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--system-prompt",
            COMMIT_SYSTEM_PROMPT,
        ])
        .current_dir(root)
        .env("PATH", path_with_executable_dir(&installation.executable));
    // An empty setting means pi's own default model.
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        command.args(["--model", model]);
    }
    let output = run_with_timeout(&mut command, Some(prompt.as_bytes()), GENERATION_TIMEOUT)?;
    clean_commit_message(&output)
}

/// The prompt for one generation: the user's instructions, a blank line, then
/// the diff, cut to `MAX_COMMIT_DIFF_CHARS` characters.
fn commit_prompt(instructions: &str, diff: &str) -> String {
    let mut prompt = format!("{instructions}\n\n");
    match diff.char_indices().nth(MAX_COMMIT_DIFF_CHARS) {
        None => prompt.push_str(diff),
        // The byte where the cap falls is a character boundary, so cutting
        // there never splits a multibyte character.
        Some((cut, _)) => {
            prompt.push_str(&diff[..cut]);
            prompt.push_str(&truncation_note());
        }
    }
    prompt
}

/// Tells pi the diff is incomplete, so it does not report the changes that
/// were cut away as missing work.
fn truncation_note() -> String {
    format!("\n\n[the diff was truncated after {MAX_COMMIT_DIFF_CHARS} characters]")
}

/// What pi printed, as the message to hand to git: trimmed, and without a
/// markdown code fence wrapping the whole answer, which models like to add.
fn clean_commit_message(output: &str) -> Result<String, String> {
    let message = strip_code_fence(output.trim()).trim();
    if message.is_empty() {
        return Err("pi returned an empty message".to_string());
    }
    if TOOL_CALL_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
    {
        return Err(
            "the model tried to call a tool instead of writing the message; \
             try again or set another commit message model"
                .to_string(),
        );
    }
    Ok(message.to_string())
}

/// The inside of `text` when a markdown code fence wraps all of it, its
/// opening line carrying an optional language tag; `text` itself otherwise.
fn strip_code_fence(text: &str) -> &str {
    let Some(after_fence) = text.strip_prefix("```") else {
        return text;
    };
    // The language tag sits on the fence line and goes with it.
    let Some((_, body)) = after_fence.split_once('\n') else {
        return text;
    };
    let Some(body) = body.strip_suffix("```") else {
        return text;
    };
    body.trim_end_matches(['\n', '\r'])
}

/// Starts `pi --mode rpc` in `workspace`: a new session, or `session` resumed.
pub fn spawn_pi(
    installation: &Installation,
    workspace: &Path,
    session: Option<&Path>,
    extensions: &[PathBuf],
) -> std::io::Result<(JsonlProcess, mpsc::Receiver<JsonlMessage>)> {
    let mut command = Command::new(&installation.executable);
    command.args(["--mode", "rpc"]);
    if let Some(bridge) = bridge_extension() {
        command.arg("--extension").arg(bridge);
    }
    // Tools that plugins ship for the agent, e.g. one whose calls a plugin draws.
    for extension in extensions {
        command.arg("--extension").arg(extension);
    }
    if let Some(session) = session {
        command.arg("--session").arg(session);
    }
    command
        .current_dir(workspace)
        .env("PATH", path_with_executable_dir(&installation.executable));
    JsonlProcess::spawn(&mut command)
}

/// The app's own extension (session tree navigation, see
/// `extensions/pi-gpui-bridge.ts`). It is embedded in the binary and written
/// to the app's data directory, then loaded with `--extension` for our pi
/// processes only. `None` if it cannot be written; tree navigation is then
/// unavailable but everything else works.
pub fn bridge_extension() -> Option<&'static Path> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        const SOURCE: &str = include_str!("../extensions/pi-gpui-bridge.ts");
        let dir = crate::sessions::app_data_directory()?.join("extensions");
        let path = dir.join("pi-gpui-bridge.ts");
        // Left over from when the app was called Pi GPU UI.
        let _ = std::fs::remove_file(dir.join("pi-gpu-ui-bridge.ts"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(SOURCE) {
            let written =
                std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, SOURCE));
            if let Err(error) = written {
                eprintln!("pi-gpui: could not write the bridge extension: {error}");
                return None;
            }
        }
        Some(path)
    })
    .as_deref()
}

/// Names a session that is not open: starts pi on it just long enough to send
/// `set_session_name`, so pi itself writes the change.
pub async fn rename_session(
    installation: Installation,
    workspace: PathBuf,
    session: PathBuf,
    name: String,
) -> Result<(), String> {
    use futures::StreamExt as _;
    let (process, mut receiver) = spawn_pi(&installation, &workspace, Some(&session), &[])
        .map_err(|error| error.to_string())?;
    process
        .send(serde_json::json!({ "id": "rename", "type": "set_session_name", "name": name }))?;
    let mut result = Err("pi exited before renaming the session".to_string());
    while let Some(message) = receiver.next().await {
        match message {
            JsonlMessage::Record(record)
                if record["type"] == "response" && record["id"] == "rename" =>
            {
                result = if record["success"].as_bool() == Some(true) {
                    Ok(())
                } else {
                    Err(record["error"]
                        .as_str()
                        .unwrap_or("rename failed")
                        .to_string())
                };
                break;
            }
            JsonlMessage::Exited { stderr_tail, .. } => {
                if !stderr_tail.trim().is_empty() {
                    result = Err(stderr_tail.trim().to_string());
                }
                return result;
            }
            _ => {}
        }
    }
    // Closing stdin ends pi; wait for it so the session file is complete.
    drop(process);
    while let Some(message) = receiver.next().await {
        if matches!(message, JsonlMessage::Exited { .. }) {
            break;
        }
    }
    result
}

/// The bridge's command; hidden from the command menu.
pub const NAVIGATE_COMMAND: &str = "pi-gpui-navigate";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_the_minimum_and_newer_minors_of_the_same_major() {
        let version = |text| Version::find_in(text).expect("version");
        assert!(!supports(version("1.3.9")));
        assert!(supports(version("1.4.0")));
        assert!(supports(version("1.4.7")));
        assert!(supports(version("1.9.0")));
        assert!(!supports(version("2.0.0")));
        assert!(!is_untested(version("1.4.7")));
        assert!(!is_untested(version("1.5.2")));
        assert!(is_untested(version("1.6.0")));
    }

    #[test]
    fn version_is_found_inside_noise() {
        assert_eq!(
            Version::find_in("pi v1.4.0\n"),
            Some(Version {
                major: 1,
                minor: 4,
                patch: 0
            })
        );
        assert_eq!(Version::find_in("no version"), None);
    }

    #[test]
    fn the_prompt_is_the_instructions_then_a_blank_line_then_the_diff() {
        assert_eq!(
            commit_prompt("write it short", "diff --git a/x b/x"),
            "write it short\n\ndiff --git a/x b/x"
        );
    }

    #[test]
    fn a_diff_of_exactly_the_limit_is_sent_whole() {
        let diff = "x".repeat(MAX_COMMIT_DIFF_CHARS);
        let prompt = commit_prompt("instructions", &diff);
        assert!(prompt.ends_with(&diff));
        assert!(!prompt.contains(&truncation_note()));
    }

    #[test]
    fn a_diff_over_the_limit_is_cut_and_says_so() {
        let diff = format!("{}the cut-away part", "x".repeat(MAX_COMMIT_DIFF_CHARS));
        let prompt = commit_prompt("instructions", &diff);
        assert!(prompt.contains(&"x".repeat(MAX_COMMIT_DIFF_CHARS)));
        assert!(!prompt.contains("cut-away"));
        assert!(prompt.ends_with(&truncation_note()));
    }

    #[test]
    fn cutting_the_diff_never_splits_a_multibyte_character() {
        // `→` is 3 bytes, so a byte-wise cut would land inside one.
        let diff = "→".repeat(MAX_COMMIT_DIFF_CHARS + 5);
        let prompt = commit_prompt("instructions", &diff);
        let kept = prompt
            .strip_prefix("instructions\n\n")
            .and_then(|rest| rest.strip_suffix(&truncation_note()))
            .expect("the prompt keeps its three parts");
        assert_eq!(kept.chars().count(), MAX_COMMIT_DIFF_CHARS);
        assert!(kept.chars().all(|character| character == '→'));
    }

    #[test]
    fn a_code_fence_wrapping_the_answer_is_removed() {
        assert_eq!(
            clean_commit_message("```\nfix the panel\n```"),
            Ok("fix the panel".to_string())
        );
        // The answer ends with the fence, whatever whitespace follows it.
        assert_eq!(
            clean_commit_message("```\ntext\n```\n"),
            Ok("text".to_string())
        );
    }

    #[test]
    fn a_code_fence_with_a_language_tag_is_removed_with_it() {
        assert_eq!(
            clean_commit_message("```text\n\nfix the panel\n\nand its test\n```"),
            Ok("fix the panel\n\nand its test".to_string())
        );
    }

    #[test]
    fn an_empty_answer_is_reported() {
        assert_eq!(
            clean_commit_message("  \n"),
            Err("pi returned an empty message".to_string())
        );
        assert_eq!(
            clean_commit_message("```\n\n```"),
            Err("pi returned an empty message".to_string())
        );
    }

    #[test]
    fn an_answer_without_a_fence_is_kept_and_trimmed() {
        assert_eq!(
            clean_commit_message("  fix the panel\n"),
            Ok("fix the panel".to_string())
        );
    }

    #[test]
    fn an_unclosed_fence_is_kept_as_it_is() {
        assert_eq!(
            clean_commit_message("```\nfix the panel"),
            Ok("```\nfix the panel".to_string())
        );
    }

    #[test]
    fn a_tool_call_written_as_text_is_an_error() {
        let answer =
            "<\u{ff5c}DSML\u{ff5c}function_calls>\n<\u{ff5c}DSML\u{ff5c}invoke name=\"bash\">";
        assert!(clean_commit_message(answer).is_err());
        assert!(clean_commit_message("<tool_call>{}</tool_call>").is_err());
    }
}
