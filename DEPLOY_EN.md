# Installation and operation

[中文](DEPLOY.md) · English · [Documentation index](DOCS.md)

## Docker Compose

Prepare the configuration described in [README](README_EN.md), then run:

```sh
docker compose --env-file /srv/claud2api-private/settings.env pull
docker compose --env-file /srv/claud2api-private/settings.env up -d --no-build
docker compose --env-file /srv/claud2api-private/settings.env logs --tail=100
```

Compose targets Linux, uses host networking and binds both API and control to
loopback. Defaults are API port 18081 and control port 18082. The container runs
as UID/GID 65532: its state directory must be owned by that identity with mode
0700; key/account files must be readable by it with mode 0600. The default memory
limit is 256 MiB; adjust it according to measured account/request load.

Only reverse-proxy `/health` and `/v1/*` to `127.0.0.1:18081`. Use an SSH tunnel
for the control port. If the reverse proxy is another container, choose its backend
address according to the host network topology; its own loopback is not this service.

The default Linux AMD64 image matches the source version and becomes available when that release is published. Before publication, set `CLAUD2API_IMAGE=ghcr.io/saito-sakuya/claud2api:0.1.0-alpha.6.1` to use an existing release. To build from source, use `docker compose --env-file /srv/claud2api-private/settings.env up -d --build`.

See [release files](RELEASES.md) for downloads and checksum verification.

## Native build

Use Rust 1.99.1 or a compatible toolchain, plus C/C++ build tools, CMake, Perl,
pkg-config and libclang. Do not substitute different dependency sources merely
because their version numbers match.

```sh
sudo apt-get install build-essential cmake perl pkg-config libclang-dev
cargo build --locked --release
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Set the environment and run `target/release/claud2api`. Native defaults are
`PORT=8080`, `BIND_HOST=127.0.0.1` and `CONTROL_BIND=127.0.0.1:18082`.

## Startup configuration

The table lists standalone binary defaults. Key `*_FILE` settings take precedence
over their direct environment variants; each file is limited to 1 MiB. Keys must
be single-line and at least 32 bytes after trimming. The three role keys must differ.

| Variable | Default / requirement |
|---|---|
| `GATEWAY_API_KEY` / `GATEWAY_API_KEY_FILE` | Required; public API authentication and root for state encryption |
| `ADMIN_API_KEY` / `ADMIN_API_KEY_FILE` | Required separate administrator key |
| `BOT_READ_KEY` / `BOT_READ_KEY_FILE` | Optional; read-only info/usage/events |
| `BIND_HOST` | `127.0.0.1`; must be an IP address |
| `PORT` | `8080`; nonzero port |
| `CONTROL_BIND` | `127.0.0.1:18082`; loopback IP and nonzero port |
| `CLAUD2API_DATA_DIR` | `/var/lib/claud2api`; absolute path required |
| `CLAUDE_ACCOUNTS_FILE` | `accounts.txt`; at most 1 MiB; blank lines and `#` comments ignored; explicitly configured file must exist |
| `CLAUDE_SESSION_FILE` | Optional single-line credential file; overrides account-list and Cookie/Session environment imports |
| `CLAUDE_COOKIE` / `CLAUDE_SESSION_KEY` | Additional environment account credential; Cookie must contain or be combined with a sessionKey |
| `STICKY_SESSION_STATE_FILE` | Optional existing absolute path for v1 JSON state import |
| `BROWSER_PROFILE` | `chrome_146_windows`; registered profile required |
| `CLAUDE_BASE_URL` | `https://claude.ai`; HTTP(S), no URL credentials/query/fragment; tests may use a local mock |
| `CLAUDE_LOCALE` | `zh-CN`; 1–64 ASCII letters, digits or `-` |
| `CLAUDE_TIMEZONE` | `Asia/Singapore`; valid IANA timezone |
| `DEFAULT_MODEL` | `claude-sonnet-5`; fallback for legacy text routes; Messages still requires model |
| `CLAUDE_THINKING` | Legacy path defaults to auto; `none`, `disabled` or `{"type":"disabled"}` disables that legacy flag |
| `CLAUDE_EFFORT` / `CLAUDE_CODE_EFFORT_LEVEL` | First takes precedence; default `medium`; accepts low/medium/high/xhigh/max, otherwise medium |

Accounts are deduplicated by Session Key with a maximum of 512. Account credentials
must differ from service keys. Environment credentials are imports; omission on a
later startup does not delete persisted accounts. Continue managing them in control.

### Persisted runtime settings

These variables seed new state; saved runtime configuration continues to apply.
Use `true` or `false` for booleans. Invalid numeric/boolean syntax or an out-of-range
value fails startup. Corresponding settings can be changed through the console.

| Variable | Default | Range / meaning |
|---|---:|---|
| `STICKY_SESSION_TTL_SECONDS` | 3600 | 60–604800 seconds after a successful turn |
| `REQUEST_TIMEOUT_SECONDS` | 300 | 1–900 seconds |
| `MAX_REQUEST_BYTES` | 4194304 | 1024–33554432 bytes |
| `SIMULATED_CACHE_ENABLED` | true | Simulated one-hour cache display |
| `CONTEXT_COMPACTION_ENABLED` | true | Proactive context compaction |
| `CONTEXT_BUDGET_TOKENS` | 128000 | 4096–1000000 conservative local budget units |
| `CONTEXT_COMPACTION_TRIGGER_PERCENT` | 75 | 10–90 |
| `CONTEXT_KEEP_RECENT_ROUNDS` | 6 | 1–32 complete text/tool units |
| `NATIVE_WEB_SEARCH_ENABLED` | true | Enable the web search/fetch group |
| `NATIVE_ARTIFACTS_ENABLED` | true | Enable the Artifacts group |
| `NATIVE_REPL_ENABLED` | true | Enable remote code/file tools |
| `NATIVE_WIDGETS_ENABLED` | true | Enable 37 Claude web information/interaction widgets |
| `NATIVE_PROJECT_KNOWLEDGE_ENABLED` | true | Allow bound-project search; requests still need a project UUID |
| `ACCOUNT_MIN_INTERVAL_SECONDS` | 5 | 1–300 seconds |
| `ACCOUNT_REQUESTS_PER_MINUTE` | 6 | 1–60 starts per rolling minute |
| `ACCOUNT_RATE_LIMIT_COOLDOWN_SECONDS` | 60 | 60–86400 seconds; minimum quota cooldown |
| `ACCOUNT_RATE_LIMIT_FALLBACK_SECONDS` | 18000 | 60–604800 seconds when Retry-After is absent/invalid |
| `ACCOUNT_OVERLOAD_COOLDOWN_SECONDS` | 15 | 1–900 seconds |

### Compose versus binary defaults

Compose uses `GATEWAY_PORT=18081`, `CONTROL_PORT=18082` and `Asia/Taipei` as the
default timezone, and fixes both listeners to loopback. Set `CLAUD2API_STATE_DIR`
to a protected host directory, mounted at `/var/lib/claud2api` in the container.
`CLAUD2API_UID`/`CLAUD2API_GID` both default to 65532; update ownership if changed.

`settings.env` supplies Compose interpolation; it does not pass arbitrary variables
into the container. Variables absent from the original environment mapping need
an entry in `compose.override.yaml`, for example:

```yaml
services:
  claud2api:
    environment:
      BROWSER_PROFILE: firefox_151_windows
      ACCOUNT_RATE_LIMIT_FALLBACK_SECONDS: "18000"
      NATIVE_WEB_SEARCH_ENABLED: "true"
      NATIVE_ARTIFACTS_ENABLED: "true"
      NATIVE_REPL_ENABLED: "true"
      NATIVE_WIDGETS_ENABLED: "true"
      NATIVE_PROJECT_KNOWLEDGE_ENABLED: "true"
```

The console can also toggle each native-tool group. Compose does not pass these
environment variables by default. They seed new state; saved runtime settings
take precedence.

Start with the override loaded explicitly:

```sh
docker compose --env-file /srv/claud2api-private/settings.env \
  -f docker-compose.yml -f compose.override.yaml up -d --build
```

An optional Bot key also needs a protected file mount and `BOT_READ_KEY_FILE`
pointing to its container path. Setting the host env variable alone does not mount
the file. Validate without printing resolved configuration using
`docker compose --env-file /srv/claud2api-private/settings.env config --quiet`.
Add the same two `-f` arguments when checking an override.

## Browsers and proxies

Set each account's profile/proxy in the console. Chrome 146–152 and Firefox
144–151 are Windows presets. An empty proxy means direct access; system proxies
are not inherited. `socks5` resolves destinations locally; `socks5h` resolves at
the proxy. One real account should be used by only one instance; other processes
and browsers do not share this scheduler.

## Upgrade and backup

Use the [version manager](UPDATES.md) for compatible online upgrades and rollback.
The state mount must permit execution and have room for three binaries, two database
backups and one download. Keep keys/config unchanged. The supervisor survives worker
updates; protocol/runtime/schema changes require full image replacement.

For a full image replacement, stop the instance before upgrading. Back up the entire state directory, account
files and matching gateway key. The original key is required to read old state;
changing it directly causes decryption failure. Saved settings override new defaults.
Only one writer may open a state directory. Compatible rollback retains current
data. Use a stopped-state backup only for disaster recovery or a verified migration
recovery; account for requests accepted since that backup and never overwrite an
active instance.

State failures degrade health and block new account work. Inspect access failures
before explicitly resuming an account. Restart does not clear pauses or quota
cooldowns. Cleanup for a disabled account remains pending until it is enabled again.

Calendar statistics use `CLAUDE_TIMEZONE`, a valid IANA timezone such as
`Asia/Taipei`. Overview defaults to the last 24 hours. New records freeze simulated
cost using that turn's cache mode/rates; historical amounts are not repriced.

See the [developer guide](DEVELOPMENT.md) for local development, tests, module
ownership and release checks.

### Managed update settings

| Variable | Default | Purpose |
|---|---|---|
| `CLAUD2API_UPDATE_REPOSITORY` | `Saito-Sakuya/Claud2Api` | Trusted public release repository |
| `CLAUD2API_UPDATE_DISABLED` | `false` | Exactly true disables managed checks/switches |
