//! One app per user on Linux and Windows. macOS routes `pi-gpui://` links and
//! reopening to the running app by itself; elsewhere every launch (from the
//! dock, a `.desktop` link handler, the Start menu or a terminal) is a new
//! process. The first one listens; later ones hand it their link and exit.
//!
//! On Linux it listens on a socket in `$XDG_RUNTIME_DIR`. Windows' standard
//! library has no local sockets, so there it listens on a loopback TCP port
//! and writes the port and a random token to a file in the user's temp
//! folder; messages without the token are ignored.

use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::path::PathBuf;

/// Sent when the app is launched again without a link: bring it forward.
pub const ACTIVATE: &str = "activate";

/// Hands `message` to an app that is already running. `true` when it took
/// it, so this process should exit.
pub fn forward(message: &str) -> bool {
    let Some((mut stream, prefix)) = connect() else {
        return false;
    };
    stream
        .write_all(format!("{prefix}{message}\n").as_bytes())
        .is_ok()
}

/// Listens for later launches; each message goes to `deliver` on a
/// background thread.
pub fn listen(deliver: impl Fn(String) + Send + 'static) {
    bind(move |line| {
        let message = line.trim();
        if !message.is_empty() {
            deliver(message.to_string());
        }
    });
}

fn read_line(stream: impl Read) -> Option<String> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    Some(line)
}

/// Development builds keep to themselves, so `cargo run` works while the
/// installed app is open.
fn endpoint_name(extension: &str) -> String {
    let name = if cfg!(debug_assertions) {
        "pi-gpui-dev"
    } else {
        "pi-gpui"
    };
    format!("{name}.{extension}")
}

#[cfg(unix)]
fn connect() -> Option<(std::os::unix::net::UnixStream, &'static str)> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path()?).ok()?;
    Some((stream, ""))
}

#[cfg(unix)]
fn bind(on_line: impl Fn(String) + Send + 'static) {
    use std::os::unix::net::UnixListener;

    let Some(path) = socket_path() else {
        return;
    };
    // `forward` found nobody listening, so a file left here is stale.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("pi-gpui: cannot listen on {}: {error}", path.display());
            return;
        }
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Some(line) = read_line(stream) {
                on_line(line);
            }
        }
    });
}

#[cfg(unix)]
fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())?;
    Some(runtime.join(endpoint_name("sock")))
}

#[cfg(windows)]
fn connect() -> Option<(std::net::TcpStream, String)> {
    let contents = std::fs::read_to_string(instance_file_path()).ok()?;
    let (port, token) = contents.trim().split_once(' ')?;
    let address = (std::net::Ipv4Addr::LOCALHOST, port.parse::<u16>().ok()?);
    let stream = std::net::TcpStream::connect_timeout(
        &address.into(),
        std::time::Duration::from_millis(500),
    )
    .ok()?;
    Some((stream, format!("{token} ")))
}

#[cfg(windows)]
fn bind(on_line: impl Fn(String) + Send + 'static) {
    let path = instance_file_path();
    let listener = match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("pi-gpui: cannot listen on a loopback port: {error}");
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("pi-gpui: cannot read the loopback port: {error}");
            return;
        }
    };
    let token = random_token();
    if let Err(error) = std::fs::write(&path, format!("{port} {token}")) {
        eprintln!("pi-gpui: cannot write {}: {error}", path.display());
        return;
    }
    let prefix = format!("{token} ");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Some(message) =
                read_line(stream).and_then(|line| line.strip_prefix(&prefix).map(str::to_string))
            {
                on_line(message);
            }
        }
    });
}

/// Removes the port file when the app quits, so the next launch does not
/// wait for a connection to a port nobody listens on.
#[cfg(windows)]
pub fn release() {
    let _ = std::fs::remove_file(instance_file_path());
}

#[cfg(windows)]
fn instance_file_path() -> PathBuf {
    std::env::temp_dir().join(endpoint_name("instance"))
}

/// 128 bits from the standard library's randomly seeded hasher keys.
#[cfg(windows)]
fn random_token() -> String {
    use std::hash::{BuildHasher as _, RandomState};

    let half = || RandomState::new().hash_one(std::process::id());
    format!("{:016x}{:016x}", half(), half())
}
