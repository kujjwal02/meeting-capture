# Libraries

Rule: use a maintained library unless none fits. Versions and 90-day download counts come from
crates.io on 2026-10-17.

## Phases 0 and 1

| Job | Pick | Version | Why | Ruled out |
|---|---|---|---|---|
| App shell | tauri + plugins autostart, single-instance, notification, opener, log | 2.12 | Stable line, tray and plugins maintained by the Tauri team, 14.6M downloads in 90 days | egui/iced: no autostart, single-instance or notification plugins. Tauri 3: alpha |
| Async runtime | tokio | 1.53 | Tauri, axum and reqwest all run on it | None viable for this stack |
| Webhook server | axum + tower-http | 0.8 / 0.7 | Runs on Tauri's tokio runtime; tower-http adds body limits and request tracing | actix-web: its own runtime, a second executor in the process |
| HTTP client | reqwest | 0.12 | Default Rust HTTP client, 220M downloads in 90 days. 0.12 matches oauth2 5.0 | - |
| LLM API | rig-core + rig-agent, prompt_typed only | 0.44 | One code path for two provider kinds chosen in settings: anthropic (default, Haiku 5.5) and openai-compatible (base URL + model: OpenAI, OpenRouter, Groq, z.ai GLM, Ollama, LM Studio). For OpenAI-style endpoints rig sends response_format json_schema with strict true (providers/openai/wire/chat.rs:359); for Anthropic it sends output_config. Breaking 0.x releases are accepted: we update our code on each upgrade | rig Extractor: forces tool_choice any, which current Claude models reject. genai: no typed helper or agent layer. llm, swiftide, aisdk: drop the schema on the Anthropic path. async-openai: OpenAI only |
| Output validation | garde |  | The pydantic half: field rules on the extraction struct; schemars 1 reads #[garde] attributes into the schema | validator: works too, older derive API. nutype: supports schemars 0.8 only |
| Structured output schema | schemars | 1.2 | Generates the JSON schema from the Rust extraction type, so schema and parser cannot drift | - |
| Google sign-in | oauth2 | 5.0 | PKCE, state, code exchange and refresh; a refresh failure comes back as an error, so a revoked account becomes "sign in again" | yup-oauth2 12.1: no PKCE or state, waits forever when the user cancels consent, and a failed refresh opens a browser from a background run |
| Calendar API | reqwest (blocking, native-tls) | 0.12 | One call, events.list, read into seven small serde types. 0.12 because oauth2 5.0 uses it. native-tls uses the OS TLS, so no ring or aws-lc-rs build | google-calendar3 7.0: built for yup-oauth2, adds a hyper stack and chrono |
| Secrets | keyring | 4.2 | macOS Keychain and Windows Credential Manager behind one API. Holds refresh tokens and the API key | tauri-plugin-stronghold: own encrypted vault file and password |
| Database | rusqlite (bundled) + rusqlite_migration | 0.40 / 2.6 | Synchronous, small, bundles SQLite so both OSes match | sqlx: compile-time query checks need a database at build time. diesel, sea-orm: ORM weight for 2 tables |
| Job execution | Own state table on rusqlite |  | See below: the one piece we write | apalis 1.0: released 2026-10-12, apalis-sqlite has 39k downloads in 90 days, and its task model does not give the per-meeting state the tray shows |
| Retry | backon | 1.6 | Async backoff with jitter | backoff: last release 2021 |
| Settings file | toml + directories | 1.1 / 6.0 | Plain TOML in the OS config folder, owned by the pipeline crate; the mc CLI reads it without Tauri | tauri-plugin-store: JSON tied to the Tauri app, the CLI could not read it |
| Time | jiff | 0.2 | Time zones built in (UTC stored, the configured zone shown), DST-safe arithmetic | chrono: needs chrono-tz for zones, weaker API. time: no zone database |
| Note rendering | minijinja + serde-saphyr | 3.0 / 2.0 | Runtime template, so the note shape changes without a rebuild. serde-saphyr writes the YAML frontmatter | askama: compiled-in templates. serde_yaml: deprecated. serde_yml: RUSTSEC-2025-0068, unsound |
| Atomic write | tempfile | 3.27 | NamedTempFile::persist renames into place on the same volume | atomic-write-file: writes the temp file inside the vault folder, where Syncthing sees it |
| File names | slug | 0.1 | Lowercase ASCII slugs; deunicode transliterates non-Latin event titles. 7.8M downloads in 90 days, last release 2024-08, a finished 100-line crate | sanitize-filename: keeps case and spaces. Own slugifier: would drop Hindi titles to nothing |
| Downloads watcher (fallback) | notify + notify-debouncer-full | 8.2 / 0.7 | Only if the webhook test fails | - |
| CLI | clap (derive) | 4.6 | Standard | - |
| Errors | thiserror in libraries, anyhow in binaries | 2.0 / 1.0 | Typed errors at crate edges, context in the app | color-eyre: last release 2025-05 |
| Logging | tracing + tracing-subscriber (JSON) + tracing-appender | 0.1 / 0.3 / 0.2 | OTel-friendly without OTel crates: one span per job step with meeting_id; field names follow OpenTelemetry semantic conventions (gen_ai.request.model, gen_ai.usage.input_tokens, http.response.status_code); JSON lines in daily rotated files. Exporting later is one tracing-opentelemetry layer | log + env_logger: no spans. OTel SDK and OTLP exporter now: nothing to send to yet |
| Tests | None until the pre-v1 hardening phase |  | Speed while the structure settles. Tests start before version 1 | - |

## What we write ourselves

- The job state table and its step runner: about 150 lines. Every library option is a generic
  queue, and the tray needs one row per meeting.
- TranscripTonic payload to transcript mapping.
- The calendar match rule: same Meet code, then largest overlap, then Meet link, then the
  account listed first.
- The note template and the extraction prompt.
- The loopback redirect listener for Google sign-in: about 100 lines on `std::net`.
- A garde check after rig's prompt_typed, with one retry that sends the validation error back.
  rig retries without error feedback.

## Later phases

| Phase | Job | Libraries |
|---|---|---|
| 2 | Call detection | Vendored Anarlog detect (MIT) |
| 3 | Mac capture | cidre 0.29, cpal 0.18, hound 3.5 |
| 4 | Local STT | sherpa-onnx 1.13 with Parakeet TDT 0.6B v3 |
| 5 | Windows capture | wasapi 0.26, cpal |
| 6 | Hinglish routing | whatlang 0.18 for the language check, reqwest for Soniox (no Rust SDK) |
| 7 | OBS | obws 0.15 |
