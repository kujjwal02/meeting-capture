//! Webhook probe: runs the receiver alone and prints each transcript it parses.
//!
//! `cargo run -p caption-receiver --example probe -- <payload-dir>` serves the webhook.
//! `cargo run -p caption-receiver --example probe -- <payload-file>` parses one saved payload.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::Context;
use caption_receiver::{DEFAULT_PORT, WEBHOOK_PATH};
use sources::Transcript;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().json().init();

    let payload_path: PathBuf = std::env::args_os()
        .nth(1)
        .context("usage: probe <payload-dir | payload-file>")?
        .into();
    if payload_path.is_file() {
        let payload = std::fs::read(&payload_path)?;
        let transcript = sources::transcriptonic::parse(&payload)?;
        print_summary(&payload_path, &transcript);
        return Ok(());
    }
    let payload_dir = payload_path;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT));
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind {address}"))?;
    println!("listening on http://{address}{WEBHOOK_PATH}");

    let (sender, mut received) = mpsc::channel(8);
    let server = tokio::spawn(caption_receiver::serve(listener, payload_dir, sender));
    while let Some(captions) = received.recv().await {
        print_summary(&captions.payload_path, &captions.transcript);
        // The probe keeps no job store; the saved payload is the record.
        let _ = captions.recorded.send(Ok(()));
    }
    server.await??;
    Ok(())
}

fn print_summary(payload_path: &Path, transcript: &Transcript) {
    println!(
        "{}: {:?} {:?} {:?}, {} to {}, {} segments, {} chat messages",
        payload_path.display(),
        transcript.platform,
        transcript.title,
        transcript.meeting_url,
        transcript.started_at,
        transcript.ended_at,
        transcript.segments.len(),
        transcript.chat_messages.len(),
    );
    for segment in transcript.segments.iter().take(5) {
        println!(
            "  [{:.1}-{:.1}] {}: {}",
            segment.start_s, segment.end_s, segment.speaker, segment.text
        );
    }
}
