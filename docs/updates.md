# Updating Nullfin from the original Remux project

The goal is to keep Nullfin close enough to Remux that updates remain routine. Community changes should be small, focused, and easy to test on their own.

## Before starting

1. Make sure the current community build is healthy.
2. Back up any test data you care about.
3. Read the incoming Remux release notes and note database migrations or configuration changes.

## Bring in the changes

```sh
git fetch --no-tags upstream refs/tags/v0.37.0:refs/remotes/upstream/v0.37.0
git switch -c codex/nullfin-remux-0.37
git merge --no-ff upstream/v0.37.0
```

The local remote named `upstream` should point to `https://github.com/lostb1t/remux.git`. Choose the intended stable tag for each update; do not import a moving development branch by accident. Keep upstream tags out of the fork's release-tag namespace so a Nullfin release can point to the integrated fork commit.

Resolve conflicts in favor of current Remux behavior first, then reapply the smallest community-specific change needed. Avoid copying whole files when a narrow edit will do.

## Check the result

```sh
cargo fmt --all -- --check
cargo test --workspace
```

Also build the container and check these by hand:

- A fresh setup can create an admin account.
- Existing data opens without a new setup prompt.
- Add-ons load and can return a playable stream.
- A Jellyfin client can sign in, browse, and start playback.
- Restarting the container keeps settings and users.

Publish a new image only after those checks pass. Keep the previous working tag available so a rollback is straightforward.

## Remux v0.37.0 integration

This update includes the intervening v0.34–v0.37 stable changes, including source selection and fallback fixes, subtitle matching, stream filters, extensionless HLS handling, collection fixes, and provider-credential redaction. The container moves to Jellyfin FFmpeg 8 and jellyfin-web v12.1.

Nullfin keeps its branding, empty-provider search fallback, configurable provider timeout, unreachable-addon import handling, source-picker compatibility, score labels, and filename/size metadata. Upstream's cached concurrent manifest loading replaces the old local-only runtime loading so its resource restrictions and recovery behavior remain effective. Listing the dashboard does not re-fetch manifests. The provider timeout includes manifest recovery and stream retrieval; score labels use the redacted provider metadata from the effective selected source.

The upstream migrations add device 4K capability, subtitle-extraction and scan-probe settings, and a date index. They invalidate disposable stream rows, which are fetched again from add-ons, while retaining library records and watch state. Upstream also handles the corrected checksum of an already-applied deduplication migration. Back up the database before upgrading; restoring the old image alone does not undo schema changes.

Every container architecture now runs `scripts/smoke-container.py` before its digest is included in a published tag. The check creates a disposable v0.33.0 database, signs in, installs a local fixture add-on, saves a library favorite, and verifies actual selected-source bytes. It upgrades that data to the candidate image and checks login, settings, content, favorite, add-on, playback and restart persistence. A separate empty volume checks fresh setup and both web UIs. No operator data, external media provider or production credentials are used. Server/SDK/utils/macros tests and desktop/dashboard builds remain part of release validation.
