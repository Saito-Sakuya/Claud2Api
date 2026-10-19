#!/usr/bin/env python3
"""Linux integration acceptance using real workers and local synthetic releases.

python3 scripts/update_acceptance.py --artifacts /tmp/update-artifacts --build
Uses the current Cargo cache/target; never contacts a provider or GitHub.
"""
import argparse
import concurrent.futures
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
ADMIN = "fixture-update-admin-key-at-least-32-characters"
KEY = "fixture-update-public-key-at-least-32-characters"
BOT = "fixture-update-bot-key-at-least-32-characters"
REPO = "Saito-Sakuya/Claud2Api"
BINARY = "claud2api-linux-amd64"
MANIFEST = "claud2api-update.json"
FORMAT = "sqlite-v1/Claud2Api-vault-v1"
ABI = "linux-amd64-glibc-2.36"


def wait(predicate, timeout=45):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(.1)
    raise AssertionError("acceptance condition timed out")


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(number, path, method="GET", body=None, key=ADMIN, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", number, timeout=65)
    hdr = {"Authorization": "Bearer " + key, "Content-Type": "application/json"}
    hdr.update(headers or {})
    conn.request(method, path, None if body is None else json.dumps(body), hdr)
    response = conn.getresponse()
    raw = response.read()
    result = (response.status, dict(response.getheaders()),
              json.loads(raw) if raw and raw[:1] in (b"{", b"[") else raw.decode())
    conn.close()
    return result


def build(artifacts):
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
    build_env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    with tempfile.TemporaryDirectory(prefix="claud2api-versions-") as folder:
        source = Path(folder) / "source"
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns(
            ".git", "target", ".local", "__pycache__"))
        toml = (source / "Cargo.toml").read_text()
        lock = (source / "Cargo.lock").read_text()
        html = (source / "ui/index.html").read_text()
        for version in ("9.1.0", "9.1.1", "9.1.3", "9.2.0"):
            (source / "Cargo.toml").write_text(re.sub(
                r'(?m)^version = "[^"]+"', 'version = "' + version + '"', toml, count=1))
            (source / "Cargo.lock").write_text(re.sub(
                r'(name = "claud2api"\nversion = ")[^"]+', r'\g<1>' + version, lock))
            # Embedded panel must represent the built release as well.
            panel = re.sub(r'V0\.1alpha1|v0\.1\.0-alpha\.\d+(?:\.\d+)*',
                           'v' + version, html, flags=re.IGNORECASE)
            (source / "ui/index.html").write_text(panel)
            subprocess.run(["cargo", "build", "--locked", "--offline"], cwd=source, env=build_env, check=True)
            output = artifacts / ("base" if version == "9.1.0" else version)
            shutil.copy2(target / "debug/claud2api", output)
            subprocess.run(["strip", "--strip-debug", str(output)], check=True)
            if version == "9.1.0":
                shutil.copy2(output, artifacts / version)
    # Valid ELF/descriptor, deliberately damages only the isolated fixture DB
    # during worker startup. This exercises restoration, not just an early reject.
    bad = artifacts / "bad.rs"
    descriptor = json.dumps({"protocol": 1, "version": "v9.1.2", "runtime_abi": ABI,
                             "data_format": FORMAT})
    bad.write_text('fn main() { if std::env::args().any(|a| a == "--describe-build") '
                   '{ println!("{}", r#"' + descriptor + '"#); return; } '
                   'let root=std::env::var("CLAUD2API_DATA_DIR").unwrap(); '
                   'std::fs::write(std::path::Path::new(&root).join("control.sqlite3"), '
                   'b"fixture candidate corruption").unwrap(); std::process::exit(1); }')
    subprocess.run(["rustc", "--edition", "2024", str(bad), "-o", str(artifacts / "9.1.2")], check=True)
    (artifacts / "9.5.0").write_bytes(b"bad checksum fixture")
    (artifacts / "9.6.0").write_bytes(b"incompatible manifest fixture")
    (artifacts / "9.7.0").write_bytes(b"unbounded stream fixture")
    for path in artifacts.iterdir():
        if path.suffix != ".rs":
            path.chmod(0o700)


class Fixture:
    def __init__(self, artifacts):
        self.artifacts = artifacts
        self.mode = "normal"
        self.calls = []
        self.github_calls = []
        self.downloads = []
        self.release_gate = threading.Event()
        self.release_gate.set()
        self.release_entered = threading.Event()
        self.hold = threading.Event()
        self.hold.set()
        self.entered = threading.Event()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def send(self, body, status=200, headers=None):
                raw = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                for k, v in (headers or {}).items():
                    self.send_header(k, str(v))
                self.end_headers()
                self.wfile.write(raw)

            def do_GET(self):
                path = urlsplit(self.path).path
                assert not self.headers.get("Authorization") and not self.headers.get("x-api-key")
                if path == "/api/organizations":
                    return self.send([{"uuid": "fixture-org"}])
                if path.endswith("/app_start"):
                    return self.send({"claude_ai_available_models": {"models": [
                        {"model_id": "claude-haiku-5-5"}]}})
                if path.endswith("/releases"):
                    fixture.release_entered.set()
                    if not fixture.release_gate.wait(90):
                        return self.send({}, 503)
                    fixture.github_calls.append(self.headers.get("If-None-Match"))
                    if fixture.mode == "rate":
                        return self.send({}, 403, {"X-RateLimit-Reset": int(time.time()) + 3600})
                    if fixture.mode == "missing":
                        return self.send({}, 404)
                    etag = '"fixture-' + fixture.mode + '"'
                    if self.headers.get("If-None-Match") == etag:
                        return self.send(b"", 304)
                    versions = fixture.versions()
                    if fixture.mode in ("9.5.0", "9.6.0", "9.7.0"):
                        versions.insert(0, fixture.mode)
                    return self.send([fixture.release(v) for v in versions], headers={"ETag": etag})
                if "/releases/tags/" in path:
                    return self.send(fixture.release(path.rsplit("/", 1)[1].removeprefix("v")))
                if path.startswith("/asset/"):
                    _, _, version, name = path.split("/")
                    # Exercise a trusted redirect before the binary response.
                    if name == BINARY:
                        return self.send(b"", 302, {"Location": "/download/" + version})
                    return self.send(fixture.manifest(version))
                if path.startswith("/download/"):
                    version = path.rsplit("/", 1)[1]
                    fixture.downloads.append(version)
                    raw = (artifacts / version).read_bytes()
                    if version == "9.7.0":
                        self.send_response(200)
                        self.end_headers()
                        self.wfile.write(raw + b"extra")
                        return
                    return self.send(raw)
                self.send({}, 404)

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                body = json.loads(raw)
                if self.path.endswith("/chat_conversations"):
                    return self.send({"uuid": str(uuid.uuid4())}, 201)
                assert self.path.endswith("/completion")
                assert not self.headers.get("Authorization") and not self.headers.get("x-api-key")
                fixture.calls.append({"id": self.path.split("/")[-2],
                                      "parent": body.get("parent_message_uuid"),
                                      "assistant": body["turn_message_uuids"]["assistant_message_uuid"]})
                if "hold-stream" in body["prompt"]:
                    fixture.entered.set()
                    fixture.hold.wait(45)
                data = ('event: content_block_delta\ndata: ' + json.dumps({
                    "type": "content_block_delta", "delta": {"type": "text_delta", "text": "OK"}})
                    + '\n\nevent: message_stop\ndata: {"type":"message_stop"}\n\n').encode()
                self.send(data, headers={"Content-Type": "text/event-stream"})

            def do_DELETE(self):
                self.send(b"", 204)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = "http://127.0.0.1:" + str(self.server.server_port)

    def manifest(self, version):
        raw = (self.artifacts / version).read_bytes()
        return {"protocol": 1, "version": "v" + version, "runtime_abi": ABI,
                "data_format": "incompatible" if version == "9.6.0" else FORMAT,
                "binary": BINARY, "size": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}

    def release(self, version):
        manifest = self.manifest(version)
        assets = []
        for i, (name, raw) in enumerate([(BINARY, (self.artifacts / version).read_bytes()),
                                        (MANIFEST, json.dumps(manifest).encode())]):
            assets.append({"id": i + 1, "name": name, "size": len(raw), "state": "uploaded",
                           "digest": "sha256:" + ("0" * 64 if version == "9.5.0" and i == 0
                                                   else hashlib.sha256(raw).hexdigest()),
                           "browser_download_url": self.url + "/asset/" + version + "/" + name})
        return {"id": int("".join(re.findall(r"\d+", version))), "tag_name": "v" + version, "draft": False,
                "prerelease": False, "published_at": "2026-10-18T00:00:00Z", "assets": assets}

    def versions(self):
        if self.mode == "auto-failure":
            return ["9.2.0", "9.1.2", "9.1.1", "9.1.0"]
        if self.mode == "next-patch":
            return ["9.2.0", "9.1.3", "9.1.1", "9.1.0"]
        return ["9.2.0", "9.1.3", "9.1.1", "9.1.0"]


def acceptance(args):
    artifacts = Path(args.artifacts).resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    if args.build:
        build(artifacts)
    fixture = Fixture(artifacts)
    fixture.release_gate.clear()
    with tempfile.TemporaryDirectory(prefix="claud2api-update-state-") as folder:
        data = Path(folder)
        public, control = args.public_port or port(), args.control_port or port()
        env = dict(os.environ, PORT=str(public), CONTROL_BIND="127.0.0.1:" + str(control),
                   BIND_HOST="127.0.0.1", GATEWAY_API_KEY=KEY, ADMIN_API_KEY=ADMIN, BOT_READ_KEY=BOT,
                   CLAUD2API_DATA_DIR=str(data), CLAUDE_SESSION_KEY="fixture-session-only",
                   CLAUDE_BASE_URL=fixture.url, CLAUD2API_UPDATE_TEST_API=fixture.url,
                   ACCOUNT_MIN_INTERVAL_SECONDS="1", ACCOUNT_REQUESTS_PER_MINUTE="60")
        log = open(artifacts / "worker.log", "a")
        process = None

        def api(path, method="GET", body=None, **kwargs):
            status, _, value = request(control, "/control/v1" + path, method, body, **kwargs)
            assert status == 200, (path, status, value)
            return value

        def start(wait_initial=True):
            nonlocal process
            process = subprocess.Popen([str(artifacts / "base")], env=env, stdout=log, stderr=log)
            wait(lambda: request(public, "/health")[0] == 200)
            wait(lambda: request(control, "/control/v1/updates")[0] == 200)
            if wait_initial:
                wait(check_idle)

        def stop(hard=False):
            nonlocal process
            if process:
                process.kill() if hard else process.terminate()
                process.wait(timeout=35)
                process = None

        def check_idle():
            state = api("/updates")
            return state if state["checked_at"] > 0 and not state["checking"] else None

        def change_policy(enabled, revision=None, key=ADMIN):
            if revision is None:
                revision = api("/updates")["auto_patch"]["revision"]
            return request(control, "/control/v1/updates/policy", "POST",
                           {"expected_revision": revision, "auto_patch": enabled}, key=key)

        def apply(version):
            current = api("/updates")
            payload = {"expected_version": current["current_version"], "target_version": "v" + version,
                       "catalog_revision": current["catalog_revision"]}
            status, _, value = request(control, "/control/v1/updates/apply", "POST", payload)
            assert status == 202, value
            return value["job_id"]

        def finished(job):
            state = api("/updates")
            return state if state["job"]["id"] == job and not state["busy"] else None

        def version_finished(version):
            state = api("/updates")
            return state if state["current_version"] == "v" + version and not state["busy"] else None

        def auto_attempt_finished(version):
            state = api("/updates")
            job = state.get("job") or {}
            if (job.get("target") == "v" + version and job.get("automatic")
                    and job.get("phase") in ("failed", "completed") and not state["busy"]):
                return state
            return None

        def contains_version(values, version):
            return version in values or "v" + version in values

        def messages(history, handle=None, stream=False):
            headers = {"x-c2a-session-id": handle or "update-fixture", "x-api-key": token}
            return request(public, "/v1/messages", "POST", {
                "model": "c2a-claude-haiku-5-5", "max_tokens": 128,
                "messages": history, "stream": stream}, key=token, headers=headers)

        def preserved():
            assert api("/settings")["settings"]["context_budget"] == 200000
            assert api("/accounts")["accounts"][0]["label"] == "Persistent fixture"
            assert api("/tokens")["tokens"][0]["id"] == token_id
            assert request(public, "/v1/models", key=token, headers={"x-api-key": token})[0] == 200
            assert api("/sessions")["sessions"]
            assert api("/logs")["records"]
            assert api("/usage")["requests"] >= 2

        try:
            start(wait_initial=False)
            wait(fixture.release_entered.is_set)
            initial = api("/updates")["current_version"]
            assert initial == "v9.1.0", initial
            assert api("/updates")["check_interval_seconds"] == 300
            before_checks = len(fixture.github_calls)
            for _ in range(3):
                api("/updates")
            assert len(fixture.github_calls) == before_checks, "status polling must not contact the release source"
            for key in (KEY, BOT, "wrong"):
                assert request(control, "/control/v1/updates", key=key)[0] == 401
            policy_state = api("/updates")
            assert policy_state["auto_patch_supported"] is True
            assert policy_state["auto_patch"]["enabled"] is True, "automatic patching should default on"
            assert policy_state["auto_patch"]["blocked_versions"] == []
            initial_policy_revision = policy_state["auto_patch"]["revision"]
            for key in (KEY, BOT, "wrong"):
                assert change_policy(False, initial_policy_revision, key=key)[0] == 401
            status, _, _ = change_policy(False, initial_policy_revision)
            assert status == 200
            disabled = api("/updates")["auto_patch"]
            assert disabled["enabled"] is False and disabled["revision"] > initial_policy_revision
            status, _, _ = change_policy(True, initial_policy_revision)
            assert status == 409, "stale policy revision must be rejected"
            assert request(public, "/control/v1/updates")[0] == 404
            assert request(control, "/control/v1/_runtime/export", "POST", {})[0] == 403
            assert request(control, "/control/v1/updates/check", "POST", {}, headers={"Origin": "https://evil.invalid"})[0] == 403

            revision = api("/settings")["revision"]
            settings = api("/settings")["settings"]
            assert settings["context_budget"] == 128000
            settings["context_budget"] = 200000
            api("/settings", "PUT", {"expected_revision": revision, "settings": settings})
            revision = api("/settings")["revision"]
            account = api("/accounts")["accounts"][0]
            api("/accounts/" + account["id"], "PATCH", {"expected_revision": revision, "label": "Persistent fixture"})
            result = api("/tokens", "POST", {"expected_revision": api("/settings")["revision"],
                                             "label": "Persistent token", "max_concurrency": 2})
            token, token_id = result["token"], result["id"]
            legacy_payload = {"model": "c2a-claude-haiku-5-5", "conversation_id": "legacy-update-fixture",
                              "messages": [{"role": "user", "content": "legacy first"}]}
            status, _, legacy = request(public, "/v1/chat/completions", "POST", legacy_payload, key=token)
            assert status == 200, legacy
            legacy_call = fixture.calls[-1]
            time.sleep(1.1)
            history = [{"role": "user", "content": "first"}]
            status, headers, result = messages(history)
            assert status == 200, result
            handle = headers["x-c2a-session-id"]
            first_call = fixture.calls[-1]

            # Finish the initial catalog check with policy disabled, then verify the setting on restart.
            fixture.release_gate.set()
            state = wait(check_idle)
            assert state["current_version"] == initial
            assert state["auto_patch"]["enabled"] is False
            assert state["patch_update"]["version"] == "v9.1.3"
            assert state["latest"]["version"] == "v9.2.0"
            stop(hard=True)
            start()
            state = api("/updates")
            assert state["current_version"] == initial
            assert state["auto_patch"]["enabled"] is False, "policy must persist across restart"
            preserved()
            # Legacy bindings are intentionally process-local; establish a fresh baseline after restart.
            time.sleep(1.1)
            legacy_payload["messages"] = [{"role": "user", "content": "legacy after policy restart"}]
            status, _, legacy = request(public, "/v1/chat/completions", "POST", legacy_payload, key=token)
            assert status == 200, legacy
            legacy_call = fixture.calls[-1]
            status, _, _ = change_policy(True, state["auto_patch"]["revision"])
            assert status == 200
            state = wait(lambda: version_finished("9.1.3"))
            assert state["latest"]["version"] == "v9.2.0"
            assert state["current_version"] == "v9.1.3", "auto-patch must not install the feature release"
            assert state["auto_patch"]["enabled"] is True
            assert not contains_version(state["auto_patch"]["blocked_versions"], "9.1.3")
            assert fixture.downloads.count("9.1.3") == 1
            preserved()
            time.sleep(1.1)
            legacy_payload["messages"] = [{"role": "user", "content": "legacy after automatic patch"}]
            status, _, legacy = request(public, "/v1/chat/completions", "POST", legacy_payload, key=token)
            assert status == 200, legacy
            assert fixture.calls[-1]["id"] == legacy_call["id"], (
                "automatic patch must preserve legacy conversation binding: "
                f"expected {(legacy_call['id'], legacy_call['assistant'])}, "
                f"got {(fixture.calls[-1]['id'], fixture.calls[-1]['parent'])}")
            assert fixture.calls[-1]["parent"] == legacy_call["assistant"]
            legacy_call = fixture.calls[-1]

            # A feature release remains manually selectable; updating waits for the active SSE.
            state = api("/updates")
            valid = {"expected_version": state["current_version"], "target_version": "v9.2.0",
                     "catalog_revision": state["catalog_revision"]}
            for changed in ({"expected_version": "v0.0.0"},
                            {"catalog_revision": state["catalog_revision"] + 999},
                            {"target_version": "v8.0.0"}):
                assert request(control, "/control/v1/updates/apply", "POST", dict(valid, **changed))[0] == 409
            assert request(control, "/control/v1/updates/apply", "POST", dict(valid, extra=True))[0] == 400
            history += [{"role": "assistant", "content": result["content"]}, {"role": "user", "content": "hold-stream"}]
            time.sleep(1.1)
            fixture.hold.clear()
            with concurrent.futures.ThreadPoolExecutor() as pool:
                held = pool.submit(messages, history, handle, True)
                wait(fixture.entered.is_set)
                job = apply("9.2.0")
                wait(lambda: api("/updates")["job"]["phase"] == "waiting")
                assert request(control, "/control/v1/updates/apply", "POST", valid)[0] == 409
                assert api("/updates")["current_version"] == "v9.1.3"
                assert api("/info")["healthy"]
                assert not held.done(), "active stream was interrupted"
                fixture.hold.set()
                status, _, sse = held.result(timeout=20)
                assert status == 200 and "message_stop" in sse
                response_times = []

                def probe():
                    while not finished(job):
                        begin = time.monotonic()
                        assert request(public, "/health")[0] == 200
                        response_times.append(time.monotonic() - begin)
                        time.sleep(.03)

                probe_future = pool.submit(probe)
                state = wait(lambda: finished(job))
                probe_future.result(timeout=20)
            assert state["job"]["phase"] == "completed", state
            assert state["current_version"] == "v9.2.0"
            assert state["auto_patch"]["enabled"] is True
            assert [v["version"] for v in state["rollback_versions"]] == ["v9.1.3", "v9.1.1"]
            preserved()
            time.sleep(1.1)
            history += [{"role": "assistant", "content": "OK"}, {"role": "user", "content": "after update"}]
            status, headers, result = messages(history, handle)
            assert status == 200, result
            assert headers["x-c2a-session-id"] == handle
            assert result["usage"]["cache_read_input_tokens"] > 0
            assert fixture.calls[-1]["id"] == first_call["id"]
            assert fixture.calls[-1]["parent"] == fixture.calls[-2]["assistant"]
            time.sleep(1.1)
            legacy_payload["messages"] = [{"role": "user", "content": "legacy after update"}]
            status, _, legacy = request(public, "/v1/chat/completions", "POST", legacy_payload, key=token)
            assert status == 200, legacy
            assert fixture.calls[-1]["id"] == legacy_call["id"]
            assert fixture.calls[-1]["parent"] == legacy_call["assistant"]
            legacy_call = fixture.calls[-1]

            # Rolling back through successive releases must turn off patching and stay put after restart.
            job = apply("9.1.3")
            state = wait(lambda: finished(job))
            assert state["job"]["phase"] == "completed"
            assert state["current_version"] == "v9.1.3"
            assert state["auto_patch"]["enabled"] is False
            preserved()
            time.sleep(1.1)
            status, _, legacy = request(public, "/v1/chat/completions", "POST", legacy_payload, key=token)
            assert status == 200, legacy
            assert fixture.calls[-1]["id"] == legacy_call["id"]
            assert fixture.calls[-1]["parent"] == legacy_call["assistant"]
            legacy_call = fixture.calls[-1]
            job = apply("9.1.1")
            state = wait(lambda: finished(job))
            assert state["job"]["phase"] == "completed"
            assert state["current_version"] == "v9.1.1"
            assert state["auto_patch"]["enabled"] is False
            preserved()

            fixture.mode = "normal"
            stop(hard=True)
            start()
            state = api("/updates")
            assert state["current_version"] == "v9.1.1", "manual rollback must persist across restart"
            assert state["auto_patch"]["enabled"] is False
            assert state["patch_update"]["version"] == "v9.1.3"
            assert fixture.downloads.count("9.1.3") == 1, "disabled policy must not upgrade after rollback"
            preserved()

            # The failed patch is blocked across restart; only an administrator's explicit retry can reattempt it.
            fixture.mode = "auto-failure"
            stop(hard=True)
            start()
            state = api("/updates")
            assert state["current_version"] == "v9.1.1"
            assert state["auto_patch"]["enabled"] is False
            assert state["patch_update"]["version"] == "v9.1.2"
            status, _, _ = change_policy(True, state["auto_patch"]["revision"])
            assert status == 200
            state = wait(lambda: auto_attempt_finished("9.1.2"))
            assert state["job"]["phase"] == "failed" and state["job"]["automatic"] is True
            assert state["job"]["recovered"] is True
            assert contains_version(state["auto_patch"]["blocked_versions"], "9.1.2")
            assert state["current_version"] == "v9.1.1"
            assert fixture.downloads.count("9.1.2") == 1
            failed_auto_job = state["job"]["id"]
            failed_policy_revision = state["auto_patch"]["revision"]
            stop(hard=True)
            start()
            state = api("/updates")
            assert state["current_version"] == "v9.1.1"
            assert contains_version(state["auto_patch"]["blocked_versions"], "9.1.2")
            assert state["auto_patch"]["revision"] == failed_policy_revision, "restart must not retry the blocked patch"
            time.sleep(.5)
            assert api("/updates")["auto_patch"]["revision"] == failed_policy_revision
            preserved()

            job = apply("9.1.2")
            state = wait(lambda: finished(job))
            assert state["job"]["phase"] == "failed" and state["job"]["automatic"] is False
            assert state["job"]["id"] != failed_auto_job, "manual retry must start a new attempt"
            assert state["auto_patch"]["revision"] > failed_policy_revision
            assert fixture.downloads.count("9.1.2") == 1, "verified failed binaries may be retried from cache"
            assert contains_version(state["auto_patch"]["blocked_versions"], "9.1.2")
            assert state["current_version"] == "v9.1.1"

            fixture.mode = "next-patch"
            stop(hard=True)
            start()
            state = wait(lambda: version_finished("9.1.3"))
            assert state["job"]["phase"] == "completed" and state["job"]["automatic"] is True
            assert state["auto_patch"]["enabled"] is True
            assert not contains_version(state["auto_patch"]["blocked_versions"], "9.1.2"), (
                "a successful newer patch should clear versions that are no longer candidates")
            assert fixture.downloads.count("9.1.3") == 1, "previously verified patch may be reused from cache"
            preserved()

            for bad in ("9.5.0", "9.6.0", "9.7.0"):
                stop()
                fixture.mode = bad
                start()
                job = apply(bad)
                state = wait(lambda: finished(job))
                assert state["job"]["phase"] == "failed", state
                assert state["job"]["recovered"] is False, state
                assert state["current_version"] == "v9.1.3"
                preserved()
            stop()
            fixture.mode = "rate"
            start()
            state = api("/updates")
            assert state["next_check_at"] >= int(time.time()) + 3500
            before_checks = len(fixture.github_calls)
            request(control, "/control/v1/updates/check", "POST", {})
            time.sleep(.2)
            assert len(fixture.github_calls) == before_checks
            preserved()
            stop()
            fixture.mode = "normal"
            start()
            report = {"passed": True, "initial_version": initial, "current_version": "v9.1.3",
                      "real_worker_versions": 4, "recovery_from_corrupt_database": True,
                      "rejected_assets": ["checksum", "data format", "oversized stream"],
                      "active_sse_preserved": True, "continuation_and_cache_preserved": True,
                      "legacy_binding_handoff_preserved": True,
                      "auto_patch_highest_same_line": "v9.1.3",
                      "feature_release_requires_manual_apply": "v9.2.0",
                      "policy_default_cas_auth_and_restart_persistence": True,
                      "blocked_patch_not_retried_and_manual_retry_allowed": "v9.1.2",
                      "later_patch_after_block": "v9.1.3",
                      "rollback_disabled_auto_patch_without_reupgrade": "v9.1.1",
                      "peak_health_queue_seconds": max(response_times, default=0),
                      "public_port": public, "control_port": control}
            (artifacts / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report), flush=True)
            if args.serve:
                print("Fixture ready for browser acceptance", flush=True)
                while True:
                    time.sleep(1)
        finally:
            fixture.hold.set()
            fixture.release_gate.set()
            stop()
            fixture.server.shutdown()
            log.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", required=True)
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--serve", action="store_true")
    parser.add_argument("--public-port", type=int)
    parser.add_argument("--control-port", type=int)
    acceptance(parser.parse_args())
