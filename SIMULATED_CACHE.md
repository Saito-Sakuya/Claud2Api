# Simulated one-hour cache metrics

[中文](SIMULATED_CACHE_ZH.md) · English · [Documentation index](DOCS.md)

`SIMULATED_CACHE_ENABLED=true` enables optional local display statistics on
Messages responses. The default is **true** for new installations; persisted settings remain unchanged. This mode estimates cache creation
and reads from the gateway's verified sticky history; it does not observe an
upstream KV cache or establish quota, performance or billing savings.

Enabled responses carry `X-Claud2Api-Cache: simulated-1h` and the limitations marker
`cache-simulated`. JSON and SSE `message_start.message.usage` share these fields:

```json
{
  "input_tokens": 0,
  "cache_read_input_tokens": 800,
  "cache_creation_input_tokens": 200,
  "cache_creation": {
    "ephemeral_5m_input_tokens": 0,
    "ephemeral_1h_input_tokens": 200
  }
}
```

The example partitions 1000 estimated logical input tokens into 800 simulated
reads and 200 simulated one-hour writes. No input is counted twice. Zero-valued
top-level optional counters may be omitted. A first/new/reset/expired session
reads zero and creates its estimated input. A verified continuation reuses at
most the saved logical-prefix estimate; additional input is simulated creation.

Only a completed successful turn updates the saved prefix estimate. The display
cache has a sliding one-hour lifetime and is also removed when its underlying
sticky conversation expires or resets. Cached state stores estimates and hashes,
never prompt text. Failed/interrupted turns do not establish a usable cache.
Invalidated history cannot produce a simulated hit. Client cache hints remain
accepted and discarded; they are not forwarded as native upstream controls.

Output tokens remain estimates. Downstream applications may display these values,
but they should not be treated as actual upstream accounting or used as evidence
of monetary savings. No response text is altered to add simulation warnings.

Proactive context rotation also resets simulated reads to zero and creates a
fresh simulated one-hour prefix. Display metrics describe estimated logical
input; the conservative physical context ledger and summary statistics are
separate. Neither reveals actual provider quota or native cache savings.
