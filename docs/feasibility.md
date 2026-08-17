# Feasibility Notes

## Verified local evidence

- Codex CLI observed locally at version `0.144.5`.
- Windows 11 environment used for manual verification in this repository implementation.
- Desktop package observed:
  - `26.810.7004.0`
  - `app` build `26.810.52044`
  - build id `6662`
- `state_5.sqlite` exposes thread list rows, `thread_spawn_edges`, and linked rollout file paths (`state_5.sqlite` can still be queried when threads are live).
- `thread_spawn_edges` plus rollout/session metadata yields a stable parent/child model in practice.
- Persisted data is useful for durable shape but not complete for live behavior:
  - cannot recover waiting/approval state with confidence
  - cannot reconstruct exact currently-invoke tool output/arguments
  - cannot reconstruct exact current tool model at this instant (beyond durable/requested best-effort)
- writer lock presence appears to indicate write activity, not guaranteed execution activity
- runtime `model/rerouted` is observed via shared/authorized runtime stream and is not durable in SQLite
- `thread` writer lock and lifecycle rows are not direct substitutes for full approval/waiting/runtime truth
- effective/requested model differences: effective model is only visible in runtime reroute stream and is not persisted in rollout/SQLite

## Source reference

- Official source reference observed for contract comparison: `c6058ccaa91ab17159cf805bf4d6d4edd87fe5fc`
- This is treated as a verified research baseline, not a guaranteed byte-identical match for installed `0.144.5` behavior.

## Scope and constraints preserved

- read-only operation only (no DB write, no auth/token reads, no local/remote state mutation)
- `--runtime-events -` support is intentionally one-shot in `probe`; long-lived modes reject stdin input to avoid blocking and keep TUI/watch deterministic
- watch cadence is debounced by cache checks on file size + mtime, avoiding repeated full replay of unchanged rollout history
- parser is intentionally conservative around malformed JSONL tails:
  - one-shot: clean trailing unterminated record can be accepted with warning
  - malformed final tail in any context is reported and skipped to keep snapshot usable

## Not claimed (to avoid overfitting)

- No claim of measured benchmark latency, p95, throughput, or startup timings were recorded for this implementation.
- No claim that external app-server process attaches are supported on Windows; in observed conditions there is no supported attach path from this tool for external sidecar process control.
