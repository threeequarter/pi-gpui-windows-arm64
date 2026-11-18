//! `@` file completion in the composer, as in pi's terminal UI.
//!
//! `@` at a token boundary opens a menu of workspace paths. `fd` walks the
//! tree (respecting .gitignore) with the same arguments and ranking as the
//! terminal UI; without fd, git's file list or a bounded walk stand in.
//! Directories keep a trailing `/` so completion can drill into them.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::pi::Installation;
use crate::platform::ConsoleWindow as _;

/// One menu entry. `path` is what gets inserted after `@`; directories end
/// with `/`. `name` is the basename shown in the menu's first column.
#[derive(Clone, Debug)]
pub(super) struct Item {
    pub path: String,
    pub name: String,
    pub is_directory: bool,
}

/// The `@token` being typed: its byte range in the composer, the query after
/// `@` (backslashes normalized to `/`), and whether it is `@"quoted"`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Token {
    pub range: Range<usize>,
    pub query: String,
    pub quoted: bool,
}

/// An open completion: the token it answers and the current suggestions.
/// `generation` discards searches that a newer keystroke superseded.
pub(super) struct Menu {
    pub token: Token,
    pub items: Vec<Item>,
    pub generation: u64,
}

fn is_delimiter(character: char) -> bool {
    character.is_whitespace() || matches!(character, '"' | '\'' | '=')
}

/// Opening wrappers that may precede a path in prose, with their closers.
const WRAPPERS: [(char, char); 5] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

fn at_boundary(text: &str, start: usize) -> bool {
    match text[..start].chars().next_back() {
        None => true,
        Some(previous) => {
            is_delimiter(previous) || WRAPPERS.iter().any(|(open, _)| *open == previous)
        }
    }
}

/// The `@token` ending at the cursor, if any. Mirrors the terminal UI:
/// a token after a delimiter (or an unclosed `@"` quote), with unclosed
/// opening wrappers stripped, so `look at (@src/ma` completes `src/ma`.
pub(super) fn typed_token(text: &str, cursor: usize) -> Option<Token> {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &text[..cursor];

    // An unclosed quote swallows spaces: `@"my fi` completes inside quotes.
    let mut quote_start = None;
    let mut in_quotes = false;
    for (index, character) in before.char_indices() {
        if character == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = Some(index);
            }
        }
    }
    if in_quotes {
        let mut start = quote_start?;
        if start > 0 && before.as_bytes()[start - 1] == b'@' {
            start -= 1;
        }
        if !at_boundary(before, start) {
            return None;
        }
        let query = before[start..].strip_prefix("@\"")?;
        return Some(Token {
            range: start..cursor,
            query: query.replace('\\', "/"),
            quoted: true,
        });
    }

    let mut start = before
        .char_indices()
        .rev()
        .find(|(_, character)| is_delimiter(*character))
        .map_or(0, |(index, character)| index + character.len_utf8());
    let mut token = &before[start..];
    // Strip opening wrappers unless the token also contains their closer,
    // e.g. keep `[slug]` but strip the `(` in `(@src`.
    while let Some(open) = token.chars().next() {
        let Some(closer) = WRAPPERS.iter().find(|(o, _)| *o == open).map(|(_, c)| *c) else {
            break;
        };
        if token[open.len_utf8()..].contains(closer) {
            break;
        }
        start += open.len_utf8();
        token = &token[open.len_utf8()..];
    }
    let query = token.strip_prefix('@')?;
    Some(Token {
        range: start..cursor,
        query: query.replace('\\', "/"),
        quoted: false,
    })
}

/// Paths with spaces or quotes are inserted as `@"…"` so the token stays one.
pub(super) fn needs_quotes(path: &str) -> bool {
    path.chars()
        .any(|character| character.is_whitespace() || character == '"')
}

/// The text that replaces the token: `@path`, quoted when needed. The caller
/// adds the trailing space for files; directories stay open for drilling.
pub(super) fn completion_value(token: &Token, item: &Item) -> String {
    let quoted = token.quoted || needs_quotes(&item.path);
    let mut value = String::from("@");
    if quoted {
        value.push('"');
    }
    value.push_str(&item.path);
    if quoted {
        value.push('"');
    }
    value
}

/// fd ships next to pi (the terminal UI downloads it there); also try PATH.
pub(super) fn fd_binary(installation: &Installation) -> Option<PathBuf> {
    let name = if cfg!(windows) { "fd.exe" } else { "fd" };
    let mut candidates = Vec::new();
    if let Some(directory) = installation.executable.parent() {
        candidates.push(directory.join(name));
    }
    if let Some(home) = home() {
        candidates.push(home.join(".pi/agent/bin").join(name));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| crate::pi::find_executable("fd"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Workspace paths matching the query, best 20 first, as in the terminal UI.
/// Runs on a background thread; `fd` may be absent.
pub(super) fn search(workspace: &Path, fd: Option<&Path>, query: &str) -> Vec<Item> {
    let query = query.replace('\\', "/");
    // `@src/ma` searches inside src/ when that directory exists; the display
    // keeps the typed prefix. `~` and absolute prefixes search outside.
    let scoped = scope(workspace, &query);
    let (base_dir, display_base, fd_query) = match scoped {
        Some((base, display, rest)) => (base, display, rest),
        None => (workspace.to_path_buf(), String::new(), query),
    };
    let entries = match fd {
        Some(fd) => {
            let mut entries = walk_fd(fd, &base_dir, &fd_query, Some(1));
            let seen: HashSet<String> = entries.iter().map(|(path, _)| path.clone()).collect();
            entries.extend(
                walk_fd(fd, &base_dir, &fd_query, None)
                    .into_iter()
                    .filter(|(path, _)| !seen.contains(path)),
            );
            entries
        }
        None => fallback_entries(&base_dir),
    };
    rank(entries, &fd_query, &display_base)
}

/// Split `src/ma` into the existing directory `src/` and the query `ma`.
fn scope(workspace: &Path, query: &str) -> Option<(PathBuf, String, String)> {
    let (display_base, rest) = match query.rfind('/') {
        Some(slash) => (&query[..slash + 1], &query[slash + 1..]),
        None if query == "~" => ("~/", ""),
        None => return None,
    };
    let base = if let Some(rest) = display_base.strip_prefix("~/") {
        home()?.join(rest)
    } else if display_base.starts_with('/') {
        PathBuf::from(display_base)
    } else {
        workspace.join(display_base)
    };
    base.is_dir()
        .then(|| (base, display_base.to_string(), rest.to_string()))
}

/// The terminal UI's fd walk: files and directories, hidden included,
/// .gitignore respected, .git excluded, 100 results.
fn walk_fd(fd: &Path, base: &Path, query: &str, max_depth: Option<u32>) -> Vec<(String, bool)> {
    let mut command = Command::new(fd);
    command
        .without_console_window()
        .arg("--base-directory")
        .arg(base)
        .args([
            "--max-results",
            "100",
            "--type",
            "f",
            "--type",
            "d",
            "--follow",
            "--hidden",
        ])
        .args([
            "--exclude",
            ".git",
            "--exclude",
            ".git/*",
            "--exclude",
            ".git/**",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(depth) = max_depth {
        command.args(["--max-depth", &depth.to_string()]);
    }
    if query.contains('/') {
        command.arg("--full-path");
    }
    if !query.is_empty() {
        command.arg(fd_pattern(query));
    }
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let path = line.replace('\\', "/");
            let is_directory = path.ends_with('/');
            let normalized = path.trim_end_matches('/');
            if normalized.is_empty()
                || normalized == ".git"
                || normalized.starts_with(".git/")
                || normalized.contains("/.git/")
            {
                return None;
            }
            Some((path, is_directory))
        })
        .collect()
}

/// The terminal UI's fd regex: escaped segments joined by a separator class,
/// so `src/ma` matches across `/` and `\`.
fn fd_pattern(query: &str) -> String {
    if !query.contains('/') {
        return escape_regex(query);
    }
    let trailing = query.ends_with('/');
    let trimmed = query.trim_matches('/');
    if trimmed.is_empty() {
        return query.to_string();
    }
    let separator = separator_class();
    let mut pattern = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(escape_regex)
        .collect::<Vec<_>>()
        .join(separator);
    if trailing {
        pattern.push_str(separator);
    }
    pattern
}

/// fd unescapes backslashes in the pattern once on Windows, so the class
/// needs doubled backslashes there to arrive at the regex as `[\,/]`.
fn separator_class() -> &'static str {
    if cfg!(windows) {
        "[\\\\\\\\/]"
    } else {
        "[\\\\/]"
    }
}

fn escape_regex(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if ".*+?^${}()|[]\\".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Without fd: git's tracked plus unignored files (folders derived from their
/// paths), or a bounded walk outside a repository.
fn fallback_entries(base: &Path) -> Vec<(String, bool)> {
    let output = Command::new("git")
        .without_console_window()
        .arg("-C")
        .arg(base)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let mut entries = Vec::new();
    if let Some(output) = output.ok().filter(|output| output.status.success()) {
        let mut directories = HashSet::new();
        for file in String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|file| !file.is_empty())
        {
            let file = file.replace('\\', "/");
            entries.push((file.clone(), false));
            let mut parent = Path::new(&file).parent();
            while let Some(directory) = parent {
                let name = directory.to_string_lossy().replace('\\', "/");
                if name.is_empty() || !directories.insert(name.clone()) {
                    break;
                }
                entries.push((format!("{name}/"), true));
                parent = directory.parent();
            }
        }
    } else {
        walk_plain(base, "", &mut entries, 0);
    }
    entries
}

/// Last resort outside git: a capped walk, `.git` the only skipped noise.
fn walk_plain(directory: &Path, prefix: &str, entries: &mut Vec<(String, bool)>, depth: usize) {
    if depth > 6 || entries.len() >= 3000 {
        return;
    }
    let Ok(read) = std::fs::read_dir(directory) else {
        return;
    };
    let mut children: Vec<_> = read.flatten().collect();
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        if entries.len() >= 3000 {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_directory = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let relative = format!("{prefix}{name}");
        entries.push((
            if is_directory {
                format!("{relative}/")
            } else {
                relative.clone()
            },
            is_directory,
        ));
        if is_directory {
            walk_plain(&entry.path(), &format!("{relative}/"), entries, depth + 1);
        }
    }
}

/// The terminal UI's ranking: exact basename 100, prefix 80, substring 50,
/// path substring 30, directories +10; then shallower, shorter, lexical.
fn rank(entries: Vec<(String, bool)>, query: &str, display_base: &str) -> Vec<Item> {
    let lower_query = query.to_lowercase();
    let mut scored: Vec<(u32, String, bool)> = entries
        .into_iter()
        .filter_map(|(path, is_directory)| {
            let score = if query.is_empty() {
                1
            } else {
                score_entry(&path, &lower_query, is_directory)
            };
            (score > 0).then_some((score, path, is_directory))
        })
        .collect();
    scored.sort_by(|(a_score, a_path, _), (b_score, b_path, _)| {
        b_score
            .cmp(a_score)
            .then_with(|| depth(a_path).cmp(&depth(b_path)))
            .then_with(|| a_path.len().cmp(&b_path.len()))
            .then_with(|| a_path.cmp(b_path))
    });
    scored
        .into_iter()
        .take(20)
        .map(|(_, path, is_directory)| {
            let trimmed = path.trim_end_matches('/');
            let display = format!("{display_base}{trimmed}");
            let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
            Item {
                name: if is_directory {
                    format!("{name}/")
                } else {
                    name.to_string()
                },
                path: if is_directory {
                    format!("{display}/")
                } else {
                    display
                },
                is_directory,
            }
        })
        .collect()
}

fn depth(path: &str) -> usize {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

fn score_entry(path: &str, lower_query: &str, is_directory: bool) -> u32 {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let mut score = if name == lower_query {
        100
    } else if name.starts_with(lower_query) {
        80
    } else if name.contains(lower_query) {
        50
    } else if path.to_lowercase().contains(lower_query) {
        30
    } else {
        0
    };
    if is_directory && score > 0 {
        score += 10;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_at_cursor_boundaries() {
        let token = typed_token("@", 1).unwrap();
        assert_eq!((token.range.clone(), token.query.as_str()), (0..1, ""));
        let token = typed_token("look at @src/ma", 15).unwrap();
        assert_eq!(
            (token.range.clone(), token.query.as_str()),
            (8..15, "src/ma")
        );
        // Cursor mid-token completes up to the cursor.
        let token = typed_token("@src/ma rest", 5).unwrap();
        assert_eq!(token.query, "src/");
        assert!(typed_token("mail@example.com", 16).is_none());
        assert!(typed_token("plain text", 10).is_none());
        // Unclosed wrappers are stripped, closed ones kept.
        assert_eq!(typed_token("(@src", 5).unwrap().range, 1..5);
        assert!(typed_token("x(@src", 6).is_none());
        // Backslashes normalize for the search.
        assert_eq!(typed_token("@src\\ui", 7).unwrap().query, "src/ui");
    }

    #[test]
    fn quoted_token_swallows_spaces() {
        let token = typed_token("see @\"my fi", 11).unwrap();
        assert!(token.quoted);
        assert_eq!(
            (token.range.clone(), token.query.as_str()),
            (4..11, "my fi")
        );
        // A closed quote before the cursor is not an open token.
        assert!(typed_token("see \"done\" @x", 12).is_none_or(|t| !t.quoted));
    }

    #[test]
    fn completion_quotes_only_when_needed() {
        let item = Item {
            path: "src/main.rs".into(),
            name: "main.rs".into(),
            is_directory: false,
        };
        let token = typed_token("@ma", 3).unwrap();
        assert_eq!(completion_value(&token, &item), "@src/main.rs");
        let spaced = Item {
            path: "my file.txt".into(),
            name: "my file.txt".into(),
            is_directory: false,
        };
        assert_eq!(completion_value(&token, &spaced), "@\"my file.txt\"");
    }

    #[test]
    fn fd_pattern_joins_segments() {
        assert_eq!(fd_pattern("ma"), "ma");
        assert_eq!(fd_pattern("a.b"), "a\\.b");
        assert_eq!(fd_pattern("src/ma"), format!("src{}ma", separator_class()));
        assert_eq!(fd_pattern("src/"), format!("src{}", separator_class()));
    }

    #[test]
    fn ranking_prefers_exact_then_shallow() {
        let entries = vec![
            ("docs/deep/main.rs".to_string(), false),
            ("main.rs".to_string(), false),
            ("src/".to_string(), true),
            ("maintain.rs".to_string(), false),
        ];
        let items = rank(entries, "main.rs", "");
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        // Exact basename wins; the shallower exact match outranks the deeper.
        // `maintain.rs` does not contain `main.rs` and drops out.
        assert_eq!(paths, ["main.rs", "docs/deep/main.rs"]);
        // Empty query: shallow entries first, directories keep the slash.
        let items = rank(
            vec![
                ("src/ui/".to_string(), true),
                ("LICENSE".to_string(), false),
                ("src/".to_string(), true),
            ],
            "",
            "",
        );
        assert_eq!(items[0].path, "src/");
        assert_eq!(items[0].name, "src/");
        assert_eq!(items[1].path, "LICENSE");
    }

    #[test]
    fn scope_splits_existing_directories() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (base, display, rest) = scope(workspace, "src/ma").unwrap();
        assert_eq!(base, workspace.join("src/"));
        assert_eq!((display.as_str(), rest.as_str()), ("src/", "ma"));
        assert!(scope(workspace, "no-such-dir/ma").is_none());
    }
}
