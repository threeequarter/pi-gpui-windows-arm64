//! Short-lived child processes: one command, its output, a timeout. Blocking;
//! call these from a background task, never from the UI.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::platform::ConsoleWindow as _;

/// Runs `command` to completion and returns its stdout. `stdin` is written to
/// the child and closed, so it sees EOF; `None` leaves stdin reading nothing.
/// On a non-zero exit the last stderr line explains the failure (e.g.
/// "env: node: No such file or directory"). A child still running after
/// `timeout` is killed with its whole tree.
pub(crate) fn run_with_timeout(
    command: &mut Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = command
        .without_console_window()
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;

    // Writing on a thread: a prompt bigger than the pipe buffer would
    // otherwise block us before the child had a chance to read it. The copy is
    // what lets the thread own its data. A failed write needs no handling here;
    // a child that did not read its input reports itself through its exit
    // status and stderr below.
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().ok_or("stdin is unavailable")?;
        let bytes = bytes.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
            // Returning drops the pipe, which is the child's EOF.
        });
    }

    let mut stdout = child.stdout.take().ok_or("stdout is unavailable")?;
    let mut stderr = child.stderr.take().ok_or("stderr is unavailable")?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    // Read stderr too, so a chatty child cannot block on a full pipe; its
    // last line explains failures (e.g. "env: node: No such file or directory").
    let error_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader
                    .join()
                    .map_err(|_| "the reader thread panicked".to_string())?
                    .map_err(|error| error.to_string())?;
                let error_text = error_reader.join().unwrap_or_default();
                return if status.success() {
                    Ok(text)
                } else {
                    match error_text
                        .lines()
                        .rev()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                    {
                        Some(line) => Err(line.to_string()),
                        None => Err(format!("exited with {status}")),
                    }
                };
            }
            Ok(None) if started.elapsed() > timeout => {
                // The tree, not just the child: on Windows `pi` runs through
                // `pi.cmd`, so the direct child is `cmd.exe` and `node` would
                // keep running behind it.
                if let Err(error) = kill_tree(&mut child) {
                    return Err(format!("timed out and could not be stopped: {error}"));
                }
                let _ = child.wait();
                return Err(format!("no answer within {} s", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Stops `child`. On Unix the program we spawned is the one doing the work, so
/// killing the child is enough.
#[cfg(not(windows))]
pub(crate) fn kill_tree(child: &mut Child) -> std::io::Result<()> {
    child.kill()
}

/// On Windows pi starts through `pi.cmd`, so the child is `cmd.exe` and pi
/// runs in a `node` process below it, which `Child::kill` would leave behind.
#[cfg(windows)]
pub(crate) fn kill_tree(child: &mut Child) -> std::io::Result<()> {
    let status = Command::new("taskkill")
        .without_console_window()
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        return Ok(());
    }
    child.kill()
}
