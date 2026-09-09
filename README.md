<div align="center">
  <img width="180" height="180" src="nullfin-logo.png" alt="Nullfin logo">
  <h1>Nullfin</h1>
  <p>A small, practical bridge between Stremio add-ons and Jellyfin clients.</p>
</div>

Nullfin takes the Stremio add-ons you already use and makes their streams available through a Jellyfin-compatible server. Connect your add-ons, point your preferred client at Nullfin, and you are ready to browse.

I made this fork after running into the same rough edges over and over: incomplete source lists, slow pickers, and migrations that could stall on one unavailable add-on. The fixes live here so the setup is simpler for everyone who comes next.

## What is different

- Compatible clients get the complete source list instead of one `Remote - Unknown` entry.
- The source picker opens without waiting for every remote file to be probed first.
- File sizes can come straight from the add-on when they are available.
- Add-on scores can appear directly in the source list.
- Selecting a source still uses the normal playback checks, so the faster list does not skip the important part.
- The fixes work with standard Stremio streaming add-ons, not just one specific service.

Regular Jellyfin playback behavior stays intact. The faster source-list path is used only for clients that need that compatibility handling.

## Run it

```yaml
services:
  nullfin:
    image: ghcr.io/nulldev85/nullfin:latest
    container_name: nullfin
    restart: unless-stopped
    ports:
      - "3000:3000"
    environment:
      ADDON_STREAM_TIMEOUT_MS: "12000"
    volumes:
      - nullfin-data:/data

volumes:
  nullfin-data:
```

Open `http://localhost:3000`, create the admin account, and add your Stremio manifest URLs from the Addons page. Then add Nullfin to your media client as a Jellyfin server.

That is enough for most setups. If an add-on cannot be reached during an import, Nullfin keeps it disabled so the rest of the migration can finish. You can fix or remove that entry afterward.

Stream providers are queried in parallel. A provider that does not answer within
12 seconds is skipped for that request, so working add-ons can still begin
playback; set `ADDON_STREAM_TIMEOUT_MS` if your providers need a different hard
deadline. Successful Stremio responses remain cached, and an empty refresh does
not erase the last known source list.

## A couple of sensible notes

Keep the data folder persistent and back it up before updating. Pin a versioned image instead of `latest` if you would rather update manually.

Manifest URLs can contain API keys or personal tokens, so do not paste configured URLs into screenshots, bug reports, or public Compose files.

## Updates

The plan is to keep Nullfin close to the project it came from and bring over useful changes without turning maintenance into a second job. New builds will be tested before the `latest` tag moves.

## Credits and license

Nullfin is a community fork of [Remux by lostb1t](https://github.com/lostb1t/remux). It remains licensed under the [GNU Affero General Public License v3.0](LICENSE), and the original copyright and license notices are preserved.
