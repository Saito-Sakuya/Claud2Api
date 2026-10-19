# Changelog

[中文](CHANGELOG.md) · English · [Documentation index](DOCS.md)

## v0.1.0-alpha.6.1 · 2026-10-19

- Install patches in the current release line automatically while retaining confirmation for feature versions; persist the panel switch, stop retrying unfinished patches, and disable automatic patches after manual rollback.
- Reuse artifacts from the exact commit's fully verified main build instead of repeating tests and compilation for the tag; retain provenance, checksum and final-container checks.
- Remove alpha6's search-first guidance; describe capabilities and transport without directing the task toward search.
- Enabled tools or earlier use do not require another call. The model follows the current request and original instructions, and may answer ordinary explanations directly.
- Add coverage for direct answers with native tools enabled while retaining explicit client tool-policy validation.
- Older installations need one container/base-supervisor upgrade to enable automatic patches, retaining their state and keys.

## v0.1.0-alpha.6 · 2026-10-19

- Include enabled native tool names on every turn and distinguish Desktop's `WebSearch` from native `web_search`.
- Clarify that earlier assistant denials do not define current tool availability; permitted searches require actual calls and honest failure reporting.
- Add regression coverage for tool names, preserved history and disabled capabilities. All 149 tests, Clippy and the release build pass.

## v0.1.0-alpha.5 · 2026-10-19

- Add native web search/fetch, Artifacts, remote code/files, 37 web widgets and explicitly selected project knowledge.
- Preserve native tool results and citations across Messages, Chat and Responses; add expiring file downloads scoped to the API token.
- Add separate console tool switches and alpha4-compatible persistence; improve continuation, cleanup and failure handling.
- Update bilingual API, source-file and GHCR installation documentation. 148 tests, the release build and local upgrade/rollback acceptance passed; project knowledge has mock validation only.

## v0.1.0-alpha.4 · 2026-10-19

- Reimplement Chat/Responses text adapters with role-preserving structured conversations and strict text validation.
- Remove the bundled web tool catalog and old snapshots; report stream failures separately and calculate consistent character-based usage estimates.
- Update the Messages prompt envelope while retaining tool-frame and persisted-state compatibility.
- Fix container packaging and publish verified Linux AMD64 Release assets, licenses and GHCR images.

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
