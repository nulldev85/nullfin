"""CI-only container checks using disposable data and a local Stremio fixture.

Run on a Linux Docker runner: python3 scripts/smoke-container.py IMAGE.
The previous release seeds upgrade data; no operator data or credentials are used.
"""

import argparse
import http.server
import json
import os
import pathlib
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True).strip()


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Fixture(http.server.BaseHTTPRequestHandler):
    video = b""

    def log_message(self, *_args):
        pass

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        if path == "/manifest.json":
            value = {
                "id": "org.nullfin.smoke", "version": "1.0.0", "name": "Smoke fixture",
                "resources": ["stream"], "types": ["movie"], "idPrefixes": ["tt"],
                "catalogs": [],
            }
        elif path.startswith("/stream/movie/"):
            value = {"streams": [{
                "name": "Local fixture", "title": "Local fixture Score: +42",
                "url": f"http://127.0.0.1:{self.server.server_port}/media.mp4",
                "behaviorHints": {"filename": "Nullfin.Smoke.1080p.mp4", "videoSize": len(self.video)},
            }]}
        elif path == "/media.mp4":
            start, end = 0, len(self.video) - 1
            ranged = self.headers.get("Range")
            if ranged:
                first, last = ranged.removeprefix("bytes=").split("-", 1)
                start = int(first) if first else max(0, len(self.video) - int(last))
                end = min(int(last), end) if first and last else end
            data = self.video[start:end + 1]
            self.send_response(206 if ranged else 200)
            self.send_header("Content-Type", "video/mp4")
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(len(data)))
            if ranged:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(self.video)}")
            self.end_headers()
            self.wfile.write(data)
            return
        else:
            self.send_error(404)
            return
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Server:
    def __init__(self, data):
        self.data = data
        self.name = "nullfin-smoke-" + uuid.uuid4().hex[:12]
        self.port = free_port()
        self.origin = f"http://127.0.0.1:{self.port}"
        self.token = None

    def start(self, image):
        docker("run", "-d", "--name", self.name, "--network", "host",
               "--user", f"{os.getuid()}:{os.getgid()}",
               "-v", f"{self.data}:/data", "-e", f"PORT={self.port}",
               "-e", "DISABLE_DHT=true", "-e", "DISABLE_STARTUP_TASKS=true",
               "-e", "TORRENT_HTTP_PORT=0", image)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            try:
                self.request("/health", raw=True)
                return
            except (OSError, urllib.error.URLError):
                if docker("inspect", self.name, "--format", "{{.State.Running}}") != "true":
                    break
                time.sleep(1)
        raise RuntimeError("container did not become healthy: " + docker("logs", self.name)[-6000:])

    def stop(self):
        docker("rm", "-f", self.name)

    def request(self, path, body=None, method=None, raw=False, headers=None):
        auth = 'MediaBrowser Client="NullfinSmoke", Device="CI", DeviceId="smoke", Version="1.0"'
        if self.token:
            auth += f', Token="{self.token}"'
        request = urllib.request.Request(
            self.origin + path, method=method,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"Authorization": auth, "Content-Type": "application/json", **(headers or {})},
        )
        with urllib.request.urlopen(request, timeout=45) as response:
            data = response.read()
            return data if raw else (json.loads(data) if data else None)

    def setup(self):
        self.request("/startup/user", {"Name": "smoke", "Password": "ci-disposable-password"})
        self.request("/startup/configuration", {"ServerName": "Nullfin upgrade smoke"})
        self.request("/startup/complete", {}, method="POST")
        self.login()

    def login(self):
        value = self.request("/users/authenticatebyname", {"Username": "smoke", "Pw": "ci-disposable-password"})
        self.token = value["AccessToken"]
        self.user_id = value["User"]["Id"]


def check_persistence(server, addon_id, movie_id):
    server.login()
    assert server.request("/system/info/public")["ServerName"] == "Nullfin upgrade smoke"
    try:
        server.request("/startup/configuration")
    except urllib.error.HTTPError as error:
        assert error.code == 403, error.code
    else:
        raise AssertionError("setup wizard reopened")
    assert any(addon["id"] == addon_id for addon in server.request("/addons"))
    item = server.request(f"/users/{server.user_id}/items/{movie_id}")
    assert item["Name"] == "Nullfin Smoke Movie"
    assert item["UserData"]["IsFavorite"], "favorite lost during migration/restart"


def check_playback(server, movie_id, check_redaction=True):
    info = server.request(f"/items/{movie_id}/playbackinfo", {})
    assert info["MediaSources"], "fixture addon returned no playable source"
    source = info["MediaSources"][0]
    assert source["SupportsDirectPlay"]
    provider = source.get("remux", {}).get("provider_info", {})
    if check_redaction:
        assert "descriptor" not in provider, "upstream credentials exposed"
    query = urllib.parse.urlencode({
        "static": "true", "MediaSourceId": source["Id"], "PlaySessionId": info["PlaySessionId"],
    })
    data = server.request(f"/videos/{movie_id}/stream?{query}", raw=True,
                          headers={"Range": "bytes=0-1023"})
    assert data == Fixture.video[:1024], "playback did not return the selected fixture bytes"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("image")
    parser.add_argument("--previous", default="ghcr.io/nulldev85/nullfin:v0.33.0")
    args = parser.parse_args()
    Fixture.video = pathlib.Path("crates/remux-server/assets/no-streams.mp4").read_bytes()
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture) as fixture:
        threading.Thread(target=fixture.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory(prefix="nullfin-smoke-") as folder:
            folder = pathlib.Path(folder)
            # Run as the current user so only disposable test data is written.
            data = folder / "data"
            data.mkdir()
            server = Server(data)
            running = False
            try:
                server.start(args.previous)
                running = True
                server.setup()
                addon = server.request("/addons", {
                    "preset": {"kind": "stremio", "config": {"manifest_url": f"http://127.0.0.1:{fixture.server_port}/manifest.json"}},
                    "name": "Local smoke fixture", "resources": ["stream"], "types": ["movie"],
                })
                server.stop()
                running = False
                movie_id = uuid.uuid4().hex
                # Populate a real old-version database with a library item. This
                # checks that migrations preserve content and rebuild stream rows.
                with sqlite3.connect(data / "db.sqlite") as db:
                    db.execute("INSERT INTO media (id,title,kind,external_ids,created_at,updated_at,refreshed_at) VALUES (?,?,?,?,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP)",
                               (uuid.UUID(movie_id).bytes, "Nullfin Smoke Movie", "movie", json.dumps({"imdb": "tt0113277"})))
                server.start(args.previous)
                running = True
                server.login()
                server.request(f"/users/{server.user_id}/favoriteitems/{movie_id}", {}, method="POST")
                check_playback(server, movie_id, check_redaction=False)
                server.stop()
                running = False
                server.start(args.image)
                running = True
                check_persistence(server, addon["id"], movie_id)
                check_playback(server, movie_id)
                docker("restart", server.name)
                deadline = time.monotonic() + 90
                while True:
                    try:
                        server.request("/health", raw=True)
                        break
                    except (OSError, urllib.error.URLError):
                        if time.monotonic() > deadline:
                            raise
                        time.sleep(1)
                check_persistence(server, addon["id"], movie_id)
                check_playback(server, movie_id)
                server.stop()
                running = False
                # A separate empty volume must also support fresh setup.
                fresh = folder / "fresh"
                fresh.mkdir()
                server = Server(fresh)
                server.start(args.image)
                running = True
                server.setup()
                for path in ("/", "/admin/"):
                    html = server.request(path, raw=True).lower()
                    assert b"<html" in html or b"<!doctype" in html, f"missing UI: {path}"
                print("PASS: old-version migration, admin login, library/addon/settings persistence, source playback, restart, fresh setup, and both web UIs")
            finally:
                if running:
                    server.stop()
                # No recursive operation touches paths outside this test's mount.
                for mounted in (data, folder / "fresh"):
                    if mounted.exists():
                        docker("run", "--rm", "-v", f"{mounted}:/smoke-data", "--entrypoint", "sh", args.image,
                               "-c", "find /smoke-data -mindepth 1 -delete")
        fixture.shutdown()


if __name__ == "__main__":
    main()
