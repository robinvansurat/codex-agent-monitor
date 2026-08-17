# codex-agent-monitor Repository Override

These instructions apply only to this repository.

## Repository-scoped development autonomy

For work inside this repository, the user explicitly authorizes autonomous
end-to-end software development.

This repository-specific authorization overrides the per-command approval
requirements in the user's Global Codex Working Policy for the development
actions explicitly listed below.

The purpose of this override is to permit normal software-development work
without requesting approval for every individual network-capable command.

## Pre-approved actions

For this repository, Codex may perform these actions without separate
per-command approval:

### Public research

- Read and search public documentation.
- Access official OpenAI documentation.
- Access public GitHub repositories, issues, pull requests, commits, and releases.
- Use read-only HTTP GET/HEAD requests to public documentation and source hosts.

### Rust development setup

- Install the Rust toolchain from official Rust distribution sources if needed.
- Download Cargo crates required by this repository.
- Run normal Cargo commands, including:
  - cargo fetch
  - cargo check
  - cargo build
  - cargo test
  - cargo clippy
  - cargo fmt

### Source control and GitHub

For this repository only, Codex may:

- git fetch
- git pull
- git push feature branches
- inspect GitHub CI
- create and update pull requests
- push fixes to the current task branch
- merge the task PR when all requested checks pass and repository permissions allow

Do not:
- force-push main/master
- delete the repository
- rewrite unrelated history
- modify unrelated repositories

### Normal local development

Codex may create, modify, rename, and delete files inside this repository
when required by the explicitly requested task.

Codex may run local binaries and tests produced by this repository.

## Engineering decisions

Within the explicitly requested product scope, Codex may autonomously choose
between reasonable technical implementations when:

- sufficient evidence exists;
- the choice is reversible through source control;
- it does not redefine product behavior;
- it does not introduce production infrastructure or external business-system risk.

The existence of multiple reasonable implementation approaches is not, by
itself, a reason to stop and ask the user.

Ordinary Rust dependencies may be added when technically justified.

Evidence requirements remain in force:
- do not invent facts;
- distinguish verified facts from inference;
- prefer unknown over unsupported claims.

## Actions that remain NOT pre-approved

This repository override does NOT authorize:

- production deployments
- external database writes or migrations
- remote cache/queue mutations
- cloud infrastructure mutations
- DNS changes
- credential or secret rotation
- purchases or billing actions
- destructive remote operations
- access to unrelated private/internal systems
- force-pushing protected/default branches
- modifying repositories unrelated to this project

These actions still require explicit user approval.

## Scope

This override applies only while working inside the
`codex-agent-monitor` repository.

It does not apply to any other repository or project.