//! Exports Markdown with the installed md-to-pdf CLI. The CLI serves its base
//! directory on all interfaces, so the input and Mermaid config stay outside it.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::mermaid;
use crate::platform::ConsoleWindow as _;

pub(super) fn export(source: &Path, destination: &Path) -> Result<(), String> {
    let scratch = tempfile::Builder::new()
        .prefix("pi-gpui-pdf-")
        .tempdir()
        .map_err(|error| format!("Could not create a workspace for md-to-pdf: {error}"))?;
    let served = scratch.path().join("served");
    fs::create_dir(&served)
        .map_err(|error| format!("Could not create an empty directory for md-to-pdf: {error}"))?;
    let (input, config) = prepare_input(source, scratch.path())
        .map_err(|error| format!("Could not prepare {} for export: {error}", source.display()))?;
    let mut basedir = OsString::from("--basedir=");
    basedir.push(&served);
    let mut command = command()?;
    if let Some(config) = config {
        command.arg("--config-file").arg(config);
    }
    let output = command
        .arg(basedir)
        .arg(r#"--marked-options={"sanitize":true,"silent":true}"#)
        .current_dir(&served)
        .without_console_window()
        .stdin(Stdio::from(input))
        .output()
        .map_err(|error| format!("Could not start md-to-pdf: {error}"))?;
    drop(command);
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
    scratch
        .close()
        .map_err(|error| format!("Could not remove temporary export files: {error}"))?;
    save(destination, &output.stdout)
        .map_err(|error| format!("Could not save {}: {error}", destination.display()))
}

fn prepare_input(source: &Path, scratch: &Path) -> io::Result<(File, Option<PathBuf>)> {
    let source = fs::read(source)?;
    let config = scratch.join("mermaid.cjs");
    let mut input = tempfile::tempfile_in(scratch)?;
    // gray-matter only parses front matter at the very start of stdin. It
    // would otherwise allow the document to configure the converter itself.
    input.write_all(b"\n")?;
    let has_diagrams = if let Ok(markdown) = str::from_utf8(&source) {
        write_markdown(markdown, &mut input, &config)?
    } else {
        input.write_all(&source)?;
        false
    };
    input.rewind()?;
    Ok((input, has_diagrams.then_some(config)))
}

fn write_markdown(source: &str, input: &mut File, config: &Path) -> io::Result<bool> {
    let mut prefix = String::from("pi-gpui-mermaid-");
    while source.contains(&prefix) {
        let padding = prefix.len();
        prefix.extend(std::iter::repeat_n('-', padding));
    }
    let mut renderer = None;
    let mut written = 0;
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut events = Parser::new_ext(source, options).into_offset_iter();
    while let Some((event, range)) = events.next() {
        let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) = event else {
            continue;
        };
        if !info
            .split_whitespace()
            .next()
            .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
        {
            continue;
        }
        let mut diagram = String::new();
        for (event, _) in events.by_ref() {
            match event {
                Event::Text(text) => diagram.push_str(&text),
                Event::End(TagEnd::CodeBlock) => break,
                _ => (),
            }
        }
        let Ok((png, width)) = mermaid::render_png(&diagram) else {
            continue;
        };
        let start = range.start;
        let marker = format!("{prefix}{start}");
        let block = &source[range];
        let opening = block.split(['\r', '\n']).next().unwrap_or(block);
        let fence_len = opening.len() - opening.trim_start_matches(['`', '~']).len();
        input.write_all(&source.as_bytes()[written..start + fence_len])?;
        write!(input, " {marker}")?;
        written = start + opening.len();
        let file = match renderer {
            Some(ref mut file) => file,
            None => {
                let mut file = BufWriter::new(File::create(config)?);
                writeln!(file, "const diagrams = Object.create(null);")?;
                renderer.insert(file)
            }
        };
        let data = STANDARD.encode(png);
        writeln!(
            file,
            r#"diagrams['{marker}'] = '<p><img alt="Mermaid diagram" width="{width}" style="max-width:100%;height:auto" src="data:image/png;base64,{data}"></p>';"#
        )?;
    }
    input.write_all(&source.as_bytes()[written..])?;
    let Some(mut renderer) = renderer else {
        return Ok(false);
    };
    renderer.write_all(
        b"module.exports = { marked_extensions: [{ renderer: { code(_text, info) { return diagrams[info] ?? false; } } }] };\n",
    )?;
    renderer.flush()?;
    Ok(true)
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
