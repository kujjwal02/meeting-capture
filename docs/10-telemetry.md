# Telemetry

Every Rust process logs through one `telemetry` crate. The goal: one saved debug report holds
enough evidence to name the failing layer, with no second call to reproduce the bug. Nothing
leaves the machine.

Roadmap phase 3d runs in six steps. Steps 1 to 4 are built. Steps 5 and 6 are planned and
marked as such below.

| Step | Delivers | State |
|---|---|---|
| 1. Core | The `telemetry` crate in every binary, correlation ids, the flight recorder with its triggers, daily log files | Built |
| 2. Debug mode | A tray toggle that turns every process's log file to DEBUG for 24 h. mc-meet follows over native messaging | Built |
| 3. Metrics and environment | Gauges every 10 s, or every 1 s in a call: audio levels and routes, caption channel series, video fps, host CPU and memory, pipeline decisions. `env.jsonl` with devices, permissions and versions | Built |
| 4. Report and tools | A report bundle for a call with its job, or for the last 30 minutes. `mc debug timeline` and `mc debug export-otlp` | Built |
| 5. The 0.8.0 caption bug | One call in debug mode on the caption bug. It checks for missing acks on Meet's channel, captions hidden in `control` frames, and throttling in hidden-box mode | Planned |
| 6. Detectors | One trait for every invariant over metrics and events. A firing raises a named problem and dumps every recorder. `mc debug replay` runs them over a saved bundle | Planned |

## Processes

| Process | `service.name` | Log file |
|---|---|---|
| Tray app | `app` | `app-<date>.jsonl` |
| The app's binary in host mode, one per Chrome profile | `native-host` | `native-host-<date>.jsonl` |
| `mc-capture audio` and `mc-capture video` | `mc-capture` | `mc-capture-<date>.jsonl` |
| `mc` CLI | `mc` | `mc-<date>.jsonl` |

All files go to `logs/` in the app data folder. The date is the UTC date. A process opens a new
file at UTC midnight. It then deletes its own service's older files and keeps 14: today's and the
13 days before. A line stamped just before midnight that arrives after the switch goes into the
new file.

Several processes of one service append to the same file. Each line is one `write` on a file
opened for append, so lines never interleave.

At launch the app deletes the plain text logs of older versions: `app.log`, `native-host.log`,
`mc-capture.log` and `mc-capture.log.1`.

The app passes `--log-dir` and `--debug-dir` to every `mc-capture` it starts. Run by hand without
them, the helper logs to stderr. A log folder that does not open also sends the log to stderr. The
app sends the helper's raw stderr, such as a runtime crash message, to `mc-capture-stderr.log`.

mc-meet sends a `telemetry` message once a second while its tab is in a call. It carries each
caption channel's counts as typed fields. The app builds a metric row from them and writes it into
the call's `metrics.jsonl` with `service.name` `mc-meet`. It logs the message's events in the app
log with target `mc_meet`. The [Meet extension design](09-meet-extension.md#telemetry)
lists the rows and events.

## The `telemetry` crate

Portable: no Tauri and no OS calls. It builds on Windows.

| Module | Job |
|---|---|
| `init` | Installs the subscriber: the file layer behind a reload filter at INFO, the recorder layer at DEBUG, and a panic hook. Returns a handle that sets the file level |
| `recorder` | The flight recorder ring and its dump |
| `fields` | Field names, spelled once |
| `trace` | Trace and span ids |
| `debug_mode` | The closed `DebugMode`, `Off` or `On { until }`, and the switch that sets the file level from it |
| `metrics` | `MetricsScope`, its gauges and counters, the sampler thread that writes `metric` rows, and `RowPoints` for a source that times its own rows |
| `env` | `EnvSnapshot` and its writer. The OS probes stay in the crates that own them and fill it |

Only the app's own crates log at DEBUG into the recorder. Dependencies stay at INFO: their
DEBUG output (HTTP, TLS, the webview) would fill the ring in seconds. An event is the app's own
when its source path is relative. Cargo passes rustc a relative path for a workspace member and
an absolute one for a registry or git dependency, so no crate list needs upkeep.

## Line format

One JSON object per line. The `record` field names the kind.

| `record` | Holds |
|---|---|
| `resource` | The process's resource. First in each log file a process opens |
| `dump` | The resource and the reason. First in each recorder dump |
| `log` | One event |
| `span` | One closed span inside a trace |
| `metric` | One interval of gauges and counters. See "Metrics and environment" |
| `env` | One environment snapshot |

A `log` line:

```json
{"record":"log","timestamp":"2026-11-01T04:49:59.18Z","severity_text":"INFO",
 "severity_number":9,"body":"capture process stopped","service.name":"app",
 "service.instance.id":"1cf6cad7bbb3c072672fcdc9f1ab50a1","scope":"meeting_capture::call_capture",
 "trace_id":"cc4d9d32da813e6ab2714e857b0b38d0","span_id":"de8f94081121dfb1",
 "span":{"name":"capture.recording"},"attributes":{"call.id":"/.../20261101T0449Z-meet.jsonl"}}
```

- `timestamp` is UTC.
- `severity_number` follows OpenTelemetry: 1 trace, 5 debug, 9 info, 13 warn, 17 error.
- `scope` is the module that logged.
- `attributes` holds the fields of every enclosing span, outer first, then the event's own. A line
  inside a call's span therefore carries `call.id`.
- `service.name` and `service.instance.id` are on every `log` and `span` line. The instance id is
  new on every process start and joins a line to its resource line.

A `span` line has `span.start_timestamp`, `span.parent_span_id` and the span's own fields. Only
spans inside a trace write one.

A field takes its OpenTelemetry semantic convention name where one exists: `exception.message`,
`error.type`, `file.path`, `gen_ai.*`. Otherwise it takes a domain prefix: `call.id`,
`segment.number`, `meeting.id`, `audio.*`, `video.*`, `browser_call.*`. Newer names start with
`mc.`, such as `mc.input.revision` and `mc.recorder.reason`.

## Correlation

| Id | Set by | Starts a trace |
|---|---|---|
| `call.id` | The call's source log path. The app's call spans carry it. The app passes it to `mc-capture` in `start`, and the helper opens a `capture.segment` span with it | Yes |
| `meeting.id` | The meeting's row in the job store. The pipeline's run and step spans carry it, so every line inside a step does. `llm.extract` gets it from its step. `google.sign_in` belongs to no meeting and has none | Only with `mc.input.revision` |
| `mc.input.revision` | The `pipeline.attempt` span, once per attempt on one input revision | With `meeting.id` |
| `job.step` | The step span: `matched`, `extracted`, `written` | No |

A trace id is the first 16 bytes of a SHA-256 over the id it stands for. The app and
`mc-capture` derive the same trace id for a call, with no propagation code. A pipeline attempt's
trace comes from `meeting.id` plus the input revision, so a rerun on a new capture is a new
trace.

A span inherits its parent's trace. A span with no traced parent starts one from its own
fields and has no parent span id. An event's own `call.id` wins over the trace of the span
around it. A line carries the span id of its innermost span in its own trace, or none: an event
with `call.id` outside any span gets the call's trace id and no span id.

Span ids are W3C 8-byte ids, unique per process and seeded from the instance id. `tracing`
reuses its own span ids, so they are not used.

## Flight recorder

Each process keeps its last 10 minutes or 8 MB of lines in memory, whichever is less. It holds
DEBUG lines from the app's crates even while the log file is at INFO.

A dump appends the lines since the last dump into the same file, after a `dump` line with the
reason. Two problems in one call then keep both windows. A dump writes through a buffer on a file
opened for append and never reads the file back. A dump file stops growing past 32 MB. It then
ends with a `dump` line with `mc.recorder.is_file_full`, and later dumps add nothing.

A dump shares the ring's lines and copies none. The audio helper dumps on its own thread and
waits for that thread before it exits.

| Trigger | Process | Dump file |
|---|---|---|
| A call problem starts | App | `debug/<call>/recorder-app.jsonl` |
| A call problem starts | Audio helper, asked with `dump_recorder` | `debug/<call>/recorder-mc-capture-audio.jsonl` |
| A capture helper exits with an error, hangs or does not start | App | `debug/<call>/recorder-app.jsonl` |
| A pipeline run fails | App or `mc` | `debug/meeting-<id>/recorder-<service>.jsonl` |
| A panic | Any | `debug/panic-<time>-<service>-<pid>/recorder-<service>.jsonl` |
| "Save debug report" | App | The call's folder, then copied into the report |

`<call>` is the call log's file name without its extension, which also names the call folder.
The video helper has no dump command yet.

The report copies the call's debug folder through the same redaction as every other file. It
skips hidden and `.partial` files. From each recorder dump it keeps only the dump headers and the
lines in the call's trace or with its `call.id`. A meeting's dump keeps the lines with its
`meeting.id` or in the trace of one of its runs. A report without text keeps only strings under
keys known to hold times, ids, versions and codes. A kept path starts at `~` in place of the
home folder. A line from a dependency also loses its body and `file.path`. Object keys stay:
the app builds them from field names and closed sets of labels, never from page or user text.

## Metrics and environment

The `metrics` module writes `metric` rows. The `env` module writes `env` rows.

### Metric rows

A `MetricsScope` owns a sampler thread. Each interval it writes one row with each gauge set
since the last row and each counter. A gauge nobody set in the interval is left out, so a
stalled source shows as a hole. Counters are cumulative and carry `start_timestamp`.

A source that brings its own timing builds `RowPoints` and writes them with `write_row`, one row
per report, stamped with the report's time. The audio heartbeat's levels and mc-meet's counts
take this path. Points keep the sampler's keys and shape. A scope from `open_rows` has no sampler
and takes only such rows.

Every scope on one debug folder shares that folder's size budget, so the cap warning comes once
per folder.

```json
{"record":"metric","timestamp":"2026-11-01T04:10:00.000Z","service.name":"app",
 "service.instance.id":"1cf6cad7bbb3c072672fcdc9f1ab50a1","trace_id":"cc4d9d32da813e6ab2714e857b0b38d0",
 "interval_ms":1000,"points":{"mc.audio.level.rms|mc.audio.stem=mic":{"value":-38.2,
 "unit":"dBFS","attributes":{"mc.audio.stem":"mic"}}}}
```

A point's key is its metric name. A point with attributes adds them, sorted: `name|k=v,k=v`.

| Scope | File | Interval | Opened by |
|---|---|---|---|
| A recording call | `debug/<call>/metrics.jsonl`, with the call's `trace_id` | 1 s | The app, at the recording's start. It closes with the recording |
| The app's life | `logs/metrics-<UTC date>.jsonl` | 10 s | The app, at launch |

mc-meet brings its own counts. The app builds their row and writes it with
`MetricsScope::write_mc_meet_row` into the recording call's scope, so it carries the call's
`trace_id`. The method sets `service.name` to `mc-meet` and writes no `service.instance.id`. A
call that records no audio has no open scope. The app then keeps one rows-only scope for that
call, with the call's `trace_id` and no sampler thread. It closes the scope once no row came for
2 minutes.

The app takes a scope out of its lock before it writes, so no file write holds the lock. A scope
that closes joins its sampler on a blocking thread, off the async runtime.

| Metric | Unit | Attributes | Source |
|---|---|---|---|
| `mc.audio.level.rms`, `mc.audio.level.peak` | dBFS | `mc.audio.stem`: `mic` or `remote` | The audio helper's heartbeat, one row per heartbeat |
| `mc.video.frames` | {frame} | `mc.video.frame.kind`: `encoded` (new frames) or `repeated` | The window video helper's heartbeat |
| `process.cpu.utilization` | 1 | `service.name`, and `process.command` for a helper | The app, every 10 s, from a process's second sample on |
| `process.memory.usage` | By | The same | The app, every 10 s |
| `system.filesystem.free` | By | None | The app, every 10 s, for the data folder's disk |
| `mc.captions.frames` | {frame} | `mc.caption.channel.label`, `mc.caption.channel.id`, `mc.caption.channel.serial`, `mc.caption.channel.is_own`, `mc.caption.frame.kind`: `raw`, `parsed`, `control` or `rejected` | mc-meet, every 1 s |
| `mc.captions.acks` | {ack} | The channel attributes | mc-meet, every 1 s |

Host gauges go into the day's file and into the call's rows. In a call's rows they appear every
10th second. Samples sit on a fixed 10 s grid, half an interval after the day's scope opens, so
each falls mid-row. A process's first sample has no CPU point: sysinfo has no earlier CPU time
to measure from. A helper that exited leaves the probe at the next sample.

### Audio levels

The stem writer sums the squares and the peak of each 16 kHz sample in its write loop. It
measures the sample as the file holds it, clamped and rounded, so a level never reads above
0 dBFS. The loop runs on the drain thread, never in the audio callback. Measuring adds no pass
and no allocation. The audio helper's heartbeat comes every 1 s and carries the second's RMS and
peak in dBFS for each stem. Silence reads -120 dBFS, since JSON has no minus infinity.

The app writes each heartbeat's levels as a row of their own, stamped when the heartbeat
arrives. A call's `metrics.jsonl` therefore holds about one level row and one sampler row a
second. Gauges read on the sampler's grid would drop a second's level or overwrite it whenever
a heartbeat lands near a row's edge.

A 1 s heartbeat costs one line of about 250 bytes a second on the helper's pipe. A 5 s
heartbeat with an array of five levels would save four small writes a second. The app would then
need a second write path that back-dates rows, and levels would arrive 5 s late. The video
helper's heartbeat comes every 1 s too.

### Environment rows

```json
{"record":"env","timestamp":"2026-11-01T04:10:01.2Z","reason":"call_start",
 "service.name":"app","service.version":"0.9.0","os.type":"darwin","host.arch":"arm64",
 "os.version":"27.0","mc.audio.input.default":{"name":"AirPods Pro","transport":"bluetooth"},
 "mc.audio.output.default":{"name":"AirPods Pro"},
 "mc.audio.mic.chosen":{"name":"MacBook Pro Microphone","reason":"default_is_bluetooth"},
 "mc.permissions":{"accessibility":"granted","audio_capture":"granted","microphone":"granted"}}
```

| `reason` | When | Files |
|---|---|---|
| `app_start` | At launch | The day's env file |
| `call_start` | The mic's first stream in a call, or `mic_unavailable` when the call has no safe mic | The call's `env.jsonl` and the day's env file |
| `change` | A later mic stream, a `device_changed` on the mic or a new segment, whose mic or route differs from the last row's | The same |

The mic's `reason` is the mic rule's closed `MicPickReason`. The audio helper sends it in
`recording` and `device_changed`, with the default input and output it read. Fields a probe
cannot fill are absent. Power, lid, displays and browser versions have no probe yet.

### Pipeline decisions

Each run logs `transcript source picked` with `mc.decision.kind` `source_pick`: the source, its
speaker naming and its segment, chat and attendee counts. Each calendar match logs `calendar
match decided` with `mc.decision.kind` `calendar_match`: the candidate count and, for the winner
and the runner-up, the rank parts (same Meet code, overlap in seconds, Meet link, account
rank). Neither holds a title, a name or an email.

## Debug mode

Debug mode turns every log file to DEBUG for 24 hours. The tray item "Debug mode (24 h)" turns it
on and off. A check marks it while on, and the tooltip says "Debug until 02 Nov 14:30 IST" in
the settings' time zone.

The settings file holds the end time:

```toml
[debug]
enabled_until = "2026-11-02T09:00:00Z"
```

The value is RFC 3339 UTC. Off removes the key. A value that is not a time fails the settings
load with an error that names `debug.enabled_until`. A time more than 24 hours ahead reads as 24
hours from the read; the app then stores that time. An edit of `[debug]` alone validates only
that table, so the toggle works while another setting fails to resolve.

| Process | How it follows |
|---|---|
| App | Reads the setting at launch and every 30 s, so a hand edit applies too, and passes each change on. Sets its file level at each toggle. At the end it removes the key, then turns debug mode off |
| Audio helper | The app sends `set_debug` with `until` right after `start` and at each change |
| Native host | Reads the setting at start and again while it relays, at most every 60 s |
| mc-meet | The app sends `{"type":"debug_mode","until":...}` after `welcome` and at each change. The worker stores `until` in `chrome.storage.session` as `debugModeUntil`, and removes it when the native port closes. The popup shows "Debug mode on until 14:30" in local time |

The video helper and `mc` do not follow it yet. A saved call report's `call-status.json` holds
`debug_mode_until`, null when off.

The recorder keeps DEBUG lines either way. Debug mode adds them to the log file, so they outlive
the 10-minute ring.

## Storage

- `logs/<service>-<UTC date>.jsonl`: 14 days.
- `debug/<call>/`, `debug/meeting-<id>/`, `debug/panic-<time>-<service>-<pid>/`: the app
  deletes each folder 14 days after its last change, at launch and once a day.
- `logs/metrics-<UTC date>.jsonl` and `logs/env-<UTC date>.jsonl`: 14 days.
- A debug folder takes no more metrics rows, frames or page snapshots once it holds 50 MB. The
  app logs this once per folder. Recorder dumps still go in.
- The retention pass then deletes the oldest folders in `debug/` and `debug-reports/` until
  they hold 1 GB or less.

## Report and tools

"Save debug report" in the tray or the popup saves a bundle for the current or last call. "Save
debug report of the last 30 min" in the tray saves one for no call in particular. Both go to
`debug-reports/` and pass every file through the same redaction.

| File in the bundle | Call | Last 30 min |
|---|---|---|
| `call-status.json` | The call on the status board, and the files cut at the size cap | The window's start and end, and the files cut at the size cap |
| `app-log.jsonl` | App lines with the call's id, trace id or session ids | |
| `<service>-log.jsonl` | `mc-capture` lines of the call's trace or `call.id` in the call's time span, 1 minute wider on each side, with the untraced lines of those helpers | Every service's lines in the window |
| The call log | Whole | |
| `audio.json`, `video.json`, `finalize.json` | From the call folder, when present | |
| `job.json` and `debug-meeting-<id>/` | The job row and debug folder of the meeting made from the call, its dumps cut to the meeting's lines | |
| `env.jsonl` | | Environment lines in the window |
| `metrics.jsonl` | In `debug/`, the call's rows | Host metric rows in the window |
| `debug/` | Every file of the call's debug folder, after a new app recorder dump | A new app recorder dump |

Without "with text", the redaction also covers the new files: device names, meeting titles,
note paths and error messages become their length. Units, stems, codecs, metric attributes and
version fields stay.

The call's time span runs from its start to its end on the status board. Once the board lost
the call, it runs from the call log's first line to its last. A log file comes with the
`resource` line of each process that has a line in the report, and only the daily files of the
days the span touches are read. Each copied file stops at 64 MB; `call-status.json` then names
it under `mc.report.truncated_files`.

`mc` reads a bundle without the app. A bundle is a report folder, a debug folder or one `.jsonl`
file. It reads every `.jsonl` file under it and keeps each line once, since a recorder dump
repeats the log file. Two lines are the same when they hold the same fields and values, in any
key order: a process encodes each event once for its log file and its recorder. A line with a
`record` it does not know, or without a field its kind needs, is skipped. `mc` names the first
five on stderr with the file and line, then prints how many it skipped. Reports saved before
this format still read: their `tracing` lines become log lines and the call log's events become
`mc-meet` lines. App log lines with scope `mc_meet` are mc-meet's events, so they count as
service `mc-meet`.

| Command | Prints |
|---|---|
| `mc debug timeline <bundle>` | One row per line in IST: time, service, severity, then the body, a metric summary or the environment |
| `mc debug timeline <bundle> --by-minute` | One row per minute: speech seconds from `mc.meet.self_speaking`, mean mic and remote dBFS over power, new caption frames per channel and kind, acks, caption presses, problems. Flags a minute with over 20 s of speech and no parsed frame |
| `mc debug timeline <bundle> --service <name>` | Only that service's lines, such as `app`, `mc-capture` or `mc-meet` |
| `mc debug export-otlp <bundle> --out <file>` | OTLP/JSON for a Collector's `otlpjsonfile` receiver |

No source writes `mc.meet.self_speaking` yet: Meet's speaking indicator has nothing stable to
read. The speech column then shows `-` and no minute is flagged.

A counter's first row in the bundle is its baseline, so a counter that started before the
bundle adds nothing in that minute. A counter whose `start_timestamp` falls inside the bundle,
or that started again, counts its whole total.

The export writes one export request per line: logs, spans and metrics of one resource each.
A resource is one process, from its `resource` line. mc-meet's rows get a resource of their
own, with `service.name` `mc-meet`. An environment line goes to the resource of the process
that wrote it. Without that process's `resource` line, the resource comes from the
environment line's own `service.*`, `os.type` and `host.arch`. Trace ids are 16 bytes and span
ids 8 bytes of hex; an id of another shape is dropped. Times are Unix nanoseconds as strings.
An attribute above the 64-bit signed range goes as a string, since OTLP's integers are signed.
A point with a `start_timestamp` is a cumulative monotonic sum, any other a gauge. Recorder
dumps and environment lines become log records.

## OpenTelemetry

The lines follow the OpenTelemetry data model. The app ships no OpenTelemetry SDK.

| Concept | Mapping | State |
|---|---|---|
| Resource | `service.name`, `service.version`, `service.instance.id`, `process.pid`, `os.type`, `host.arch`. In each file's `resource` line and each `dump` line | Built |
| Traces | A call is one trace; a pipeline attempt is one trace. `span` lines carry W3C span and parent ids | Built |
| Logs | `log` lines with severity, body, attributes, trace and span ids | Built |
| Metrics | Gauges and counters with OpenTelemetry names and UCUM units, such as `process.memory.usage` (By) and `mc.audio.level.rms` (dBFS) | Built |
| Export | `mc debug export-otlp <bundle>` writes OTLP/JSON lines for a Collector's `otlpjsonfile` receiver. No network code in the app | Built |

A recorder dump has the same line shape as a log file, so one converter reads both.

## Privacy

Logs, dumps and reports stay in the app data folder. Caption text, LLM text and frame bytes
leave it only in a "with text" report the user saves. Nothing is uploaded.

## Cost

The recorder builds every DEBUG line of the app's crates, also while the log file drops it.
Audio callbacks never log per buffer.

Memory, measured on macOS with 500-byte lines in a release build:

- A full 8 MB ring adds 11 MB of resident memory.
- A dump adds under 1 MB: it shares the ring's lines.
- Ten rounds of 60,000 lines with a dump each settle at 21 MB over the process's start. The
  allocator keeps freed pages, so the figure stays there and does not grow.

The CPU budget is under 1% per process with debug mode off, to be measured on a 30-minute call.
