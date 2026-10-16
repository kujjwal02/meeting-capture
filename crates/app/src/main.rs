//! Tray app. Runs from login with no Dock icon and no window until settings open.

// Release builds on Windows would otherwise open a console window next to the tray icon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::RunEvent;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;

const QUIT_MENU_ID: &str = "quit";

fn main() -> tauri::Result<()> {
    let app = tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let quit = MenuItem::with_id(app, QUIT_MENU_ID, "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;
            let icon = app
                .default_window_icon()
                .cloned()
                .ok_or("no app icon bundled")?;
            TrayIconBuilder::new()
                .icon(icon)
                .tooltip("Meeting Capture")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    if event.id().as_ref() == QUIT_MENU_ID {
                        app.exit(0);
                    }
                })
                .build(app)?;
            Ok(())
        })
        .build(tauri::generate_context!())?;

    app.run(|_, event| {
        // Closing the last window must not quit a tray app; only the Quit item does.
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
    Ok(())
}
