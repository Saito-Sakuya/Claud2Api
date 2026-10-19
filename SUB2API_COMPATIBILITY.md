# Compatibility controls

[中文](SUB2API_COMPATIBILITY_ZH.md) · English · [Documentation index](DOCS.md)

Optional thinking, output_config, sampling, stop sequences, metadata/cache hints
and strict-generation annotations are accepted and discarded at the supported
request boundary. They do not enter upstream payloads or prompts. Text, client
tools, tool arguments/results and supported tool choice remain validated.

Supported session metadata may be consumed locally for sticky routing. Unknown
content types remain errors; diagnostics use fixed allowlisted kinds and paths.
Assistant-history thinking/redacted_thinking blocks are discarded without losing
supported text or tool data. No native thinking or cache capability is implied.

See [API.md](API.md), [STICKY_SESSIONS.md](STICKY_SESSIONS.md) and the configurable
[simulated display metrics](SIMULATED_CACHE.md).
