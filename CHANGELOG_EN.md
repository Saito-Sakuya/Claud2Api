# Changelog

[中文](CHANGELOG.md) · English · [Documentation index](DOCS.md)

## v0.1.0-alpha.4 · 2026-10-19

- Reimplement Chat/Responses text adapters with role-preserving structured conversations and strict text validation.
- Remove the bundled web tool catalog and old snapshots; report stream failures separately and calculate consistent character-based usage estimates.
- Update the Messages prompt envelope while retaining tool-frame and persisted-state compatibility.

## v0.1.0-alpha.3 · 2026-10-18

- Add visible version management, amber update hints, five-minute checks and automatic panel reload.
- Add verified GitHub release downloads, drained single-writer switches, data-preserving rollback to two preceding releases and failed-start backup recovery.
- Prepare release packaging/workflow and local real-process update acceptance. Public publication and production deployment are separate steps.

## v0.1.0-alpha.2 · 2026-10-18

- Add per-account current/previous five-hour usage, independent account name editing, named import and atomic shared-proxy assignment; edit token metadata and live limits without changing credentials.
- Ship the Rust service, dark administration console and Docker Compose installation.
- Support account pools, per-account proxy/browser profiles, model configuration and token management.
- Retain sticky sessions, context management, simulated one-hour caching, usage analytics and input/output Token logs.
- Prepare an independent clean source tree; remove duplicate documentation and a duplicate license copy.
- Add a comprehensive developer guide and complete Chinese/English documentation with a shared index.
- Set new-state compaction defaults to a 128000-unit budget, a 75% trigger and six retained complete interactions; preserve saved settings and add long-conversation comparison and restart regressions.
- Combine message-log Tokens into two lines for ordinary input, output and simulated cache read/write (1h), with compact counts and exact-value tooltips.
- Normalize recognized changing system attribution from older Claude Code clients to avoid unnecessary web conversation resets; preserve actual instructions, tool and history checks without client environment changes.
- Continue append-only system/developer messages after verifying committed history and prior instructions; preserve new instructions and genuine-edit checks, and support recognized attribution without cch.

Alpha release. Usage, cache metrics and costs are local estimates, not official billing.
