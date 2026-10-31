//! Finds the hinted window in ScreenCaptureKit's shareable content. The call answers on a
//! framework queue, never the main thread, and has hung for 76 s in Cap, so the caller gives up
//! after 3 s and asks again later.

use std::sync::mpsc;
use std::time::Duration;

use cidre::sc;

use crate::mac::stream::SharedWindow;
use crate::{ListedWindow, WindowHint, pick_window};

/// Opens this process's connection to the window server. ScreenCaptureKit asserts that the
/// connection exists, and a process with no app object has none until a CoreGraphics call opens
/// it.
pub fn connect_window_server() {
    let main_display = cidre::cg::DirectDisplayId::main();
    tracing::debug!(display.id = main_display.0, "video.window_server_connected");
}

/// How long a lookup may take before the caller gives up on it (Loom, Fathom).
pub const LOOKUP_LIMIT: Duration = Duration::from_secs(3);

pub enum WindowLookup {
    Found {
        window: SharedWindow,
        listed: ListedWindow,
    },
    /// No window matches the hint now.
    NotFound,
    Failed {
        message: String,
    },
}

/// Asks ScreenCaptureKit for every window, on and off screen, and picks the hinted one.
/// `on_done` runs once, on a framework queue.
pub fn look_up_window(hint: WindowHint, on_done: impl FnOnce(WindowLookup) + Send + 'static) {
    let mut on_done = Some(on_done);
    sc::ShareableContent::current_with_ch(move |content, error| {
        let Some(on_done) = on_done.take() else {
            return;
        };
        let lookup = match (content, error) {
            (Some(content), _) => {
                let windows = content.windows();
                let listed: Vec<(ListedWindow, &sc::Window)> = windows
                    .iter()
                    .map(|window| (listed(window), window))
                    .collect();
                let plain: Vec<ListedWindow> =
                    listed.iter().map(|(window, _)| window.clone()).collect();
                match pick_window(&hint, &plain) {
                    Some(picked) => listed
                        .iter()
                        .find(|(window, _)| window.window_id == picked.window_id)
                        .map_or(WindowLookup::NotFound, |(listed, window)| {
                            WindowLookup::Found {
                                window: SharedWindow::new(window),
                                listed: listed.clone(),
                            }
                        }),
                    None => WindowLookup::NotFound,
                }
            }
            (None, Some(error)) => WindowLookup::Failed {
                message: error.localized_desc().to_string(),
            },
            (None, None) => WindowLookup::Failed {
                message: "no content and no error".to_owned(),
            },
        };
        on_done(lookup);
    });
}

/// Every window ScreenCaptureKit lists, for people picking a window id.
///
/// # Errors
///
/// Fails when the list fails or takes over 3 s.
pub fn list_windows() -> Result<Vec<ListedWindow>, String> {
    let (done, outcome) = mpsc::sync_channel(1);
    sc::ShareableContent::current_with_ch(move |content, error| {
        let listed = match (content, error) {
            (Some(content), _) => Ok(content.windows().iter().map(listed).collect()),
            (None, Some(error)) => Err(error.localized_desc().to_string()),
            (None, None) => Err("no content and no error".to_owned()),
        };
        let _unheard = done.try_send(listed);
    });
    match outcome.recv_timeout(LOOKUP_LIMIT) {
        Ok(listed) => listed,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "the window list took over {} s",
            LOOKUP_LIMIT.as_secs()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("the window list dropped its completion without calling it".to_owned())
        }
    }
}

fn listed(window: &sc::Window) -> ListedWindow {
    let app = window.owning_app();
    let frame = window.frame();
    ListedWindow {
        window_id: window.id(),
        bundle_id: app.as_ref().map(|app| app.bundle_id().to_string()),
        title: window.title().map(|title| title.to_string()),
        pid: app.as_ref().map_or(0, |app| app.process_id()),
        layer: i64::try_from(window.window_layer()).unwrap_or(i64::MAX),
        width_pt: frame.size.width,
        height_pt: frame.size.height,
        is_on_screen: window.is_on_screen(),
    }
}
