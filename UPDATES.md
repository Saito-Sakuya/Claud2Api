# Update and roll back

[中文](UPDATES_ZH.md) · English · [Documentation index](DOCS.md)

## Update

The app checks for updates at startup and every five minutes. The version button
at the top left glows amber when a newer version is available.

1. Connect to the admin panel and click **版本管理** at the top left.
2. Select the new version and click update.
3. Wait for it to finish. The panel reloads automatically.

Accounts, tokens, settings, sessions and statistics are kept. The app lets current
requests finish before switching; new requests may wait briefly.

## Roll back

You can open **版本管理** even without an update. Select an older version and click
rollback. Your current data is kept. Up to two preceding published versions are
offered; if none are available, there are no rollback choices.

## Before you start

Save open edits and any newly created token. Keep the original data directory and
key, and leave enough disk space for downloads and backups.

Failed downloads or an unreachable GitHub leave the service running. If the new
version fails to start, the app tries to restore the original version automatically.
If a version requires a manual upgrade, follow its release notes.

For development, publishing and manual recovery, see the
[developer guide](DEVELOPMENT.md#managed-version-changes).
