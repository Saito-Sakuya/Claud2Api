# Release files

[中文](RELEASES_ZH.md) · English · [Documentation index](DOCS.md)

Current version: **v0.1.0-alpha.7**. Linux AMD64 is supported; these are Alpha prereleases.

## Versions

- **alpha4**: Chat/Responses text compatibility, durable sessions, the console and container packaging fixes.
- **alpha5**: adds native web tools, citations, generated-file downloads and explicitly selected project knowledge. Project search has mock coverage but has not been tested with a real project.

- **alpha6**: makes current native tool names explicit, separates client and web tools, and corrects misleading historical claims about unavailable search.

- **alpha7**: restores neutral capability guidance; availability or earlier use does not mandate another call. The model chooses tools for the current task and can answer directly.

## Docker images

```sh
docker pull ghcr.io/saito-sakuya/claud2api:0.1.0-alpha.7
```

Use `0.1.0-alpha.4` for alpha4, `0.1.0-alpha.5` for alpha5, `0.1.0-alpha.6` for alpha6, or `0.1.0-alpha.7` for alpha7. Matching `v`-prefixed tags are also provided.
Pin a specific version and follow [README](README_EN.md) for configuration. The image contains the verified binary from the same Release.

## Downloads

| File | Purpose |
|---|---|
| `claud2api-linux-amd64` | Native executable and the panel updater's payload; ordinary Docker users do not need to download it manually |
| `claud2api-update.json` | Version, platform, data format and binary SHA-256 for the updater |
| `SHA256SUMS` | Verify downloaded files with `sha256sum -c SHA256SUMS` |
| `claud2api-licenses.tar.gz` | Project and bundled dependency licenses and third-party notices |
| GitHub's automatic Source code archives | Tagged source, not an executable or container image |

For manual installation, unpack the license archive and keep it with the binary. See [version management](UPDATES.md) for updates and rollback; retain the original state directory and keys.
