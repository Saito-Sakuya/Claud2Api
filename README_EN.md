<p align="center">
  <img src="ui/icon.svg" alt="Claud2Api Logo" width="112" height="112" />
</p>

<h1 align="center">Claud2Api</h1>

<p align="center"><strong>v0.1.0-alpha.7</strong></p>

<p align="center">
  <a href="DEPLOY_EN.md"><img src="docs/badges/linux-amd64.svg" alt="Supported platform: Linux AMD64" height="26" /></a>
  <a href="DEPLOY_EN.md#docker-compose"><img src="docs/badges/docker-compose.svg" alt="Deployment: Docker Compose" height="26" /></a>
</p>

[中文](README.md) · English · [Documentation index](DOCS.md)

Claud2Api provides a Messages-compatible service and a private administration
console for accounts, browser profiles, account proxies, model configuration,
API tokens, sessions, estimated usage and message-status logs.
Messages can also use Claude web search/fetch, Artifacts, remote code/file tools,
information widgets and a specified project's knowledge, subject to account access.
Alpha7 adds image/PDF/text attachments and real client-tool channels, returning native results as standard text by default. See [test summary (Chinese)](TEST_SUMMARY.md) for real-client validation and remaining gaps.

## Installation

Docker Compose on a Linux AMD64 VPS is the recommended deployment. Access the
console through an SSH tunnel.
Images are available after the matching Release is published. Before then, pin an existing release with `CLAUD2API_IMAGE` or [build from source](DEPLOY_EN.md).

Obtain the source, enter its directory and prepare protected configuration:

```sh
cd Claud2Api
sudo install -d -m 700 /srv/claud2api-private
sudo install -d -m 700 -o 65532 -g 65532 /srv/claud2api-state
sudo python3 scripts/generate_key.py /srv/claud2api-private/gateway-key.txt
sudo python3 scripts/generate_key.py /srv/claud2api-private/admin-key.txt
sudo touch /srv/claud2api-private/accounts.txt
sudo chmod 600 /srv/claud2api-private/*.txt
sudo chown 65532:65532 /srv/claud2api-private/*.txt
```

Create `/srv/claud2api-private/settings.env`:

```dotenv
CLAUD2API_STATE_DIR=/srv/claud2api-state
GATEWAY_API_KEY_FILE=/srv/claud2api-private/gateway-key.txt
ADMIN_API_KEY_FILE=/srv/claud2api-private/admin-key.txt
CLAUDE_ACCOUNTS_FILE=/srv/claud2api-private/accounts.txt
GATEWAY_PORT=18081
CONTROL_PORT=18082
SIMULATED_CACHE_ENABLED=true
CLAUDE_TIMEZONE=Asia/Taipei
CLAUDE_LOCALE=zh-CN
```

Protect the configuration and start:

```sh
sudo chmod 600 /srv/claud2api-private/settings.env
sudo docker compose --env-file /srv/claud2api-private/settings.env pull
sudo docker compose --env-file /srv/claud2api-private/settings.env up -d --no-build
curl http://127.0.0.1:18081/health
```

The account file may be empty; import accounts in the console after startup.
Alternatively, put one authorized Session Key or complete Cookie on each line.
Never commit account credentials, proxy passwords or keys to Git.

See [installation and operation](DEPLOY_EN.md) for native builds and configuration.

Online update checks, upgrades and rollback: [Version manager](UPDATES.md). See [release files](RELEASES.md) for image tags and downloads.

## Administration console

Create a tunnel from your computer:

```sh
ssh -N -L 18082:127.0.0.1:18082 your-vps
```

Open `http://127.0.0.1:18082/` and sign in with the separate key in `admin-key.txt`.

- **Accounts:** bulk import `account name:Session Key`, edit names and view each
  account's independent current/previous five-hour cycle with compact Token counts
  and estimated USD. Choose Chrome/Firefox profiles and independent proxies while
  idle, or assign one proxy to multiple selected accounts.
- **Models:** explicitly fetch an account's complete upstream catalog and merge
  missing models into the draft. Preserve aliases, edits and disabled entries;
  choose native targets and save to apply. Native mode exposes the account's
  complete returned catalog. Configure downstream ID, display name, target and
  enabled state; response `model` follows the selected downstream ID.
- **API tokens:** create, edit or delete tokens. Change the name, concurrency or
  expiry while retaining the secret; the complete credential appears only once
  on creation.
- **Sticky sessions:** inspect and clear idle sessions; default expiry is one
  hour after the last successful request.
- **Runtime settings:** change session/request/scheduling limits and toggle each
  native web-tool group. Simulated one-hour cache display is enabled by default.
- **Version manager:** check at startup and every five minutes. Patches within the
  current release line install automatically; feature updates glow amber and need
  confirmation. Keep data and reload the panel automatically, with rollback to the
  two preceding published versions. Rollback disables automatic patches.
- **Message logs:** compact two-line input/output and simulated cache read/write
  (`1h`) counts; ordinary input excludes cache, and hover reveals exact values.
  Show upstream/downstream JSON/SSE status and token names. Filter
  by all/error/normal or account; unknown and incomplete output are distinct.
- **Overview:** per-token trends, shares and simulated USD costs. Default range
  is 24h; alternatives are today, 7d, this month, last month and lifetime. Trends
  contain 24 hourly or seven daily points. Refresh defaults to 20 seconds after
  login and can be changed.

Proxy URLs support `http://`, `https://`, `socks5://`, `socks5h://` and authentication.
Profile/proxy edits do not proactively clear sessions or valid cache state. Wait
for active account work to finish before changing them. Access control only
through SSH; do not put its port behind a public reverse proxy.

Simulated costs use the record's model rate and input/output/cache categories.
New requests after disabling cache use ordinary input pricing; history is unchanged.
Initial one-hour creation may increase the simulated cost. Unknown prices and old
missing data remain unpriced. Usage/costs are not official billing. See
[usage and simulated costs](USAGE_ANALYTICS_EN.md).

## API usage

Use the service root as Base URL, for example `http://127.0.0.1:18081`. Authenticate
with `gateway-key.txt` or a console-created API token, never the admin key or an
account credential.

```python
from pathlib import Path
import time
from anthropic import Anthropic

client = Anthropic(
    base_url="http://127.0.0.1:18081",
    api_key=Path("/private/gateway-key.txt").read_text().strip(),
    max_retries=0,
)
models = client.models.list()
model_id = models.data[0].id
time.sleep(5)
message = client.messages.create(
    model=model_id,
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}],
)
print(message.content)
```

The example requires the Python `anthropic` package. It waits five seconds for
the default start interval. Adjust that wait if configured differently. Model
listing also uses account scheduling. Wait for the configured start interval
before immediately sending another request; handle 429/Retry-After in clients.

Use actual native account model names, such as `claude-haiku-5-5` or
`claude-sonnet-5-5` when available. To identify this bridge separately in an
intermediary, use `c2a-claude-haiku-5-5` or `c2a-claude-sonnet-5-5`. Both target the
same real models with the same permissions/allowance. With custom model management,
use IDs returned by `/v1/models`. The `c2a-` model prefix differs from the
`c2a_tok_` credential prefix.

See [English API](API.md) / [中文 API](API_ZH.md) for JSON, streaming, tools and
model naming. Administration and restricted information endpoints are documented
in the [control API](CONTROL_API_EN.md).

## Disclaimer

Claud2Api is an independent project, not an official Anthropic product, and has no
affiliation, partnership or endorsement from Claude/Anthropic. Claude and related
trademarks belong to their respective owners. Users must be authorized to use the
accounts, data and services involved and comply with applicable laws and terms.
The project guarantees neither account safety, service availability, allowance
nor real cache savings. Statistics are informational. This is an Alpha release;
back up necessary data before use.

## License

[MIT](LICENSE). Third-party components keep their own licenses; see
[third-party licenses](THIRD_PARTY.md).

## Documentation

Use the [bilingual index](DOCS.md) to find all references. Developers should start
with the [developer guide](DEVELOPMENT.md).
