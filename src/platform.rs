//! Platform pieces gpui 0.2.2 does not provide: AppKit calls on macOS, the
//! freedesktop equivalents on Linux, Explorer and the Recycle Bin on Windows.

// `objc` 0.2 macros probe a `cargo-clippy` feature that modern rustc flags.
#![allow(unexpected_cfgs)]

use gpui::Window;

/// Console programs we start (git, pi, PowerShell) would each open a console
/// window on Windows, where the release build is a GUI app without a console
/// to share. A no-op elsewhere.
pub trait ConsoleWindow {
    fn without_console_window(&mut self) -> &mut Self;
}

impl ConsoleWindow for std::process::Command {
    #[cfg(windows)]
    fn without_console_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt as _;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(windows))]
    fn without_console_window(&mut self) -> &mut Self {
        self
    }
}

/// Starts dragging the window from our custom title bar.
///
/// Call it on the first mouse move after a mouse down in the title bar, like
/// Zed does. Newer gpui implements this in `Window::start_window_move` with
/// `-[NSWindow performWindowDragWithEvent:]`; in 0.2.2 that method is a no-op
/// on macOS, so we send the same message ourselves.
#[cfg(target_os = "macos")]
pub fn start_window_drag(window: &Window) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `Window` also has an inherent `window_handle()`; ask the raw-window-handle trait.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    let view = appkit.ns_view.as_ptr() as *mut Object;

    // SAFETY: `view` is gpui's live NSView for this window (we are inside one of
    // its event handlers, on the main thread). All messages are standard AppKit
    // API, and null results are checked before use.
    unsafe {
        let ns_window: *mut Object = msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let event: *mut Object = msg_send![app, currentEvent];
        if event.is_null() {
            return;
        }
        let _: () = msg_send![ns_window, performWindowDragWithEvent: event];
    }
}

/// On Windows, gpui 0.2.2's `start_window_move` is a no-op, and the native
/// caption drag never starts on its own: the focusable root consumes the
/// caption mouse down (`prevent_default`), so `DefWindowProc` never sees the
/// `HTCAPTION` press. Hand the drag to the system move loop ourselves.
///
/// The message must be posted, not sent: the move loop is modal and keeps
/// dispatching `WM_TIMER`/`WM_PAINT`, which gpui answers by re-entering the
/// app. Running it from inside this listener, while gpui holds the app and the
/// window's input callback, panics across the window procedure and aborts.
#[cfg(windows)]
pub fn start_window_drag(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetCursorPos(point: *mut CursorPoint) -> i32;
        fn ReleaseCapture() -> i32;
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    }
    #[repr(C)]
    struct CursorPoint {
        x: i32,
        y: i32,
    }
    const WM_SYSCOMMAND: u32 = 0x0112;
    const SC_MOVE: usize = 0xF010;
    const HTCAPTION: usize = 2;

    // `Window` also has an inherent `window_handle()`; ask the raw-window-handle trait.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let mut point = CursorPoint { x: 0, y: 0 };
    // SAFETY: `hwnd` is gpui's live window handle. GetCursorPos writes to a
    // valid local. SC_MOVE | HTCAPTION is what DefWindowProc itself sends for a
    // caption drag; gpui passes WM_SYSCOMMAND through to DefWindowProc once the
    // main loop dispatches it, and the move loop runs until the button is up.
    unsafe {
        GetCursorPos(&mut point);
        ReleaseCapture();
        let position = (point.x as u16 as isize) | ((point.y as u16 as isize) << 16);
        PostMessageW(
            win32.hwnd.get(),
            WM_SYSCOMMAND,
            SC_MOVE | HTCAPTION,
            position,
        );
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn start_window_drag(window: &Window) {
    window.start_window_move();
}

/// Maximizes the window, or restores it when it is maximized. On Windows,
/// gpui 0.2.2's `zoom_window` only maximizes, so we restore through Win32.
#[cfg(windows)]
pub fn toggle_maximized(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn ShowWindowAsync(hwnd: isize, command: i32) -> i32;
    }
    const SW_RESTORE: i32 = 9;

    if !window.is_maximized() {
        window.zoom_window();
        return;
    }
    // `Window` also has an inherent `window_handle()`; ask the raw-window-handle trait.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    // SAFETY: `hwnd` is gpui's live window handle; ShowWindowAsync only posts
    // a show command to it.
    unsafe { ShowWindowAsync(win32.hwnd.get(), SW_RESTORE) };
}

#[cfg(not(windows))]
pub fn toggle_maximized(window: &Window) {
    window.zoom_window();
}

/// A window's Win32 handle, kept for calls that must run outside gpui updates.
#[cfg(windows)]
#[derive(Clone, Copy)]
pub struct NativeWindow(isize);

#[cfg(windows)]
pub fn native_window(window: &Window) -> Option<NativeWindow> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `Window` also has an inherent `window_handle()`; ask the raw-window-handle trait.
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    Some(NativeWindow(win32.hwnd.get()))
}

/// Centers `window` on the work area of the monitor `anchor` is on, `size`
/// logical pixels large but never larger than that work area.
///
/// gpui 0.2.2 converts the bounds of a new window with the DPI of the
/// monitor Windows first creates it on (the primary one), not of the target
/// display, so on a second monitor with another scale the window lands off
/// center or partly off screen. Here the rectangle is computed in physical
/// pixels of the anchor's monitor instead.
///
/// Call it outside gpui updates: `SetWindowPos` sends `WM_MOVE`, `WM_SIZE`
/// and `WM_DPICHANGED` synchronously, and gpui answers them by re-entering
/// the app.
#[cfg(windows)]
pub fn center_on_monitor_of(
    window: NativeWindow,
    anchor: NativeWindow,
    size: gpui::Size<gpui::Pixels>,
) {
    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        _monitor: Rect,
        work: Rect,
        _flags: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
        fn GetDpiForWindow(hwnd: isize) -> u32;
        fn SetWindowPos(
            hwnd: isize,
            after: isize,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> i32;
    }
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const DEFAULT_DPI: f32 = 96.;

    // SAFETY: both handles are gpui's live windows; the calls only read
    // monitor data into valid locals and move `window`.
    unsafe {
        let monitor = MonitorFromWindow(anchor.0, MONITOR_DEFAULTTONEAREST);
        let mut info = MonitorInfo {
            size: std::mem::size_of::<MonitorInfo>() as u32,
            ..MonitorInfo::default()
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        // 0 when the hub window is already gone.
        let dpi = GetDpiForWindow(anchor.0);
        if dpi == 0 {
            return;
        }
        let scale = dpi as f32 / DEFAULT_DPI;
        let work = info.work;
        let width = ((f32::from(size.width) * scale) as i32).min(work.right - work.left);
        let height = ((f32::from(size.height) * scale) as i32).min(work.bottom - work.top);
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.top + (work.bottom - work.top - height) / 2;
        // Crossing to a monitor with another DPI makes gpui apply the size
        // Windows suggests for it; the second call restores the exact
        // rectangle once the window is on the target monitor.
        for _ in 0..2 {
            SetWindowPos(
                window.0,
                0,
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Tells macOS whether the app looks dark or light, so system-drawn parts
/// (traffic lights, menus, scroll bars, permission sheets) and other apps
/// asking for our appearance match the theme instead of the system setting.
#[cfg(target_os = "macos")]
pub fn set_app_appearance(dark: bool) {
    use std::ffi::CString;

    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    let name = if dark {
        "NSAppearanceNameDarkAqua"
    } else {
        "NSAppearanceNameAqua"
    };
    let Ok(name) = CString::new(name) else {
        return;
    };
    // SAFETY: called on the main thread after AppKit finished launching; all
    // messages are standard AppKit API and null results are checked.
    unsafe {
        let name: *mut Object = msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
        let appearance: *mut Object = msg_send![class!(NSAppearance), appearanceNamed: name];
        if appearance.is_null() {
            return;
        }
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setAppearance: appearance];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_app_appearance(_dark: bool) {}

/// Moves a file to the Trash, so a deleted session can still be restored.
#[cfg(target_os = "macos")]
pub fn move_to_trash(path: &std::path::Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    use objc::runtime::{NO, Object};
    use objc::{class, msg_send, sel, sel_impl};

    let path_c = CString::new(path.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    // SAFETY: standard Foundation calls on the main thread; the out-pointers
    // are valid for the duration of the call and results are checked.
    unsafe {
        let string: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: path_c.as_ptr()];
        let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: string];
        let manager: *mut Object = msg_send![class!(NSFileManager), defaultManager];
        let mut error: *mut Object = std::ptr::null_mut();
        let ok: objc::runtime::BOOL = msg_send![
            manager,
            trashItemAtURL: url
            resultingItemURL: std::ptr::null_mut::<*mut Object>()
            error: &mut error
        ];
        if ok != NO {
            return Ok(());
        }
        if error.is_null() {
            return Err("the file could not be moved to the Trash".to_string());
        }
        let description: *mut Object = msg_send![error, localizedDescription];
        let utf8: *const std::os::raw::c_char = msg_send![description, UTF8String];
        Err(std::ffi::CStr::from_ptr(utf8)
            .to_string_lossy()
            .into_owned())
    }
}

/// `gio trash` (GLib, on GNOME and most other desktops), then `kioclient`
/// (KDE), then `trash-put` (trash-cli). Never deletes for good: without any of
/// them the file stays where it is.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn move_to_trash(path: &std::path::Path) -> Result<(), String> {
    use std::process::Command;

    let attempts: [(&str, &[&str]); 3] = [
        ("gio", &["trash", "--"]),
        ("kioclient", &["move"]),
        ("trash-put", &["--"]),
    ];
    for (program, args) in attempts {
        let mut command = Command::new(program);
        command.args(args).arg(path);
        if program == "kioclient" {
            command.arg("trash:/");
        }
        match command.output() {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(if message.is_empty() {
                    "the file could not be moved to the Trash".to_string()
                } else {
                    message
                });
            }
            Err(_) => continue,
        }
    }
    Err("no trash tool found (install gio, kioclient or trash-cli)".to_string())
}

/// Moves a file or folder to the Recycle Bin through .NET's `FileSystem`,
/// which every Windows PowerShell has. The path goes in an environment
/// variable, so it is never parsed as PowerShell code.
#[cfg(windows)]
pub fn move_to_trash(path: &std::path::Path) -> Result<(), String> {
    const SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
        Add-Type -AssemblyName Microsoft.VisualBasic; \
        $path = $env:PI_GPUI_TRASH_PATH; \
        if (Test-Path -LiteralPath $path -PathType Container) { \
        [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory(\
        $path, 'OnlyErrorDialogs', 'SendToRecycleBin') } else { \
        [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile(\
        $path, 'OnlyErrorDialogs', 'SendToRecycleBin') }";

    let output = std::process::Command::new("powershell")
        .without_console_window()
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("PI_GPUI_TRASH_PATH", path)
        .output()
        .map_err(|error| format!("could not run PowerShell: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if message.is_empty() {
        "the file could not be moved to the Recycle Bin".to_string()
    } else {
        message
    })
}

/// Shows the file in Finder, or in the file manager on Linux.
#[cfg(not(windows))]
pub fn reveal_in_file_manager(path: &std::path::Path) {
    use std::process::Command;

    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn().map(drop)
    } else {
        // The freedesktop FileManager1 interface selects the file (Nautilus,
        // Dolphin, Nemo, Thunar…); otherwise just open its folder.
        let uri = file_uri(path);
        let selected = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.freedesktop.FileManager1",
                "--object-path",
                "/org/freedesktop/FileManager1",
                "--method",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("['{}']", uri.replace('\'', "%27")))
            .arg("")
            .output()
            .is_ok_and(|output| output.status.success());
        if selected {
            Ok(())
        } else {
            let folder = path.parent().unwrap_or(path);
            Command::new("xdg-open").arg(folder).spawn().map(drop)
        }
    };
    if let Err(error) = result {
        eprintln!("pi-gpui: could not reveal {}: {error}", path.display());
    }
}

/// Opens Explorer with the file selected. Explorer wants `/select,"path"` as
/// one argument with the quotes inside, which the standard quoting cannot
/// produce.
#[cfg(windows)]
pub fn reveal_in_file_manager(path: &std::path::Path) {
    use std::os::windows::process::CommandExt as _;

    let result = std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
    if let Err(error) = result {
        eprintln!("pi-gpui: could not reveal {}: {error}", path.display());
    }
}

/// Opens a file in Zed when it is installed, otherwise in the default editor.
pub fn open_in_editor(path: &std::path::Path) {
    use std::process::Command;

    let result = if cfg!(target_os = "macos") {
        let zed = std::path::Path::new("/Applications/Zed.app");
        let mut command = Command::new("open");
        if zed.exists() {
            command.arg("-a").arg(zed);
        } else {
            command.arg("-t");
        }
        command.arg(path).spawn().map(drop)
    } else {
        // Zed's CLI is `zed`, or `zeditor` in some distributions' packages.
        // Explorer opens a file with the app Windows associates with it.
        let fallback = if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        match ["zed", "zeditor"]
            .into_iter()
            .find(|program| on_path(program))
        {
            Some(zed) => Command::new(zed)
                .without_console_window()
                .arg(path)
                .spawn()
                .map(drop),
            None => Command::new(fallback).arg(path).spawn().map(drop),
        }
    };
    if let Err(error) = result {
        eprintln!("pi-gpui: could not open {}: {error}", path.display());
    }
}

/// Opens a file with the app the system associates with it, like a
/// double-click in the file manager.
#[cfg(windows)]
pub fn open_with_default_app(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            verb: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut std::ffi::c_void, concurrency: u32) -> i32;
    }
    const SW_SHOWNORMAL: i32 = 1;
    const SUCCESS_ABOVE: isize = 32;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let display = path.display().to_string();
    // ShellExecuteW can pump messages (DDE, COM activation, open-with UI);
    // inside a gpui listener that re-enters the app and aborts, like the
    // native move loop. Run it on its own STA thread instead.
    std::thread::spawn(move || {
        // SAFETY: the wide strings are null-terminated and live for the
        // call; no window handle is needed. ShellExecuteW reports success
        // above 32.
        unsafe {
            CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
            let result = ShellExecuteW(
                0,
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
            if result <= SUCCESS_ABOVE {
                eprintln!("pi-gpui: could not open {display} with the default app (code {result})");
            }
        }
    });
}

#[cfg(not(windows))]
pub fn open_with_default_app(path: &std::path::Path) {
    use std::process::Command;

    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg(path).spawn().map(drop)
    } else {
        Command::new("xdg-open").arg(path).spawn().map(drop)
    };
    if let Err(error) = result {
        eprintln!("pi-gpui: could not open {}: {error}", path.display());
    }
}

/// Opens the system's terminal in `directory`: Windows Terminal (its default
/// profile) or a console with the command shell on Windows, Terminal on
/// macOS, and on Linux the first terminal found, starting with the desktop's
/// choice (`xdg-terminal-exec`, `$TERMINAL`, Debian's `x-terminal-emulator`).
pub fn open_terminal(directory: &std::path::Path) {
    use std::process::Command;

    let result = if cfg!(target_os = "macos") {
        Command::new("open")
            .args(["-a", "Terminal"])
            .arg(directory)
            .spawn()
            .map(drop)
    } else if cfg!(windows) {
        Command::new("wt")
            .arg("-d")
            .arg(directory)
            .spawn()
            .or_else(|_| {
                let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
                let mut command = Command::new(shell);
                command.current_dir(directory);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt as _;
                    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
                    command.creation_flags(CREATE_NEW_CONSOLE);
                }
                command.spawn()
            })
            .map(drop)
    } else {
        // Terminals open in the working directory they are started in.
        let preferred = std::env::var("TERMINAL")
            .ok()
            .filter(|name| !name.is_empty());
        let candidates = ["xdg-terminal-exec"]
            .into_iter()
            .map(str::to_string)
            .chain(preferred)
            .chain(
                [
                    "x-terminal-emulator",
                    "gnome-terminal",
                    "konsole",
                    "xfce4-terminal",
                    "kitty",
                    "alacritty",
                    "wezterm",
                    "foot",
                    "xterm",
                ]
                .map(str::to_string),
            );
        let mut result = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        for program in candidates {
            result = Command::new(&program)
                .current_dir(directory)
                .spawn()
                .map(drop);
            if result.is_ok() {
                break;
            }
        }
        result
    };
    if let Err(error) = result {
        eprintln!(
            "pi-gpui: could not open a terminal in {}: {error}",
            directory.display()
        );
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            directory
                .join(format!("{program}{}", std::env::consts::EXE_SUFFIX))
                .is_file()
        })
    })
}

/// `file://` URI with the bytes that need it percent-encoded.
#[cfg(not(windows))]
fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// Files on the clipboard, and whether they were cut rather than copied.
pub struct FileClipboard {
    pub paths: Vec<std::path::PathBuf>,
    pub cut: bool,
}

pub fn clipboard_file_paths() -> Vec<std::path::PathBuf> {
    read_file_clipboard()
        .map(|clipboard| clipboard.paths)
        .unwrap_or_default()
}

/// A private pasteboard type marking our own Cut, so a later Paste moves.
/// Finder ignores it; Finder's own Cut is not readable, so it pastes as copy.
#[cfg(target_os = "macos")]
const CUT_MARKER: &str = "application/x-pi-gpui-cut";

/// Files copied in Finder (file URLs on the general pasteboard).
#[cfg(target_os = "macos")]
pub fn read_file_clipboard() -> Option<FileClipboard> {
    use std::ffi::{CStr, CString};

    use objc::runtime::{Class, Object, YES};
    use objc::{class, msg_send, sel, sel_impl};

    let Ok(key) = CString::new("NSPasteboardURLReadingFileURLsOnlyKey") else {
        return None;
    };
    let mut paths = Vec::new();
    // SAFETY: standard AppKit/Foundation calls on the main thread; every
    // returned object is checked for null before use.
    unsafe {
        let pasteboard: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pasteboard.is_null() {
            return None;
        }
        let url_class: *const Class = class!(NSURL);
        let classes: *mut Object = msg_send![class!(NSArray), arrayWithObject: url_class];
        let key: *mut Object = msg_send![class!(NSString), stringWithUTF8String: key.as_ptr()];
        let yes: *mut Object = msg_send![class!(NSNumber), numberWithBool: YES];
        let options: *mut Object =
            msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: key];
        let urls: *mut Object =
            msg_send![pasteboard, readObjectsForClasses: classes options: options];
        if urls.is_null() {
            return None;
        }
        let count: usize = msg_send![urls, count];
        for index in 0..count {
            let url: *mut Object = msg_send![urls, objectAtIndex: index];
            let path: *mut Object = msg_send![url, path];
            if path.is_null() {
                continue;
            }
            let utf8: *const std::os::raw::c_char = msg_send![path, UTF8String];
            if !utf8.is_null() {
                paths.push(std::path::PathBuf::from(
                    CStr::from_ptr(utf8).to_string_lossy().into_owned(),
                ));
            }
        }
        let Ok(marker) = CString::new(CUT_MARKER) else {
            return Some(FileClipboard { paths, cut: false });
        };
        let marker: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: marker.as_ptr()];
        let cut: *mut Object = msg_send![pasteboard, stringForType: marker];
        Some(FileClipboard {
            paths,
            cut: !cut.is_null(),
        })
    }
}

/// File URLs for Finder, plus the private cut marker for our own Paste.
#[cfg(target_os = "macos")]
pub fn write_file_clipboard(paths: &[std::path::PathBuf], cut: bool) {
    use std::ffi::CString;

    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    // SAFETY: standard AppKit calls on the main thread; every returned
    // object is checked for null before use.
    unsafe {
        let pasteboard: *mut Object = msg_send![class!(NSPasteboard), generalPasteboard];
        if pasteboard.is_null() {
            return;
        }
        let _: () = msg_send![pasteboard, clearContents];
        let urls: *mut Object = msg_send![class!(NSMutableArray), array];
        for path in paths {
            let Ok(text) = CString::new(path.to_string_lossy().as_bytes()) else {
                continue;
            };
            let string: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()];
            if string.is_null() {
                continue;
            }
            let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: string];
            if url.is_null() {
                continue;
            }
            let _: () = msg_send![urls, addObject: url];
        }
        let _: objc::runtime::BOOL = msg_send![pasteboard, writeObjects: urls];
        if !cut {
            return;
        }
        let Ok(key) = CString::new(CUT_MARKER) else {
            return;
        };
        let marker: *mut Object = msg_send![class!(NSString), stringWithUTF8String: key.as_ptr()];
        let value: *mut Object = msg_send![class!(NSString), stringWithUTF8String: c"1".as_ptr()];
        let _: () = msg_send![pasteboard, setString: value forType: marker];
    }
}

/// Files copied or cut in Explorer (CF_HDROP); the Preferred DropEffect
/// format tells a cut from a copy, the same way Explorer does.
#[cfg(windows)]
pub fn read_file_clipboard() -> Option<FileClipboard> {
    use std::os::windows::ffi::OsStringExt as _;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(format: u32) -> isize;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn DragQueryFileW(drop: isize, index: u32, buffer: *mut u16, len: u32) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalLock(handle: isize) -> *mut std::ffi::c_void;
        fn GlobalUnlock(handle: isize) -> i32;
    }
    const CF_HDROP: u32 = 15;
    const DROPEFFECT_MOVE: u32 = 2;

    // SAFETY: the clipboard stays open for the reads and is closed on every
    // path; the handles GetClipboardData returns remain owned by the
    // clipboard and are only read while it is open.
    unsafe {
        if OpenClipboard(0) == 0 {
            return None;
        }
        let drop = GetClipboardData(CF_HDROP);
        if drop == 0 {
            CloseClipboard();
            return None;
        }
        let count = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
        let mut paths = Vec::new();
        for index in 0..count {
            let len = DragQueryFileW(drop, index, std::ptr::null_mut(), 0);
            let mut buffer = vec![0u16; len as usize + 1];
            DragQueryFileW(drop, index, buffer.as_mut_ptr(), len + 1);
            paths.push(std::path::PathBuf::from(std::ffi::OsString::from_wide(
                &buffer[..len as usize],
            )));
        }
        let name: Vec<u16> = "Preferred DropEffect"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let format = RegisterClipboardFormatW(name.as_ptr());
        let mut cut = false;
        let effect = GetClipboardData(format);
        if effect != 0 {
            let value = GlobalLock(effect) as *mut u32;
            if !value.is_null() {
                cut = *value & DROPEFFECT_MOVE != 0;
                GlobalUnlock(effect);
            }
        }
        CloseClipboard();
        Some(FileClipboard { paths, cut })
    }
}

/// CF_HDROP for Explorer and the drop effect for cut, so a paste in Explorer
/// or in the Files panel moves instead of copying.
#[cfg(windows)]
pub fn write_file_clipboard(paths: &[std::path::PathBuf], cut: bool) {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd: isize) -> i32;
        fn EmptyClipboard() -> i32;
        fn CloseClipboard() -> i32;
        fn SetClipboardData(format: u32, handle: isize) -> isize;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, size: usize) -> isize;
        fn GlobalLock(handle: isize) -> *mut std::ffi::c_void;
        fn GlobalUnlock(handle: isize) -> i32;
        fn GlobalFree(handle: isize) -> isize;
    }
    const CF_HDROP: u32 = 15;
    const GMEM_MOVEABLE: u32 = 0x0002;
    const GMEM_ZEROINIT: u32 = 0x0040;
    const DROPEFFECT_COPY: u32 = 1;
    const DROPEFFECT_MOVE: u32 = 2;
    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        point: [i32; 2],
        f_nc: i32,
        f_wide: i32,
    }

    let mut list: Vec<u16> = Vec::new();
    for path in paths {
        list.extend(path.as_os_str().encode_wide().chain(Some(0)));
    }
    list.push(0);
    let header_size = std::mem::size_of::<DropFiles>();
    // SAFETY: the globals are filled through GlobalLock and handed to the
    // clipboard, which owns them once SetClipboardData succeeds; a failed
    // set frees them. The clipboard is closed on every path.
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, header_size + list.len() * 2);
        if handle == 0 {
            return;
        }
        let memory = GlobalLock(handle) as *mut u8;
        if memory.is_null() {
            GlobalFree(handle);
            return;
        }
        let header = DropFiles {
            p_files: header_size as u32,
            point: [0, 0],
            f_nc: 0,
            f_wide: 1,
        };
        std::ptr::copy_nonoverlapping(
            &header as *const DropFiles as *const u8,
            memory,
            header_size,
        );
        std::ptr::copy_nonoverlapping(
            list.as_ptr() as *const u8,
            memory.add(header_size),
            list.len() * 2,
        );
        GlobalUnlock(handle);
        if OpenClipboard(0) == 0 {
            GlobalFree(handle);
            return;
        }
        EmptyClipboard();
        if SetClipboardData(CF_HDROP, handle) == 0 {
            GlobalFree(handle);
        }
        let name: Vec<u16> = "Preferred DropEffect"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let format = RegisterClipboardFormatW(name.as_ptr());
        let effect = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, 4);
        if effect != 0 {
            let value = GlobalLock(effect) as *mut u32;
            if !value.is_null() {
                *value = if cut {
                    DROPEFFECT_MOVE
                } else {
                    DROPEFFECT_COPY
                };
                GlobalUnlock(effect);
            }
            if SetClipboardData(format, effect) == 0 {
                GlobalFree(effect);
            }
        }
        CloseClipboard();
    }
}

/// The GNOME copied-files format carries the operation (copy/cut) above the
/// URIs; plain text/uri-list is the fallback other desktops write.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn read_file_clipboard() -> Option<FileClipboard> {
    const GNOME: &str = "x-special/gnome-copied-files";
    const URI_LIST: &str = "text/uri-list";
    let attempts = [
        ("wl-paste", vec!["--type", GNOME], true),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", GNOME, "-o"],
            true,
        ),
        ("wl-paste", vec!["--type", URI_LIST], false),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", URI_LIST, "-o"],
            false,
        ),
    ];
    for (program, arguments, gnome) in attempts {
        let Ok(output) = std::process::Command::new(program)
            .args(&arguments)
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        let cut = gnome && lines.next() == Some("cut");
        let paths: Vec<std::path::PathBuf> = lines.filter_map(parse_file_uri).collect();
        if !paths.is_empty() {
            return Some(FileClipboard { paths, cut });
        }
    }
    None
}

/// One clipboard target only: wl-copy and xclip each own the selection with
/// a single format, so the GNOME format wins for its copy/cut line.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn write_file_clipboard(paths: &[std::path::PathBuf], cut: bool) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut content = String::from(if cut { "cut\n" } else { "copy\n" });
    for path in paths {
        content.push_str(&file_uri(path));
        content.push('\n');
    }
    let attempts: [(&str, &[&str]); 2] = [
        ("wl-copy", &["--type", "x-special/gnome-copied-files"]),
        (
            "xclip",
            &[
                "-selection",
                "clipboard",
                "-t",
                "x-special/gnome-copied-files",
                "-i",
            ],
        ),
    ];
    for (program, arguments) in attempts {
        let spawned = Command::new(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(content.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }
    eprintln!("pi-gpui: no clipboard tool found (install wl-copy or xclip)");
}

#[cfg(not(any(target_os = "macos", windows)))]
fn parse_file_uri(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::new();
    let mut characters = rest.bytes();
    while let Some(byte) = characters.next() {
        if byte == b'%' {
            let hex = [characters.next()?, characters.next()?];
            let hex = std::str::from_utf8(&hex).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    Some(std::path::PathBuf::from(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

/// Asks for one or more folders with the system open panel.
///
/// gpui builds a new `NSOpenPanel` for every prompt, and building one blocks
/// the main thread while AppKit connects to its out-of-process panel service
/// (about half a second the first time, 150 ms after that). We build the panel
/// once and reuse it, so only the first prompt pays that cost.
///
/// Must not be polled while gpui is mid-update: call it from a spawned task.
#[cfg(target_os = "macos")]
pub async fn pick_folders(prompt: &str) -> Result<Option<Vec<std::path::PathBuf>>, String> {
    use std::cell::{Cell, RefCell};
    use std::ffi::{CStr, CString};

    use block::ConcreteBlock;
    use futures::channel::oneshot;
    use objc::runtime::{NO, Object, YES};
    use objc::{class, msg_send, sel, sel_impl};

    thread_local! {
        static PANEL: Cell<*mut Object> = const { Cell::new(std::ptr::null_mut()) };
    }
    const MODAL_RESPONSE_OK: isize = 1;

    let prompt = CString::new(prompt).map_err(|error| error.to_string())?;
    let (done_tx, done_rx) = oneshot::channel::<Option<Vec<std::path::PathBuf>>>();
    // SAFETY: runs on the main thread (gpui's foreground executor). The panel
    // is retained for the life of the app, so the pointer the completion block
    // captures stays valid; all other messages are standard AppKit API.
    unsafe {
        let mut panel = PANEL.with(Cell::get);
        if panel.is_null() {
            panel = msg_send![class!(NSOpenPanel), openPanel];
            if panel.is_null() {
                return Err("NSOpenPanel is unavailable".to_string());
            }
            let _: *mut Object = msg_send![panel, retain];
            PANEL.with(|cell| cell.set(panel));
        }
        let visible: objc::runtime::BOOL = msg_send![panel, isVisible];
        if visible != NO {
            // Already asking; bring it forward instead of starting a second prompt.
            let _: () = msg_send![panel, makeKeyAndOrderFront: std::ptr::null_mut::<Object>()];
            return Ok(None);
        }
        let _: () = msg_send![panel, setCanChooseDirectories: YES];
        let _: () = msg_send![panel, setCanChooseFiles: NO];
        let _: () = msg_send![panel, setAllowsMultipleSelection: YES];
        let _: () = msg_send![panel, setCanCreateDirectories: YES];
        let _: () = msg_send![panel, setResolvesAliases: NO];
        let prompt: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: prompt.as_ptr()];
        let _: () = msg_send![panel, setPrompt: prompt];

        let done_tx = RefCell::new(Some(done_tx));
        let block = ConcreteBlock::new(move |response: isize| {
            let mut paths = Vec::new();
            if response == MODAL_RESPONSE_OK {
                let urls: *mut Object = msg_send![panel, URLs];
                let count: usize = if urls.is_null() {
                    0
                } else {
                    msg_send![urls, count]
                };
                for index in 0..count {
                    let url: *mut Object = msg_send![urls, objectAtIndex: index];
                    let path: *mut Object = msg_send![url, path];
                    if path.is_null() {
                        continue;
                    }
                    let utf8: *const std::os::raw::c_char = msg_send![path, UTF8String];
                    if !utf8.is_null() {
                        paths.push(std::path::PathBuf::from(
                            CStr::from_ptr(utf8).to_string_lossy().into_owned(),
                        ));
                    }
                }
            }
            if let Some(done_tx) = done_tx.borrow_mut().take() {
                let _ = done_tx.send((response == MODAL_RESPONSE_OK).then_some(paths));
            }
        });
        let block = block.copy();
        let _: () = msg_send![panel, beginWithCompletionHandler: &*block];
    }
    Ok(done_rx.await.unwrap_or(None))
}
