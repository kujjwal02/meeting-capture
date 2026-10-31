//! ScreenCaptureKit window capture, VideoToolbox encoding and AVAssetWriter fragments.

pub mod encoder;
pub mod muxer;
pub mod stream;
pub mod window;

/// The capture and encode rate. A meeting window changes little; 15 fps keeps the bitrate low.
pub const FRAMES_PER_SECOND: i32 = 15;
