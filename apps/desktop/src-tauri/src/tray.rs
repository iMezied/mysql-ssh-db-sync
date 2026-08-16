//! Tray / menu-bar presence.
//!
//! The tray is not decoration: it is the thing that lets the app keep running
//! after its window is closed, and therefore the thing that makes a schedule
//! configured in the app actually fire. Without it, "close the window" and
//! "cancel every backup" would be the same gesture.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

pub const TRAY_ID: &str = "dbsync-main";

/// Bring the main window to the front, restoring it if it was hidden.
///
/// `unminimize` matters: a window hidden while minimised comes back invisible
/// otherwise, which reads as the app being broken.
pub fn focus_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("no main window to focus");
        return;
    };

    let _ = window.unminimize();
    if let Err(e) = window.show() {
        tracing::warn!("could not show the main window: {e}");
    }
    let _ = window.set_focus();
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open DBSync Studio", true, None::<&str>)?;
    let schedules = MenuItem::with_id(app, "schedules", "Schedules…", true, None::<&str>)?;
    let jobs = MenuItem::with_id(app, "jobs", "Recent jobs…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    // Distinct from closing the window, and labelled so that difference is
    // obvious: this is the one that stops schedules from running.
    let quit = MenuItem::with_id(app, "quit", "Quit (stops schedules)", true, None::<&str>)?;

    let menu = Menu::with_items(app, &[&open, &schedules, &jobs, &separator, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("DBSync Studio")
        .menu(&menu)
        // The left click opens the window; the menu belongs on the right, which
        // is the platform convention everywhere except a pure menu-bar app.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => focus_main_window(app),
            "schedules" => {
                focus_main_window(app);
                navigate(app, "/schedules");
            }
            "jobs" => {
                focus_main_window(app);
                navigate(app, "/jobs");
            }
            "quit" => {
                // Marked so the exit handler lets the process go rather than
                // treating it as another close-to-tray.
                crate::request_quit(app);
            }
            other => tracing::debug!("unhandled tray menu id {other:?}"),
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                focus_main_window(tray.app_handle());
            }
        });

    builder = with_icon(builder, app);
    builder.build(app)?;
    Ok(())
}

/// macOS wants a *template* image in the menu bar: a flat silhouette it tints
/// for the current appearance and inverts when the menu is open. The full
/// colour app tile reads as wrong there and stays dark against a dark menu bar.
#[cfg(target_os = "macos")]
fn with_icon<R: tauri::Runtime>(
    builder: TrayIconBuilder<R>,
    _app: &AppHandle<R>,
) -> TrayIconBuilder<R> {
    match tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png")) {
        Ok(icon) => builder.icon(icon).icon_as_template(true),
        Err(e) => {
            tracing::warn!("could not decode the menu-bar icon: {e}");
            builder
        }
    }
}

/// Everywhere else the coloured application icon is the convention.
///
/// Takes the handle concretely rather than any `Manager`, because
/// `default_window_icon` is not a `Manager` method — it comes from
/// `shared_app_impl!`, which covers `App` and `AppHandle` and nothing else.
/// The bound was wrong from the day it was written and nobody could see it:
/// macOS compiles the other arm of this `cfg`, so the only machine anyone
/// builds on skipped straight past it.
#[cfg(not(target_os = "macos"))]
fn with_icon<R: tauri::Runtime>(
    builder: TrayIconBuilder<R>,
    app: &AppHandle<R>,
) -> TrayIconBuilder<R> {
    match app.default_window_icon() {
        Some(icon) => builder.icon(icon.clone()),
        None => {
            tracing::warn!("no default window icon; the tray will use a placeholder");
            builder
        }
    }
}

/// Ask the frontend to route somewhere.
///
/// An event rather than a URL change, because reloading the webview would throw
/// away any in-flight job's live progress view.
fn navigate(app: &AppHandle, route: &str) {
    use tauri_specta::Event as _;
    if let Err(e) = crate::events::NavigateTo(route.to_string()).emit(app) {
        tracing::warn!("could not ask the window to navigate to {route}: {e}");
    }
}
