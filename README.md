# meeting-capture

Bot-free meeting transcripts for Mac and Windows. A Rust tray app detects calls, records the mic and
system audio, receives Google Meet captions, transcribes locally after the call, and writes a meeting
note into the Obsidian vault.

Status: phase 0. A workspace skeleton and a tray icon with a Quit item.

- [Overview](docs/01-overview.md): problem, goals, non-goals, budget
- [Architecture](docs/02-architecture.md): components, design rules, transcript format
- [Roadmap](docs/03-roadmap.md): phases, open questions, risks
- [Libraries](docs/04-libraries.md): crates per job, what was ruled out, what we write

## Development

Install the tools once (Mac):

```sh
brew install just lefthook taplo typos-cli biome rumdl cargo-deny cargo-shear
cargo install tauri-cli --version "^2" --locked
just hooks
```

`rust-toolchain.toml` pins Rust; rustup installs it on first build.

- `just check`: every format, lint, licence and advisory check. Run it before pushing.
- `just fmt`: rewrite files to the formatters' output.
- `just dev`: run the tray app.
