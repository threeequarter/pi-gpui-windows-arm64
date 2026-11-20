use std::collections::HashMap;
use std::path::Path;

use crate::git::{self, GitError, RepoStatus, StatusEntry};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LineStats {
    pub(super) additions: usize,
    pub(super) deletions: usize,
}

impl LineStats {
    fn add(&mut self, other: Self) {
        self.additions += other.additions;
        self.deletions += other.deletions;
    }
}

#[derive(Clone, Debug)]
pub(super) struct ChangeStats {
    pub(super) files: usize,
    pub(super) lines: LineStats,
    by_path: HashMap<String, LineStats>,
}

impl ChangeStats {
    pub(super) fn read(root: &Path, status: &RepoStatus) -> Result<Self, GitError> {
        // Count both paths of renames like Zed, but keep them paired for file actions.
        let unpaired;
        let entries = if status
            .entries
            .iter()
            .any(|entry| entry.original_path.is_some())
        {
            unpaired = git::status_without_renames(root)?;
            &unpaired.entries
        } else {
            &status.entries
        };
        let by_path = match status.head.as_deref() {
            Some(head) => parse_numstat(&git::git(
                root,
                &[
                    "diff",
                    "--numstat",
                    "--no-renames",
                    "-z",
                    "--no-ext-diff",
                    head,
                    "--",
                ],
            )?),
            None => HashMap::new(),
        };
        let mut lines = LineStats::default();
        for entry in entries.iter().filter(|entry| !entry.untracked) {
            if let Some(stat) = by_path.get(&entry.path) {
                lines.add(*stat);
            }
        }
        Ok(Self {
            files: entries.len(),
            lines,
            by_path,
        })
    }

    pub(super) fn for_entry(&self, entry: &StatusEntry) -> LineStats {
        if entry.untracked {
            return LineStats::default();
        }
        let mut stats = self.by_path.get(&entry.path).copied().unwrap_or_default();
        if entry.code.contains(&'R')
            && let Some(source) = entry
                .original_path
                .as_ref()
                .and_then(|path| self.by_path.get(path))
        {
            stats.add(*source);
        }
        stats
    }
}

fn parse_numstat(output: &str) -> HashMap<String, LineStats> {
    output
        .split_terminator('\0')
        .filter_map(|record| {
            let mut fields = record.splitn(3, '\t');
            let additions = parse_count(fields.next()?)?;
            let deletions = parse_count(fields.next()?)?;
            let path = fields.next()?.to_owned();
            Some((
                path,
                LineStats {
                    additions,
                    deletions,
                },
            ))
        })
        .collect()
}

fn parse_count(count: &str) -> Option<usize> {
    if count == "-" {
        Some(0)
    } else {
        count.parse().ok()
    }
}
