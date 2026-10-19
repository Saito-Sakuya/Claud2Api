# Usage and simulated costs

[中文](USAGE_ANALYTICS.md) · English · [Documentation index](DOCS.md)

Overview treats each API token as a user; the environment gateway key is a separate
category. Logs freeze token names at request time, retaining them after deletion.
Old unattributed usage is shown as historical/unassigned, never assigned to a
current user.

The default range is the last 24h; alternatives are today, 7d, this month, last
month and lifetime. Calendar boundaries use server `CLAUDE_TIMEZONE`. Lifetime
means existing durable ledger history, not process uptime. Independently choose
24 hourly segments over the last 24h or seven 24-hour segments over the last 7d.
Trends can show input/output/total Tokens or simulated USD; use the legend to hide
series and hover points for details. The donut uses input plus output Tokens in
the selected summary range.

The account pool separately shows per-account current/previous five-hour cycles, starting at first generation and restarting on the next generation after idle expiry. Include internal summaries, label old completion-time fallback and unknown failed consumption. These differ from overview ranges and are not provider quota. See [Control API](CONTROL_API_EN.md).

Display quantities consistently with K/M/B abbreviations. Account cycles show only simulated dollars and Tokens, with details on hover; unpriced amounts use `$—` and partially priced amounts carry `*`.


## Accounting basis

All Tokens are local estimates. Costs apply API reference rates to those counts;
they are not web-subscription bills. The built-in rate snapshot cites
[Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing) and
is dated 2026-10-18 in `src/usage.rs`; rates are not fetched live for a report.
The baseline uses global standard API rates, excluding Fast, Batch, regional
uplifts and server-tool charges.

Each turn uses its native model's rate; aliases do not change pricing. Partition
input once:

```text
ordinary_input = total_input_estimate - simulated_1h_creation - simulated_read
simulated_cost = (ordinary_input * input_rate + output * output_rate
                  + creation_1h * write_1h_rate + read * read_rate) / 1,000,000
uncached_baseline = (total_input_estimate * input_rate + output * output_rate) / 1,000,000
```

Cache mode, counts and rates are saved when each record is created; reports do not
reprice it. New cache-disabled requests use ordinary input. A first cache-enabled
turn has no read hit and prices creation at the one-hour write rate; continuations
price reads and new creation separately. Internal compaction and legacy
Chat/Responses use ordinary input without assumed cache hits. Output is not also
included in the current turn's cache creation. Costs accumulate as integer
nanoUSD, with each category rounded to 10⁻⁹ USD; the UI shows up to nine decimals.

The built-in table covers explicit native IDs for Haiku/Sonnet/Opus 5.5, Sonnet 5,
selected Opus 4.x/5, Sonnet 4.5/4.6 and Haiku 4.5. Other IDs remain unpriced.
Haiku 5.5 tiering checks whether the entire request input exceeds 100000, including
cache reads/writes, rather than testing ordinary input alone.

For Sonnet 5.5 with 1000 estimated input and 200 output Tokens:

| Mode | Ordinary input | 1h creation | Read | Simulated USD |
|---|---:|---:|---:|---:|
| Cache disabled | 1000 | 0 | 0 | 0.004 |
| Enabled, first creation | 0 | 1000 | 0 | 0.006 |
| Enabled, read 900 | 0 | 100 | 900 | 0.00249 |

Enabling cache need not save money on each turn. The simulated difference from
baseline may be a reduction or an increase; no fixed discount is fabricated.
Local cache simulation does not establish upstream KV caching, allowance savings
or a speed improvement.

## Data completeness

Total cost is the subtotal of priced records. Unknown prices and old successful
records missing pricing metadata are counted as unpriced; their amount-series
points remain missing. Failed requests have unknown real consumption. Disconnects,
pre-account rejection and other services' account use may not be fully metered.
Business records and internal compaction counts are separate. Current switches,
rate updates or token names never rewrite past quotes.

Analytics does not limit lifetime totals to the latest 10000 rows. A timed-out
query returns an error rather than presenting partial totals as complete. Charts
group excess tokens for readability while keeping totals complete; empty windows
show no data. The separate Bot-compatible `/usage` endpoint does have a 10000-row
cap and exposes `truncated`; it is not the lifetime analytics endpoint.
