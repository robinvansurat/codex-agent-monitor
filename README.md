# codex-agent-monitor

`codex-agent-monitor` is an open-source read-only local monitor for Codex persisted state.

## What it is / is not

It is:

- a command-line utility to inspect persisted thread trees and runtime-visible metadata
- a lightweight cross-platform Rust binary (`Windows`, `Linux`, `macOS`)
- a read-only forensic view of durable + one-shot runtime event snapshots

It is **not**:

- a replacement for the Codex CLI or Desktop app-server
- a controller (no retry/abort/kill or mutation endpoints)
- a guaranteed real-time feed for transient active-tool state

## Features

- Probe persisted state and rollout logs with filters (`project`, `thread`, `state`, `role`, `all`, `depth`, `recent`, `--runtime-events`)
- Build deterministic parent/child thread hierarchy with explicit `thread_spawn_edges` precedence
- Parse rollout JSONL safely (tolerant of malformed lines, canonical thread identity checks, missing files)
- Expose evidence-aware snapshots and provenance (`source`, `confidence`, `observed_at`, `detail`)
- Expose the latest cumulative per-task token usage observation from rollout `token_count` events
- Show the latest account usage allowance observed in monitored rollout `rate_limits` events
- Optional TUI for navigation and thread details
- Optional `--runtime-events` overlay (`model/rerouted`) for ephemeral effective model/reroute facts

## Install/build

Requirements:

- Rust toolchain (local build verified with stable `1.97.1`)
- OS with terminal support (Windows and Unix-family tested)

```powershell
cargo build --release
cargo test --all-features --locked
```

Optional install:

```powershell
cargo install --path .
```

## CLI

Commands:

- `probe` — one-shot snapshot to human or JSON
- `watch` — periodic snapshot refresh
- `tui` — interactive terminal tree view
- no subcommand defaults to TUI

Global/command options include:

- `--project <path>`: filter by workspace/root path (matched against persisted `cwd`)
- `--thread <id>`: exact thread id
- `--state {running,idle,interrupted,failed,unknown}`
- `--role <name>`
- `--all` include historical scope; without it default scope is recent (`--recent`, default 1440 min)
- `--depth <n>`: truncate by ancestor depth
- `--runtime-events <path|-`
  - `path` = read file once as JSON/JSON-RPC records
  - `-` is supported for `probe` only
  - watch/TUI reject `-` to avoid blocking
- `--json` and `--format json` (both supported)

## Output and JSON schema

`probe --json` emits:

- `schema_version`
- `generated_at`
- `environment` (`os`, optional detected `codex --version`/path)
- `query` (normalized query inputs)
- `threads` and flattened `tree`
- `warnings`
- `account_usage`: latest timestamped rollout allowance snapshot with typed `primary` and `secondary` windows (`used_percent`, positive `window_minutes`, optional `resets_at`), plus evidence metadata
- `recent_activity` per thread (safe event/tool name/status only)
- `token_usage` per thread: latest cumulative `token_count` breakdown (`input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens`, `total_tokens`, and optional `context_window`) with evidence metadata; missing or malformed counters remain unknown

Model fields are separated:

- `configured`: best-effort from `config.toml` + agent config file
- `requested`: durable persisted/runtime-stamped model/effort from rollout or DB fallback
- `effective`: runtime-only `model/rerouted` overlay (transient)
- `rerouted_from` / `reroute_reason`: optional runtime-only fields

Evidence objects expose `source`/`confidence`/`detail`/`observed_at`; unknown fields are explicitly marked with low-confidence unknown evidence.

## Safe content policy

Rollout parsing stores only non-sensitive telemetry fields:

- event `kind`
- tool `name` (when present)
- status and timestamps

Message text, instructions, tool input/output, summaries, and results are intentionally not retained in memory or JSON output.

## TUI

Controls:

- `j`/`k` or `↑`/`↓` move selection
- `f` toggles the local state filter between `All` and `Running`; `Running` is ordered newest-first by latest known activity
- `/` enters search mode for name / ID / role / nickname / cwd / model / effort
- `Enter` toggles recent activity expansion
- `i` toggles technical details
- `r` or `F5` forces an immediate snapshot refresh
- `q`, `Esc`, or `Ctrl-C` to quit
- `?` toggles a help overlay in the right pane

Behavior notes:

- Top summary shows counts for `RUNNING`, `IDLE`, `DONE`, and `FAILED`.
- `DONE` is derived from `Idle` + `turn_completed`; other `Idle` entries remain `IDLE`.
- `FAILED` reflects only real failed states (no fabricated failures).
- Right pane is read-only and shows selected agent name, humanized state, age, current activity, preferred model/effort, measured token-usage breakdown, and latest safe recent activity entries. Full path and provenance remain in technical details when expanded.
- The top bar shows `Usage left` from the latest timestamped `rate_limits` observation. Window names come from their observed duration (for example, `Weekly` for 10080 minutes and `5h` for 300 minutes); expired windows stay unavailable until a newer observation arrives.

## Privacy and limits

Persisted observations are best-effort and partial by design:

- task lifecycle from rollout events
  - `task_complete` with `error:null` => completed turn (`turn_completed`)
  - `task_complete` with any non-null `error` => `failed`
  - `turn_aborted` => `interrupted`
- lock presence in DB indicates writer lock only; not equivalent to active in-memory agent status
- effective model is not present in persisted rollout or DB output; only supplied runtime stream updates (`model/rerouted`) can expose transient effective model/reroute state
- runtime `model/rerouted` is transient and not persisted in DB
- per-task credit consumption is not available from rollout data; `rate_limits.credits` is account-level status, so the monitor does not estimate or display task credits/cost
- account usage is derived only from already parsed monitored rollouts. CLI project/thread/role/recent filters bound the evidence that can contribute to the snapshot; state/depth and local TUI search/filtering only change the displayed rows after the account snapshot is assembled.
- canonical thread-parent inference uses **thread id equality**, not copied historical metadata

## Verified environment notes

Implementation notes are based on local/manual verification (not guaranteed identical for all installs):

- Codex CLI: `0.144.5`
- Windows 11 with Desktop package: `26.810.7004.0`, `app` build `26.810.52044`, build id `6662`
- research commit reference: `c6058ccaa91ab17159cf805bf4d6d4edd87fe5fc`
- Desktop-owned stdio app-server had no supported external attach path on Windows in observed environment

See `docs/feasibility.md` for the full note and caveats.

## Development checks

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo build --release --locked
```
