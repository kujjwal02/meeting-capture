//! The macOS writer of `call.mp4`, through AVFoundation.

mod mp4;
mod transcode;

pub(crate) use mp4::write_call_mp4;
