# meeting-capture

Rust (Tauri 2) tray app for Mac and Windows: call detection, two-stem audio capture, local
speech-to-text, and a pipeline that writes meeting notes into `~/knowledge`. Read `docs/` first.

## Where things live

| | |
| --- | --- |
| `docs/` | Overview, architecture, roadmap. The current plan. |
| `~/knowledge/areas/infrastructure/meeting-recording/local-capture/` | Research behind the plan: tool comparisons, costs, sources. `10-desktop-app-plan.md` is the detailed plan. |
| `scratch/` | Working scripts and dumps, one `YYYY-MM-DD-what/` per job. Gitignored. |

## Conventions

- Mac is the primary dev machine; every non-capture crate must also build on Windows.
- Vendored Anarlog crates (MIT, <https://github.com/fastrepl/anarlog>) keep the licence and record the
  upstream commit in their README.
- Stored times are UTC; anything reported to me is IST.
- Nothing committed refers to `scratch/`, since it is never committed.
- Docs never link vault files: people reading the repo do not have them.
- Never write media into the vault. Client call recordings never go into the repo.
- Stay on Tauri 2 stable. Tauri 3 is alpha.
- No tests until phase 8 (pre-v1 hardening). `just check` is the gate. The Check workflow runs it
  on each push, with clippy on Windows. A `v*` tag runs the Release workflow.
- The repo is public on GitHub. No secrets, client data or OAuth client secrets in commits.
- Logs stay OpenTelemetry-friendly: `tracing` spans, semantic-convention field names, no OTel SDK yet.
- LLM calls go through rig and work with `anthropic` or any `openai-compatible` endpoint. No
  provider-specific code outside the provider setup.
- Use rig's `prompt_typed`, never its `Extractor`: the Extractor forces `tool_choice` any, which current
  Claude models reject.
