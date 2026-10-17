//! The `claude-code` provider: a signed-in Claude Code CLI, billed to the user's Claude plan.
//! One `claude -p` process per extraction, in stream-json mode, so repair turns reuse the open
//! conversation and no session file with the transcript is written.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{Conversation, ExtractionError, Reply, SYSTEM_PROMPT};
use crate::extraction::{self, MeetingNotes};
use crate::settings::{AppDirs, Effort, LlmSettings};

// A turn on a 60-minute call took under a minute in testing; Opus on a busy plan can take several.
const TURN_TIMEOUT: Duration = Duration::from_mins(5);
// Keeps a run well inside the pipeline's 15-minute lease cap.
const TOTAL_TIMEOUT: Duration = Duration::from_mins(10);
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(15);
// Enough of stderr to explain a crash.
const STDERR_TAIL_BYTES: usize = 2000;

// Any of these makes Claude Code bill an API account instead of the plan, or sends the
// transcript to another endpoint.
const ROUTING_ENV_VARS: [&str; 9] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
];

#[derive(Debug, Clone)]
pub(super) struct ClaudeCode {
    binary: PathBuf,
    model: String,
    effort: Option<Effort>,
    // An empty folder, so Claude Code finds no project files or CLAUDE.md to read.
    work_dir: PathBuf,
}

impl ClaudeCode {
    pub(super) fn new(
        llm: &LlmSettings,
        model: String,
        app_dirs: &AppDirs,
    ) -> Result<Self, ExtractionError> {
        Ok(Self {
            binary: locate_binary(llm.claude_path.as_deref())?,
            model,
            effort: llm.effort,
            work_dir: app_dirs.claude_code_dir(),
        })
    }

    pub(super) fn start(&self, label: &str) -> Result<impl Conversation + use<>, ExtractionError> {
        let schema = extraction::reply_schema().to_string();
        let mut command = base_command(&self.binary);
        command
            .args(["-p", "--model", &self.model])
            .args(["--tools", "", "--safe-mode", "--setting-sources", ""])
            .args(["--no-session-persistence"])
            .args(["--input-format", "stream-json"])
            .args(["--output-format", "stream-json", "--verbose"])
            .args(["--system-prompt", SYSTEM_PROMPT])
            .args(["--json-schema", &schema])
            .current_dir(&self.work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(effort) = self.effort {
            command.args(["--effort", effort.as_str()]);
        }
        std::fs::create_dir_all(&self.work_dir).map_err(|error| ExtractionError::Transport {
            provider: label.to_owned(),
            detail: format!("could not create {}: {error}", self.work_dir.display()),
        })?;
        let mut child = command
            .spawn()
            .map_err(|error| ExtractionError::Transport {
                provider: label.to_owned(),
                detail: format!("could not start {}: {error}", self.binary.display()),
            })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        // A prompt is larger than the pipe buffer, so a stalled claude would block the write. On
        // its own thread the write cannot stop the turn timeout, and the kill in drop ends it.
        let (prompt_sender, prompts) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut stdin = stdin;
            for prompt in prompts {
                if writeln!(stdin, "{prompt}")
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    return;
                }
            }
        });
        let (line_sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line_sender.send(line).is_err() {
                    return;
                }
            }
        });
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            let start = text.len().saturating_sub(STDERR_TAIL_BYTES);
            let start = (start..text.len())
                .find(|&index| text.is_char_boundary(index))
                .unwrap_or(text.len());
            if let Ok(mut tail) = tail.lock() {
                text[start..].clone_into(&mut tail);
            }
        });
        Ok(Session {
            child,
            prompts: Some(prompt_sender),
            lines,
            stderr_tail,
            deadline: Instant::now() + TOTAL_TIMEOUT,
            label: label.to_owned(),
        })
    }
}

struct Session {
    child: Child,
    /// To the thread that writes claude's stdin. Dropping it closes stdin.
    prompts: Option<Sender<String>>,
    lines: Receiver<std::io::Result<String>>,
    stderr_tail: Arc<Mutex<String>>,
    deadline: Instant,
    label: String,
}

/// The parts of a stream-json event this provider reads.
#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    /// On `assistant` events: why the call failed, such as `authentication_failed`.
    error: Option<String>,
    message: Option<AssistantMessage>,
    // On the `result` event.
    is_error: Option<bool>,
    result: Option<String>,
    structured_output: Option<serde_json::Value>,
    stop_reason: Option<String>,
    api_error_status: Option<u16>,
    usage: Option<TokenUsage>,
    total_cost_usd: Option<f64>,
    claude_code_version: Option<String>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(rename = "input_tokens")]
    input: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read: Option<u64>,
    #[serde(rename = "output_tokens")]
    output: Option<u64>,
}

impl Conversation for Session {
    fn send(&mut self, message: &str) -> Result<Reply, ExtractionError> {
        let event = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": message},
        });
        let is_sent = self
            .prompts
            .as_ref()
            .is_some_and(|prompts| prompts.send(event.to_string()).is_ok());
        if !is_sent {
            return Err(self.transport("could not write the prompt: claude closed its input"));
        }

        let turn_deadline = (Instant::now() + TURN_TIMEOUT).min(self.deadline);
        let mut failure_kind: Option<String> = None;
        let mut stopped_at_limit = false;
        loop {
            let wait = turn_deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(wait) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => {
                    return Err(self.transport(&format!("could not read the reply: {error}")));
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ExtractionError::Timeout {
                        provider: self.label.clone(),
                        limit_s: if turn_deadline == self.deadline {
                            TOTAL_TIMEOUT.as_secs()
                        } else {
                            TURN_TIMEOUT.as_secs()
                        },
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let stderr = self
                        .stderr_tail
                        .lock()
                        .map(|tail| tail.clone())
                        .unwrap_or_default();
                    return Err(self
                        .transport(&format!("claude exited before replying: {}", stderr.trim())));
                }
            };
            // Claude Code adds event types over time; unknown or malformed lines are skipped.
            let Ok(event) = serde_json::from_str::<StreamEvent>(&line) else {
                continue;
            };
            match event.kind.as_str() {
                "system" => {
                    if let Some(version) = &event.claude_code_version {
                        tracing::info!(cli.version = %version, "claude code started");
                    }
                }
                "assistant" => {
                    if event.error.is_some() {
                        failure_kind = event.error;
                    }
                    if event
                        .message
                        .and_then(|message| message.stop_reason)
                        .is_some_and(|reason| reason == "max_tokens")
                    {
                        stopped_at_limit = true;
                    }
                }
                "result" => {
                    return self.finish_turn(event, failure_kind.as_deref(), stopped_at_limit);
                }
                _ => {}
            }
        }
    }
}

impl Session {
    fn finish_turn(
        &self,
        event: StreamEvent,
        failure_kind: Option<&str>,
        stopped_at_limit: bool,
    ) -> Result<Reply, ExtractionError> {
        if let Some(usage) = &event.usage {
            tracing::info!(
                gen_ai.usage.input_tokens = usage.input.unwrap_or(0),
                gen_ai.usage.cache_read.input_tokens = usage.cache_read.unwrap_or(0),
                gen_ai.usage.output_tokens = usage.output.unwrap_or(0),
                claude_code.cost_usd = event.total_cost_usd.unwrap_or(0.0),
                "claude code turn done"
            );
        }
        let text = event.result.unwrap_or_default();
        if stopped_at_limit || event.stop_reason.as_deref() == Some("max_tokens") {
            return Err(ExtractionError::Truncated {
                provider: self.label.clone(),
            });
        }
        if event.is_error == Some(true) {
            let provider = self.label.clone();
            return Err(match (failure_kind, event.api_error_status) {
                (Some("authentication_failed" | "oauth_org_not_allowed"), _)
                | (_, Some(401 | 403)) => ExtractionError::NotSignedIn {
                    provider,
                    detail: format!("{text}; run `claude` and sign in"),
                },
                (Some("rate_limit"), _) | (_, Some(429)) => {
                    ExtractionError::RateLimited { provider }
                }
                _ => ExtractionError::Transport {
                    provider,
                    detail: text,
                },
            });
        }
        Ok(match event.structured_output {
            Some(object) => match serde_path_to_error::deserialize::<_, MeetingNotes>(object) {
                Ok(notes) => Reply::Parsed(notes),
                Err(error) => Reply::Unparsed(format!("{}: {}", error.path(), error.inner())),
            },
            None => match extraction::parse_reply(&text) {
                Ok(notes) => Reply::Parsed(notes),
                Err(error) => Reply::Unparsed(error),
            },
        })
    }

    fn transport(&self, detail: &str) -> ExtractionError {
        ExtractionError::Transport {
            provider: self.label.clone(),
            detail: detail.to_owned(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The conversation is over, so nothing the process would still do is wanted.
        drop(self.prompts.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What `claude auth status` reports. It makes no model call.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeAuth {
    pub logged_in: bool,
    /// `claude.ai` or `oauth_token` bill the plan; anything else bills an API account.
    pub auth_method: String,
    pub email: Option<String>,
    pub subscription_type: Option<String>,
}

impl ClaudeCodeAuth {
    #[must_use]
    pub fn is_on_plan(&self) -> bool {
        self.logged_in && matches!(self.auth_method.as_str(), "claude.ai" | "oauth_token")
    }
}

/// Finds `claude` and asks it who is signed in, with the same environment an extraction gets.
///
/// # Errors
///
/// Returns `ExtractionError::CliMissing` when no binary is found, or `Transport` when it does not
/// answer.
pub fn claude_code_auth(claude_path: Option<&Path>) -> Result<ClaudeCodeAuth, ExtractionError> {
    let binary = locate_binary(claude_path)?;
    let label = "claude-code".to_owned();
    let mut child = base_command(&binary)
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ExtractionError::Transport {
            provider: label.clone(),
            detail: format!("could not start {}: {error}", binary.display()),
        })?;
    let Some(mut stdout) = child.stdout.take() else {
        unreachable!("stdout is piped");
    };
    let (sender, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = sender.send(text);
    });
    let text = output.recv_timeout(AUTH_STATUS_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();
    let text = text.map_err(|_| ExtractionError::Timeout {
        provider: label.clone(),
        limit_s: AUTH_STATUS_TIMEOUT.as_secs(),
    })?;
    serde_json::from_str(&text).map_err(|error| ExtractionError::Transport {
        provider: label,
        detail: format!("`claude auth status` printed something unexpected: {error}"),
    })
}

fn base_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    for name in ROUTING_ENV_VARS {
        command.env_remove(name);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // No console window flashes up from the tray app.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

// A login item does not get the shell's PATH, so the installers' folders are searched first.
fn locate_binary(claude_path: Option<&Path>) -> Result<PathBuf, ExtractionError> {
    if let Some(path) = claude_path {
        return Ok(path.to_path_buf());
    }
    let file_name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(user_dirs) = directories::UserDirs::new() {
        candidates.push(
            user_dirs
                .home_dir()
                .join(".local")
                .join("bin")
                .join(file_name),
        );
    }
    if cfg!(unix) {
        candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
        candidates.push(PathBuf::from("/usr/local/bin/claude"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(file_name)));
    }
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .ok_or_else(|| ExtractionError::CliMissing {
            searched: candidates
                .iter()
                .take(3)
                .map(|candidate| candidate.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        })
}
