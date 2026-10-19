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
{"type":"image","source":{"type":"base64","media_type":"image/png","data":"<base64 image bytes>"}}
```

```json
{"type":"document","title":"notes.txt","source":{"type":"text","media_type":"text/plain","data":"Document text"}}
```

Images support PNG, JPEG, GIF and WebP. Documents support base64 PDF,
base64 UTF-8 plain text, and the text source shown above. Put them in user
content or `tool_result.content`. Files are sent through the selected account
with the corresponding turn; raw file bytes do not enter prompts or the gateway database.
Limits are 10 MiB per file, 20 MiB of media per request, and 20 distinct upload
references. The complete HTTP request, including base64 overhead, still follows
the configured request limit (4 MiB by default, configurable up to 32 MiB);
oversized bodies return 413. Remote media URLs, official Files API `file_id`
references and enabled document citations return an explicit 400.
Chat/Responses retain their text input adapters. Token estimates cover the text
envelope and do not represent actual attachment usage.

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
results precede ordinary user text. Result content can also be an array of text,
image or document blocks. IDs, result data and is_error survive prompt conversion.

Custom tools have name, description and an object `input_schema`. JSON Schema
validation supports local schema references; remote/file schema loading is
disabled. Duplicate names and unsupported tool types are rejected. The client
executes custom tools and submits their results in a later user `tool_result`.
Tool choice supports auto, none, any and tool (named), plus
disable_parallel_tool_use. On Messages, `none` disables client and native tools
for that request. `any`, `tool` and `disable_parallel_tool_use` constrain only
client tools; they do not guarantee that native tools will run serially. Invalid
model-generated arguments or violations of required client-tool choice are errors,
not fabricated client-tool results.

Client tools use real web function/result channels with separate upstream aliases. A waiting tool holds the account slot. Results must arrive within the smaller of the request timeout and 600 seconds, and within 840 seconds of the upstream stream start. After expiry or restart, the old state cannot be recovered; the old result returns 409 and the client must start a new conversation. A mismatched request or result also returns 409, but does not consume a still-valid pending call or receipt, so the correct request can still be retried. Old tool tasks are never replayed. The tool-result sidechannel carries only the client's returned result, without gateway-added policy or instruction text.

`tool_choice` is checked per HTTP request. An initial `any` or named `tool` constrains only the response that emits the call; returning its pure result with `auto` can resume the same upstream stream. A later request that switches to `any`, `tool` or `none`, adds user text or appends a system instruction starts a fresh upstream completion from the full validated history; `none` makes that new turn tool-free. A PDF in a tool result also takes the full-history route and is uploaded separately rather than sent through the sidechannel. The upstream returned HTTP 400 for a PDF sidechannel request, so that case cannot resume as an ordinary text result on the same stream.

Claude Code appends system content to tool-result requests, so even a result with no added text starts a fresh completion from full history; the gateway stops the old response first. If a fresh request fails before upstream generation starts, the client may retry the exact same token, full history (including the tool result), context and tool choice for at most `min(request_timeout, 600 seconds)`. Retry receipts store keyed digests, not result bodies, and are capped at 512. If stopping the old response failed, retry must use the original account and stop it first. If generation starts and is then interrupted, retrying its old result returns 409. Expired or restarted state is unrecoverable: the old result returns 409 and that state is cleared. A request that does not match a still-valid receipt also returns 409, but leaves the receipt available for the correct retry. Client tools are never replayed. Each fresh completion adds latency and an account request start. See the test summary (Chinese) document for current and historical client evidence.

### Claude web tools (Claud2Api extension)

Messages can enable remote Claude web tool groups: web search and fetch
(`web_search`), Artifacts (`artifacts`), remote code and file tools (`repl`), and
a set of 37 current web widget descriptors (`widgets`). If `claud2api_tools` is
omitted, administrator-enabled groups are used. An array selects groups for one
request; `[]` disables them. Requesting a group disabled by the administrator
returns 400 before an account is assigned.
If `tool_choice` is `none`, native descriptors are removed for the request.
Otherwise, standard `tools` and `tool_choice` do not select native groups; use
`claud2api_tools` for that. `tool_choice:any`, `tool` and
`disable_parallel_tool_use` apply only to client-tool calls.
Widgets include ask-user, information, consent-prompt and scheduling-display
cards. They are returned as web component data; the gateway does not grant
an interactive reply path for these cards, grant consent, connect MCP services
or execute scheduled actions. Availability depends on the upstream account.
These descriptors are not an account-availability list and do not establish that
all web tools are available. See the [test summary (Chinese)](TEST_SUMMARY.md)
for the tested scope and uncovered cases.

```json
{
  "model": "claude-sonnet-5-5",
  "max_tokens": 1024,
  "claud2api_tools": ["web_search"],
  "claud2api_native_output": "extensions",
  "messages": [{"role":"user","content":"Search Claude's official site and summarize it."}]
}
```

These tools run in Claude's remote environment. The gateway never runs
model-generated commands on its own host. By default,
`claud2api_native_output` is `standard`: native results, citations and downloadable
file links are rendered as ordinary text blocks, while client tools still use
standard `tool_use` and `tool_result` blocks. Set it to `extensions` to receive
the gateway's structured extension format. In JSON, each native block is wrapped
as `claud2api_native`; its `block` field preserves the upstream `tool_use` or
`tool_result` object. In SSE, native start/delta/stop lifecycle events and raw
blocks are preserved. Tool errors retain `is_error`; upstream approval keys and
MCP server-address control metadata are removed.

For `extensions`, round-trip the returned assistant `content` array unchanged in
the complete Messages history; do not create a separate `tool_result`. The next
request validates and retains those blocks. When native groups are active, the
output mode and selected groups participate in the Messages context fingerprint;
changing either starts a fresh upstream conversation, so keep the mode stable and
continue sending the complete supported history. Use standard mode for clients
that expect only standard Messages content block types. Chat/Responses keep their existing
text and top-level extension formats; this option applies to Messages only.

In `extensions` mode, citations use `claud2api_native`, with `block.type` set to
`citation`; the nested `block.citation` has available source fields plus
`text_block_index`, `start_index`, `end_index` and `cited_text`. Offsets count
Unicode characters in the original text block. In `standard` mode, citations are
rendered as source text. These web-session source objects differ from the
encrypted citation format of the official API. Dated official API server-tool
blocks remain unsupported; use `claud2api_tools` instead. Custom MCP connections,
authorization execution and scheduling actions are outside this extension.

Project knowledge search binds one known project through
`claud2api_project_uuid`, a canonical lowercase UUID. The gateway does not list or
read an account's private projects; the selected upstream account must already
have access. When a UUID is supplied and the administrator has enabled project
knowledge, omitting `claud2api_tools` adds `project_knowledge` to the four default
groups. An explicit array may select it. Selecting it without a UUID is a 400
error. If the administrator disabled project knowledge, any project UUID is
rejected, even when `claud2api_tools` is `[]`. Project UUID and tool groups are
part of the Messages session fingerprint; changing them starts a fresh upstream
conversation, so continue sending complete supported history.

```json
{
  "claud2api_project_uuid": "<replace with an authorized canonical lowercase project UUID>",
  "claud2api_tools": ["project_knowledge"],
  "claud2api_native_output": "extensions"
}
```

In Messages `extensions` mode and in Chat/Responses, generated output files carry
`claud2api_download_url` in the native `local_resource` item or
`tool_result.structured_content`. Messages `standard` mode renders the file name
and download path as text. Fetch the provided link at
`GET /v1/native-files/{opaque-handle}` using the same API credential that created
it (the same API token or configured gateway key).
The encrypted, owner-bound link expires within one hour; downloads are limited to
32 MiB and forced as attachments. The originating upstream conversation is kept
until the link expires. Upstream authorization flags are never auto-approved.

## Responses and streaming

Responses have id, type message, role assistant, content, model, stop_reason,
stop_sequence and usage. A client-tool request ends with stop_reason tool_use;
the client executes it and submits the complete transcript with a user
tool_result. Native web tools execute remotely in the same turn and return their
result blocks in the assistant response.

SSE frames use `event: <type>` and `data: <JSON>`. The stream contains
message_start, content_block_start/delta/stop, message_delta and message_stop.
Tool deltas use input_json_delta; arguments are emitted after complete validation.
An interrupted stream or invalid tool frame ends with error rather than a
successful message_stop. In Messages `extensions` mode, native tool deltas use
`claud2api_native_delta` and contain the raw upstream delta; the matching
`content_block_stop` carries the complete block in `claud2api_block`, which
replaces the partial block in history. `standard` mode streams native results,
citations and file links as text and emits no `claud2api_native` content blocks.
Text streams as upstream deltas arrive; no fake delays are added. The first event
may wait for the first supported upstream content.

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
  the web client-tool channel, native web-tool extensions and
  `controls-not-forwarded`. Discarded controls do not
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
  intact. Malformed JSON, invalid required fields, unknown content types,
  unsupported media sources, server tools and invalid tool references still return 400.
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
  String content and supported text blocks preserve their roles. Client tool
  definitions and tool histories remain unsupported, but requests can select native
  web tool groups with `claud2api_tools`. Chat and Responses expose
  `claud2api_native` and `claud2api_citations`; Chat and Responses SSE also use
  `claud2api.native_start`, `claud2api.native_delta`, `claud2api.native_stop`
  and `claud2api.citation` extension events. Responses adds a standard
  `url_citation` annotation only when the cited text uniquely matches the output.
  `claud2api_native_output` applies to Messages only; Chat and Responses retain
  these existing text and top-level extension formats.
  Inputs stay text-structured; native extension blocks in a response cannot be
  submitted as history through these adapters. Use Messages for full structural
  round-tripping.
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
  With a conversation_id, keep `claud2api_tools` and `claud2api_project_uuid`
  unchanged. Changing either returns 409 before upstream access; retain the old
  settings or start with a new conversation_id.
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
