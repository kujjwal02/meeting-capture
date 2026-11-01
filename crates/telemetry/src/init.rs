//! The subscriber each process installs once, and the calls that reach it later.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tracing::{Level, Metadata};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Filter, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{Layer as _, Registry, reload};

use crate::line::{LineLayer, SpanContextLayer};
use crate::log_file::{DailyLogFile, FileSink};
use crate::recorder::{DumpError, DumpReason, FlightRecorder};
use crate::resource::{Resource, Service};
use crate::trace::SpanIds;

/// What a layer keeps: INFO and above from every crate, and DEBUG from the app's own crates when
/// `is_workspace_debug`. Dependencies' DEBUG output (HTTP, TLS, the webview) would fill the ring
/// in seconds.
#[derive(Debug, Clone, Copy)]
struct LevelRule {
    is_workspace_debug: bool,
}

impl LevelRule {
    fn keeps(self, metadata: &Metadata<'_>) -> bool {
        let level = *metadata.level();
        level <= Level::INFO
            || (self.is_workspace_debug && level == Level::DEBUG && is_workspace_crate(metadata))
    }
}

impl<S> Filter<S> for LevelRule {
    fn enabled(&self, metadata: &Metadata<'_>, _context: &Context<'_, S>) -> bool {
        self.keeps(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(if self.is_workspace_debug {
            LevelFilter::DEBUG
        } else {
            LevelFilter::INFO
        })
    }
}

// Cargo passes rustc a relative source path for a workspace member and an absolute one for a
// registry or git dependency, so `file!()` tells them apart with no list of crate names to keep
// up to date. A `log` crate record has no file and counts as a dependency's.
fn is_workspace_crate(metadata: &Metadata<'_>) -> bool {
    metadata
        .file()
        .is_some_and(|file| Path::new(file).is_relative())
}

/// The log file's rule at its current level, which [`FileLevelHandle`] turns.
pub(crate) struct FileRule {
    /// Owns the rule: the handle holds it weakly.
    _owner: reload::Layer<LevelRule, Registry>,
    handle: reload::Handle<LevelRule, Registry>,
}

impl FileRule {
    pub(crate) fn keeps(&self, metadata: &Metadata<'_>) -> bool {
        self.handle
            .with_current(|rule| rule.keeps(metadata))
            .unwrap_or(false)
    }
}

/// Where the log file layer writes.
#[derive(Debug, Clone)]
pub enum LogSink {
    /// `<dir>/<service>-<UTC date>.jsonl`, kept 14 days.
    DailyFiles { dir: PathBuf },
    /// Stderr, for a helper run by hand.
    Stderr,
}

#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    pub service: Service,
    /// The binary's version, `env!("CARGO_PKG_VERSION")`.
    pub service_version: &'static str,
    pub log_sink: LogSink,
    /// A panic dumps the recorder into a new folder here. `None` keeps a panic in the log only.
    pub panic_dump_dir: Option<PathBuf>,
}

/// What the log file keeps. The recorder keeps DEBUG either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLevel {
    Info,
    /// The app's own DEBUG lines too, as the recorder keeps them.
    Debug,
}

/// Turns the log file's level at run time.
#[derive(Clone)]
pub struct FileLevelHandle(reload::Handle<LevelRule, Registry>);

impl FileLevelHandle {
    /// # Errors
    ///
    /// Fails only when the subscriber is gone, which happens at process exit.
    pub fn set(&self, level: FileLevel) -> Result<(), reload::Error> {
        self.0.reload(file_rule(level))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("could not open the log file: {0}")]
    LogFile(#[from] std::io::Error),
    #[error("logging was set up twice: {0}")]
    AlreadySet(#[from] tracing_subscriber::util::TryInitError),
}

struct Installed {
    service: Service,
    recorder: Arc<FlightRecorder>,
    resource: Arc<Resource>,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Installs the subscriber and the panic hook. Call once, before anything logs.
///
/// # Errors
///
/// Fails when the log folder cannot be written, or when a subscriber is already installed.
pub fn init(config: TelemetryConfig) -> Result<FileLevelHandle, InitError> {
    let resource = Arc::new(Resource::of_this_process(
        config.service,
        config.service_version,
    ));
    let file_sink = match &config.log_sink {
        LogSink::DailyFiles { dir } => FileSink::Daily(DailyLogFile::open(
            dir.clone(),
            config.service,
            Arc::clone(&resource),
        )?),
        LogSink::Stderr => FileSink::Stderr,
    };
    let recorder = Arc::new(FlightRecorder::new(Arc::clone(&resource)));
    let (file_rule_owner, file_level) = reload::Layer::new(file_rule(FileLevel::Info));
    let file_rule = FileRule {
        _owner: file_rule_owner,
        handle: file_level.clone(),
    };
    let layers = vec![
        SpanContextLayer::new(SpanIds::new(resource.span_id_seed()))
            .with_filter(recorder_rule())
            .boxed(),
        LineLayer::new(
            file_sink,
            file_rule,
            Arc::clone(&recorder),
            Arc::clone(&resource),
        )
        .with_filter(recorder_rule())
        .boxed(),
    ];
    tracing_subscriber::registry().with(layers).try_init()?;
    if let LogSink::DailyFiles { dir } = &config.log_sink {
        // Now that the subscriber runs, so each removal is logged.
        crate::log_file::remove_expired(dir, config.service);
    }
    let _first_init = INSTALLED.set(Installed {
        service: config.service,
        recorder,
        resource: Arc::clone(&resource),
    });
    install_panic_hook(config.service, config.panic_dump_dir);
    tracing::info!(
        service.name = config.service.name(),
        service.version = config.service_version,
        service.instance.id = resource.instance_id,
        process.pid = resource.pid,
        "telemetry started"
    );
    Ok(FileLevelHandle(file_level))
}

/// Writes the recorder's lines since the last dump into `path`, after a line with `reason`.
/// Returns how many lines it added.
///
/// # Errors
///
/// Fails before `init`, or when the file cannot be written.
pub fn dump_recorder(path: &Path, reason: DumpReason) -> Result<usize, DumpError> {
    let installed = INSTALLED.get().ok_or(DumpError::NotStarted)?;
    installed.recorder.dump(path, reason)
}

/// Dumps the recorder into `folder/recorder-<service>.jsonl` and returns that path.
///
/// # Errors
///
/// Fails before `init`, or when the file cannot be written.
pub fn dump_recorder_into(folder: &Path, reason: DumpReason) -> Result<PathBuf, DumpError> {
    let installed = INSTALLED.get().ok_or(DumpError::NotStarted)?;
    let path = folder.join(installed.service.recorder_file_name());
    let line_count = installed.recorder.dump(&path, reason)?;
    tracing::info!(
        file.path = %path.display(),
        mc.recorder.reason = reason.as_str(),
        mc.recorder.line_count = line_count,
        "dumped the flight recorder"
    );
    Ok(path)
}

/// This process's resource, for rows written outside the log layers. `None` before `init`.
pub(crate) fn installed_resource() -> Option<Arc<Resource>> {
    INSTALLED
        .get()
        .map(|installed| Arc::clone(&installed.resource))
}

fn file_rule(level: FileLevel) -> LevelRule {
    match level {
        FileLevel::Info => LevelRule {
            is_workspace_debug: false,
        },
        FileLevel::Debug => recorder_rule(),
    }
}

fn recorder_rule() -> LevelRule {
    LevelRule {
        is_workspace_debug: true,
    }
}

// A panic on a worker thread would otherwise reach only stderr, which a login item and a helper
// started by the app discard.
fn install_panic_hook(service: Service, panic_dump_dir: Option<PathBuf>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        // The sink may hold the lock a log line or a dump would wait on. The default hook still
        // prints the panic to stderr.
        if crate::line::is_inside_sink() {
            default_hook(panic);
            return;
        }
        let stacktrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            exception.message = %panic,
            exception.stacktrace = %stacktrace,
            "panic"
        );
        if let Some(dir) = &panic_dump_dir {
            // The pid keeps apart two processes of one service that panic in the same second.
            let folder = dir.join(format!(
                "panic-{}-{}-{}",
                jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ"),
                service.name(),
                std::process::id()
            ));
            if let Err(error) = dump_recorder_into(&folder, DumpReason::Panic) {
                tracing::error!(exception.message = %error, "could not dump the flight recorder");
            }
        }
        default_hook(panic);
    }));
}
