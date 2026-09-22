# codex-agent-monitor

`codex-agent-monitor` is an open-source read-only local monitor for Codex, Claude Code, Kiro, and Ollama persisted state.

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
- Expose the latest cumulative per-task token usage observation from Codex rollout `token_count` events
- Expose Kiro native current-context occupancy from persisted percentage/window evidence, clearly labeled as approximate and non-cumulative
- Show the latest account usage allowance observed in monitored rollout `rate_limits` events
- Optional TUI for navigation and thread details
- Optional `--runtime-events` overlay (`model/rerouted`) for ephemeral effective model/reroute facts
- Claude Code CLI session inspection with `--provider claude` or `--provider all`, including per-session cumulative token counters and live-process state
- Kiro CLI and ACP session inspection with `--provider kiro` or `--provider all`
- Ollama Desktop chat metadata with `--provider ollama` or `--provider all`; local Ollama API availability and loaded model names are reported when reachable
- Live `ollama run <model>` process rows with safe model/PID telemetry, plus a running row per model resident in the local Ollama API (`ollama:api:<model>`), labeled as keep-alive residency rather than a confirmed in-flight request
- `--provider ollama` also returns Codex threads whose persisted `model_provider` is an Ollama backend, shown as `<model> via Ollama`
- Kiro native task progress (numeric task IDs and normalized statuses only) plus safe native tool call/result activity

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

- `--provider {codex,claude,kiro,ollama,all}`: choose the persisted source (default `all`; use `--provider codex` for Codex-only mode). `--provider ollama` covers both Ollama's own sources and threads of other providers running on an Ollama backend
- `--claude-home <path>` (or `CLAUDE_CONFIG_DIR`): Claude Code home to inspect; defaults to `~/.claude`
- `--claude-usage-file <path>`: explicit Claude plan utilisation history; otherwise the Claude desktop app's `plan-usage-history.json` is used
- `--kiro-home <path>` (or `KIRO_HOME`): Kiro home to inspect; defaults to `~/.kiro`
- `--kiro-db <path>`: explicit legacy Kiro SQLite database override
- `--ollama-db <path>`: explicit Ollama Desktop SQLite database override; otherwise macOS `~/Library/Application Support/Ollama/db.sqlite` is used
- `--kiro-cli <path>`: explicit `kiro-cli` executable for the authenticated Kiro credit lookup
- `--no-kiro-usage`: skip the native authenticated Kiro credit lookup

- `--project <path>`: filter by workspace/root path (matched against persisted `cwd`)
- `--thread <id>`: exact thread id
- `--state {running,idle,unknown}`
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

- `schema_version` (`codex-agent-monitor.probe.v2`)
- `generated_at`
- `environment` (`os`, optional detected `codex --version`/path)
- `query` (normalized query inputs)
- `threads` and flattened `tree`
- `warnings`
- `account_usage`: latest timestamped rollout allowance snapshot with typed `primary` and `secondary` windows (`used_percent`, positive `window_minutes`, optional `resets_at`), plus evidence metadata
- `recent_activity` per thread (safe event/tool name/status only)
- `task_progress`: numeric IDs and normalized `pending`/`in_progress`/`completed`/`unknown` statuses when valid Kiro native task files exist; otherwise Kiro rows can carry a safe availability explanation without task text
- `context_usage` when valid Kiro native evidence exists: current `used_percent`, `context_window_tokens`, and `used_tokens_approx` derived from those two persisted values; this is current context occupancy, not cumulative or billed token usage
- `state` is only `running`, `idle`, or `unknown`; the latest terminal result is preserved separately as evidence-aware `last_terminal_event` (`completed`, `failed`, or `interrupted`)
- `activity_signal` is evidence-aware `recent`, `stale`, or `unknown` using a shared 15-minute freshness window; it never asserts that a thread is running
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

Claude Code support follows the same policy. Transcripts under
`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` are streamed read-only and
only telemetry is retained: session ID, `cwd`, git branch, entrypoint, model,
effort, timestamps, `stop_reason`, tool-call names, and tool-result status.
Prompts, assistant text, thinking blocks, tool inputs and results,
attachments, file-history snapshots, custom titles, and agent names are never
read into memory or output. `~/.claude/history.jsonl` is not read at all
because its rows carry prompt text. Claude IDs are shown as
`claude:<session-id>`; `--thread` also accepts the raw session ID.

Claude sessions are independent roots. Claude Code's `parentUuid` links
messages inside one transcript rather than spawned sessions, and subagent turns
are stored inline as `isSidechain` records, so the monitor does not synthesize
parent/child thread edges for them. Sidechain turns still contribute their token
counters, but never override the session's own model, effort, or workspace.

Claude plan utilisation comes from the Claude desktop app's
`plan-usage-history.json`, whose samples record a five-hour (`fh`) and
seven-day (`sd`) percentage. The newest sample carrying a percentage is
reported as the standard primary/secondary account windows; the `org` account
identifier in that file is never read out. The history records utilisation
only, so `resets_at` stays unknown rather than being estimated, and the
displayed percentage is remaining allowance, matching the Codex line. When the
desktop app is not installed the file is absent and the value stays unknown.

Claude token usage is cumulative per session, summed across responses and
de-duplicated by response ID, because one API response is persisted as several
records that each repeat the same usage block. `total_tokens` is the sum of
input, cache-read, cache-write, and output tokens. `context_window` stays
unknown because Claude Code does not persist a window size, and for the same
reason Claude rows expose no `context_usage`. Claude lifecycle state is
conservative: a session is `running` only while `~/.claude/sessions/<pid>.json`
registers it and that process is still visible, `idle` once records exist
without a live process, and `unknown` for an empty transcript. As with the other
providers, a live process means an open session, not necessarily an in-flight
turn; `activity_signal` remains the freshness evidence.

Kiro support follows the same policy. Native task files are read only from the
session's adjacent `tasks` directory; the monitor retains numeric task IDs and
normalized statuses, but never task subjects or descriptions. Native tool events
retain only the tool name and result status, never tool IDs, purposes, inputs, or
outputs. Native CLI files under `sessions/cli`, ACP
worker files under `sessions/<workspace>/sess_*/`, and the legacy
`data.sqlite3` conversation tables are read locally and read-only. Kiro IDs are
shown as `kiro:<native-id>`; `--thread` also accepts the raw native ID. Native
session files take precedence over duplicate legacy SQLite rows. Kiro model
and effort fields are requested-session observations; configured/effective
models, cumulative tokens, and parent relationships remain unknown because the
local Kiro formats do not provide reliable evidence for them. Native sessions
may separately expose current context occupancy from Kiro's persisted percentage
and model context window. The displayed token count is explicitly approximate
(`percentage × window`) and is never presented as cumulative usage, billing, or
credits. Kiro lifecycle
state is conservative: a timestamped pending native prompt is `running` only
while its adjacent session lock names a live process; a completed turn is
`idle`, and missing, malformed, or stale lock evidence leaves a pending turn
`unknown`. Activity timestamps do not by themselves imply that a session is
running.

When `--provider kiro` or `--provider all` uses the normal Kiro home, the
monitor can make one read-only native ACP request to
`_kiro/account/getUsage` and report observed plan credits, reset date, and
valid bonus/add-on balances. The request uses Kiro's own stored login and does
not create a session or read project content. Use `--no-kiro-usage` for an
offline-only run. When `--kiro-home`/`KIRO_HOME` points at an explicit data
directory, account lookup stays disabled unless `--kiro-cli` is also supplied.

For Kiro session persistence and commands, see the
[Kiro session management guide](https://kiro.dev/docs/cli/chat/session-management/).
The internal JSON and SQLite shapes were verified against local Kiro 2.21.x
files (including 2.21.2 and 2.21.3) and may evolve with future Kiro releases.

Examples:

```text
codex-agent-monitor --provider kiro probe
codex-agent-monitor --provider claude probe --json
codex-agent-monitor --provider all --project /work/my-repo probe --json
codex-agent-monitor --provider kiro --kiro-home /tmp/kiro tui
codex-agent-monitor --provider kiro --no-kiro-usage probe
```

## TUI

Controls:

- `j`/`k` or `↑`/`↓` move selection
- `f` toggles the local state filter between `All` and `Running`; `Running` is ordered newest-first by latest known activity
- `/` enters search mode for provider (`codex`/`claude`/`kiro`), name / ID / role / nickname / cwd / model / effort
- `Enter` toggles recent activity expansion
- `i` toggles technical details
- `r` or `F5` forces an immediate snapshot refresh
- `q`, `Esc`, or `Ctrl-C` to quit
- `?` toggles a help overlay in the right pane

Behavior notes:

- Top summary shows counts for `RUNNING`, `IDLE`, and `UNKNOWN`.
- The right pane shows the latest terminal result and activity signal separately from current state.
- The `Running` filter intentionally hides idle and unknown sessions after a turn ends; switch to `All` to keep their cards visible.
- Right pane is read-only and shows selected agent name, humanized state, age, current activity, preferred model/effort, cumulative token-usage breakdown when available, current Kiro context occupancy when available, latest safe recent activity entries, and Kiro task IDs/statuses. Kiro cards show `Ctx ~<tokens>` and, when present, completed/total task progress; details explicitly say when Kiro did not persist a task plan. Full path and provenance remain in technical details when expanded.
- The top bar separates `Codex usage` from `Kiro credits`. Codex windows come from the latest timestamped `rate_limits` observation; Kiro credits come from the native authenticated lookup when enabled. Unknown, loading, and stale values remain explicit.

## Privacy and limits

Persisted observations are best-effort and partial by design:

- task lifecycle from rollout events
  - `task_complete` with `error:null` => idle plus `last_terminal_event=completed`
  - `task_complete` with any non-null `error` => idle plus `last_terminal_event=failed`
  - `task_done` => idle plus `last_terminal_event=completed`
  - `turn_aborted` => idle plus `last_terminal_event=interrupted`
- no Codex writer-lock support is inferred from persisted state
- effective model is not present in persisted rollout or DB output; only supplied runtime stream updates (`model/rerouted`) can expose transient effective model/reroute state
- runtime `model/rerouted` is transient and not persisted in DB
- per-task credit consumption is not available from rollout data; `rate_limits.credits` is account-level status, so the monitor does not estimate or display task credits/cost
- Kiro native `metering_usage` records are credit-denominated in the validated format and are never mapped to tokens; all-zero native token-counter records remain unknown rather than being reported as zero usage
- account usage is derived only from already parsed monitored rollouts. CLI project/thread/role/recent filters bound the evidence that can contribute to the snapshot; state/depth and local TUI search/filtering only change the displayed rows after the account snapshot is assembled.
- canonical thread-parent inference uses **thread id equality**, not copied historical metadata

## Verified environment notes

Implementation notes are based on local/manual verification (not guaranteed identical for all installs):

- Codex CLI: `0.144.5` is the only explicitly validated version; parser shape tolerance is not a version range
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

build get codex-agent-monitor : cargo install --path . --force --locked
