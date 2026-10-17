//! `mc`: reruns pipeline steps without the tray app.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use jiff::tz::TimeZone;
use jobs::{JobStore, Meeting, MeetingId};
use pipeline::extraction::{self, CheckedNotes, Prompt};
use pipeline::keychain::Keychain;
use pipeline::llm::Provider;
use pipeline::settings::{AppDirs, ProviderKind, Settings};
use pipeline::{Pipeline, RunOutcome};

#[derive(Parser)]
#[command(name = "mc", version, about = "Rerun meeting-capture pipeline steps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a saved TranscripTonic advanced webhook body as a new meeting and process it.
    Ingest { payload_file: PathBuf },
    /// Run every step again for a meeting, rewriting its note.
    Reprocess {
        meeting_id: i64,
        /// Overwrite the note even if it was edited after the last write.
        #[arg(long)]
        discard_edits: bool,
    },
    /// Resume a failed meeting at the step that failed.
    Retry { meeting_id: i64 },
    /// Run only the extraction for a meeting and print the checked notes as JSON. Writes nothing.
    Extract {
        meeting_id: i64,
        /// Check this saved reply, one JSON object in the reply schema, instead of calling the LLM.
        #[arg(long, conflicts_with_all = ["provider", "model"])]
        reply: Option<PathBuf>,
        /// Use this provider instead of the one in settings.
        #[arg(long, value_enum)]
        provider: Option<ProviderArg>,
        /// Use this model instead of the one in settings.
        #[arg(long)]
        model: Option<String>,
        /// Print the prompt instead of calling the LLM.
        #[arg(long, conflicts_with = "reply")]
        print_prompt: bool,
    },
    /// Save an LLM API key in the OS keychain, read from stdin so it stays out of shell history.
    SetApiKey {
        #[arg(value_enum)]
        provider: ApiKeyProviderArg,
    },
    /// Show the most recent meetings.
    List {
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    let app_dirs = AppDirs::locate()?;
    app_dirs.create()?;
    let settings = Settings::load(&app_dirs)?;
    let store = JobStore::open(&app_dirs.job_store_file())?;
    let pipeline = Pipeline::new(app_dirs.clone(), settings, store);

    let time_zone = pipeline.settings().time_zone.clone();
    match cli.command {
        Command::Ingest { payload_file } => {
            let id = ingest(&pipeline, &app_dirs, &payload_file)?;
            print_outcome(&pipeline.run(id)?, &time_zone);
        }
        Command::Reprocess {
            meeting_id,
            discard_edits,
        } => {
            print_outcome(
                &pipeline.reprocess(MeetingId::new(meeting_id), discard_edits)?,
                &time_zone,
            );
        }
        Command::Retry { meeting_id } => {
            print_outcome(&pipeline.run(MeetingId::new(meeting_id))?, &time_zone);
        }
        Command::Extract {
            meeting_id,
            reply,
            provider,
            model,
            print_prompt,
        } => {
            let prompt = pipeline.extraction_prompt(MeetingId::new(meeting_id))?;
            if print_prompt {
                print!("{}", prompt.render());
                return Ok(());
            }
            let checked = if let Some(reply_file) = reply {
                check_saved_reply(&prompt, &reply_file)?
            } else {
                {
                    let mut llm = pipeline.settings().llm.clone();
                    if let Some(provider) = provider {
                        llm.provider = Some(provider.into());
                    }
                    if model.is_some() {
                        llm.model = model;
                    }
                    llm.validate()?;
                    let keychain = Keychain::new(app_dirs.keychain_service());
                    let provider = Provider::from_settings(&llm, &keychain, &app_dirs)?
                        .context("no LLM provider is set; pass --provider or set llm.provider")?;
                    provider.extract(&prompt)?
                }
            };
            println!("{}", serde_json::to_string_pretty(&checked)?);
        }
        Command::SetApiKey { provider } => {
            let mut api_key = String::new();
            std::io::stdin()
                .read_line(&mut api_key)
                .context("could not read the key from stdin")?;
            let api_key = api_key.trim();
            anyhow::ensure!(!api_key.is_empty(), "stdin held no key");
            Keychain::new(app_dirs.keychain_service()).set_llm_api_key(provider.into(), api_key)?;
            println!("saved");
        }
        Command::List { limit } => {
            for meeting in pipeline.store().recent(limit)? {
                print_meeting(&meeting, &time_zone);
            }
        }
    }
    Ok(())
}

// Copies the payload into the app's payload folder under a new name, so the job never depends on
// a file the user may move, delete or reuse.
fn ingest(
    pipeline: &Pipeline,
    app_dirs: &AppDirs,
    payload_file: &Path,
) -> anyhow::Result<MeetingId> {
    let payload = std::fs::read(payload_file)
        .with_context(|| format!("could not read {}", payload_file.display()))?;
    let transcript = sources::transcriptonic::parse(&payload)?;
    let payload_path = sources::transcriptonic::save_payload(&app_dirs.payload_dir(), &payload)
        .context("could not save the payload in the app folder")?;
    Ok(pipeline.record_transcriptonic(&payload_path, &transcript)?)
}

fn check_saved_reply(prompt: &Prompt, reply_file: &Path) -> anyhow::Result<CheckedNotes> {
    let reply = std::fs::read_to_string(reply_file)
        .with_context(|| format!("could not read {}", reply_file.display()))?;
    let notes = extraction::parse_reply(&reply).map_err(anyhow::Error::msg)?;
    let errors = extraction::reply_errors(&notes, prompt);
    if !errors.is_empty() {
        anyhow::bail!(
            "the reply does not fit this meeting:\n- {}",
            errors.join("\n- ")
        );
    }
    Ok(extraction::check_citations(notes, prompt))
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProviderArg {
    Anthropic,
    OpenaiCompatible,
    ClaudeCode,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ApiKeyProviderArg {
    Anthropic,
    OpenaiCompatible,
}

impl From<ApiKeyProviderArg> for ProviderKind {
    fn from(provider: ApiKeyProviderArg) -> Self {
        match provider {
            ApiKeyProviderArg::Anthropic => Self::Anthropic,
            ApiKeyProviderArg::OpenaiCompatible => Self::OpenaiCompatible,
        }
    }
}

impl From<ProviderArg> for ProviderKind {
    fn from(provider: ProviderArg) -> Self {
        match provider {
            ProviderArg::Anthropic => Self::Anthropic,
            ProviderArg::OpenaiCompatible => Self::OpenaiCompatible,
            ProviderArg::ClaudeCode => Self::ClaudeCode,
        }
    }
}

fn print_outcome(outcome: &RunOutcome, time_zone: &TimeZone) {
    match outcome {
        RunOutcome::Finished(meeting) => print_meeting(meeting, time_zone),
        RunOutcome::AlreadyRunning => {
            println!("another process is running this meeting; it will pick up the new input");
        }
    }
}

fn print_meeting(meeting: &Meeting, time_zone: &TimeZone) {
    let started_at = meeting
        .started_at
        .to_zoned(time_zone.clone())
        .strftime("%Y-%m-%d %H:%M %Z");
    println!(
        "{:>4}  {}  {:<9}  {}",
        meeting.id,
        started_at,
        meeting.state().label(),
        meeting.title.as_deref().unwrap_or("(no title)"),
    );
    if let Some(failure) = &meeting.failure {
        println!("      failed: {failure}");
    }
    if let Some(note_path) = &meeting.note_path {
        println!("      note: {}", note_path.display());
    }
}
