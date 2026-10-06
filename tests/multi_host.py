#!/usr/bin/env python3
"""Real HTTP/MCP integration; no external credentials or services needed."""
import concurrent.futures
import contextlib
import json
import http.server
import threading
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

TOKEN = "integration-token-0123456789"

def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]

class MCP:
    def __init__(self, p):
        self.url = f"http://127.0.0.1:{p}/local"
        self.session = None
        self.sequence = 0
        self.request("initialize", {"protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"integration","version":"1"}})
        self.request("notifications/initialized", {}, notification=True)

    def request(self, method, params, notification=False):
        self.sequence += 1
        payload = {"jsonrpc":"2.0","method":method,"params":params}
        if not notification:
            payload["id"] = self.sequence
        headers = {"Authorization":f"Bearer {TOKEN}", "Content-Type":"application/json",
            "Accept":"application/json, text/event-stream", "MCP-Protocol-Version":"2025-06-18"}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        req = urllib.request.Request(self.url, json.dumps(payload).encode(), headers)
        with urllib.request.urlopen(req, timeout=20) as response:
            self.session = response.headers.get("Mcp-Session-Id", self.session)
            if response.status == 202 or notification:
                return None
            if "text/event-stream" in response.headers.get("Content-Type",""):
                for line in response:
                    if line.startswith(b"data:"):
                        try:
                            message = json.loads(line[5:])
                        except json.JSONDecodeError:
                            continue
                        if message.get("id") == payload["id"]:
                            return message
                raise AssertionError("SSE ended without matching result")
            return json.load(response)

    def call(self, name, **args):
        return self.request("tools/call", {"name":name,"arguments":args})

def body(message):
    assert "error" not in message, message
    result = message["result"]
    assert not result.get("isError"), message
    return result["content"][0]["text"]

def fails(message):
    assert "error" in message or message.get("result", {}).get("isError"), message

@contextlib.contextmanager
def process(binary, work, name, mode="client", connections=None, allow_exec=True):
    root = work / name
    root.mkdir()
    log = open(work / f"{name}.log", "w+")
    p = port()
    env = dict(os.environ)
    # Explicit settings prevent developer environment leaking into the test.
    for key in list(env):
        if key.startswith("LOCAL_MCP_"):
            del env[key]
    env.update(LOCAL_MCP_MODE=mode, LOCAL_MCP_HOSTNAME=name, LOCAL_MCP_ROOT=str(root),
        LOCAL_MCP_BIND=f"127.0.0.1:{p}", LOCAL_MCP_TOKEN=TOKEN,
        LOCAL_MCP_STATE_DB=str(work/f"{name}.db"), LOCAL_MCP_ALLOWED_HOSTS="127.0.0.1,localhost",
        LOCAL_MCP_ALLOW_EXEC=str(allow_exec).lower(), LOCAL_MCP_LOG="warn",
        LOCAL_MCP_COMMAND_TIMEOUT="1")
    if connections:
        env["LOCAL_MCP_CONNECTIONS_FILE"] = str(connections)
    child = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=log)
    try:
        for _ in range(100):
            if child.poll() is not None:
                log.seek(0)
                raise AssertionError(log.read())
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{p}/healthz", timeout=.1).close()
                break
            except (OSError, urllib.error.URLError):
                time.sleep(.05)
        else:
            raise AssertionError("server failed readiness")
        yield p, root, child
    finally:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
        log.close()

@contextlib.contextmanager
def broken_child(kind):
    observed = {"writes": 0, "initialize": 0, "redirect_hits": 0}
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *_):
            pass
        def answer(self, status, data=None, extra=None):
            payload = json.dumps(data).encode() if data is not None else b""
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            for key, value in (extra or {}).items():
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(payload)
        def do_GET(self):
            self.answer(405)
        def do_DELETE(self):
            self.answer(200)
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/redirected":
                observed["redirect_hits"] += 1
                self.answer(500)
                return
            method = request["method"]
            if method == "initialize":
                observed["initialize"] += 1
                self.answer(200, {"jsonrpc":"2.0", "id":request["id"], "result":{
                    "protocolVersion":"2025-06-18", "capabilities":{"tools":{}},
                    "serverInfo":{"name":"failure-fixture", "version":"1"}}},
                    {"Mcp-Session-Id":"fixture-session"})
            elif method.startswith("notifications/"):
                self.answer(202)
            elif method == "tools/call" and request["params"]["name"] == "connections":
                self.answer(200, {"jsonrpc":"2.0", "id":request["id"], "result":{
                    "content":[{"type":"text","text":json.dumps({"mode":"client",
                        "connections":[{"hostname":"broken","root":"/fixture","allow_exec":True}]})}]}})
            elif method == "tools/call":
                observed["writes"] += 1
                if kind == "drop-after-write":
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    self.close_connection = True
                elif kind == "expired":
                    self.answer(404)
                else:
                    self.answer(307, extra={"Location":f"http://127.0.0.1:{server.server_port}/redirected"})
            else:
                self.answer(400)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        yield server.server_port, observed
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)

def failure_regressions(binary, work):
    for kind in ["drop-after-write", "expired", "redirect"]:
        with broken_child(kind) as (p, observed):
            registry = work / (kind + ".json")
            registry.write_text(json.dumps({"connections":[{
                "hostname":"broken", "url":f"http://127.0.0.1:{p}/local",
                "token_file":"token"}]}))
            with process(binary, work, kind, "server", registry) as (h, root, _):
                client = MCP(h)
                answer = client.call("write_file", connection="broken", path="never-local.txt", content="x")
                fails(answer)
                assert observed["writes"] == 1, (kind, observed)
                assert observed["initialize"] == 1, (kind, observed)
                assert observed["redirect_hits"] == 0, (kind, observed)
                assert not (root/"never-local.txt").exists()
                assert TOKEN not in json.dumps(answer)
    print("PASS: response lost after write, session-expired 404 and redirect do not retry or forward credentials")

def main():
    binary = Path(sys.argv[1] if len(sys.argv)>1 else "target/debug/local-mcp").resolve()
    with tempfile.TemporaryDirectory(prefix="localmcp-multihost-") as temp:
        work = Path(temp)
        (work/"token").write_text(TOKEN)
        with process(binary, work, "mac") as (a, ar, _), process(binary, work, "gpu") as (b, br, bp):
            (ar/"same.txt").write_text("MAC original")
            (br/"same.txt").write_text("GPU original")
            registry = work/"connections.json"
            registry.write_text(json.dumps({"connections":[
                {"hostname":"mac","aliases":["100.64.0.1"],"url":f"http://127.0.0.1:{a}/local","token_file":"token"},
                {"hostname":"gpu","aliases":["100.64.0.2"],"url":f"http://127.0.0.1:{b}/local","token_file":"token"}]}))
            with process(binary, work, "hub", "server", registry) as (h, hr, _):
                client = MCP(h)
                tools = client.request("tools/list", {})["result"]["tools"]
                assert len(tools) == 10, [t["name"] for t in tools]
                for tool in tools:
                    if tool["name"] != "connections":
                        assert "connection" in tool["inputSchema"]["required"]
                info = json.loads(body(client.call("connections")))
                assert [x["hostname"] for x in info["connections"]] == ["gpu","mac"]
                assert all(x["status"]=="connected" for x in info["connections"]), info
                assert str(ar) in json.dumps(info) and str(br) in json.dumps(info)
                assert TOKEN not in json.dumps(info) and "token_file" not in json.dumps(info)
                assert "MAC original" in body(client.call("read_file", connection="mac", path="same.txt"))
                assert "GPU original" in body(client.call("read_file", connection="100.64.0.2", path="same.txt"))
                body(client.call("edit_file", connection="gpu", path="same.txt", old_text="original", new_text="edited"))
                assert (br/"same.txt").read_text()=="GPU edited"
                assert (ar/"same.txt").read_text()=="MAC original"
                fails(client.call("write_file", path="oops.txt", content="wrong"))
                fails(client.call("write_file", connection="missing", path="oops.txt", content="wrong"))
                fails(client.call("write_file", connection=None, path="oops.txt", content="wrong"))
                assert not any((r/"oops.txt").exists() for r in [ar,br,hr])
                body(client.call("write_file", connection="mac", path="only-mac.txt", content="created"))
                assert (ar/"only-mac.txt").read_text()=="created"
                assert not (br/"only-mac.txt").exists() and not (hr/"only-mac.txt").exists()
                # Route other filesystem tools and independent concurrent calls.
                assert "only-mac.txt" in body(client.call("list_dir", connection="mac"))
                assert "edited" in body(client.call("search", connection="gpu", pattern="edited"))
                with concurrent.futures.ThreadPoolExecutor() as pool:
                    results = list(pool.map(lambda target: body(MCP(h).call("read_file", connection=target, path="same.txt")), ["mac","gpu"]*3))
                assert sum("MAC original" in x for x in results)==3
                assert sum("GPU edited" in x for x in results)==3
                started = body(client.call("start_command", connection="gpu", command="sleep 0.2; echo job-gpu"))
                job = re.search(r"[0-9a-f-]{36}", started).group()
                fails(client.call("poll_job", connection="mac", job_id=job))
                time.sleep(.3)
                assert "job-gpu" in body(client.call("poll_job", connection="100.64.0.2", job_id=job))
                assert "shell-mac" in body(client.call("execute", connection="mac", command="echo shell-mac"))
                long = body(client.call("start_command", connection="mac", command="sleep 30"))
                long_id = re.search(r"[0-9a-f-]{36}", long).group()
                body(client.call("stop_job", connection="mac", job_id=long_id))
                # Child itself must not accept a selector for a different host.
                fails(MCP(a).call("write_file", connection="gpu", path="oops.txt", content="wrong"))
                # Loss of one child does not hide the healthy child or cause fallback.
                bp.terminate()
                bp.wait(timeout=5)
                fails(client.call("write_file", connection="gpu", path="dead.txt", content="wrong"))
                assert not (ar/"dead.txt").exists() and not (hr/"dead.txt").exists()
                info = json.loads(body(client.call("connections")))
                by_host = {x["hostname"]:x for x in info["connections"]}
                assert by_host["gpu"]["status"]=="unavailable", info
                assert by_host["mac"]["status"]=="connected", info
            with process(binary, work, "readonly-hub", "server", registry, False) as (h, _, _):
                client = MCP(h)
                names = [t["name"] for t in client.request("tools/list", {})["result"]["tools"]]
                assert "execute" not in names and "start_command" not in names
                fails(client.call("execute", connection="mac", command="touch disabled.txt"))
                assert not (ar/"disabled.txt").exists()
            # A misconfigured URL must not silently target a different host.
            wrong_registry = work/"wrong-host.json"
            wrong_registry.write_text(json.dumps({"connections":[{
                "hostname":"impostor", "url":f"http://127.0.0.1:{a}/local", "token_file":"token"}]}))
            with process(binary, work, "identity-hub", "server", wrong_registry) as (h, _, _):
                fails(MCP(h).call("write_file", connection="impostor", path="wrong-identity.txt", content="wrong"))
                assert not (ar/"wrong-identity.txt").exists()
            # Default standalone remains compatible without connection arguments.
            with process(binary, work, "standalone", "standalone") as (s, sr, _):
                standalone = MCP(s)
                body(standalone.call("write_file", path="old.txt", content="backward compatible"))
                assert "backward compatible" in body(standalone.call("read_file", path="old.txt"))
        failure_regressions(binary, work)
    print("PASS: two-child discovery, aliases, read/write/edit/search/list, concurrency, jobs, selector errors, no fallback, exec policy and standalone compatibility")

if __name__=="__main__":
    main()
