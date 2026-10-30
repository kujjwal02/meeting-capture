//! The macOS reader: finds Slack huddle windows through the accessibility API and turns what they
//! show into observations for the tracker.

mod ax;
mod huddle;
mod tree;
mod watcher;

use std::time::Duration;

use crate::{Notice, WatchPaths, WatchSettings};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) fn run(
    paths: &WatchPaths,
    settings: &impl Fn() -> WatchSettings,
    notify: impl FnMut(Notice),
) {
    let mut watcher = watcher::Watcher::new(paths, notify);
    loop {
        // AppKit hands back autoreleased objects, and this thread has no run loop to drain them.
        objc2::rc::autoreleasepool(|_| watcher.tick(&settings()));
        std::thread::sleep(POLL_INTERVAL);
    }
}
