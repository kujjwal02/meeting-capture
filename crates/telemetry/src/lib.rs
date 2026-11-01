//! Logging every Rust process of the app shares: the app, its native host mode, `mc-capture` and
//! `mc`. Each process calls [`init`] once, first thing.
//!
//! Lines follow the OpenTelemetry log data model without the OpenTelemetry SDK: a severity, a UTC
//! timestamp, a body, attributes with semantic-convention names, and a `trace_id` and `span_id`
//! inside a call or a pipeline run. A call's trace id is derived from its `call.id`, so every
//! process that logs the call joins the same trace with no id passed around. See
//! [`fields`] for the names and [`trace`] for the derivation.
//!
//! Two sinks take the lines:
//!
//! - The log file, `logs/<service>-<UTC date>.jsonl`, at INFO. A process that opens a new day's
//!   file keeps its service's last 14 days of files and deletes older ones. The level is held by
//!   a reload filter, so debug mode turns it to DEBUG through [`debug_mode::DebugModeSwitch`].
//! - The flight recorder: the last 10 minutes or 8 MB of DEBUG lines, in memory. A problem, a
//!   panic, a failed job or a failed helper writes it to a debug folder with [`dump_recorder`].
//!
//! The crate is portable: no Tauri and no OS calls.

pub mod debug_mode;
pub mod env;
pub mod fields;
mod init;
mod line;
mod log_file;
pub mod metrics;
mod recorder;
mod resource;
pub mod trace;

pub use init::{
    FileLevel, FileLevelHandle, InitError, LogSink, TelemetryConfig, dump_recorder,
    dump_recorder_into, init,
};
pub use log_file::daily_log_files;
pub use recorder::{DumpError, DumpReason};
pub use resource::Service;
