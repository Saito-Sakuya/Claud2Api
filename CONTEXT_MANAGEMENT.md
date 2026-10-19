# Conversation context management

[中文](CONTEXT_MANAGEMENT_ZH.md) · English · [Documentation index](DOCS.md)

Messages sessions can compact completed older history into a bounded handoff and
continue in a new web conversation on the same account. The opaque local handle
and the client's full logical history remain unchanged. This is a gateway text
summary, not a native API compaction block or an account-limit reset.

## Defaults and estimates

| Setting | Default | Meaning |
| --- | --- | --- |
| CONTEXT_COMPACTION_ENABLED | true | Enable proactive compaction in production |
| CONTEXT_BUDGET_TOKENS | 128000 | Conservative local context budget, 4096..1000000 |
| CONTEXT_COMPACTION_TRIGGER_PERCENT | 75 | Start considering compaction, 10..90 |
| CONTEXT_KEEP_RECENT_ROUNDS | 6 | Retained completed text/tool units, 1..32 |

The context ledger uses ceil(UTF-8 bytes / 2), including actual bridge prompts,
repeated system/tool/protocol instructions, successful raw output and a bounded
output reserve. These are local estimates, not exact tokens, verified model
capacity, provider billing or remaining account quota. Older state is backfilled
conservatively. Display-only simulated cache usage is a separate calculation.

Only matching established sessions with an older completed prefix are eligible.
The current input is always retained. A completed text answer or fully returned
tool batch is a safe unit; a tool call and any of its results cannot straddle the
cut. Six recent completed units are retained by default, plus current input.
Initial giant inputs and oversized recent/system/tool data cannot be solved by
silently truncating them. No generic upstream 400 is guessed to be a context error.

The trigger compares accumulated physical context, the current bridge prompt and
an output reserve against `budget * percent / 100`. The reserve is
`min(max(max_tokens, 1024), budget / 4)`. With the new defaults, the trigger is
96000 local units; for `max_tokens=8192`, accumulated context plus the current
prompt can reach approximately 87808 units before compaction becomes eligible.
The previous 32768/60% defaults left approximately 11468 units in that case.
Eligibility still requires an older completed prefix and a safe cut.

These defaults balance retained raw history against additional summary requests;
they are not measured upstream capacity or a guarantee of quota savings. Existing
saved settings are preserved across upgrades and restarts. Use the console's
runtime settings to change an existing instance explicitly; environment variables
only initialize a new state directory.

## Summary and rotation

1. Generate one strict JSON handoff in a temporary conversation on the same
   leased account. Supply the previous handoff and newly covered completed turns.
   Preserve goals, constraints, completed work, reported evidence, unresolved
   issues and next steps. The original system and tool definitions are retained
   word for word separately from the summary.
2. Disable native tools/thinking for this internal operation. No tools are
   executed by the gateway. The handoff never enters the client's business
   response. It targets roughly 2000 estimated tokens and is capped at 8192 bytes;
   invalid, truncated or oversized output is rejected. Summarization is lossy.
3. Require meaningful estimated context reduction and keep protected checkpoint
   ownership. Invalid/unhelpful summaries fall back to the unchanged source and
   back off for one minute. Authentication, transport and deadline failures are
   not blindly retried. A rejected quota attempt may use existing pool handling.
4. Wait for the next request-start admission on that account before business generation,
   under the same single-account lock. Internal generation does not bypass
   pacing, rolling-minute limits, provider cooldown or cancellation. Streaming
   waits may send ping events without invented assistant content.
5. Create a new web ID and nonce with the handoff, retained raw turns and current
   input. Only successful business completion replaces the old binding. Failed
   destinations retain owned cleanup records and leave the original binding.
   Cleanup rechecks its pending record after acquiring the account.

Each request attempts summarization at most once. Summarization itself consumes
generation capacity; a compaction turn can require two generations. It may reduce
later context burden, but does not guarantee quota savings on the current turn.
New conversations and deleting old ones never reset an account's allowance.

## Continuation, limits and privacy

Checkpoints bind a covered-turn count to the canonical logical-history HMAC and
model/system/tool context. Changed prefixes or instructions cannot reuse stale
summaries. Ordinary continuation submits only new turns. A rebuild after explicit
pre-generation quota rejection uses a matching checkpoint plus uncovered history;
without one, the existing full-history path remains. Partial/uncertain business
generation and tool output never trigger replay. Whole-pool exhaustion or a
deadline can still require a sanitized error.

The protected sticky state now contains actual summary text in addition to
hashes, web IDs, parent state, estimated context and pending cleanup. It is
outside the repository, mode 0600 in a 0700 directory in the container, and expires
with the sliding session TTL (default one idle hour). Full transcripts are not
persisted. Never log summaries, prompts, tool arguments or credentials. Logs
contain only action/reason, counts and estimates. X-Claud2Api-Context:
managed-estimates discloses the local policy without changing assistant text.

Compaction and account migration start new simulated one-hour creation with zero
simulated reads when that display option is enabled. These counters do not prove
native cache behavior. The Rust binary stores checkpoints in its encrypted
SQLite snapshot. An optional version1 JSON state import is available.
Preserve the original JSON, database and matching gateway key before migration
or rollback. Never run the old and new processes against the same real accounts.
