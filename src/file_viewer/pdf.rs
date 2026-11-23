//! Exports Markdown with the installed md-to-pdf CLI. The CLI serves its base
//! directory on all interfaces, so it gets an empty folder, not the workspace.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Seek as _, Write as _};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::platform::ConsoleWindow as _;

pub(super) fn export(source: &Path, destination: &Path) -> Result<(), String> {
    let scratch = tempfile::tempdir()
        .map_err(|error| format!("Could not create a workspace for md-to-pdf: {error}"))?;
    let input = prepare_input(source)
        .map_err(|error| format!("Could not prepare {} for export: {error}", source.display()))?;
    let mut basedir = OsString::from("--basedir=");
    basedir.push(scratch.path());
    let output = command()?
        .arg(basedir)
        .arg(r#"--marked-options={"sanitize":true,"silent":true}"#)
        .current_dir(scratch.path())
        .without_console_window()
        .stdin(Stdio::from(input))
        .output()
        .map_err(|error| format!("Could not start md-to-pdf: {error}"))?;
    if !output.status.success() || !output.stdout.starts_with(b"%PDF-") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail: String = stderr.trim().chars().take(600).collect();
        let reason = if output.status.success() {
            "md-to-pdf did not produce a PDF".into()
        } else {
            format!("md-to-pdf exited with {}", output.status)
        };
        return Err(if detail.is_empty() {
            reason
        } else {
            format!("{reason}: {detail}")
        });
    }
    save(destination, &output.stdout)
        .map_err(|error| format!("Could not save {}: {error}", destination.display()))
}

fn prepare_input(source: &Path) -> io::Result<File> {
    let mut source = File::open(source)?;
    let mut input = tempfile::tempfile()?;
    // gray-matter only parses front matter at the very start of stdin. It
    // would otherwise allow the document to configure the converter itself.
    input.write_all(b"\n")?;
    io::copy(&mut source, &mut input)?;
    input.rewind()?;
    Ok(input)
}

fn save(destination: &Path, pdf: &[u8]) -> io::Result<()> {
    let directory = destination.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(pdf)?;
    staged.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(windows)]
fn command() -> Result<Command, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let node = std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("node.exe"))
        .find(|node| node.is_file())
        .ok_or("Node.js is not on PATH; md-to-pdf requires node.exe.")?;
    let npm = std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("npm"));
    let entry = npm
        .into_iter()
        .chain(std::env::split_paths(&path))
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("node_modules/md-to-pdf/dist/cli.js"))
        .find(|script| script.is_file())
        .ok_or("md-to-pdf was not found. Install it with npm install -g md-to-pdf.")?;
    // npm's Windows shim is a batch file; call the installed CLI without cmd.
    let mut command = Command::new(node);
    command.arg(entry);
    Ok(command)
}

#[cfg(not(windows))]
fn command() -> Result<Command, String> {
    Ok(Command::new("md-to-pdf"))
}
