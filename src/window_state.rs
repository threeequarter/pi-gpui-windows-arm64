//! The main window's last bounds and display, restored at the next start,
//! like Zed does. Saved shortly after the window moves or resizes, and once
//! more when it closes or the app quits.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, Bounds, Context, DisplayId, Pixels, Window, WindowBounds, point, px, size};
use serde_json::{Value, json};

/// Moves and resizes arrive in bursts; save after they settle.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// Smaller saved sizes are treated as damaged.
const MIN_SIZE: f32 = 200.;

/// Saved bounds and the display they were on, if that display is still
/// connected; `None` opens the default window.
pub fn restore(cx: &App) -> Option<(WindowBounds, DisplayId)> {
    let value: Value = serde_json::from_slice(&std::fs::read(file_path()?).ok()?).ok()?;
    let number = |key: &str| value[key].as_f64().map(|number| px(number as f32));
    let bounds = Bounds {
        origin: point(number("x")?, number("y")?),
        size: size(number("width")?, number("height")?),
    };
    if bounds.size.width < px(MIN_SIZE) || bounds.size.height < px(MIN_SIZE) {
        return None;
    }
    let uuid = value["display"].as_str()?;
    let display = cx
        .displays()
        .into_iter()
        .find(|display| display.uuid().is_ok_and(|id| id.to_string() == uuid))?;
    let bounds = match value["state"].as_str() {
        Some("maximized") => WindowBounds::Maximized(bounds),
        Some("fullscreen") => WindowBounds::Fullscreen(bounds),
        _ => WindowBounds::Windowed(bounds),
    };
    Some((bounds, display.id()))
}

/// Saves the window's bounds whenever they change and when it closes.
pub fn track<T: 'static>(window: &mut Window, cx: &mut Context<T>) {
    let generation = Rc::new(Cell::new(0u64));
    cx.observe_window_bounds(window, move |_, window, cx| {
        let Some(snapshot) = snapshot(window, cx) else {
            return;
        };
        // Only the last change of a burst is written.
        let current = generation.get() + 1;
        generation.set(current);
        let generation = generation.clone();
        let timer = cx.background_executor().timer(SAVE_DELAY);
        cx.spawn(async move |_, _| {
            timer.await;
            if generation.get() == current {
                save(&snapshot);
            }
        })
        .detach();
    })
    .detach();
    window.on_window_should_close(cx, |window, cx| {
        save_now(window, cx);
        true
    });
}

/// Saves right away: the window is about to close or the app to quit.
pub fn save_now(window: &Window, cx: &App) {
    if let Some(snapshot) = snapshot(window, cx) {
        save(&snapshot);
    }
}

fn snapshot(window: &Window, cx: &App) -> Option<Value> {
    let (state, bounds) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => ("windowed", bounds),
        WindowBounds::Maximized(bounds) => ("maximized", bounds),
        WindowBounds::Fullscreen(bounds) => ("fullscreen", bounds),
    };
    let display = window.display(cx)?.uuid().ok()?.to_string();
    let number = |value: Pixels| f32::from(value);
    Some(json!({
        "state": state,
        "x": number(bounds.origin.x),
        "y": number(bounds.origin.y),
        "width": number(bounds.size.width),
        "height": number(bounds.size.height),
        "display": display,
    }))
}

fn save(snapshot: &Value) {
    let Some(path) = file_path() else {
        return;
    };
    if let Err(error) = crate::hub::write_atomically(&path, snapshot) {
        eprintln!("pi-gpui: could not save the window position: {error}");
    }
}

fn file_path() -> Option<PathBuf> {
    Some(crate::sessions::app_data_directory()?.join("window.json"))
}
