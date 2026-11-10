# Copilot session monitoring design

Monitor locally stored GitHub Copilot Desktop, CLI, and VS Code sessions alongside existing providers, across all models. Show session identity, selected model, lifecycle state, and token usage only when supported by evidence. Gosoft aliases are requested names, not proof of upstream model identity.

Use local read-only sources and existing output contracts. Desktop and CLI share Copilot session-state; VS Code uses its own chat storage. Support the standard Copilot home, COPILOT_HOME, and an explicit path. Merge records only when identity is proven. Do not read private message or tool content into retained objects or output. Unknown data stays unknown; context occupancy is not cumulative usage. Preserve other providers and user changes.

Alternatives considered: local persisted metadata (selected: matches existing monitor and requires no new service); live instrumentation or a gateway integration (additional setup and uncertain source coverage). Local persistence may not expose live state, tokens, or every session; report those limits rather than guessing.

Source evidence: https://docs.github.com/en/copilot/concepts/security-governance-and-network-settings/session-data ; https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/chat/common/model/objectMutationLog.ts . Local Copilot SDK type declarations describe shutdown counters and ephemeral idle events. VS Code formats and Copilot identification must be verified against installed versions and tested with synthetic, privacy-safe fixtures.
