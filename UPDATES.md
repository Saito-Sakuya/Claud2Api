# Update and roll back

[中文](UPDATES_ZH.md) · English · [Documentation index](DOCS.md)

## Update

The app checks at startup and every five minutes. Patches within the current
release line install automatically, such as `alpha.7 → alpha.7.1`. Moving from
`alpha.6.1` to `alpha.7` is a feature update requiring confirmation. You can disable automatic patches in
**版本管理**.

1. The version button glows amber for an available update. Click **版本管理**.
2. Select the new version and click update.
3. Wait for it to finish. The panel reloads automatically.

Accounts, tokens, settings, sessions and statistics are kept. The app lets current
requests finish before switching; new requests may wait briefly.

## Roll back

You can open **版本管理** even without an update. Select an older version and click
rollback. Your current data is kept. Up to two preceding published versions are
offered; if none are available, there are no rollback choices. Manual rollback
disables automatic patches so the app does not immediately upgrade again. You can
enable them again when ready.

## Before you start

Save open edits and any newly created token. Keep the original data directory and
key, and leave enough disk space for downloads and backups.
Finish any pending tool-result round before upgrading; the old waiting call cannot resume afterward.

Failed downloads or an unreachable GitHub leave the service running. If the new
version fails to start, the app tries to restore the original version automatically.
An unfinished patch is not retried automatically; retry it in the panel, or wait
for a newer patch, which remains eligible for automatic installation.
If a version requires a manual upgrade, follow its release notes.

To enable automatic patches on an older installation, update the container image
or base executable once, retaining the state directory and keys. Updating only the
worker under an old supervisor cannot add this capability; the panel explains this.

For development, publishing and manual recovery, see the
[developer guide](DEVELOPMENT.md#managed-version-changes).
