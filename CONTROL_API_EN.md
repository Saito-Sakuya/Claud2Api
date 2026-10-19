# Control API

[中文](CONTROL_API.md) · English · [Documentation index](DOCS.md)

`/control/v1/*` exists only on the separate loopback listener. Authenticate with
`Authorization: Bearer <admin key>`. Public API credentials cannot access it.
Responses include `schema_version:1`.

## Read-only information

| Endpoint | Content |
|---|---|
| `GET /info` | Version, health, account capacity, session counts and browser profiles |
| `GET /usage?since=<Unix seconds>` | Local usage estimates; defaults to the last five hours |
| `GET /events?after=<cursor>&limit=100` | State-change and operation events |

An optional `BOT_READ_KEY_FILE` permits only these three GET endpoints for an
external notification service. Provider remaining allowance is always unknown.
Failures, disconnects and other services' account use may be incompletely metered.

`since` is clamped to the last 31 days through now. `/usage` reads at most the
latest 10000 matching rows and reports `truncated`; inspect that flag before
treating it as complete. `/events` returns `cursor` and `events`; each event has
seq/at/kind/data. Persist cursor and use it as the next `after`. Limit defaults to
100 and is clamped to 1–200. There is no built-in push delivery.

## Administration operations

All paths below have the `/control/v1` prefix.

| Method / path | Purpose |
|---|---|
| `GET /accounts` | Account state, profile and sanitized proxy settings |
| `POST /accounts/import` | Import 1–512 accounts, deduplicated by Session Key |
| `PATCH /accounts/{id}` | Change label, enabled state, browser profile or proxy |
| `PATCH /accounts/proxy` | Atomically set one proxy or direct connection for selected accounts |
| `POST /accounts/{id}/resume` | Clear access pause after operator review; retain cooldown |
| `GET /sessions` | Local session ID, account, model, expiry and active state |
| `DELETE /sessions/{id}` | Clear an idle session; active sessions return 409 |
| `GET /settings` | Settings and revision |
| `PUT /settings` | Replace complete runtime settings |
| `GET /logs` | Administrator-only message-status query |
| `GET /models` | Read model configuration |
| `PUT /models` | Update published models, labels and alias mappings |
| `POST /models/upstream` | Explicitly observe an account's native catalog/default templates |
| `GET /tokens` | Token metadata and current concurrency use |
| `GET /analytics` | Administrator-only token usage, trends and simulated costs |
| `POST /tokens` | Create a token; return its complete credential only this once |
| `PATCH /tokens/{id}` | Edit label, concurrency and expiry while retaining the secret |
| `DELETE /tokens/{id}` | Revoke a token for future requests |

Account/settings/model/token/session mutations require `expected_revision`.
A stale value returns 409. Catalog observation is the exception described below.
Background refresh preserves unsaved forms; after a conflicting edit, reread and
review the current revision before saving.

Account import body:

```json
{"expected_revision":1,"accounts":[{"credential":"<credential>","label":"Account name","browser_profile":"firefox_151_windows","proxy_url":"socks5h://<host>:1080"}]}
```

Account PATCH accepts enabled/label/browser_profile/proxy_url. Omit `proxy_url`
to retain it; an empty string clears it. Proxy credentials are write-only.
Changing a busy account's browser or proxy returns 409. Enable/disable does not
forcibly interrupt admitted work.


Import `credential` accepts a raw Session Key, full Cookie or `account name:Session Key`.
Only non-Cookie lines split at the first ASCII colon; an explicit `label` wins.
Duplicates do not overwrite accounts. Names can be edited independently during
generation. `PATCH /accounts/proxy` accepts `expected_revision`, 1..512 distinct
`account_ids` and `proxy_url` (empty means direct for all). Validate every account
and acquire its original concurrency gate before one atomic save. Busy/missing
accounts, revision conflicts or invalid proxies fail the entire batch. Preserve
browser profiles/sessions and write-only proxy authentication.

Each `GET /accounts` entry includes `usage_cycles`: `current`, `previous`,
`duration_seconds:18000`, `estimated:true` and observation time. Each cycle has
`start`, exclusive `end`, `fallback_records` and analytics-compatible `totals`.
Five-hour cycles start at each account's first generation. After idle expiry,
current is null and the last used cycle remains previous; the next generation
starts a new cycle, without inserting empty cycles. Include internal summaries
and recorded failures; failed consumption can be unknown. Old rows fall back to
completion time, counted by `fallback_records`. Stream the entire ledger without
a 10000-row cap; return 504 after eight seconds. These local estimates are not
provider quota; external usage and unrecorded failures are not measured.

`PATCH /tokens/{id}` accepts complete editable metadata: `expected_revision`,
`label`, `max_concurrency`, `expires_at` (null for never, otherwise a future Unix
second). Preserve the original secret, ID, creation time and sessions. Changes
affect new admissions; lowering the cap lets existing requests drain, so active
count can temporarily exceed the cap. Editing never reveals the complete secret.
These writes are admin-only; stale revisions return 409. Cancel and reopen the
editor to read latest metadata before retrying.

## Runtime settings

`GET /settings` returns revision, the full settings object and effective_for.
Read existing values before editing; `PUT /settings` requires the complete object,
not a partial patch. This example contains the defaults:

```json
{
  "expected_revision": 1,
  "settings": {
    "session_ttl_seconds": 3600,
    "request_timeout_seconds": 300,
    "max_request_bytes": 4194304,
    "simulated_cache": true,
    "compaction_enabled": true,
    "context_budget": 128000,
    "compaction_trigger_percent": 75,
    "keep_recent_rounds": 6,
    "account_min_interval_seconds": 5,
    "account_requests_per_minute": 6,
    "rate_limit_min_seconds": 60,
    "rate_limit_fallback_seconds": 18000,
    "overload_seconds": 15,
    "native_web_search": true,
    "native_artifacts": true,
    "native_repl": true,
    "native_widgets": true,
    "native_project_knowledge": true
  }
}
```

See [persisted runtime settings](DEPLOY_EN.md#persisted-runtime-settings) for ranges.
Unknown settings fields are rejected. Edits affect new requests; in-flight requests
retain their captured limits. Saved values survive restart. The sample revision
is illustrative: use the current GET value and review existing settings before
replacing them.
The five `native_*` switches default to enabled and can be changed separately in
the settings page. Older state files that omit them migrate with all five enabled.
Requests may select a subset with `claud2api_tools`, but cannot override a group
disabled by the administrator. Project knowledge also requires the request's
`claud2api_project_uuid`; without one it is not added to the default groups.

## Model configuration

```json
{"expected_revision":1,"models":{"enabled":true,"entries":[{"id":"work-haiku","display_name":"Work model","upstream_model":"claude-haiku-5-5","enabled":true}]}}
```

GET returns schema_version/revision/models. Default enabled=false uses native
catalogs. It also returns `upstream_catalog` (account_id/fetched_at/native models)
and `default_models` in the entries shape; without observation these are null/empty.
This GET reads the last local observation without an upstream request.

At most 256 entries; IDs are unique, 1–150 ASCII letters/digits or `-_.:`. Display
names are 1–100 characters without controls. Upstream targets must be native IDs,
not `c2a-` names; chained mappings are unsupported. Downstream `c2a-` is reserved
for `claude-*`. Save validates configuration locally, not upstream permissions.
An enabled empty list exposes no model.

In custom mode `/v1/models` returns enabled entries intersecting the selected
account's actual catalog, not the pool union. Send `id`; `display_name` is a human
label. Generation response model follows the selected ID. Actual target access
is still checked, and edits do not rename an in-flight response. See
[model naming](API.md#native-names-c2a--names-and-configured-aliases).

`POST /models/upstream` accepts `{"account_id":null}` or an existing account ID,
without expected_revision. It runs only on explicit administrator action and
reuses account concurrency one, proxy/profile, pause/cooldown and pacing. It does
not generate a message. Automatic selection chooses a ready idle account. A
missing selected account returns 404, busy/cooling 429 and paused 503. Access
failure follows account-pause policy and preserves the last successful observation.
The request is bounded to eight seconds.

Success returns schema_version/revision/upstream_catalog/default_models and
persists the observation/sanitized audit. It changes neither mappings, enablement
nor global configuration revision. Each template uses the same native ID as
id/display_name/upstream_model. After an explicit successful fetch, the console
adds every missing native model to its draft, deduplicated by ID and enabled,
while preserving existing labels, mappings and disabled rows. Save applies it.
Repeated merge, add-one and per-row upstream selection are also available.

The catalog and generation paths do not restrict model families. Actual account
catalogs must still confirm availability. An initially empty disabled mapping
can show editable defaults; row edits enable custom mode, but only Save changes
service behavior. Save disabled mode to return to native catalogs. Fetch/poll
must not overwrite unsaved drafts.

## API tokens

Creation body:

```json
{"expected_revision":2,"label":"Work client","max_concurrency":2,"expires_at":1893456000}
```

Label is nonempty and at most 80 characters; max_concurrency is 1–512; expires_at
is a future Unix second or null/omitted for no expiry. At most 512 records,
including expired ones; delete unused records. The response contains
schema_version/revision/id/token. The complete `c2a_tok_...` appears only here and
cannot be read again.

GET returns tokens with id/label/max_concurrency/created_at/expires_at/expired/
active_requests, never plaintext, digests or recoverable credentials.
`environment_key_enabled:true` means the original environment gateway key still
works; this page cannot revoke it, and managed-token limits do not cap it.
Changing that key affects encryption and is not a substitute for token revocation.
DELETE accepts `{"expected_revision":3}` and returns revision/deleted=true.

Token limits cover authenticated work and complete JSON/SSE delivery; exhaustion
returns 429. Account and global caps remain effective. Deletion/expiry rejects new
requests while admitted work drains. Session namespaces are isolated by owner.
API tokens have no control/Bot scope. Bot credentials cannot read models, tokens
or logs.

## Message-status logs

```text
GET /control/v1/logs?level=all&account_id=<account ID>&limit=100&before=<cursor>
```

Level is all/error/normal; omit account for all accounts. Before pages backward.
Limit defaults to 100 and is clamped to 1–200. Response includes records,
next_before, has_more, retention limits, healthy and dropped queue count.

Records retain request/account IDs, requested `model`, token_id/token_name,
upstream/downstream direction, JSON/SSE mode, phase, status, timings, bytes and
chunks. Token names are request-time snapshots and survive token deletion; the
environment key has a separate category. Old missing attribution is not guessed.
Model retains the incoming ID, including c2a-, without other request content.
Absent/old model metadata displays as `—`, without backfill.

Retention is at most 10000 records/seven days. Unfinished records after a crash
are marked interrupted. Streaming HTTP 200 only confirms headers; terminal errors
still belong in error results. No chat text, internal summaries, Cookie or proxy
passwords are stored.

Generation records may contain `usage`; older records/non-generation operations
use null, never fabricated backfill or zero:

```json
{"input_tokens":1200,"output_tokens":80,"cache_creation_input_tokens":200,"cache_read_input_tokens":1000,"estimated":true,"complete":true,"basis":"logical_request"}
```

- Downstream basis=logical_request uses complete logical input, **including**
  simulated cache creation/read, and the same output estimate as the usage ledger.
  Messages log input equals ordinary input plus creation plus read in response
  usage; output equals response output_tokens. Chat/Responses retain their wire
  format while logs use the local ledger, not legacy placeholder usage values.
- Upstream basis=sent_prompt independently estimates each sent prompt and received
  text/thinking/partial_json fragments, excluding history already in the web chat.
  Internal summaries/retries have their own counts. Accumulate Unicode characters
  then ceil(count/4), rather than rounding each stream chunk separately.
- complete=false means unfinished/failed. Received partial upstream output can
  be shown as incomplete; a failed attempt with no observed output is null.
  Only a successful actually-empty output is zero. Downstream publishes final
  output only after successful body EOF; failed/disconnected output is null.
- Cache fields are null when disabled or success is not established. Counts are
  local estimates with estimated=true, not official metering or billing.

The console combines Token usage immediately after the requested model: input and
output on the first line; confirmed simulated cache read/write and a `1h` badge
below. Ordinary input equals total input minus cache creation and read, avoiding
double counting. Each historical record keeps its own cache split regardless of
the current setting. Compact K/M counts expose exact values and total input on
hover; unknown/pending values and received partial output remain distinct.

## Token analytics and simulated amounts

`GET /control/v1/analytics?range=24h&granularity=hour` is admin-only; the original
Bot `/usage` interface is unchanged.

Range is today/24h(default)/7d/month/last_month/all. Calendar windows use configured
IANA CLAUDE_TIMEZONE. Last month excludes the current month's start. All covers
the durable ledger and survives restart. Granularity is hour(default: 24 hourly
segments over the last 24h) or day(seven 24h segments over the last 7d). Trend
window is independent of summary range. `trend_window.buckets` supplies boundaries;
empty segments are zero-filled and the final segment includes the current second.

Response includes schema_version/range/granularity/timezone/window/trend_window/
totals/tokens/series/by_account, plus estimated/currency=USD/price_source/price_date/
ledger_since/truncated=false. Tokens contain id/name/totals; series contain
id/name/points, each point using Totals. Display up to 12 token series plus an
others group. Above 2048 historical groups, grouping is marked by
grouped_historical_tokens while totals remain complete.

Totals fields:

- records, requests(business), internal_requests, successful_requests,
  unmetered_requests, legacy_records, priced_requests, unpriced_requests,
  cache_enabled_requests;
- input_estimate, output_estimate, physical_estimate, uncached_input,
  cache_creation_simulated, cache_read_simulated;
- total_nano_usd, baseline_nano_usd, input_nano_usd, output_nano_usd,
  cache_write_nano_usd, cache_read_nano_usd;
- priced_uncached_input, priced_output, priced_cache_creation and priced_cache_read
  are priced Token subsets, preventing unknown-price input being paired with a
  partial amount subtotal.

Amounts are integer 10⁻⁹ USD; divide by 10⁹ for display. Unpriced or unknown
consumption does not mean free. Unknown models are not guessed and old records
are not repriced. Pre-account rejection, disconnects and internal failures may
be incompletely metered; diagnose state from logs. Queries do not truncate to
10000 rows; after eight seconds they return 504 rather than a false complete total.
No provider query is made. See [accounting examples](USAGE_ANALYTICS_EN.md).

## Version management

Administrator only: `GET /updates`, `POST /updates/check`, `POST /updates/apply`,
and `POST /updates/policy`.
Bot reads and public API credentials are denied. Check/apply return 202; apply
requires `expected_version`, `target_version` and `catalog_revision` from GET.
Stale versions/catalogs, unoffered targets or concurrent switches return 409;
invalid requests return 400. GET includes `current_version`, `supervisor_version`,
`repository`, `enabled`, `checking`, check times, `check_error`, `update_available`,
`latest`, up to two `rollback_versions`, `catalog_revision`, `busy`, `job` and
download progress. A failed job may report `recovered:true`.
Capable supervisors also return `auto_patch_supported:true`, `auto_patch`
(`enabled`, `revision`, `blocked_versions`), `patch_update` and `job.automatic`.
The current line's `patch_update` may differ from the overall `latest` feature update.
POST policy with `{"expected_revision":1,"auto_patch":false}`, using
`auto_patch.revision`. Success returns 200; a stale policy or active switch returns
409. Unfinished installations are not retried automatically but may be retried
manually. Manual rollback persistently disables automatic patches. Missing fields
identify an older supervisor requiring a base upgrade. Standalone unsupported
platforms report `enabled:false`. Internal `/_runtime/*` operations are never
exposed by the supervisor. See [Version manager](UPDATES.md) for release/data rules.
