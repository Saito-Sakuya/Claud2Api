# Sticky Messages conversations

[中文](STICKY_SESSIONS_ZH.md) · English · [Documentation index](DOCS.md)

Production `POST /v1/messages` binds a local session to a configured account and
one real claude.ai conversation. The default idle TTL is **3600 seconds**. A
completed turn refreshes the deadline; active turns cannot expire. The managed
server checks idle expiry at most every 15 seconds, even without new requests.

Different sessions prefer ready idle accounts with fewer live bindings. Multiple
web conversations may belong to one account. Every request and cleanup still
holds that account's single in-flight lease. Busy accounts return 429 rather
than switching a bound conversation to another account merely because it is busy.
Explicit upstream rate-limit rejection can migrate as described below; pause and
cooldown protection remain enforced. Idle bindings do not reserve request capacity.

## Identification

Session identifiers are consumed only for local routing and stored as keyed
hashes. They never enter the web prompt or upstream headers. Precedence:

1. `X-Claud2Api-Session-ID`, `X-C2A-Session-ID`, or the observed pi
   `X-Session-Affinity` header.
2. The gateway `conversation_id` extension, or `metadata.session_id`.
3. A session ID in structured Claude Code `metadata.user_id`: JSON with
   `session_id`, or the observed legacy user/device/account/session format.
4. With no session identifier, an unambiguous exact prefix of previously
   completed, normalized transcript history.

A plain `metadata.user_id` is a user identity and is not used as a session ID.
System prompt, tool definitions or first-user text alone never identify a
conversation. Separate new chats with the same initial text start separate web
conversations. Ambiguous/missing history starts a new binding; it is not guessed.
Clients that truncate/edit history without a stable ID may start a new binding.

Response `X-C2A-Session-ID` and `X-Claud2Api-Session-ID` contain an opaque
gateway session handle. A
client able to retain it can send it on subsequent requests, but clients sending
full supported history need no configuration change for unique history matching.
`X-Session-Affinity` supplies an ordinary fingerprint rather than a returned
opaque handle. Managed-token namespaces are isolated; another token's known
handle returns 403. Whether an intermediary preserves an ID/header is separate
from gateway support.

## Continuations and expiry

The first request submits the complete supported history. A verified continuation
uses the same web conversation, parent assistant UUID and tool-protocol delimiter,
and submits only new turns. Tool result IDs/data remain intact. With an explicit
session ID, changed model/system/tool definitions or nonmatching prior history
resets the web conversation on the same bound account; the previous web ID is
queued for deletion. Without an ID, changed context may require a new binding.
Failed or
interrupted streams invalidate incremental state instead of automatically
replaying an uncertain generation. Unsupported models are rejected before reset.

An append-only system/developer message after the completed transcript is a
supported continuation: both the old conversational history and its complete
instruction context must match exactly. All new instructions remain in the web
prompt. Edits, deletions or insertions inside completed history still reset;
top-level system/model/tool changes do too. Checkpoint validation remains strict:
a summary from a different full instruction context is discarded, never relabeled.

For older Claude Code clients, the gateway removes only the recognized leading
top-level system attribution (`x-anthropic-billing-header`, version, entrypoint,
optional five-hex `cch`, optional workload; at most512 bytes). A merged first line is removed
without dropping the rest of its block. This prevents changing client metadata
alone from resetting the web conversation; no client environment change is needed.
Unknown/malformed attribution and all other system/tool/history changes retain
the existing checks. This is a web-bridge compatibility rule, not native Anthropic
attribution support. Existing sessions whose saved hash includes that line may
rebuild once after upgrade. Compaction settings and saved state are not migrated.

After one idle hour, the local binding/history hashes are removed and the owned
web conversation is queued for deletion. Cleanup acquires the same account lease
as HTTP requests. If the account is busy, cooling, rate limited or paused, deletion
waits; successful deletion or upstream 404 removes the job. Active streams are
never deleted. Removing credentials before cleanup completes can leave web
conversations requiring manual deletion with the original account.

This is conversation reuse. Official prompt-cache statistics and quota savings
are not inferred. Usage remains an estimate of the complete logical context,
not a reported cache discount. Optional [simulated display metrics](SIMULATED_CACHE.md)
can partition this estimate into labeled local reads/one-hour creation; they are
enabled by default for new installations and do not establish actual upstream cache behavior.

## Rate limits and migration

For Messages requests, HTTP 429 or an upstream rate_limit_error before any
content/generation event can use another configured account's available capacity.
A binding already known to be rate limited takes this path before generation.
Authentication/pause, transport errors, truncation, arbitrary errors and partial
text/tool output never trigger replay. Each request tries each account at most
once and checks the exact same model in the destination catalog.

Operator disablement between turns can also move a binding on the next request;
already-admitted work drains. An access pause still requires review and is never
bypassed by selecting another account for that binding.

Migration creates a new web conversation with a matching protected summary plus
uncovered client history when available; otherwise it uses complete supported
history. Original system/tool definitions and retained tool results remain intact.
Neither old web IDs nor parent UUIDs are reused. The local session handle stays
the same. The old committed binding is retained until the destination succeeds;
then it is replaced and the old web ID is queued for deletion with its original
account. Failed destination IDs remain owned cleanup jobs. Cleanup waits for
account limits to allow it. A migrated turn has zero simulated cache reads and
creates a fresh simulated one-hour prefix when that display option is enabled.

A busy otherwise eligible destination is awaited within REQUEST_TIMEOUT_SECONDS
(default 300). Streaming waits send ping events at most every 15 seconds, without
inventing assistant content. Client disconnect cancels waiting. If the entire
pool is limited/paused/ineligible, or the deadline expires, a sanitized error is
still necessary; unconditional uninterrupted work is not guaranteed.

Observed rate-limit deadlines are persisted in the protected SQLite vault.
A valid Retry-After is respected with the configured minimum cooldown. Without
valid timing, ACCOUNT_RATE_LIMIT_FALLBACK_SECONDS defaults to 18000 (five hours,
allowed 60..604800). This fallback applies conservatively to all upstream 429s;
it does not distinguish minute, five-hour or weekly exhaustion and is not an
observed provider reset time. No quota endpoint is polled and a local cooldown
expiring does not assert that the provider's quota has reset.

## Configuration and state

- `STICKY_SESSION_TTL_SECONDS`: default 3600; allowed 60..604800.
- `CLAUD2API_DATA_DIR`: absolute persistent directory, default `/var/lib/claud2api`.
  Both the binary and container persist state there.
- `STICKY_SESSION_STATE_FILE`: optional existing version1 JSON import path;
  the importer leaves that file unchanged and marks a successful import once.
- The container state directory must be owned by UID/GID 65532 and protected
  with 0700. Database and lock files use mode 0600.

An authenticated encrypted snapshot contains account credentials, keyed history
hashes, owned web IDs, parent state, cooldowns, pending deletion jobs and bounded
handoff summaries. Encryption derives from the gateway key; preserve that key
with the database backup. Full transcripts and gateway/admin keys are not stored
in the snapshot. Summaries expire with the session. Normal
restarts retain bindings and cleanup obligations. In-flight uncertain state is
reset and queued for deletion on restore. Corrupt/unreadable state prevents
startup; a write failure blocks new sticky requests pending operator review.
The encrypted snapshot is bounded to 16 MiB, 512 live bindings and bounded hashes;
new sessions/reset work is throttled if cleanup is severely backlogged.

The binary runs its janitor every 15 seconds and drains on shutdown. An exclusive
filesystem lock prevents two processes from opening the same data directory.
Run one gateway instance for each configured real account: separate directories
or browser sessions do not share account locks.

## Proactive context compaction

See [CONTEXT_MANAGEMENT.md](CONTEXT_MANAGEMENT.md) for the default 75-percent
trigger, conservative 128000-unit operating budget, recent-tool-pair preservation,
internal summarization admission and transactional same-account rotation. Logical
history fingerprints remain distinct from the smaller web context. Context management keeps complete tool interactions during rotation.
