# Architecture

`codex-agent-monitor` is structured into small, deterministic layers:

- `cli`: argument parsing (`clap`) and command normalization (`probe`/`watch`/`tui`)
- `config`: resolve `CODEX_HOME`, `sqlite_home`, and read config files
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

No writes are made to Codex files, DB, or runtime processes.

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

## Watch model

Long-running refresh in `watch` is implemented as efficient polling with rollout cache:

- per-thread rollout parsing is cached by `(path, size, mtime)`
- unchanged files are not reparsed every cycle
- runtime overlay is refreshed from file once per cycle; stdin stream is rejected for watch to avoid blocking

## TUI behavior

- hierarchy rendering from built tree
- shows configured/requested/effective model and effort separately
- shows source labels for evidence (`state`, `nickname`, `role`, `parent`, `cwd`, `source_kind`)
- no mutation controls (read-only only)

## CLI/runtime limits

- `--runtime-events -` is accepted in `probe`
- `--runtime-events -` in `watch` returns explicit error
- unknown runtime notification types are ignored with warning
- runtime updates are not persisted and do not imply durability
