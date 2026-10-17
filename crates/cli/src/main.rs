//! `mc`: reruns pipeline steps without the tray app.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use jiff::tz::TimeZone;
use jobs::{InputKind, JobState, JobStore, Meeting, MeetingId, RunOutcome};
use pipeline::Pipeline;
use pipeline::extraction::{self, CheckedNotes, Prompt, Turn, Verdict};
use pipeline::llm::Provider;
use pipeline::settings::{AppDirs, ProviderKind, Settings};

#[derive(Parser)]
#[command(name = "mc", version, about = "Rerun meeting-capture pipeline steps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a saved input as a new meeting and process it.
    Ingest {
        /// A TranscripTonic advanced webhook body, or a Slack huddle log.
        input_file: PathBuf,
        #[arg(long, value_enum, default_value_t = InputKindArg::TranscriptonicPayload)]
        kind: InputKindArg,
    },
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
    /// Run every meeting the tray's sweep would: unfinished ones and due extraction retries.
    Sweep,
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
        Command::Ingest { input_file, kind } => {
            let id = match kind {
                InputKindArg::TranscriptonicPayload => ingest(&pipeline, &app_dirs, &input_file)?,
                InputKindArg::SlackHuddleLog => ingest_huddle(&pipeline, &app_dirs, &input_file)?,
            };
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
                    if let Some(provider) = provider.map(ProviderKind::from)
                        && llm.provider != Some(provider)
                    {
                        llm.provider = Some(provider);
                        // The stored key belongs to the settings' provider, not this one.
                        llm.api_key = None;
                    }
                    if model.is_some() {
                        llm.model = model;
                    }
                    llm.validate()?;
                    let provider = Provider::from_settings(&llm, &app_dirs)?
                        .context("no LLM provider is set; pass --provider or set llm.provider")?;
                    provider.extract(&prompt)?
                }
            };
            println!("{}", serde_json::to_string_pretty(&checked)?);
        }
        Command::Sweep => {
            for meeting in pipeline.store().unfinished()? {
                match pipeline.run(meeting.id) {
                    Ok(outcome) => print_outcome(&outcome, &time_zone),
                    Err(error) => println!("{:>4}  run stopped: {error}", meeting.id),
                }
            }
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
    Ok(pipeline.record(InputKind::TranscriptonicPayload, &payload_path, &transcript)?)
}

// Copies the log into the app's huddle folder under its start time, unless it is already there.
// A huddle the app skipped as too short is already there, and ingesting it writes its note.
fn ingest_huddle(
    pipeline: &Pipeline,
    app_dirs: &AppDirs,
    log_file: &Path,
) -> anyhow::Result<MeetingId> {
    let log = std::fs::read(log_file)
        .with_context(|| format!("could not read {}", log_file.display()))?;
    let transcript = sources::slack_huddle::parse(&log)?;
    let log_path = app_dirs
        .huddle_log_dir()
        .join(sources::slack_huddle::log_file_name(transcript.started_at));
    let is_in_place = std::fs::canonicalize(log_file).ok() == std::fs::canonicalize(&log_path).ok();
    if !is_in_place {
        std::fs::copy(log_file, &log_path)
            .with_context(|| format!("could not copy the log to {}", log_path.display()))?;
    }
    Ok(pipeline.record(InputKind::SlackHuddleLog, &log_path, &transcript)?)
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum InputKindArg {
    TranscriptonicPayload,
    SlackHuddleLog,
}

fn check_saved_reply(prompt: &Prompt, reply_file: &Path) -> anyhow::Result<CheckedNotes> {
    let reply = std::fs::read_to_string(reply_file)
        .with_context(|| format!("could not read {}", reply_file.display()))?;
    // A saved reply gets no repair turn, so it is judged as the last reply.
    match extraction::judge(&reply, prompt, Turn::Last) {
        Verdict::Accepted {
            notes,
            dropped_caption_fixes,
        } => {
            for error in dropped_caption_fixes {
                eprintln!("caption fix dropped: {error}");
            }
            Ok(notes)
        }
        Verdict::Repair { errors } | Verdict::Rejected { errors, .. } => anyhow::bail!(
            "the reply does not fit this meeting:\n- {}",
            errors.join("\n- ")
        ),
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProviderArg {
    Anthropic,
    OpenaiCompatible,
    ClaudeCode,
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
    if let (JobState::ExtractionPending, Some(retry_at), Some(error)) = (
        meeting.state(),
        meeting.next_extraction_at,
        &meeting.extraction_error,
    ) {
        let retry_at = retry_at.to_zoned(time_zone.clone()).strftime("%H:%M %Z");
        println!(
            "      extraction failed {} times, next try at {retry_at}: {error}",
            meeting.extraction_attempts
        );
    }
    if let Some(note_path) = &meeting.note_path {
        println!("      note: {}", note_path.display());
    }
}
