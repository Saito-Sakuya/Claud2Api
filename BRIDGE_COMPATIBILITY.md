# Explicit bridge model IDs

[中文](BRIDGE_COMPATIBILITY_ZH.md) · English · [Documentation index](DOCS.md)

With custom model management disabled (default), the generation endpoints accept
`c2a-` followed by an exact `claude-*` model ID.
The prefix is resolved once and never changes the actual model version. The
selected account's current catalog must contain the target. Responses report
its canonical ID; `/v1/models` keeps the observed catalog unchanged.

With custom model management enabled, published IDs and explicit alias mappings
control the list and response `model` fields. Exact entries take precedence,
including disabled entries; otherwise a bridge name may resolve to an exposed
native entry. Targets must be native model IDs and still pass account validation.
Use the returned list for this mode. See [中文 API](API_ZH.md) / [English API](API.md).

An intermediary may map caller model names to these explicit bridge names when
it needs to distinguish this gateway's control-stripping contract. Configuration
of that intermediary is separate from gateway installation. No client or other
service is automatically reconfigured, and no model downgrade is inferred.

This does not provide native signed thinking, exact generation controls or
prompt caching. See [API.md](API.md) for the actual supported contract.
