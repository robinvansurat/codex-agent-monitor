# Architecture

`codex-agent-monitor` is structured into small, deterministic layers:

- `cli`: argument parsing (`clap`) and command normalization (`probe`/`watch`/`tui`)
- `config`: resolve `CODEX_HOME`, `sqlite_home`, and read config files
- `claude`: read-only ingestion for Claude Code transcripts and the live session registry
- `kiro`: read-only ingestion for native CLI, ACP worker, and legacy SQLite sessions
- `db`: read-only SQLite ingestion with schema/table introspection
- `rollout`: stream parser for thread JSONL files
- `runtime`: optional one-shot runtime event overlay (`model/rerouted`)
- `tree`: parent-edge precedence and cycle-safe hierarchy
- `observer`: snapshot assembly, evidence labeling, filtering, ordering
- `output` / `tui_ui`: formatter and interactive renderer

## Data sources and precedence

- `state_5.sqlite` (required base of truth for thread list, timestamps, agent metadata, rollout file paths, parent edges)
- rollout JSONL files referenced by thread rows (`rollout_path`)
- optional runtime overlay source (`--runtime-events`)
- Claude Code transcripts and the `sessions/<pid>.json` registry when `--provider`
  is `claude` or `all`
- Kiro session files and the legacy Kiro conversation database when `--provider`
  is `kiro` or `all`
- optional native Kiro account lookup through `kiro-cli acp` for plan credits
- Ollama Desktop's local SQLite database and live `ollama run` process list when
  `--provider` is `ollama` or `all`, plus bounded read-only GET access to the
  local `/api/ps` endpoint for server availability and loaded model names

No writes are made to Codex files, DB, or runtime processes.

## Claude Code ingestion

Claude Code sessions use namespaced IDs (`claude:<session-id>`) and remain
independent roots. Each transcript under
`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` is streamed line by line
with a line cap, and at most 500 transcripts are read, newest first by file
modification time. The encoded directory name is only an index; `cwd` is taken
from the records themselves because the encoding is not reversible for paths
that already contain `-`. Malformed lines are counted and skipped rather than
failing the read.

`parentUuid` is a message-level link inside one transcript and subagent turns
are inline `isSidechain` records, so neither is promoted to a thread parent
edge. Sidechain records contribute token counters but do not set the session's
model, effort, `cwd`, branch, or entrypoint.

Token counters are summed across responses and de-duplicated by
`message.id`, because Claude Code writes one record per content block and each
repeats the response's usage. `context_window` is left unknown; Claude Code
does not persist one, so no `context_usage` is derived.

Plan utilisation is read from the Claude desktop app's
`plan-usage-history.json` (`<config dir>/Claude/`), a bounded rolling list of
`{t, org, u:{fh, sd}}` samples. The newest sample with at least one percentage
becomes the primary (five-hour, 300 minutes) and secondary (seven-day, 10080
minutes) account windows. Percentages outside 0-100 are discarded, `resets_at`
is left unknown because the file records no window start, and the `org`
account identifier is never retained.

`sessions/<pid>.json` registers a running CLI process. An entry only makes a
session `running` while its PID is still visible, reusing the same liveness
check as the Kiro session lock. Stale entries are ignored, and a live process
means an open session rather than an in-flight turn.

Only telemetry is retained: session ID, `cwd`, git branch, entrypoint, model,
effort, timestamps, `stop_reason`, tool-call names, and tool-result status.
Prompts, assistant text, thinking, tool inputs and results, attachments, file
snapshots, custom titles, and agent names are never retained.
`~/.claude/history.jsonl` is not read because its rows carry prompt text.

## Ollama ingestion

Ollama Desktop chats use namespaced IDs (`ollama:<chat-id>`) and remain
independent roots. The reader opens the Desktop database read-only with
`query_only`, uses the known `chats`/`messages` metadata shape, limits traversal
to 500 rows, and never selects titles, prompts, responses, thinking, or
tool-content columns. Missing schemas produce warnings and no rows. Chat state
is `running` only for a recent unfinished thinking interval, `idle` for
persisted message activity, and `unknown` for empty chats or missing evidence.
Loaded models from `/api/ps` also produce one row per resident model
(`ollama:api:<model>`, source kind `ollama_api`, state `running`). Residency is
keep-alive evidence that the model was used recently by some local client; it is
not proof of an in-flight request, and the row's state evidence says so. These
rows carry only the model name and the observation time.

Live terminal rows use namespaced IDs (`ollama:cli:<pid>`) and are running only
because a matching `ollama run <model>` process exists. Process rows retain the
PID, model token, elapsed-derived timestamps, and safe source label; prompts,
remaining arguments, tokens, context, and parentage are discarded. Desktop
rows use `ollama_desktop`, while terminal rows use `ollama_cli`.

`--provider ollama` is a backend filter as well as a source filter: a Codex
thread whose persisted `model_provider` starts with `ollama` is included, keeps
its own Codex source kind, and shows its model as `<model> via Ollama`. Codex
threads on any other provider are excluded, and `--provider codex` is unchanged.
Codex config is loaded for this provider only to resolve the state database
location; Codex model defaults are never applied to Ollama-sourced rows.

The JSON schema is `codex-agent-monitor.probe.v2`. Current thread state is
limited to `running`, `idle`, and `unknown`; terminal lifecycle results are
retained separately as `last_terminal_event`. A shared 15-minute freshness
window drives the evidence-aware `activity_signal` (`recent`, `stale`, or
`unknown`) without asserting that activity means a running thread.

## Read-only database behavior

SQLite is opened with:

- `SQLITE_OPEN_READ_ONLY`
- `query_only = ON`
- `busy_timeout` (short)

The code intentionally does not issue `PRAGMA journal_mode = WAL` because that is a mutating pragma in some environments. `read_only` access still observes WAL state while respecting this constraint.

Schema and columns are discovered dynamically; missing tables/columns are tolerated with warnings.

## Rollout parsing details

Expected line shape is tolerant of wrappers:

- top-level object with `timestamp`, `type`, `payload`
- `type=session_meta` with canonical identity in `payload.id`
- optional `payload.source.subagent.thread_spawn.parent_thread_id`
- optional direct parent fields such as `payload.parent_thread_id`
- `type=event_msg` + `payload.type` for nested event kinds

Parsing rules:

- only lines matching canonical thread id are used for canonical metadata (nickname/role/path/parent)
- later copied metadata snapshots are ignored if `id` mismatches canonical identity
- malformed lines are diagnosed and skipped
- one-shot parsing allows a clean trailing unterminated record only with warning; otherwise tail malformed record is skipped and marked
- `task_complete`, `task_done`, and `turn_aborted` produce current `idle` state while preserving completed, failed, or interrupted terminal evidence
- a rollout ending in a start event remains `running`; old or missing lifecycle timestamps lower state evidence confidence

## Model/evidence precedence

Requested model/effort are derived as:

1. latest `turn_context` / `thread_settings_applied` in rollout stream order (supports `reasoning_effort` / `reasoningEffort`)
2. DB thread columns as durable fallback (`model`/`reasoning_effort` aliases)
3. unknown

Requested/Configured/Effective are separate fields:

- `configured`: best-effort from config files
- `requested`: durable thread settings or fallback DB values
- `effective`: runtime overlay only (`model/rerouted`), ephemeral

Requested and configured values include explicit source/confidence/detail metadata. Unknowns retain `source=unknown`.

## Tree assembly

Parent inference is two-phase with deterministic precedence:

1. explicit `thread_spawn_edges` (highest precedence)
2. structured source `source.subagent.thread_spawn...`
3. canonical session metadata (`payload.parent_thread_id` and `source.subagent` fallback paths)

Each candidate edge is cycle-checked before insertion. Orphans remain roots.
Explicit duplicate-child edges use a deterministic parent tie-breaker, and
visited guards bound depth/tree traversal even for cyclic input.

## Activity parsing

`recent_activity` captures only safe fields:

- `kind`
- optional `tool_name` (from common aliases and nested shapes)
- optional `status`
- `timestamp`

Thread snapshots expose latest 25 entries in ordering logic, with confidence lowered when timestamps are absent.

## Token usage parsing

`event_msg` records with `payload.type=token_count` are parsed for the latest valid
`info.total_token_usage` observation. The cumulative breakdown is retained as
evidence-aware `ThreadSnapshot.token_usage`; fields that are missing or malformed
remain unknown rather than being replaced with zero. `info.model_context_window`
is retained when present. Records from copied history before the canonical session
metadata boundary are ignored using the same stale-record guard as other rollout
facts. Account-level `rate_limits.credits` is not per-task consumption, so no task
credit or cost estimate is produced.

Kiro native session metadata is deliberately not coerced into this cumulative
model. Credit-denominated `metering_usage` is ignored for token accounting, and
all-zero native token counters remain unknown. When both a validated current
context percentage (`0..=100`) and positive model context window are persisted,
the observer emits a separate `ThreadSnapshot.context_usage` value. Its
`used_tokens_approx` is `round(percentage × window)` and is always labeled as
current, approximate context occupancy—not cumulative use, billing, or credits.
Malformed or incomplete pairs remain unknown.

## Watch model

Long-running refresh in `watch` is implemented as efficient polling with rollout cache:

- per-thread rollout parsing is cached by `(path, size, mtime)`
- unchanged files are not reparsed every cycle
- runtime overlay is refreshed from file once per cycle; stdin stream is rejected for watch to avoid blocking
- Codex CLI path/version detection is performed once when `Monitor` is created

## TUI behavior

- hierarchy rendering from built tree
- shows configured/requested/effective model and effort separately
- shows source labels for evidence (`state`, `nickname`, `role`, `parent`, `cwd`, `source_kind`)
- shows `RUNNING`/`IDLE`/`UNKNOWN` summaries plus latest terminal/activity evidence
- no mutation controls (read-only only)

## CLI/runtime limits

- `--runtime-events -` is accepted in `probe`
- `--runtime-events -` in `watch` returns explicit error
- unknown runtime notification types are ignored with warning
- runtime updates are not persisted and do not imply durability

Only Codex CLI `0.144.5` is explicitly validated. Parser shape tolerance is
intentional compatibility handling, not a supported version range.

## Kiro ingestion

Kiro native CLI sessions are read from `sessions/cli/<id>.json` and their
adjacent JSONL telemetry; ACP workers are read from the bounded
`sessions/<workspace>/sess_<id>/` layout. Legacy conversations are read from
`data.sqlite3` in read-only mode. Directory traversal is bounded to those
known layouts and symlinks are skipped. Kiro records use namespaced IDs
(`kiro:<native-id>`), are independent roots, and retain only safe telemetry.
Native session files win when the same native ID is also present in legacy
SQLite. Native CLI task files are read only from
`sessions/cli/<id>/tasks/<numeric-id>.json`; snapshots retain only numeric IDs
and normalized lifecycle statuses, never task subjects or descriptions. When no
valid native task files exist, the task observation has no value and carries a
safe availability detail; the monitor never synthesizes task entries.
`AssistantMessage`/`ToolResults` envelopes are reduced to tool names and result
statuses while tool IDs, purposes, inputs, and outputs are discarded. JSONL
stream order is preserved because native tool records may omit timestamps.
For native CLI sessions, a prompt newer than the latest terminal record is
classified as `running` only when the adjacent JSON lock contains a valid PID
for a live process. The same live lock without a pending prompt does not imply
work; missing, malformed, or stale lock evidence keeps a pending turn
`unknown`. Completed turns remain `idle`. Kiro lifecycle evidence is timestamp
ordered where timestamps exist, and incomplete or unsupported records remain
unknown with a warning.

Kiro account credits are a separate observed field. When enabled, the monitor
spawns `kiro-cli acp --agent-engine v3 --auth-method cli` in a neutral temporary
directory, sends `initialize`, waits for its response, then sends
`_kiro/account/getUsage`. It does not create a session, load a workspace, or
execute callbacks. Probe performs a bounded initial wait; watch and TUI refresh
in the background and cache successful results for five minutes. Explicit
`--kiro-home` data directories stay offline unless `--kiro-cli` is supplied.
