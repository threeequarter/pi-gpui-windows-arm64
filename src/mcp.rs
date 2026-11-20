//! MCP server status for the sidebar, from `pi mcp list --json`.
//!
//! Each pi session holds its own MCP connections and exposes no state over
//! RPC, so the app asks pi's shell command instead: it connects to every
//! configured server once, reports, and disconnects. Only user-level servers
//! (`~/.pi/agent/mcp.json`) are listed; the command runs outside any project.

use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::pi::{self, Installation};
use crate::process;

/// Long enough for every server's connect attempt, including pi's retries of
/// transient HTTP errors.
const LIST_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connected,
    NeedsAuth,
    Failed,
    Disabled,
    /// Anything else pi reports, such as `disconnected`.
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub name: String,
    pub state: State,
    /// The first line of the connection error, when not connected.
    pub error: Option<String>,
}

impl Server {
    /// Short text after the name, like pi's own `/mcp` list.
    pub fn summary(&self) -> String {
        match self.state {
            State::Connected => "connected".to_string(),
            State::NeedsAuth => "needs sign-in".to_string(),
            State::Failed => "failed".to_string(),
            State::Disabled => "disabled".to_string(),
            State::Other => "not connected".to_string(),
        }
    }
}

/// Connects to every configured server and reports each. Blocking.
pub fn list(installation: &Installation) -> Result<Vec<Server>, String> {
    let mut command = Command::new(&installation.executable);
    command
        .args(["mcp", "list", "--json"])
        .current_dir(std::env::temp_dir())
        .env(
            "PATH",
            pi::path_with_executable_dir(&installation.executable),
        );
    let output = process::run_to_end(&mut command, None, LIST_TIMEOUT)?;
    // Exit status 1 only means a server is down; the report says which.
    parse(&output.stdout).ok_or_else(|| {
        output
            .stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or_else(
                || format!("unexpected output from pi mcp list ({})", output.status),
                str::to_string,
            )
    })
}

fn parse(stdout: &str) -> Option<Vec<Server>> {
    let report: Value = serde_json::from_str(stdout.trim()).ok()?;
    let servers = report["servers"].as_array()?;
    Some(
        servers
            .iter()
            .filter_map(|server| {
                let name = server["name"].as_str()?.to_string();
                let state = match server["state"].as_str() {
                    Some("connected") => State::Connected,
                    Some("needs-auth") => State::NeedsAuth,
                    Some("failed") => State::Failed,
                    Some("disabled") => State::Disabled,
                    _ => State::Other,
                };
                let error = server["error"]
                    .as_str()
                    .and_then(|error| error.lines().map(str::trim).find(|line| !line.is_empty()))
                    .map(str::to_string);
                Some(Server { name, state, error })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_states_and_first_error_line() {
        let stdout = r#"{
          "servers": [
            { "name": "memory", "state": "connected", "tools": ["a", "b"] },
            { "name": "docs", "state": "failed", "tools": [], "error": "fetch failed\n  at x" },
            { "name": "sentry", "state": "needs-auth", "tools": [] },
            { "name": "off", "state": "disabled", "tools": [] }
          ],
          "errors": []
        }"#;
        let servers = parse(stdout).unwrap();
        assert_eq!(servers.len(), 4);
        assert_eq!(servers[0].state, State::Connected);
        assert_eq!(servers[0].summary(), "connected");
        assert_eq!(servers[1].state, State::Failed);
        assert_eq!(servers[1].error.as_deref(), Some("fetch failed"));
        assert_eq!(servers[2].summary(), "needs sign-in");
        assert_eq!(servers[3].state, State::Disabled);
    }

    #[test]
    fn rejects_non_json_output() {
        assert!(parse("Error: unknown command mcp").is_none());
        assert!(parse(r#"{"errors": []}"#).is_none());
    }
}
