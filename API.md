# API contract

[中文](API_ZH.md) · English · [Documentation index](DOCS.md)

The service exposes `POST /v1/messages`. The official Anthropic SDK base URL is
the service root, for example `http://127.0.0.1:18081`, without a trailing `/v1`.

## Authentication

Send `x-api-key: <API token>` or `Authorization: Bearer <API token>`.
Use the environment gateway key or a token created on the console Token Management
page. Managed tokens have independent concurrency limits (1..512) and optional
expiration; they cannot access the control API. The environment key remains valid
and has no additional token limit; account/global limits still apply. Deleting or
expiring a token rejects new requests with 401, while admitted requests may finish.
A token concurrency slot remains held until its response completes or disconnects.
Exceeding this limit returns 429 with `Retry-After: 1`.
Browser credentials are configured on the server through `CLAUDE_SESSION_FILE`.
Sending `X-Claude-Cookie` is rejected. Configuring an account never disables
authentication. Supplying conflicting gateway headers is rejected.

Existing tokens can be edited in the console: name, concurrency and expiration. Preserve the original secret and sessions. Lower limits let active requests drain and govern new admissions; edits never reveal the secret again.

## SDK example

```python
from pathlib import Path
import time
from anthropic import Anthropic

client = Anthropic(
    api_key=Path('/private/client/api-token.txt').read_text().strip(),
    base_url='http://127.0.0.1:18081',
    max_retries=0,
)
model_id = client.models.list().data[0].id
time.sleep(5)
message = client.messages.create(
    model=model_id,
    max_tokens=1024,
    messages=[{'role': 'user', 'content': 'Say hello'}],
)
time.sleep(5)
with client.messages.stream(
    model=model_id, max_tokens=1024,
    messages=[{'role': 'user', 'content': 'Say hello'}],
) as stream:
    for text in stream.text_stream:
        print(text, end='', flush=True)
```

The example requires the Python `anthropic` package and waits for the default
five-second account interval. Adjust it to saved settings and handle 429/Retry-After.
Choose a model the account can actually use. The sample ID is not a permission
guarantee. File paths in these examples are client-local, not browser credentials.

`GET /v1/models` refreshes the selected account's authenticated web model catalog.
Use an exact returned ID. All three generation endpoints check that account's
catalog before creating a conversation or writing SSE output; unknown IDs return
400 `invalid_request_error`. Catalogs are cached per account for five minutes;
an explicit model-list request refreshes them sooner. Missing/failed catalogs
return an upstream error and invalidate the cached list. There is no static
allowlist, unconfigured guessed model alias, stale-list fallback or silent model substitution.
The web service still decides the account's final model permissions.

Query example (API_TOKEN is a client-side credential):

```sh
curl http://127.0.0.1:18081/v1/models \
  -H "x-api-key: $API_TOKEN"
```

The list has an OpenAI-style shape:

```json
{"object":"list","data":[{"id":"claude-haiku-5-5","object":"model","created":0,"owned_by":"anthropic"}]}
```

### Native names, c2a- names and configured aliases

With Model Management disabled (default), use an exact native name from the
catalog, e.g. `claude-haiku-5-5` or `claude-sonnet-5-5` if available. A bridge
integration may instead send `c2a-claude-haiku-5-5` or
`c2a-claude-sonnet-5-5`; the gateway resolves the prefix once to the same real
model, validates it against the selected account and reports the native ID.
The list in native mode contains native IDs.

The `c2a-` namespace lets an intermediary distinguish this bridge's optional-control
compatibility contract from its native API model configuration. It does not grant
another model, extra quota or native thinking/cache support. Configure any mapping
in your own client or intermediary; the gateway cannot repair requests rejected
before arrival. `c2a-` model names and `c2a_tok_` API credentials serve different
purposes. See [integration notes](BRIDGE_COMPATIBILITY.md).

With Model Management enabled, the administrator configures `id`, `display_name`,
`upstream_model` and `enabled` on the Model Configuration page. `/v1/models` returns
only enabled configured entries whose native target exists in the selected account's
current catalog. It does not aggregate the whole pool, so heterogeneous account
permissions can produce different lists. `display_name` is a human label; send `id`
as the request `model`. All generation JSON/SSE formats report that configured ID
in their `model` fields, including an explicitly configured `c2a-claude-*` ID.
Mappings use a single native target; they never override actual account permissions.

Use the exposed IDs, including aliases such as `work-haiku` if configured. Exact
configured IDs take precedence, including explicitly disabled entries. A `c2a-claude-*` request can also resolve to an
exposed native `claude-*` entry; the reverse is not implicit. The `c2a-` prefix is
reserved for `claude-*`, so use another prefix for arbitrary aliases. Disabled or
unexposed IDs return 400. Config changes affect new requests; in-flight responses
retain their selected ID. Saving or polling management configuration makes no
provider call. [Control API](CONTROL_API_EN.md) describes configuration payloads.

## Supported input

`model`, `max_tokens` (1..131072) and a nonempty `messages` array are required.
`stream` is optional. The transcript must begin and end with a user turn; assistant
prefill is unsupported. `system` accepts a string or text-block array. Messages
accept strings or arrays of these blocks. The examples are independent; their
valid roles and tool pairing must still be respected:

```json
{"type":"text","text":"hello"}
```

```json
{"type":"tool_use","id":"toolu_example","name":"lookup","input":{"key":"x"}}
```

```json
{"type":"tool_result","tool_use_id":"toolu_example","content":"result","is_error":false}
```

Claude Code compatibility: the recognized leading `x-anthropic-billing-header`
attribution line in top-level `system` is removed before context hashing, web
prompt projection and local input estimates. A standalone first text block or
the first line of a merged block/string is supported; remaining instructions
are preserved exactly. Unknown formats, interior blocks, message-role instructions
and tool data remain intact. See [session behavior](STICKY_SESSIONS.md).

Compatibility extension: messages with role system/developer at any position are
text-only instructions, merged after top-level system in encounter order. They do not
become conversational turns or get discarded. At least one user turn must remain.
Instructions appended after the completed transcript can continue the same web
conversation when the prior transcript and prior instructions match exactly.
New instructions still reach the model. Editing, removing or inserting instructions
inside completed history, or changing top-level system/model/tools, still resets.
Conversational turns keep their relative order. Tool/function/model roles and
unrecognized roles remain errors; the gateway does not guess their semantics.
Role errors include the original message index and an allowlisted role_kind.
Logs contain these safe fields, never raw unknown role values or message content.

Messages use [sticky web conversations](STICKY_SESSIONS.md) with a default
one-hour sliding idle TTL. Explicit session identifiers take precedence;
otherwise only an unambiguous exact completed-history prefix is reused.
Managed tokens have independent session namespaces. An opaque handle from another
token is rejected with 403. The environment key has its own namespace. New
sessions prefer idle accounts with fewer bindings; multiple sessions may share
one account while each account remains at concurrency one. Responses include
opaque `X-C2A-Session-ID` and `X-Claud2Api-Session-ID` handles, accepted as
request headers on later turns. `X-Session-Affinity` also supplies an ordinary
stable fingerprint; it is not interpreted as a returned opaque handle.
Expiry removes the local binding and queues web deletion under the account lock.

Assistant history may contain `thinking` or `redacted_thinking` blocks from an
API-backed conversation. These blocks, including signatures/encrypted data, are
accepted and discarded before prompt conversion. Text and client tool blocks
retain their order and data; assistant entries containing only discarded thinking
are omitted. This does not preserve native thinking state. System/user content
and tool results remain text/tool validated. Unsupported blocks report an
allowlisted `block_kind` and content path, without echoing arbitrary type values.

Tool use belongs to assistant turns, results belong to the following user turn.
Every outstanding call needs one result before the next assistant turn. Tool
results precede ordinary user text. Result content can also be an array of text
blocks. IDs, all result text and is_error survive prompt conversion.

Custom tools have name, description and an object `input_schema`. JSON Schema
validation supports local schema references; remote/file schema loading is
disabled. Duplicate names and unsupported tool types are rejected. The bridge
does not execute any tool. Tool choice supports auto, none, any and tool (named),
plus disable_parallel_tool_use. Invalid model-generated arguments or violations
of required tool choice are errors, not fabricated tool results.

## Responses and streaming

Responses have id, type message, role assistant, content, model, stop_reason,
stop_sequence and usage. A tool request ends with stop_reason tool_use. The client
executes that tool and submits the complete transcript with a user tool_result.
No proxy-generated tool_result is returned as an assistant content block.

SSE frames use `event: <type>` and `data: <JSON>`. The stream contains
message_start, content_block_start/delta/stop, message_delta and message_stop.
Tool deltas use input_json_delta; arguments are emitted after complete validation.
An interrupted stream or invalid tool frame ends with error rather than a
successful message_stop. Text streams as upstream deltas arrive; no fake delays
are added. The first event may wait for the first supported upstream content.

## Limits and unsupported capabilities

- Per-account concurrency is fixed at 1 across all `/v1` routes, including stream
  shutdown and bounded cleanup. Busy capacity returns 429 with `Retry-After: 1`.
  Global concurrency settings cannot raise this account limit. Duplicate cookie
  variants of a sessionKey share capacity; pinned conversations cannot switch
  accounts to bypass a busy lease. Use one instance/session per real account;
  this is an in-process lock, not a shared lock across deployments/browser use.
- Production defaults additionally require 5 seconds between account request
  starts and permit 6 starts in a rolling minute. Too-early requests return 429
  with Retry-After and are not forwarded. All admitted `/v1` routes count.
- Upstream 429/SSE rate limits honor valid Retry-After with a 60-second minimum;
  absent/invalid timing conservatively cools for five hours by default; overload cools for at least 15 seconds. Authentication/access errors
  and API HTML/redirect challenges pause the account. Later requests return 503
  locally until explicit operator review through the private control API.
  Pauses, pacing and cooldowns survive restart; restart does not clear them.
- `X-Claud2Api-Limitations` declares estimated usage, advisory max_tokens,
  the text-tool bridge and `controls-not-forwarded`. Discarded controls do not
  produce warnings in the response content or request failures.
- Usage is a character-based estimate, not official token accounting or billing.
  Prompt-cache counters are enabled by default for new installations. The setting
  SIMULATED_CACHE_ENABLED adds [simulated one-hour display metrics](SIMULATED_CACHE.md)
  with an explicit response header; these do not establish native caching/savings.
  The private overview shows per-token trends, shares and simulated USD costs.
  Rates, cache mode and category counts are captured per usage record; changing the
  current cache switch never reprices history. Unknown rates/old records stay
  unpriced. This does not change public usage or create a real provider bill.
  See [usage analytics and cost examples](USAGE_ANALYTICS_EN.md).
- The requested max_tokens does not precisely control the private web model's
  generation budget. Response bytes, request duration and concurrency are bounded.
- Unsupported optional controls are accepted and silently discarded, regardless
  of their JSON value: thinking (enabled/adaptive/disabled), output_config,
  temperature/top_p/top_k, stop_sequences, metadata, cache_control and tool strict
  annotations. Unknown optional request/known-block fields are also ignored.
  No native thinking, output formatting, sampling, stop enforcement, caching or
  strict generation is implied. Supported session metadata may be consumed
  locally for routing before projection. See [Sub2API notes](SUB2API_COMPATIBILITY.md).
- Text, tool definitions, tool arguments/results and supported tool_choice are
  preserved. Keys such as thinking inside tool input are semantic data and remain
  intact. Malformed JSON, invalid required fields, unknown content types, images,
  documents, server tools and invalid tool references still return 400.
- `/v1/messages/count_tokens`, batches and other public Anthropic API features
  are not implemented; unknown endpoints return a structured 404.
- conversation_id is an optional gateway session-identity extension on Messages.
  Standard callers still send their complete history. Verified continuations
  submit only new turns upstream; configured persistent state survives restart.
  Usage estimates the complete logical context; enabled simulation partitions
  that estimate into explicitly labeled local cache categories.
- `/v1/chat/completions`, `/v1/responses`, `/v1/models` and conversation deletion
  remain compatibility interfaces. Models use an OpenAI-style catalog shape, not
  the complete native Anthropic model-list schema. Chat accepts text messages;
  Responses accepts text input or ordered text messages and optional instructions.
  String content and supported text blocks preserve their roles. Non-text blocks
  and unsupported tool calls are rejected; no native web tools are enabled.
  Both can stream. Usage estimates the actual bridge prompt and output with the
  same character estimator; total tokens is their sum. Chat streaming accepts
  `stream_options.include_usage` for a final usage-only chunk on success.
  A Chat stream failure emits a structured error instead of assistant text or a
  successful stop. Responses sends named SSE events with sequence numbers and
  terminates with `response.completed` or `response.failed`.
  Their optional conversation_id maps are owner-scoped, process-local and lost
  on restart; DELETE /v1/conversations/{id} deletes only an owned legacy mapping.
  This extension appends each payload to the existing web conversation: send
  only new messages when using it. Clients resending complete history should
  omit conversation_id; this adapter does not deduplicate compatibility history.
  They do not share the durable Messages session deletion path or its automatic
  context/summary and quota-migration behavior.

## Errors

```json
{"type":"error","error":{"type":"invalid_request_error","message":"explanation"},"request_id":"req_example"}
```

404 indicates an unknown endpoint or missing legacy conversation.
401 indicates an invalid, expired or deleted API token; 403 an unavailable session scope; 400 unsupported/invalid input; 413 oversized
body; 429 gateway/upstream rate limiting; 502 upstream account/transport/protocol
failure; 503 paused account; 504 timeout; 529 upstream overload. Browser authentication/access errors
are reported as an upstream failure, not as an invalid client gateway key.
Retry-After is preserved when safe numeric metadata is available. The request-id
response header connects responses to sanitized request logs. Raw upstream
response bodies are never forwarded. Partial/uncertain business output is never
replayed; only the documented pre-generation quota path may migrate.

The transport's coherent browser bundle and actual comparison scope are described
in the [installation and transport options](DEPLOY_EN.md). Header matching does not guarantee browser equivalence
or immunity from account restrictions.

Quota-aware Messages migration is documented in [STICKY_SESSIONS.md](STICKY_SESSIONS.md).
Explicit pre-generation rate-limit rejection can use another configured account
with full client history; partial/uncertain generations are never replayed.
Operator disablement between turns can also rebind the next request; access
pauses still require review and are not bypassed.

Proactive [context management](CONTEXT_MANAGEMENT.md) preserves original system/tools,
recent complete tool batches and current input. Internal summaries are not API
response content. X-Claud2Api-Context: managed-estimates discloses the policy;
new physical contexts reset simulated reads and create a new one-hour prefix.
