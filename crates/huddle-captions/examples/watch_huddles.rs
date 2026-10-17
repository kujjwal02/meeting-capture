//! Runs the huddle watcher outside the app and prints what it reports.
//! Usage: `cargo run -p huddle-captions --example watch_huddles -- <log dir> [layout override dir]`

use std::path::PathBuf;

use huddle_captions::{WatchPaths, WatchSettings};

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let Some(log_dir) = args.next() else {
        eprintln!("usage: watch_huddles <log dir> [layout override dir]");
        std::process::exit(2);
    };
    let layout_override_dir = args.next().unwrap_or_else(|| log_dir.join("slack-layouts"));
    let auto_start_captions = std::env::var_os("NO_AUTO_START").is_none();
    let watcher = huddle_captions::spawn(
        WatchPaths {
            log_dir,
            layout_override_dir,
        },
        move || WatchSettings {
            read_captions: true,
            auto_start_captions,
        },
        |notice| println!("{notice:?}"),
    );
    match watcher {
        Ok(Some(handle)) => {
            let _ = handle.join();
        }
        Ok(None) => eprintln!("this OS has no huddle watcher"),
        Err(error) => eprintln!("could not start the watcher: {error}"),
    }
}
