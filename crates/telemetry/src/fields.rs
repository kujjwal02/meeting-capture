//! Field names, spelled once. `tracing` macros take literal field names, so call sites spell them
//! too; these constants are for code that reads or writes the lines. A name is the OpenTelemetry
//! semantic convention's where one exists. Otherwise it has a domain prefix, such as `call.` or
//! `meeting.`, and new names from the telemetry plan start with `mc.`.

/// Resource: which program wrote the line.
pub const SERVICE_NAME: &str = "service.name";
/// Resource: the app version.
pub const SERVICE_VERSION: &str = "service.version";
/// Resource: one id per process run. It joins a file's resource line to its log lines.
pub const SERVICE_INSTANCE_ID: &str = "service.instance.id";
pub const PROCESS_PID: &str = "process.pid";
pub const OS_TYPE: &str = "os.type";
pub const HOST_ARCH: &str = "host.arch";

/// The call: its source's log path. A span with it starts the call's trace.
pub const CALL_ID: &str = "call.id";
/// The meeting's row in the job store.
pub const MEETING_ID: &str = "meeting.id";
/// The input revision a pipeline attempt runs on. With `meeting.id` on the same span it starts
/// the attempt's trace.
pub const INPUT_REVISION: &str = "mc.input.revision";
/// The pipeline step a span runs, as the job store names the step it finishes.
pub const JOB_STEP: &str = "job.step";

/// 32 hex digits, W3C trace context.
pub const TRACE_ID: &str = "trace_id";
/// 16 hex digits, W3C trace context.
pub const SPAN_ID: &str = "span_id";
pub const PARENT_SPAN_ID: &str = "parent_span_id";

/// Why a flight recorder dump was written.
pub const RECORDER_REASON: &str = "mc.recorder.reason";
/// On the `dump` line that ends a dump file grown to its cap: later dumps add nothing.
pub const RECORDER_IS_FILE_FULL: &str = "mc.recorder.is_file_full";

pub const EXCEPTION_MESSAGE: &str = "exception.message";
pub const EXCEPTION_STACKTRACE: &str = "exception.stacktrace";
