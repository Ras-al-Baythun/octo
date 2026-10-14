#!/usr/bin/env python3
"""Black-box HTTP parity harness for the C# -> Rust rewrite of Octo.

Commands (stdlib only, Python 3.10+):

  parity.py up      [--image IMG] [--port P] [--nd-port P] [--project NAME] [--settle S]
      Recreate the compose project from scratch (volumes included), wait for Navidrome to
      finish scanning the fixture library, add the extra Navidrome users, and wait for Octo.
  parity.py down    [--project NAME]
      Remove the project's containers, networks and volumes (never images).
  parity.py record  --target http://127.0.0.1:18480 --out recordings/<name>/ [--only GLOB[,GLOB...]]
                    [--shard NAME] [--waits-from REF]
      Replay the corpus (corpus/*.json, in file order) against a running target and write
      one JSON file per corpus file. --shard records one shard of shards.json; --waits-from
      shortens the waits the reference recording (C#) met to a multiple of its times.
  parity.py diff    A B [--mode structural|bytes] [--allowlist FILE] [--report FILE] [--only GLOB]
      Compare two recordings after normalisation. Exit 1 on any diff not covered by the
      allowlist or the intentional diffs, 0 otherwise, 3 when --only picks no request. Ends
      with the diffs per backlog group. --only picks as record's does (a file name, a glob over
      the file names or over <file>/<request>, several comma-separated).
  parity.py run     --out recordings/<name>/ [up options] [record options] [--keep]
      up + record + down in one go.
  parity.py merge   RUN-DIR SHARD-DIR...
      Join shard recordings into one recording in corpus order, for diff.
  parity.py shards  check|show|digest [--times RECORDING]
      Check the shard plan (shards.json), show it with each shard's length, or print its hash.

See parity/README.md for the corpus format, the normalisation rules and what is covered.
"""
from __future__ import annotations

import argparse
import base64
import fnmatch
import glob
import hashlib
import http.client
import json
import os
import re
import subprocess
import sys
import time
import urllib.parse
import xml.etree.ElementTree as ET
from typing import Any

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
COMPOSE_FILE = os.path.join(HERE, "docker-compose.yml")
CORPUS_DIR = os.path.join(HERE, "corpus")
DEFAULT_ALLOWLIST = os.path.join(HERE, "allowlist.json")
NORMALIZE_FILE = os.path.join(HERE, "normalize.json")
KNOWN_DIFFS = os.path.join(REPO, "docs", "rust-migration", "known-diffs.md")

# The C# release the Rust build is held to: its published image, pulled, never built here.
DEFAULT_IMAGE = "ghcr.io/winters27/octo:2026.10.05"
DEFAULT_PORT = 18480
DEFAULT_ND_PORT = 18453
INTENTIONAL = os.path.join(HERE, "intentional-diffs.json")
GROUPS_FILE = os.path.join(HERE, "groups.json")
SHARDS_FILE = os.path.join(HERE, "shards.json")
# Each run works on a fresh, writable copy of the fixture library (library edits, the trash,
# explicit marks and downloads write to it), kept under parity/.run/<project>/.
RUN_DIR = os.path.join(HERE, ".run")
# The second Octo ("alt": sign-in off, the dashboard on a port of its own) listens on these
# host ports by default: the main port plus one and plus two.
ALT_OFFSET, ALT_ADMIN_OFFSET = 1, 2
# A fixed multipart boundary, so an upload's bytes are the same in every run.
MULTIPART_BOUNDARY = "octo-parity-boundary-7c1e2f"

# Fixture credentials (docker-compose.yml and `up` create these users).
ADMIN_USER, ADMIN_PASS = "admin", "parity-admin"
LISTENER_USER, LISTENER_PASS = "listener", "listener-pass"
TOKEN_SALT = "c0ffee42"
API_KEY = "parity-not-a-real-key"
EXPECTED_TRACKS = 10
# The fixture library's mtime (make-library.sh's stamp as the baseline recorded it). Git does not
# keep mtimes, and Navidrome relays them as Last-Modified, so `up` puts them back on a fresh checkout.
FIXTURE_MUSIC = os.path.join(HERE, "fixtures", "music")
FIXTURE_MTIME = 1577909045  # 2020-01-01T20:04:05Z

# Bodies longer than this are stored as a hash only.
MAX_STORED_TEXT = 64 * 1024

BUILTIN_VARS = {
    "admin_user": ADMIN_USER,
    "admin_pass": ADMIN_PASS,
    "listener_user": LISTENER_USER,
    "listener_pass": LISTENER_PASS,
    "salt": TOKEN_SALT,
    "token": hashlib.md5((ADMIN_PASS + TOKEN_SALT).encode()).hexdigest(),
    "enc_pass": "enc:" + ADMIN_PASS.encode().hex(),
    "api_key": API_KEY,
}


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------------------
# Compose orchestration
# --------------------------------------------------------------------------------------

def compose(project: str, *args: str, env: dict | None = None, check: bool = True) -> None:
    cmd = ["docker", "compose", "-f", COMPOSE_FILE, "-p", project, *args]
    full_env = dict(os.environ)
    full_env.update(env or {})
    log("$ " + " ".join(cmd))
    subprocess.run(cmd, env=full_env, check=check)


def http_get_json(url: str, headers: dict | None = None, timeout: float = 5) -> Any:
    parts = urllib.parse.urlsplit(url)
    conn = http.client.HTTPConnection(parts.hostname, parts.port, timeout=timeout)
    path = parts.path + (("?" + parts.query) if parts.query else "")
    conn.request("GET", path, headers=headers or {})
    resp = conn.getresponse()
    data = resp.read()
    conn.close()
    return resp.status, json.loads(data) if data else None


def http_post_json(url: str, body: Any, headers: dict | None = None, timeout: float = 10) -> tuple[int, Any]:
    parts = urllib.parse.urlsplit(url)
    conn = http.client.HTTPConnection(parts.hostname, parts.port, timeout=timeout)
    h = {"Content-Type": "application/json"}
    h.update(headers or {})
    conn.request("POST", parts.path, body=json.dumps(body).encode(), headers=h)
    resp = conn.getresponse()
    data = resp.read()
    conn.close()
    try:
        return resp.status, json.loads(data) if data else None
    except ValueError:
        return resp.status, data.decode("utf-8", "replace")


def wait_for(desc: str, probe, timeout: float) -> Any:
    deadline = time.time() + timeout
    last_err: Any = None
    while time.time() < deadline:
        try:
            result = probe()
            if result:
                log(f"  ready: {desc}")
                return result
        except Exception as exc:  # noqa: BLE001 - any failure means "not yet"
            last_err = exc
        time.sleep(1)
    raise SystemExit(f"timed out waiting for {desc} (last error: {last_err})")


def subsonic_qs(user: str = ADMIN_USER, password: str = ADMIN_PASS) -> str:
    return urllib.parse.urlencode({"u": user, "p": password, "v": "1.16.1", "c": "parity-setup", "f": "json"})


def stack_env(args) -> dict:
    """The compose variables of one stack. The alt ports follow the main port unless given."""
    return {
        "OCTO_IMAGE": args.image,
        "PARITY_PORT": str(args.port),
        "PARITY_ND_PORT": str(args.nd_port),
        "PARITY_ALT_PORT": str(args.port + ALT_OFFSET),
        "PARITY_ALT_ADMIN_PORT": str(args.port + ALT_ADMIN_OFFSET),
        "PARITY_MUSIC": run_music_dir(args.project),
    }


def run_music_dir(project: str) -> str:
    return os.path.join(RUN_DIR, project, "music")


def fresh_music(project: str) -> None:
    """A writable copy of the fixture library for this run, every file and folder at the
    baseline's instant (git does not keep mtimes, and Navidrome relays them)."""
    import shutil
    dest = run_music_dir(project)
    if os.path.isdir(dest):
        shutil.rmtree(dest)
    shutil.copytree(FIXTURE_MUSIC, dest)
    for root, dirs, files in os.walk(dest, topdown=False):
        for name in files + dirs:
            os.utime(os.path.join(root, name), (FIXTURE_MTIME, FIXTURE_MTIME))
    os.utime(dest, (FIXTURE_MTIME, FIXTURE_MTIME))
    # The containers run as root and as other users (Navidrome); let every one of them write.
    os.chmod(dest, 0o777)


def cmd_up(args) -> None:
    env = stack_env(args)
    compose(args.project, "down", "-v", "--remove-orphans", env=env, check=False)
    fresh_music(args.project)
    compose(args.project, "up", "-d", env=env)
    nd = f"http://127.0.0.1:{args.nd_port}"

    def nd_ping():
        status, doc = http_get_json(f"{nd}/rest/ping?{subsonic_qs()}")
        return status == 200 and doc["subsonic-response"]["status"] == "ok"

    wait_for("navidrome ping", nd_ping, 180)

    def nd_scanned():
        _, doc = http_get_json(f"{nd}/rest/getScanStatus?{subsonic_qs()}")
        scan = doc["subsonic-response"]["scanStatus"]
        return (not scan["scanning"]) and scan["count"] >= EXPECTED_TRACKS

    wait_for(f"navidrome scan ({EXPECTED_TRACKS} tracks)", nd_scanned, 180)

    status, login = http_post_json(f"{nd}/auth/login", {"username": ADMIN_USER, "password": ADMIN_PASS})
    if status != 200:
        raise SystemExit(f"navidrome admin login failed: {status} {login}")
    status, made = http_post_json(
        f"{nd}/api/user",
        {"userName": LISTENER_USER, "name": "Listener", "email": "", "password": LISTENER_PASS, "isAdmin": False},
        headers={"X-ND-Authorization": f"Bearer {login['token']}"},
    )
    if status not in (200, 201):
        raise SystemExit(f"creating the listener user failed: {status} {made}")
    log(f"  navidrome user '{LISTENER_USER}' created")

    for label, port in (("octo", args.port), ("octo-alt", args.port + ALT_OFFSET)):
        octo = f"http://127.0.0.1:{port}"

        def octo_ping(octo=octo):
            status, doc = http_get_json(f"{octo}/rest/ping?{subsonic_qs()}")
            return status == 200 and doc["subsonic-response"]["status"] == "ok"

        wait_for(f"{label} ping (relayed to navidrome)", octo_ping, 180)
    if args.settle > 0:
        log(f"  settling {args.settle}s for startup work (library detection, service checks)")
        time.sleep(args.settle)
    log(f"up: target http://127.0.0.1:{args.port} (image {args.image}, project {args.project})")


def cmd_down(args) -> None:
    import shutil
    compose(args.project, "down", "-v", "--remove-orphans", check=False)
    shutil.rmtree(os.path.join(RUN_DIR, args.project), ignore_errors=True)


# --------------------------------------------------------------------------------------
# Corpus
# --------------------------------------------------------------------------------------

def only_globs(only: str | None) -> list[str]:
    """--only is one glob or several, comma-separated (a shard's file list)."""
    return [g.strip() for g in (only or "").split(",") if g.strip()]


def only_match(rid: str, globs: list[str]) -> bool:
    """Whether "<file>/<name>" is picked by any of the globs; a glob may also name the file alone."""
    stem = rid.split("/", 1)[0]
    return any(fnmatch.fnmatch(rid, g) or fnmatch.fnmatch(stem, g) for g in globs)


def refuse_empty_only(only: str | None) -> None:
    """Exit 3 when --only picks no request (00-setup, which always runs, aside)."""
    globs = only_globs(only)
    if globs and not any(only_match(f"{stem}/{r['name']}", globs)
                         for stem, doc in load_corpus(only) for r in doc["requests"]):
        log(f"--only {only!r} picks no request (only 00-setup would run)")
        sys.exit(3)


def load_corpus(only: str | None) -> list[tuple[str, dict]]:
    files = sorted(glob.glob(os.path.join(CORPUS_DIR, "*.json")))
    globs = only_globs(only)
    out = []
    for path in files:
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
        stem = os.path.splitext(os.path.basename(path))[0]
        names = set()
        for req in doc["requests"]:
            if req["name"] in names:
                raise SystemExit(f"{path}: duplicate request name {req['name']}")
            names.add(req["name"])
        if globs and stem != "00-setup":
            doc = dict(doc)
            doc["requests"] = [r for r in doc["requests"] if only_match(f"{stem}/{r['name']}", globs)]
            if not doc["requests"]:
                continue
        out.append((stem, doc))
    return out


VAR_RE = re.compile(r"\{\{(\w+)\}\}")


def subst(value: Any, vars_: dict) -> Any:
    if isinstance(value, str):
        def repl(m):
            key = m.group(1)
            if key not in vars_:
                raise KeyError(f"unknown variable {{{{{key}}}}}")
            return str(vars_[key])
        return VAR_RE.sub(repl, value)
    if isinstance(value, list):
        return [subst(v, vars_) for v in value]
    if isinstance(value, dict):
        return {k: subst(v, vars_) for k, v in value.items()}
    return value


def auth_params(kind: str | None, client: str) -> list[tuple[str, str]]:
    base = [("v", "1.16.1"), ("c", client)]
    if kind in (None, "none"):
        return []
    if kind == "p":
        return [("u", ADMIN_USER), ("p", ADMIN_PASS)] + base
    if kind == "enc":
        return [("u", ADMIN_USER), ("p", BUILTIN_VARS["enc_pass"])] + base
    if kind == "t":
        return [("u", ADMIN_USER), ("t", BUILTIN_VARS["token"]), ("s", TOKEN_SALT)] + base
    if kind == "bad":
        return [("u", ADMIN_USER), ("p", "wrong-password")] + base
    if kind == "listener":
        return [("u", LISTENER_USER), ("p", LISTENER_PASS)] + base
    if kind == "apikey":
        return [("apiKey", API_KEY)] + base
    if kind == "u-only":
        return [("u", ADMIN_USER)] + base
    raise SystemExit(f"unknown auth kind {kind!r}")


# Dashboard sign-ins the corpus can use ("session": "<name>"), by the variable holding the
# session token. 00-setup captures them.
ADMIN_COOKIE = "octo_browse"
SESSIONS = {"admin": "admin_session", "recovery": "recovery_session"}


def multipart_body(parts: list[dict], vars_: dict) -> bytes:
    """multipart/form-data with a fixed boundary. A part is {"name", "value"} or
    {"name", "filename", "contentType", "file"} (a path under parity/) or the same with "text"
    (inline file content)."""
    out = bytearray()
    for part in parts:
        out += f"--{MULTIPART_BOUNDARY}\r\n".encode()
        name = subst(part["name"], vars_)
        if "filename" in part:
            out += (f'Content-Disposition: form-data; name="{name}"; '
                    f'filename="{subst(part["filename"], vars_)}"\r\n').encode()
            out += f"Content-Type: {part.get('contentType', 'application/octet-stream')}\r\n\r\n".encode()
            if "file" in part:
                with open(os.path.join(HERE, part["file"]), "rb") as fh:
                    out += fh.read()
            else:
                out += subst(part.get("text", ""), vars_).encode()
        else:
            out += f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode()
            out += subst(str(part.get("value", "")), vars_).encode()
        out += b"\r\n"
    out += f"--{MULTIPART_BOUNDARY}--\r\n".encode()
    return bytes(out)


def build_request(req: dict, defaults: dict, vars_: dict) -> tuple[str, str, dict, bytes | None]:
    merged = dict(defaults)
    merged.update(req)
    method = merged.get("method", "GET").upper()
    path = subst(merged["path"], vars_)
    if "rawQuery" in merged:
        query = subst(merged["rawQuery"], vars_)
    else:
        pairs = auth_params(merged.get("auth"), merged.get("client", "parity"))
        if merged.get("f") is not None:  # "f": null drops a default format
            pairs.append(("f", merged["f"]))
        q = merged.get("query") or []
        if isinstance(q, dict):
            q = list(q.items())
        pairs += [(k, subst(str(v), vars_)) for k, v in q]
        query = urllib.parse.urlencode(pairs, quote_via=urllib.parse.quote)
    target = path + (("?" + query) if query else "")
    headers = {k: subst(v, vars_) for k, v in (merged.get("headers") or {}).items()}
    # "session": sign the request in to the dashboard with a session made earlier in the corpus
    # (00-setup), unless it carries its own cookie or browse token. Like a browser, only on the
    # cookie's path (/api/admin).
    session = merged.get("session")
    if (session and path.startswith("/api/admin")
            and not any(k.lower() in ("cookie", "x-octo-browse-token") for k in headers)):
        if session not in SESSIONS:
            raise SystemExit(f"unknown session {session!r} in {req['name']}")
        headers["Cookie"] = f"{ADMIN_COOKIE}={subst('{{' + SESSIONS[session] + '}}', vars_)}"
    body = None
    spec = merged.get("body")
    if spec is not None:
        if "multipart" in spec:
            body = multipart_body(spec["multipart"], vars_)
            headers.setdefault("Content-Type", f"multipart/form-data; boundary={MULTIPART_BOUNDARY}")
        elif "json" in spec:
            body = json.dumps(subst(spec["json"], vars_), ensure_ascii=False).encode()
            headers.setdefault("Content-Type", "application/json")
        elif "form" in spec:
            body = urllib.parse.urlencode([(k, subst(str(v), vars_)) for k, v in spec["form"]]).encode()
            headers.setdefault("Content-Type", "application/x-www-form-urlencoded")
        elif "raw" in spec:
            body = subst(spec["raw"], vars_).encode()
            if "contentType" in spec:
                headers.setdefault("Content-Type", spec["contentType"])
        else:
            raise SystemExit(f"bad body spec in {req['name']}")
    return method, target, headers, body


# --------------------------------------------------------------------------------------
# Recording
# --------------------------------------------------------------------------------------

def body_kind(headers: list[tuple[str, str]], data: bytes) -> str:
    if not data:
        return "empty"
    get = {k.lower(): v for k, v in headers}
    if "content-encoding" in get:
        return "binary"
    ctype = get.get("content-type", "").lower()
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        return "binary"
    if "json" in ctype:
        return "json"
    if "xml" in ctype and "svg" not in ctype:
        return "xml"
    if ctype.startswith("text/") or "javascript" in ctype or "svg" in ctype or not ctype:
        return "text"
    return "binary"


def encode_body(headers: list[tuple[str, str]], data: bytes) -> dict:
    kind = body_kind(headers, data)
    out: dict[str, Any] = {"kind": kind, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    if kind in ("json", "xml", "text"):
        if len(data) <= MAX_STORED_TEXT:
            out["text"] = data.decode("utf-8")
        else:
            out["stored"] = "hash-only"
    elif kind == "binary":
        out["head"] = base64.b64encode(data[:24]).decode()
    return out


def json_get(doc: Any, path: str) -> Any:
    cur = doc
    for part in re.findall(r"[^.\[\]]+|\[\d+\]", path):
        if part.startswith("["):
            cur = cur[int(part[1:-1])]
        else:
            cur = cur[part]
    return cur


def do_capture(spec: str, headers: list[tuple[str, str]], data: bytes) -> str | None:
    kind, _, expr = spec.partition(":")
    try:
        if kind == "json":
            value = json_get(json.loads(data), expr)
            return None if value is None else str(value)
        if kind == "header":
            for k, v in headers:
                if k.lower() == expr.lower():
                    return v
            return None
        if kind == "cookie":
            for k, v in headers:
                if k.lower() == "set-cookie":
                    m = re.match(rf"\s*{re.escape(expr)}=([^;]*)", v)
                    if m:
                        return m.group(1)
            return None
        if kind == "regex":
            m = re.search(expr, data.decode("utf-8", "replace"))
            return m.group(1) if m else None
    except (KeyError, IndexError, TypeError, ValueError):
        return None
    raise SystemExit(f"bad capture spec {spec!r}")


def continue_or_answer(sock, wait: float) -> str:
    """After headers with Expect: 100-continue: "continue" once the interim 100 is read off the
    socket, "answer" when the server sent its final answer without asking for the body, "silent"
    when it said nothing in time (the body is then sent anyway, as RFC 9110 allows)."""
    import select

    if not select.select([sock], [], [], wait)[0]:
        return "silent"
    head = sock.recv(12, socket_peek_flag())
    if not head.startswith(b"HTTP/1.1 100") and not head.startswith(b"HTTP/1.0 100"):
        return "answer"
    seen = b""
    while not seen.endswith(b"\r\n\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            break
        seen += chunk
    return "continue"


def socket_peek_flag() -> int:
    import socket

    return socket.MSG_PEEK


def send(base: urllib.parse.SplitResult, method: str, target: str, headers: dict, body: bytes | None,
         timeout: float) -> tuple[int, str, list[tuple[str, str]], bytes, float]:
    conn = http.client.HTTPConnection(base.hostname, base.port or 80, timeout=timeout)
    start = time.time()
    try:
        conn.putrequest(method, target, skip_accept_encoding=True)
        sent = {k.lower() for k in headers}
        for k, v in headers.items():
            conn.putheader(k, v)
        if body is not None and "content-length" not in sent:
            conn.putheader("Content-Length", str(len(body)))
        if body is not None and len(body) > MAX_STORED_TEXT:
            # A big upload asks first (Expect: 100-continue). A server that answers without
            # reading the body (a route that does not exist) would otherwise close the socket
            # under the client's feet, and whether the answer or a reset arrives would be luck.
            conn.putheader("Expect", "100-continue")
            conn.endheaders()
            if continue_or_answer(conn.sock, 2.0) != "answer":
                conn.sock.sendall(body)
        else:
            conn.endheaders(body)
        resp = conn.getresponse()
        data = resp.read()
        return resp.status, resp.reason, list(resp.getheaders()), data, time.time() - start
    finally:
        conn.close()


def run_exec(project: str, service: str, cmd: list[str], timeout: float) -> tuple[int, bytes]:
    """Run a command in a container of the stack under test: exit code and stdout."""
    full = ["docker", "compose", "-f", COMPOSE_FILE, "-p", project, "exec", "-T", service, *cmd]
    done = subprocess.run(full, capture_output=True, timeout=timeout, check=False)
    return done.returncode, done.stdout


def until_met(spec: dict, status: int, headers: list[tuple[str, str]], data: bytes) -> bool:
    """The condition of an "until": the answer's status is one of "status" (default: any), and
    the value at "from" is one of "in" (or none of "notIn"), and the body matches "regex"."""
    if "status" in spec and status not in spec["status"]:
        return False
    if "regex" in spec and not re.search(spec["regex"], data.decode("utf-8", "replace")):
        return False
    if "from" in spec:
        value = do_capture(spec["from"], headers, data)
        if "in" in spec and value not in [str(v) for v in spec["in"]]:
            return False
        if "notIn" in spec and value in [str(v) for v in spec["notIn"]]:
            return False
    return True


def server_targets(args, base: urllib.parse.SplitResult) -> dict[str, urllib.parse.SplitResult]:
    """Where "server": main, alt and alt-admin send their requests."""
    alt = getattr(args, "alt_port", None) or (base.port + ALT_OFFSET)
    alt_admin = getattr(args, "alt_admin_port", None) or (base.port + ALT_ADMIN_OFFSET)
    return {
        "main": base,
        "alt": urllib.parse.urlsplit(f"http://{base.hostname}:{alt}"),
        "alt-admin": urllib.parse.urlsplit(f"http://{base.hostname}:{alt_admin}"),
    }


# The compose service behind each "server", for "exec" requests.
EXEC_SERVICES = {"main": "octo", "alt": "octo-alt", "alt-admin": "octo-alt"}


def send_once(args, servers, server, method, cmd, target, headers, body, timeout):
    """One attempt: (status, reason, headers, body bytes, seconds)."""
    if method == "EXEC":
        start = time.time()
        status, data = run_exec(args.project, EXEC_SERVICES[server], cmd, timeout)
        try:
            json.loads(data)
            rheaders = [("Content-Type", "application/json")]
        except ValueError:
            rheaders = [("Content-Type", "text/plain")]
        return status, "", rheaders, data, time.time() - start
    return send(servers[server], method, target, headers, body, timeout)


def octo_started(project: str) -> float | None:
    """When the stack's Octo container started (epoch seconds), or None without one."""
    try:
        cid = subprocess.run(["docker", "compose", "-f", COMPOSE_FILE, "-p", project, "ps", "-q", "octo"],
                             capture_output=True, text=True, timeout=30, check=False).stdout.strip()
        if not cid:
            return None
        when = subprocess.run(["docker", "inspect", "-f", "{{.State.StartedAt}}", cid.splitlines()[0]],
                              capture_output=True, text=True, timeout=30, check=False).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        return None
    m = re.match(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(\.\d+)?Z$", when)
    if not m:
        return None
    import calendar
    return calendar.timegm(time.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S")) + float(m.group(2) or 0)


# Adaptive waits (record --waits-from REF): a wait whose condition the reference recording (the
# C# one) met gets max(WAIT_FLOOR_MS, WAIT_FACTOR x the time it took there) instead of the
# corpus's timeoutMs, never more than that, and a wait that took the reference WAIT_JOB_MS or
# more (background work, not an answer already there) at least WAIT_JOB_EXTRA_MS more than it
# took. README "Adaptive waits" has the numbers' reasons.
WAIT_FACTOR = 3
WAIT_FLOOR_MS = 20000
WAIT_JOB_MS = 1000
WAIT_JOB_EXTRA_MS = 60000


def load_wait_refs(path: str | None, shard: dict | None) -> dict[str, dict]:
    """The waits of a reference recording, by request id: {waitMs, atMs, timedOut}. Waits the
    reference has no times for (a recording made before waitMs) are left out. For a shard, the
    reference's recording of the same shard is read (a merged reference names it in "parts"):
    only that one ran the shard's setup files on the same clock. Without it, the instants (atMs)
    of files the shard does not own are dropped, since they come from another shard's clock."""
    if not path:
        return {}
    owned = None
    if shard:
        with open(os.path.join(path, "meta.json"), encoding="utf-8") as fh:
            meta = json.load(fh)
        same = [p for p in meta.get("parts") or [] if p.get("name") == shard["name"]]
        if same and os.path.exists(os.path.join(same[0]["dir"], "meta.json")):
            path = same[0]["dir"]
            meta = {"shard": {"name": shard["name"]}}
        if (meta.get("shard") or {}).get("name") != shard["name"]:
            owned = set(shard["files"])
            log(f"  --waits-from {path}: not shard {shard['name']}'s own recording; "
                f"the instants of files it does not own are not used")
    _, entries, _ = load_recording(path)
    out = {}
    for rid, e in entries.items():
        if "waitMs" in e:
            at = e.get("atMs") if owned is None or rid.split("/", 1)[0] in owned else None
            out[rid] = {"waitMs": e["waitMs"], "atMs": at, "timedOut": bool(e.get("untilTimedOut"))}
    if not out:
        raise SystemExit(f"--waits-from {path}: the recording has no wait times (made before waitMs)")
    return out


def adaptive_limit(ref: dict | None, corpus_ms: int, clock_ms: int) -> int:
    """The wait limit for one request under --waits-from. A wait the reference timed out on, or has
    no time for, keeps the corpus's limit. A met one gets the larger of the floor and WAIT_FACTOR
    times the reference's time; one that took the reference a second or more (a job, a download,
    a Navidrome scan) at least a minute more than that time, since a scan can wait for the one
    before it; and also enough to reach the instant the reference met it (a timer counted from
    Octo's start, such as Sounds alike's first look, fires at the same instant in both builds
    however fast each got there), plus the floor. Never more than the corpus's limit."""
    if ref is None or ref["timedOut"]:
        return corpus_ms
    limit = max(WAIT_FLOOR_MS, WAIT_FACTOR * ref["waitMs"])
    if ref["waitMs"] >= WAIT_JOB_MS:
        limit = max(limit, ref["waitMs"] + WAIT_JOB_EXTRA_MS)
    if ref.get("atMs") is not None:
        limit = max(limit, ref["atMs"] - clock_ms + WAIT_FLOOR_MS)
    return min(corpus_ms, limit)


def cmd_record(args) -> None:
    base = urllib.parse.urlsplit(args.target.rstrip("/"))
    if base.scheme != "http":
        raise SystemExit("--target must be an http:// URL")
    servers = server_targets(args, base)
    out_dir = args.out
    os.makedirs(out_dir, exist_ok=True)
    shard = apply_shard(args)
    vars_: dict[str, str] = dict(BUILTIN_VARS)
    vars_["host"] = base.netloc
    vars_["host_alt"] = servers["alt"].netloc
    vars_["host_alt_admin"] = servers["alt-admin"].netloc
    captured: dict[str, str] = {}
    kept: dict[str, str] = {}
    corpus = load_corpus(args.only)
    refuse_empty_only(args.only)
    refs = load_wait_refs(args.waits_from, shard)
    total = 0
    failures = 0
    # Wait times are kept in every recording (waitMs, and atMs on this clock), so a later
    # recording can adapt its waits to them. The clock starts when the stack's Octo started (its
    # own timers count from there), or with the recording when there is no stack to ask.
    record_start = time.time()
    inputs = inputs_digest()  # as the recording starts: the files may change while it runs
    clock0 = octo_started(args.project) or record_start
    file_seconds: dict[str, float] = {}
    adapted = 0
    for stem, doc in corpus:
        file_start = time.time()
        defaults = doc.get("defaults", {})
        entries = []
        # A build that lacks a file's feature would sit out every wait in it. After
        # --until-patience waits in one file time out, the rest of that file's waits get at most
        # 20 s (enough for the timers a build that has the feature would be waiting on).
        timeouts_here = 0
        for req in doc["requests"]:
            rid = f"{stem}/{req['name']}"
            merged = dict(defaults)
            merged.update(req)
            server = merged.get("server") or "main"
            if server not in servers:
                raise SystemExit(f"{rid}: unknown server {server!r}")
            if req.get("delayBeforeMs"):
                time.sleep(req["delayBeforeMs"] / 1000.0)
            timeout = req.get("timeout", args.timeout)
            cmd: list[str] = []
            try:
                if "exec" in req:
                    cmd = subst(req["exec"], vars_)
                    method, target, headers, body = "EXEC", " ".join(cmd), {}, None
                else:
                    method, target, headers, body = build_request(req, defaults, vars_)
            except KeyError as exc:
                log(f"  SKIP {rid}: {exc}")
                entries.append({"name": req["name"], "skipped": str(exc)})
                continue
            until = req.get("until")
            if until:
                try:
                    until = subst(until, vars_)
                except KeyError as exc:
                    log(f"  SKIP {rid}: {exc}")
                    entries.append({"name": req["name"], "skipped": str(exc)})
                    continue
            wait_ms = req.get("timeoutMs", 60000)
            wait_start = time.time()
            if until and refs:
                limit = adaptive_limit(refs.get(rid), wait_ms, int((wait_start - clock0) * 1000))
                if limit < wait_ms:
                    adapted += 1
                    wait_ms = limit
            if timeouts_here >= args.until_patience:
                wait_ms = min(wait_ms, 20000)
            deadline = wait_start + wait_ms / 1000.0
            timed_out = False
            try:
                while True:
                    status, reason, rheaders, data, elapsed = send_once(
                        args, servers, server, method, cmd, target, headers, body, timeout)
                    if not until or until_met(until, status, rheaders, data):
                        break
                    if time.time() >= deadline:
                        timed_out = True
                        timeouts_here += 1
                        log(f"  WAIT {rid}: condition not met in {wait_ms} ms")
                        break
                    time.sleep(req.get("pollMs", 500) / 1000.0)
            except (OSError, http.client.HTTPException, subprocess.TimeoutExpired) as exc:
                failures += 1
                log(f"  ERR  {rid}: {exc}")
                entries.append({"name": req["name"], "request": {"method": method, "target": target},
                                "error": f"{type(exc).__name__}: {exc}"})
                continue
            wait_end = time.time()
            total += 1
            for var, spec in (req.get("capture") or {}).items():
                # A capture is either "kind:expr" (a volatile value: the diff normalises it back
                # to {{var}}) or {"from": "kind:expr", "normalize": false} for a value that is
                # deterministic and must itself match between builds (it is only reused).
                normalise = True
                if isinstance(spec, dict):
                    normalise = spec.get("normalize", True)
                    spec = spec["from"]
                value = do_capture(spec, rheaders, data)
                if value is None:
                    log(f"  WARN {rid}: capture {var} ({spec}) found nothing")
                    continue
                vars_[var] = value
                if normalise:
                    captured[var] = value
                else:
                    kept[var] = value
            entry = {
                "name": req["name"],
                "request": {
                    "method": method,
                    "target": target,
                    "headers": headers,
                    "body": body.decode("utf-8", "replace") if body else None,
                },
                "status": status,
                "reason": reason,
                "headers": [[k, v] for k, v in rheaders],
                "body": encode_body(rheaders, data),
            }
            if server != "main":
                entry["server"] = server
            if timed_out:
                entry["untilTimedOut"] = True
            if until:
                # How long the wait took, its limit and when it ended (on the clock above). The
                # diff never compares these; record --waits-from reads them.
                entry["waitMs"] = round((wait_end - wait_start) * 1000)
                entry["waitLimitMs"] = wait_ms
                entry["atMs"] = round((wait_end - clock0) * 1000)
            entries.append(entry)
            if args.verbose:
                log(f"  {status} {method} {target[:110]} ({elapsed * 1000:.0f} ms)")
        with open(os.path.join(out_dir, f"{stem}.json"), "w", encoding="utf-8") as fh:
            json.dump({"group": doc.get("group", stem), "entries": entries}, fh, ensure_ascii=False, indent=1)
            fh.write("\n")
        file_seconds[stem] = round(time.time() - file_start, 1)
        log(f"recorded {stem}: {len(entries)} requests in {file_seconds[stem]:.0f} s")
    meta = {
        "target": args.target,
        "host": base.netloc,
        "hosts": {"host_alt": servers["alt"].netloc, "host_alt_admin": servers["alt-admin"].netloc},
        "image": args.image_label,
        "inputs": inputs,
        "recordedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "vars": captured,
        "keptVars": kept,
        "requests": total,
        "errors": failures,
        "seconds": round(time.time() - record_start, 1),
        "fileSeconds": file_seconds,
        "clock": "octo-start" if clock0 != record_start else "record-start",
    }
    if refs:
        meta["waitsFrom"] = {"recording": os.path.abspath(args.waits_from), "factor": WAIT_FACTOR,
                             "floorMs": WAIT_FLOOR_MS, "adapted": adapted}
    if shard:
        meta["shard"] = shard
    with open(os.path.join(out_dir, "meta.json"), "w", encoding="utf-8") as fh:
        json.dump(meta, fh, ensure_ascii=False, indent=1)
        fh.write("\n")
    log(f"recorded {total} responses into {out_dir} ({failures} transport errors)")
    if failures:
        sys.exit(2)


# --------------------------------------------------------------------------------------
# Shards: the corpus in pieces that run side by side, each on a fresh stack of its own
# --------------------------------------------------------------------------------------

def file_digest(path: str) -> str:
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read().replace(b"\r\n", b"\n")).hexdigest()


def load_shards(path: str | None = None) -> list[dict]:
    """shards.json: an ordered list of {"name", "files", "setup"?, "reads"?, "why"?}. "files" are
    the corpus files the shard owns (its recordings are the ones merge keeps), in corpus order;
    00-setup runs in every shard whoever owns it. "setup" are request globs from other files run
    for their captures only (the owner's recording of those is the one kept). "reads" holds, per
    corpus file, the earlier files whose leftover state its answers depend on, with the reason."""
    path = path or SHARDS_FILE
    with open(path, encoding="utf-8") as fh:
        plan = json.load(fh)
    if not isinstance(plan, list) or not plan:
        raise SystemExit(f"{path}: the plan is an ordered list of shards")
    names = set()
    for shard in plan:
        if not isinstance(shard, dict) or not shard.get("name") or not isinstance(shard.get("files"), list):
            raise SystemExit(f"{path}: every shard needs a name and a files list ({shard!r})")
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", shard["name"]):
            raise SystemExit(f"{path}: shard name {shard['name']!r} must be lower-case letters, digits and dashes")
        if shard["name"] in names:
            raise SystemExit(f"{path}: two shards named {shard['name']}")
        names.add(shard["name"])
    return plan


def apply_shard(args) -> dict | None:
    """--shard NAME: record that shard of the plan (its files and setup requests, as --only would).
    The recording's meta.json then says which shard it is, which is what merge goes by."""
    if not getattr(args, "shard", None):
        return None
    if args.only:
        raise SystemExit("--shard and --only do not go together (a shard is its own --only)")
    path = args.shards or SHARDS_FILE
    plan = load_shards(path)
    found = [s for s in plan if s["name"] == args.shard]
    if not found:
        raise SystemExit(f"no shard {args.shard!r} in {path} ({', '.join(s['name'] for s in plan)})")
    shard = found[0]
    args.only = ",".join(shard["files"] + shard.get("setup", []))
    return {"name": shard["name"], "files": shard["files"], "setup": shard.get("setup", []),
            "plan": file_digest(path)}


def corpus_runs(only: str | None) -> list[tuple[str, dict]]:
    """Every request a recording with this --only sends, in order: (request id, request with its
    file's defaults)."""
    return [(f"{stem}/{req['name']}", {**doc.get("defaults", {}), **req})
            for stem, doc in load_corpus(only) for req in doc["requests"]]


def var_uses(req: dict) -> set[str]:
    """The variables a request needs: its {{vars}}, and the sign-in its "session" sends."""
    found = set(VAR_RE.findall(json.dumps({k: v for k, v in req.items() if k != "capture"})))
    if req.get("session") in SESSIONS and str(req.get("path", "")).startswith("/api/admin"):
        found.add(SESSIONS[req["session"]])
    return found


def providers(runs: list[tuple[str, dict]]) -> dict[str, dict[str, str]]:
    """For each request, the request that captured each variable it uses (the latest capture
    before it), as {request id: {var: capturing request id}}."""
    last: dict[str, str] = {}
    out: dict[str, dict[str, str]] = {}
    for rid, req in runs:
        out[rid] = {v: last[v] for v in var_uses(req) if v in last}
        for var in (req.get("capture") or {}):
            last[var] = rid
    return out


def check_shards(plan: list[dict]) -> list[str]:
    """What is wrong with a plan: a corpus file in no shard or in two, files out of corpus order,
    a request whose variables come from a request its shard does not run first, and a file whose
    "reads" (state earlier files leave) its shard does not run before it in full."""
    problems: list[str] = []
    stems = [s for s, _ in load_corpus(None)]
    owner: dict[str, str] = {}
    reads: dict[str, dict[str, str]] = {}
    for shard in plan:
        files = shard["files"]
        for stem in files:
            if stem not in stems:
                problems.append(f"{shard['name']}: no corpus file {stem}")
            elif stem in owner:
                problems.append(f"{stem} is in two shards: {owner[stem]} and {shard['name']}")
            else:
                owner[stem] = shard["name"]
        known = [s for s in files if s in stems]
        if known != sorted(known):
            problems.append(f"{shard['name']}: files out of corpus order ({', '.join(files)})")
        for g in shard.get("setup", []):
            if not any(only_match(rid, [g]) for rid, _ in corpus_runs(None)):
                problems.append(f"{shard['name']}: setup {g!r} matches no request")
        for stem, deps in (shard.get("reads") or {}).items():
            if stem in reads:
                problems.append(f"{stem}: reads declared twice")
            reads[stem] = deps
    for stem in stems:
        if stem not in owner:
            problems.append(f"{stem} is in no shard")
    full = corpus_runs(None)
    need = providers(full)
    for shard in plan:
        only = ",".join(shard["files"] + shard.get("setup", []))
        runs = corpus_runs(only)
        ran = {rid for rid, _ in runs}
        got = providers(runs)
        short_of: dict[tuple[str, str, str], list[str]] = {}
        for rid, _ in runs:
            for var, src in need.get(rid, {}).items():
                if got[rid].get(var) != src:
                    short_of.setdefault((rid.split("/")[0], var, src), []).append(rid)
        for (stem, var, src), rids in short_of.items():
            problems.append(f"{shard['name']}: {len(rids)} request(s) of {stem} (first {rids[0]}) use "
                            f"{{{{{var}}}}} from {src}, which the shard does not run before them")
        files_run = [s for s, _ in load_corpus(only)]
        for stem in shard["files"]:
            for dep, why in (reads.get(stem) or {}).items():
                if dep not in stems:
                    problems.append(f"{stem} reads {dep}, which is no corpus file")
                    continue
                whole = all(f"{dep}/{r['name']}" in ran for r in dict(load_corpus(None))[dep]["requests"])
                if not (dep in files_run and whole and stems.index(dep) < stems.index(stem)):
                    problems.append(f"{shard['name']}: {stem} reads what {dep} leaves ({why}), "
                                    f"and the shard does not run all of {dep} before it")
    for stem in reads:
        if stem not in stems:
            problems.append(f"reads names {stem}, which is no corpus file")
    return problems


def cmd_shards(args) -> None:
    path = args.shards or SHARDS_FILE
    if args.action == "digest":
        print(file_digest(path))
        return
    plan = load_shards(path)
    times: dict[str, float] = {}
    if args.times:
        with open(os.path.join(args.times, "meta.json"), encoding="utf-8") as fh:
            times = json.load(fh).get("fileSeconds") or {}
        if not times:
            raise SystemExit(f"{args.times}: no fileSeconds in its meta.json")
    for shard in plan:
        line = f"  {shard['name']:<12} {len(shard['files']):>2} files"
        if times:
            # Every file the shard runs whole (00-setup, its own, whole files run as setup).
            whole = {"00-setup", *shard["files"], *(g for g in shard.get("setup", []) if "/" not in g)}
            line += f"  ~{sum(times.get(s, 0) for s in whole):>5.0f} s"
        print(line + f"  {', '.join(shard['files'])}" + (f"  (setup: {', '.join(shard['setup'])})"
                                                          if shard.get("setup") else ""))
    if args.action == "show":
        return
    problems = check_shards(plan)
    for p in problems:
        print(f"FAIL {p}")
    if problems:
        sys.exit(1)
    print(f"shards check: {len(plan)} shards cover the {len(load_corpus(None))} corpus files")


def cmd_merge(args) -> None:
    """Join shard recordings into one recording in corpus order. Each corpus file comes from the
    shard that owns it; the per-shard values the diff normalizes (the host, volatile captures)
    stay with that shard's files (meta.json "parts")."""
    import shutil
    parts = []
    owner: dict[str, int] = {}
    for i, d in enumerate(args.shard_dirs):
        mpath = os.path.join(d, "meta.json")
        if not os.path.exists(mpath):
            raise SystemExit(f"{d}: no meta.json (not a finished recording)")
        with open(mpath, encoding="utf-8") as fh:
            meta = json.load(fh)
        recorded = sorted(os.path.splitext(os.path.basename(f))[0]
                          for f in glob.glob(os.path.join(d, "*.json")) if os.path.basename(f) != "meta.json")
        if "parts" in meta:
            raise SystemExit(f"{d} is a merged recording already")
        shard = meta.get("shard")
        name = shard["name"] if shard else os.path.basename(os.path.normpath(d))
        owns = list(shard["files"]) if shard else [s for s in recorded if s != "00-setup" or "00-setup" not in owner]
        for stem in owns:
            if stem not in recorded:
                raise SystemExit(f"{d}: shard {name} owns {stem} but did not record it")
            if stem in owner:
                raise SystemExit(f"{stem} is owned by two shards: {parts[owner[stem]]['name']} and {name}")
            owner[stem] = i
        parts.append({"name": name, "dir": os.path.abspath(d), "meta": meta, "files": owns})
    if not parts:
        raise SystemExit("merge needs at least one shard recording")
    first = parts[0]["meta"]
    for p in parts[1:]:
        for key in ("inputs", "image"):
            if p["meta"].get(key) != first.get(key):
                raise SystemExit(f"shard {p['name']} was recorded with another {key} "
                                 f"({p['meta'].get(key)} against {first.get(key)})")
        a, b = (first.get("shard") or {}).get("plan"), (p["meta"].get("shard") or {}).get("plan")
        if a != b:
            raise SystemExit(f"shard {p['name']} comes from another shard plan")
    missing = [s for s, _ in load_corpus(None) if s not in owner]
    if missing and not args.partial:
        raise SystemExit(f"no shard recorded {', '.join(missing)} (--partial merges what there is)")
    out = args.run_dir
    os.makedirs(out, exist_ok=True)
    for f in glob.glob(os.path.join(out, "*.json")):
        os.remove(f)
    file_seconds: dict[str, float] = {}
    for stem, i in sorted(owner.items()):
        shutil.copyfile(os.path.join(parts[i]["dir"], f"{stem}.json"), os.path.join(out, f"{stem}.json"))
        if stem in (parts[i]["meta"].get("fileSeconds") or {}):
            file_seconds[stem] = parts[i]["meta"]["fileSeconds"][stem]
    for p in parts:
        logs = os.path.join(p["dir"], "logs")
        if os.path.isdir(logs):
            dest = os.path.join(out, "logs", p["name"])
            shutil.rmtree(dest, ignore_errors=True)
            shutil.copytree(logs, dest)
    entries = []
    for f in glob.glob(os.path.join(out, "*.json")):
        with open(f, encoding="utf-8") as fh:
            entries += json.load(fh)["entries"]
    merged = {
        "target": first.get("target"),
        "host": first.get("host"),
        "hosts": first.get("hosts"),
        "image": first.get("image"),
        "inputs": first.get("inputs"),
        "recordedAt": max(p["meta"].get("recordedAt", "") for p in parts),
        "vars": {},
        "keptVars": {},
        "requests": sum(1 for e in entries if "status" in e),
        "errors": sum(1 for e in entries if "error" in e),
        "seconds": max(p["meta"].get("seconds", 0) for p in parts),
        "fileSeconds": file_seconds,
        "shardPlan": (first.get("shard") or {}).get("plan"),
        "parts": [{"name": p["name"], "dir": p["dir"], "files": p["files"],
                   "host": p["meta"].get("host"), "hosts": p["meta"].get("hosts"),
                   "vars": p["meta"].get("vars") or {}, "keptVars": p["meta"].get("keptVars") or {},
                   "seconds": p["meta"].get("seconds"), "waitsFrom": p["meta"].get("waitsFrom")}
                  for p in parts],
    }
    with open(os.path.join(out, "meta.json"), "w", encoding="utf-8") as fh:
        json.dump(merged, fh, ensure_ascii=False, indent=1)
        fh.write("\n")
    log(f"merged {len(parts)} shards into {out}: {len(owner)} corpus files, {merged['requests']} responses, "
        f"slowest shard {merged['seconds']:.0f} s" + (f"; missing {', '.join(missing)}" if missing else ""))


def inputs_digest() -> str:
    """One hash over everything a recording depends on besides the image: the corpus, the stubs,
    the fixtures, the certificates and the compose file. A recording made from other inputs is
    stale (lxc131.sh records the C# side again when it changes)."""
    h = hashlib.sha256()
    for sub in ("corpus", "stubs", "fixtures", "certs"):
        for root, dirs, files in os.walk(os.path.join(HERE, sub)):
            dirs.sort()
            for name in sorted(files):
                if name.endswith((".pyc",)):
                    continue
                path = os.path.join(root, name)
                h.update(os.path.relpath(path, HERE).replace(os.sep, "/").encode())
                with open(path, "rb") as fh:
                    data = fh.read()
                if b"\0" not in data:  # text: the same digest on a Windows checkout (CRLF)
                    data = data.replace(b"\r\n", b"\n")
                h.update(hashlib.sha256(data).digest())
    with open(COMPOSE_FILE, "rb") as fh:
        h.update(fh.read().replace(b"\r\n", b"\n"))
    return h.hexdigest()


# --------------------------------------------------------------------------------------
# Normalisation
# --------------------------------------------------------------------------------------

# Hop-by-hop and framing headers. Always ignored: Date. Ignored in structural mode only:
# the framing ones (the body is compared instead).
ALWAYS_DROP_HEADERS = {"date", "connection", "keep-alive"}
STRUCTURAL_DROP_HEADERS = {"transfer-encoding", "content-length"}

TRACE_RE = re.compile(r"00-[0-9a-f]{32}-[0-9a-f]{16}-00")
JWT_RE = re.compile(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")
ISO_RE = re.compile(r"\b(20\d\d)-(\d\d)-(\d\d)[T ](\d\d):(\d\d):(\d\d)(\.\d+)?(Z|[+-]\d\d:?\d\d)?")
HTTP_DATE_RE = re.compile(r"\b(Mon|Tue|Wed|Thu|Fri|Sat|Sun), \d\d (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) (20\d\d) "
                          r"\d\d:\d\d:\d\d GMT")
# Anything dated on or after this year was produced by the run itself (fixture dates are 2018-2021).
RUNTIME_YEAR = 2025


def iso_placeholder(m: re.Match, frac_digits: bool) -> str:
    """A run-time timestamp becomes a placeholder that keeps its shape: separator, whether it has
    a fraction, and the zone form. The fraction's digit count is dropped (frac_digits=False in both
    modes): Navidrome (Go) trims trailing zeros, so the count varies from run to run."""
    if int(m.group(1)) < RUNTIME_YEAR:
        return m.group(0)
    frac = m.group(7)
    zone = m.group(8) or ""
    sep = m.group(0)[10]
    frac_shape = (".f" + (str(len(frac) - 1) if frac_digits else "")) if frac else ""
    return f"TS({sep}{frac_shape}{zone if zone in ('Z', '') else '+hh:mm'})"


def http_date_placeholder(m: re.Match) -> str:
    return "HTTP-DATE" if int(m.group(3)) >= RUNTIME_YEAR else m.group(0)


def load_normalize_rules() -> dict:
    if not os.path.exists(NORMALIZE_FILE):
        return {"rules": [], "ignoreKeys": []}
    with open(NORMALIZE_FILE, encoding="utf-8") as fh:
        return json.load(fh)


class Normalizer:
    def __init__(self, meta: dict, rules: dict, mode: str = "structural"):
        self.mode = mode
        self.host = meta.get("host", "")
        self.other_hosts = sorted((meta.get("hosts") or {}).items(), key=lambda kv: -len(kv[1]))
        # Longest values first, so a value that contains another is replaced whole.
        self.vars = sorted(((k, v) for k, v in (meta.get("vars") or {}).items() if v and len(v) >= 6),
                           key=lambda kv: -len(kv[1]))
        self.rules = []
        for r in rules.get("rules", []):
            rr = dict(r)
            rr["_re"] = re.compile(r["regex"]) if "regex" in r else None
            self.rules.append(rr)
        self.global_ignore_keys = set(rules.get("ignoreKeys", []))
        # Arrays (JSON) or runs of sibling elements (XML) under these names have no defined
        # order upstream (Navidrome builds them from Go maps), so they are sorted.
        self.unordered_keys = set(rules.get("unorderedKeys", []))
        self._json_runs = [re.compile(r'("%s":\[)([^\[\]]*)(\])' % re.escape(k)) for k in self.unordered_keys]
        self._xml_runs = [re.compile(r"(?:<%s>[^<]*</%s>\s*){2,}" % (re.escape(k), re.escape(k)))
                          for k in self.unordered_keys]

    def text(self, s: str, rid: str, where: str) -> str:
        for name, value in self.vars:
            s = s.replace(value, "{{" + name + "}}")
            enc = urllib.parse.quote(value, safe="")
            if enc != value:
                s = s.replace(enc, "{{" + name + "}}")
        for name, value in self.other_hosts:
            if value:
                s = s.replace(value, "{{" + name + "}}")
        if self.host:
            s = s.replace(self.host, "{{host}}")
        s = TRACE_RE.sub("00-TRACE-00", s)
        s = JWT_RE.sub("JWT", s)
        s = ISO_RE.sub(lambda m: iso_placeholder(m, False), s)
        s = HTTP_DATE_RE.sub(http_date_placeholder, s)
        if where == "body":
            s = self.sort_runs(s)
        for r in self.rules:
            if not fnmatch.fnmatch(rid, r.get("requests", "*")):
                continue
            if "where" in r and not fnmatch.fnmatch(where, r["where"]):
                continue
            if r["_re"] is not None:
                s = r["_re"].sub(r.get("replace", "<normalised>"), s)
        return s

    def sort_runs(self, s: str) -> str:
        """Text-level sort of the unordered runs, so bytes mode sees them in a fixed order too."""
        for rx in self._json_runs:
            s = rx.sub(lambda m: m.group(1) + ",".join(sorted(m.group(2).split(","))) + m.group(3), s)
        for rx in self._xml_runs:
            def fix(m):
                run = m.group(0)
                items = re.findall(r"<[^>]+>[^<]*</[^>]+>", run)
                seps = re.split(r"<[^>]+>[^<]*</[^>]+>", run)[1:]
                return "".join(i + sep for i, sep in zip(sorted(items), seps))
            s = rx.sub(fix, s)
        return s

    def headers(self, entry: dict, rid: str, mode: str, keep_length: bool = False) -> list[tuple[str, str]]:
        drop = set(ALWAYS_DROP_HEADERS)
        if mode == "structural" and not keep_length:
            drop |= STRUCTURAL_DROP_HEADERS
        out = []
        for k, v in entry.get("headers", []):
            lk = k.lower()
            if lk in drop:
                continue
            out.append((lk, self.text(v, rid, f"header:{lk}")))
        if mode == "structural":
            # Order between different header names carries no meaning; repeats keep their order.
            out = sorted(out, key=lambda kv: kv[0])
        return out


def normalizers(meta: dict, rules: dict, mode: str):
    """The normalizer for each corpus file of a recording, as a function of the file. A merged
    recording (parity.py merge) keeps each shard's host and volatile captures with the files that
    shard recorded, since every shard ran on a stack of its own."""
    whole = Normalizer(meta, rules, mode)
    by_stem: dict[str, Normalizer] = {}
    for part in meta.get("parts") or []:
        norm = Normalizer(part, rules, mode)
        for stem in part["files"]:
            by_stem[stem] = norm
    return lambda stem: by_stem.get(stem, whole)


def apply_ignore(value: Any, keys: set[str]) -> Any:
    """Drop ignored members entirely: a volatile value may also be absent on one side
    (Navidrome fills some fields in the background)."""
    if isinstance(value, dict):
        return {k: apply_ignore(v, keys) for k, v in value.items() if k not in keys}
    if isinstance(value, list):
        return [apply_ignore(v, keys) for v in value]
    return value


def canon(value: Any) -> str:
    return json.dumps(value, sort_keys=True, ensure_ascii=False)


def sort_arrays(value: Any) -> Any:
    if isinstance(value, dict):
        return {k: sort_arrays(v) for k, v in value.items()}
    if isinstance(value, list):
        return sorted((sort_arrays(v) for v in value), key=canon)
    return value


def xml_tree(elem: ET.Element, ignore: set[str]) -> dict:
    attrs = {k: v for k, v in elem.attrib.items() if k.split("}")[-1] not in ignore}
    text = (elem.text or "").strip()
    return {"tag": elem.tag, "attrs": attrs, "text": text, "children": [xml_tree(c, ignore) for c in elem]}


def xml_sort(node: dict) -> dict:
    kids = sorted((xml_sort(c) for c in node["children"]), key=canon)
    return dict(node, children=kids)


# --------------------------------------------------------------------------------------
# Comparison
# --------------------------------------------------------------------------------------

def short(v: Any, n: int = 120) -> str:
    s = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)
    return s if len(s) <= n else s[: n - 3] + "..."


def diff_json(a: Any, b: Any, path: str, out: list[str], limit: int = 25) -> None:
    if len(out) >= limit:
        return
    if isinstance(a, dict) and isinstance(b, dict):
        for k in a.keys() - b.keys():
            out.append(f"{path}.{k}: only in A ({short(a[k])})")
        for k in b.keys() - a.keys():
            out.append(f"{path}.{k}: only in B ({short(b[k])})")
        for k in a.keys() & b.keys():
            diff_json(a[k], b[k], f"{path}.{k}", out, limit)
        return
    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            out.append(f"{path}: array length {len(a)} != {len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff_json(x, y, f"{path}[{i}]", out, limit)
        return
    if type(a) is not type(b) or a != b:
        out.append(f"{path}: {short(a)} != {short(b)}")


def diff_xml(a: dict, b: dict, path: str, out: list[str], limit: int = 25) -> None:
    if len(out) >= limit:
        return
    here = f"{path}/{a['tag'].split('}')[-1]}"
    if a["tag"] != b["tag"]:
        out.append(f"{path}: element {a['tag']} != {b['tag']}")
        return
    for k in a["attrs"].keys() | b["attrs"].keys():
        va, vb = a["attrs"].get(k), b["attrs"].get(k)
        if va != vb:
            out.append(f"{here}@{k}: {short(va) if va is not None else '(absent)'} != "
                       f"{short(vb) if vb is not None else '(absent)'}")
    if a["text"] != b["text"]:
        out.append(f"{here} text: {short(a['text'])} != {short(b['text'])}")
    if len(a["children"]) != len(b["children"]):
        out.append(f"{here}: {len(a['children'])} children != {len(b['children'])}")
    for i, (x, y) in enumerate(zip(a["children"], b["children"])):
        diff_xml(x, y, f"{here}[{i}]", out, limit)


def body_view(entry: dict, norm: Normalizer, rid: str, mode: str, req: dict) -> tuple[str, Any]:
    """The comparable form of a body: ('hash', sha) / ('text', str) / ('json', obj) / ('xml', tree)."""
    body = entry.get("body") or {"kind": "empty"}
    kind = body["kind"]
    if kind == "empty":
        return "empty", None
    if "text" not in body:
        return "hash", f"{body['sha256']} ({body['size']} bytes, {kind})"
    text = norm.text(body["text"], rid, "body")
    ignore = norm.global_ignore_keys | set(req.get("ignoreKeys", []))
    unordered = req.get("unordered", False)
    # Bytes mode compares the normalised text exactly, except where the corpus says the upstream
    # order is undefined: there only the structure can be compared.
    # "anyKeyOrder": an object whose member order the C# build itself does not keep from run to
    # run (a dictionary filled in parallel); bytes mode compares it as parsed JSON instead.
    if kind == "text" or (mode == "bytes" and not unordered and not req.get("anyKeyOrder")):
        return "text", normalised_text(entry, norm, rid, req)
    if kind == "json":
        try:
            doc = apply_ignore(json.loads(text), ignore)
        except ValueError:
            return "text", text
        return "json", sort_arrays(doc) if unordered else doc
    if kind == "xml":
        try:
            tree = xml_tree(ET.fromstring(text), ignore)
        except ET.ParseError:
            return "text", text
        return "xml", xml_sort(tree) if unordered else tree
    return "text", text


def _skip_json_value(text: str, i: int) -> int:
    """Index just past the JSON value starting at text[i] (whitespace allowed before it)."""
    n = len(text)
    while i < n and text[i] in " \t\r\n":
        i += 1
    if i >= n:
        return i
    if text[i] == '"':
        i += 1
        while i < n and text[i] != '"':
            i += 2 if text[i] == "\\" else 1
        return i + 1
    if text[i] in "{[":
        depth = 0
        while i < n:
            c = text[i]
            if c == '"':
                i = _skip_json_value(text, i)
                continue
            if c in "{[":
                depth += 1
            elif c in "}]":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        return i
    while i < n and text[i] not in ",}] \t\r\n":
        i += 1
    return i


def ignore_in_text(text: str, keys: set[str]) -> str:
    """Bytes-mode version of ignoreKeys: remove the JSON member (any value, nested ones too) or
    the XML attribute, with its separating comma, leaving every other byte as it was."""
    for k in keys:
        needle = re.compile(r'"%s"\s*:' % re.escape(k))
        pos = 0
        while True:
            m = needle.search(text, pos)
            if not m:
                break
            end = _skip_json_value(text, m.end())
            start = m.start()
            # Take the comma that separated this member from a neighbour.
            before = text[:start].rstrip()
            if before.endswith(","):
                start = len(before) - 1
            else:
                after = len(text) - len(text[end:].lstrip())
                if after < len(text) and text[after] == ",":
                    end = after + 1
            text = text[:start] + text[end:]
            pos = start
        text = re.sub(r'\s%s="[^"]*"' % re.escape(k), '', text)
    return text


def normalised_text(entry: dict, norm: "Normalizer", rid: str, req: dict) -> str | None:
    body = entry.get("body") or {}
    if body.get("text") is None:
        return None
    text = norm.text(body["text"], rid, "body")
    ignore = norm.global_ignore_keys | set(req.get("ignoreKeys", []))
    return ignore_in_text(text, ignore) if ignore else text


def text_diff(a: str, b: str) -> list[str]:
    import difflib
    lines = list(difflib.unified_diff(a.splitlines(), b.splitlines(), "A", "B", lineterm="", n=1))
    if not lines and a != b:
        # Same lines, different line endings or trailing newline.
        return [f"texts differ only in line endings/trailing whitespace ({len(a)} vs {len(b)} chars)"]
    return [short(line, 200) for line in lines[:40]]


def length_is_meaningful(ea: dict, eb: dict, na: Normalizer, nb: Normalizer, rid: str, req: dict) -> bool:
    ta, tb = (ea.get("body") or {}).get("text"), (eb.get("body") or {}).get("text")
    if ta is None or tb is None:
        return True
    if req.get("unordered"):
        return False
    da = len(ta.encode()) - len(normalised_text(ea, na, rid, req).encode())
    db = len(tb.encode()) - len(normalised_text(eb, nb, rid, req).encode())
    return da == db


def compare_entry(rid: str, ea: dict, eb: dict, na: Normalizer, nb: Normalizer, mode: str,
                  req: dict) -> dict[str, list[str]]:
    """Diffs by aspect: 'status', 'header:<name>', 'body'."""
    diffs: dict[str, list[str]] = {}
    if "error" in ea or "error" in eb or "skipped" in ea or "skipped" in eb:
        if ea.get("error") != eb.get("error") or ea.get("skipped") != eb.get("skipped"):
            diffs["transport"] = [f"A: {ea.get('error') or ea.get('skipped') or 'ok'}",
                                  f"B: {eb.get('error') or eb.get('skipped') or 'ok'}"]
        return diffs
    if ea.get("untilTimedOut") != eb.get("untilTimedOut"):
        diffs["until"] = [f"A: {'timed out' if ea.get('untilTimedOut') else 'condition met'}",
                          f"B: {'timed out' if eb.get('untilTimedOut') else 'condition met'}"]
    if ea["status"] != eb["status"]:
        diffs["status"] = [f"{ea['status']} != {eb['status']}"]
    # "keepLengthHeaders": whether a length is promised is the point of the request (a transcode's
    # estimated length), so Content-Length and Transfer-Encoding count in structural mode too.
    keep = bool(req.get("keepLengthHeaders"))
    ha, hb = na.headers(ea, rid, mode, keep), nb.headers(eb, rid, mode, keep)
    if (mode == "bytes" or keep) and not length_is_meaningful(ea, eb, na, nb, rid, req):
        # The bodies' lengths moved with volatile values (timestamps, Go's trimmed fractions,
        # ids), so Content-Length says nothing the body comparison does not.
        ha = [(k, v) for k, v in ha if k != "content-length"]
        hb = [(k, v) for k, v in hb if k != "content-length"]
    if ha != hb:
        if mode == "bytes" and sorted(ha) == sorted(hb):
            diffs["header-order"] = ["A: " + ", ".join(k for k, _ in ha), "B: " + ", ".join(k for k, _ in hb)]
        else:
            names = {k for k, _ in ha} | {k for k, _ in hb}
            for name in sorted(names):
                va = [v for k, v in ha if k == name]
                vb = [v for k, v in hb if k == name]
                if va != vb:
                    diffs[f"header:{name}"] = [f"A: {va if va else '(absent)'}", f"B: {vb if vb else '(absent)'}"]
    ka, va_ = body_view(ea, na, rid, mode, req)
    kb, vb_ = body_view(eb, nb, rid, mode, req)
    if (ka, va_) != (kb, vb_):
        lines: list[str] = []
        if ka != kb:
            lines.append(f"body kind {ka} != {kb}")
        if ka == kb == "json":
            diff_json(va_, vb_, "$", lines)
        elif ka == kb == "xml":
            diff_xml(va_, vb_, "", lines)
        elif ka == kb == "text":
            lines += text_diff(va_, vb_)
        else:
            lines.append(f"A: {short(va_)}")
            lines.append(f"B: {short(vb_)}")
        diffs["body"] = lines or ["bodies differ"]
    return diffs


def load_recording(path: str) -> tuple[dict, dict[str, dict], list[str]]:
    with open(os.path.join(path, "meta.json"), encoding="utf-8") as fh:
        meta = json.load(fh)
    entries: dict[str, dict] = {}
    order: list[str] = []
    for f in sorted(glob.glob(os.path.join(path, "*.json"))):
        stem = os.path.splitext(os.path.basename(f))[0]
        if stem == "meta":
            continue
        with open(f, encoding="utf-8") as fh:
            doc = json.load(fh)
        for e in doc["entries"]:
            rid = f"{stem}/{e['name']}"
            entries[rid] = e
            order.append(rid)
    return meta, entries, order


def load_allowlist(path: str | None) -> list[dict]:
    if not path or not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as fh:
        rows = json.load(fh)
    known = open(KNOWN_DIFFS, encoding="utf-8").read() if os.path.exists(KNOWN_DIFFS) else ""
    for row in rows:
        ref = row.get("knownDiff")
        if not ref:
            raise SystemExit(f"allowlist entry {row.get('id')} has no knownDiff reference")
        if ref not in known:
            raise SystemExit(f"allowlist entry {row.get('id')}: knownDiff {ref!r} not found in {KNOWN_DIFFS}")
    return rows


def allowed(rid: str, aspect: str, allow: list[dict]) -> str | None:
    for row in allow:
        if not any(fnmatch.fnmatch(rid, pat) for pat in row.get("requests", ["*"])):
            continue
        for pat in row.get("aspects", []):
            if fnmatch.fnmatch(aspect, pat):
                return row["id"]
    return None


def corpus_index() -> dict[str, dict]:
    """Every corpus request with its file's defaults merged in, by "<file>/<name>"."""
    idx = {}
    for stem, doc in load_corpus(None):
        defaults = doc.get("defaults", {})
        for req in doc["requests"]:
            merged = dict(defaults)
            merged.update(req)
            idx[f"{stem}/{req['name']}"] = merged
    return idx


def backlog_of(req: dict) -> list[str]:
    """The backlog groups a request checks ("backlog": "D" or "D,B"); "base" when untagged, for
    the 2026.10.04 behavior the Rust port already had."""
    tag = req.get("backlog") or "base"
    return [t.strip() for t in str(tag).split(",") if t.strip()]


def load_groups() -> list[dict]:
    """groups.json: rules that put a diff on a backlog group by what differs, ahead of the
    request's own tag (the admin security headers are group J whatever the request checks)."""
    if not os.path.exists(GROUPS_FILE):
        return []
    with open(GROUPS_FILE, encoding="utf-8") as fh:
        rows = json.load(fh)["rules"]
    for row in rows:
        row["_re"] = re.compile(row["match"]) if "match" in row else None
    return rows


def groups_of(rid: str, aspect: str, lines: list[str], req: dict, rules: list[dict]) -> set[str]:
    """The backlog groups one differing aspect belongs to. A rule takes the whole aspect by its
    name, or single diff lines by a regex; whatever no rule takes falls to the request's tag."""
    found: set[str] = set()
    left = list(lines)
    for row in rules:
        if not any(fnmatch.fnmatch(rid, pat) for pat in row.get("requests", ["*"])):
            continue
        if not any(fnmatch.fnmatch(aspect, pat) for pat in row.get("aspects", ["*"])):
            continue
        if row["_re"] is None:
            found.add(row["backlog"])
            left = []
            break
        hit = [line for line in left if row["_re"].search(line)]
        if hit:
            found.add(row["backlog"])
            left = [line for line in left if line not in hit]
    if left or not found:
        found.update(backlog_of(req))
    return found


def load_intentional(path: str | None) -> list[dict]:
    """intentional-diffs.json: deliberate improvements in the Rust build, each reviewed by name.
    An entry without "reviewed" is only proposed and explains nothing."""
    if not path or not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as fh:
        rows = json.load(fh)["diffs"]
    for row in rows:
        for key in ("id", "backlog", "requests", "aspects", "reason"):
            if not row.get(key):
                raise SystemExit(f"intentional diff {row.get('id')!r} has no {key!r}")
        if any(pat == "*" for pat in row["requests"]):
            raise SystemExit(f"intentional diff {row['id']!r}: name the requests, never '*'")
        row["_lines"] = [re.compile(x) for x in row.get("lines", [])]
    return rows


def intended(rid: str, aspect: str, lines: list[str], rows: list[dict]) -> tuple[str, bool] | None:
    """The intentional diff that covers this aspect, and whether it is reviewed. With "lines",
    every diff line must match one of them, so a different change on the same aspect still shows."""
    for row in rows:
        if not any(fnmatch.fnmatch(rid, pat) for pat in row["requests"]):
            continue
        if not any(fnmatch.fnmatch(aspect, pat) for pat in row["aspects"]):
            continue
        if row["_lines"] and not all(any(rx.search(line) for rx in row["_lines"]) for line in lines):
            continue
        return row["id"], bool(row.get("reviewed"))
    return None


def cmd_diff(args) -> None:
    meta_a, ea, order_a = load_recording(args.a)
    meta_b, eb, order_b = load_recording(args.b)
    rules = load_normalize_rules()
    na, nb = normalizers(meta_a, rules, args.mode), normalizers(meta_b, rules, args.mode)
    allow = load_allowlist(args.allowlist)
    intent = load_intentional(args.intentional)
    group_rules = load_groups()
    reqs = corpus_index()
    report: list[str] = []
    unexplained = explained = intentional = same = 0
    used_allow: dict[str, int] = {}
    used_intent: dict[str, int] = {}
    pending: dict[str, int] = {}
    by_group: dict[str, set[str]] = {}
    diffed: set[str] = set()

    globs = only_globs(args.only)
    ids = [i for i in order_a if not globs or only_match(i, globs)]
    missing_b = [i for i in ids if i not in eb]
    extra_b = [i for i in order_b if i not in ea and (not globs or only_match(i, globs))]
    if not ids and not extra_b:
        # Never a silent "0 compared": a selection that picks nothing is a mistake, not a pass.
        what = f"--only {args.only!r} picks no request" if globs else "no requests in the recordings"
        log(f"diff: {what} ({args.a}: {len(order_a)} requests, {args.b}: {len(order_b)})")
        sys.exit(3)
    for rid in missing_b + extra_b:
        report.append(f"MISSING in {'B' if rid in missing_b else 'A'}: {rid}")
        unexplained += 1
        diffed.add(rid)
        for g in backlog_of(reqs.get(rid, {})):
            by_group.setdefault(g, set()).add(rid)

    for rid in ids:
        if rid not in eb:
            continue
        req_spec = reqs.get(rid, {})
        stem = rid.split("/", 1)[0]
        diffs = compare_entry(rid, ea[rid], eb[rid], na(stem), nb(stem), args.mode, req_spec)
        if not diffs:
            same += 1
            continue
        open_aspects = {}
        by_intent = False
        for aspect, lines in diffs.items():
            hit = allowed(rid, aspect, allow)
            if hit:
                used_allow[hit] = used_allow.get(hit, 0) + 1
                continue
            meant = intended(rid, aspect, lines, intent)
            if meant and meant[1]:
                used_intent[meant[0]] = used_intent.get(meant[0], 0) + 1
                by_intent = True
                continue
            if meant:
                pending[meant[0]] = pending.get(meant[0], 0) + 1
            open_aspects[aspect] = lines
        if not open_aspects:
            if by_intent:
                intentional += 1
            else:
                explained += 1
            continue
        unexplained += 1
        diffed.add(rid)
        groups: set[str] = set()
        for aspect, lines in open_aspects.items():
            groups |= groups_of(rid, aspect, lines, req_spec, group_rules)
        for g in groups:
            by_group.setdefault(g, set()).add(rid)
        req = ea[rid].get("request", {})
        report.append(f"DIFF {rid}  [{req.get('method', '?')} {short(req.get('target', '?'), 140)}]"
                      f"  groups: {','.join(sorted(groups))}")
        for aspect, lines in open_aspects.items():
            report.append(f"  {aspect}:")
            for line in lines:
                report.append(f"    {line}")

    summary = [
        f"parity diff ({args.mode}): A={args.a} ({meta_a.get('image') or meta_a.get('target')})",
        f"                       B={args.b} ({meta_b.get('image') or meta_b.get('target')})",
        f"  compared {len(ids) - len(missing_b)} requests: {same} identical, {explained} explained by the "
        f"allowlist, {intentional} intentional, {unexplained} unexplained",
    ]
    if meta_a.get("inputs") and meta_b.get("inputs") and meta_a["inputs"] != meta_b["inputs"]:
        summary.append("  WARNING: the recordings were made from different corpus, stub or fixture files")
    for aid, n in sorted(used_allow.items()):
        summary.append(f"  allowlist {aid}: {n} aspect(s)")
    for iid, n in sorted(used_intent.items()):
        summary.append(f"  intentional {iid}: {n} aspect(s)")
    for iid, n in sorted(pending.items()):
        summary.append(f"  intentional {iid} NOT REVIEWED, so not applied: {n} aspect(s)")
    table = group_table(reqs, ids, by_group, diffed)
    text = "\n".join(summary + ([""] + report if report else []) + [""] + table) + "\n"
    sys.stdout.write(text if not args.summary else "\n".join(summary + [""] + table) + "\n")
    if args.report:
        with open(args.report, "w", encoding="utf-8") as fh:
            fh.write(text)
    if args.groups_out:
        with open(args.groups_out, "w", encoding="utf-8") as fh:
            json.dump({g: sorted(r) for g, r in sorted(by_group.items())}, fh, indent=1)
            fh.write("\n")
    sys.exit(1 if unexplained else 0)


def group_table(reqs: dict[str, dict], ids: list[str], by_group: dict[str, set[str]],
                diffed: set[str]) -> list[str]:
    """Per backlog group: the requests tagged with it, how many of those have an unexplained
    diff ("own"), and how many requests anywhere have a diff the group explains ("diffs": its
    own, plus other requests whose differing part groups.json puts on it, such as the admin
    security headers on every admin answer for J). One request can count in several groups."""
    tagged: dict[str, int] = {}
    own: dict[str, int] = {}
    for rid in ids:
        for g in backlog_of(reqs.get(rid, {})):
            tagged[g] = tagged.get(g, 0) + 1
            if rid in diffed:
                own[g] = own.get(g, 0) + 1
    names = sorted(set(tagged) | set(by_group), key=lambda g: (g == "base", g))
    out = ["Diffs by backlog group (tagged: requests tagged with the group; own: of those, with an unexplained",
           "diff; diffs: requests anywhere with a diff put on the group):",
           f"  {'group':<6} {'tagged':>6} {'own':>6} {'diffs':>6}"]
    for g in names:
        out.append(f"  {g:<6} {tagged.get(g, 0):>6} {own.get(g, 0):>6} {len(by_group.get(g, ())):>6}")
    return out


def cmd_show(args) -> None:
    _, entries, order = load_recording(args.recording)
    for rid in order:
        if not fnmatch.fnmatch(rid, args.glob):
            continue
        e = entries[rid]
        req = e.get("request", {})
        print(f"=== {rid}: {req.get('method')} {req.get('target')}")
        if "status" not in e:
            print(f"    {e.get('error') or e.get('skipped')}")
            continue
        print(f"    {e['status']} {e.get('reason', '')}")
        for k, v in e["headers"]:
            print(f"    {k}: {v}")
        body = e["body"]
        text = body.get("text")
        print(f"    [{body['kind']}, {body['size']} bytes]" + ("" if text is not None else f" sha256={body.get('sha256')}"))
        if text:
            print("    " + (text if len(text) <= args.max else text[: args.max] + " ..."))


def save_logs(args) -> None:
    """The containers' logs beside the recording (<out>/logs/), for reading a run afterwards: the
    stubs' log lists every upstream call and the mapping row that answered it (UNMATCHED ones too)."""
    logs = os.path.join(args.out, "logs")
    os.makedirs(logs, exist_ok=True)
    for service in ("stubs", "octo", "octo-alt", "navidrome"):
        with open(os.path.join(logs, f"{service}.log"), "wb") as fh:
            subprocess.run(["docker", "compose", "-f", COMPOSE_FILE, "-p", args.project, "logs", "--no-color",
                            "--no-log-prefix", service], stdout=fh, stderr=subprocess.DEVNULL, check=False)


def cmd_run(args) -> None:
    refuse_empty_only(args.only)  # before a stack is started for nothing
    cmd_up(args)
    try:
        args.target = f"http://127.0.0.1:{args.port}"
        cmd_record(args)
    finally:
        try:
            save_logs(args)
        finally:
            if not args.keep:
                cmd_down(args)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    def up_opts(p):
        p.add_argument("--image", default=os.environ.get("OCTO_IMAGE", DEFAULT_IMAGE))
        p.add_argument("--port", type=int, default=int(os.environ.get("PARITY_PORT", DEFAULT_PORT)))
        p.add_argument("--nd-port", type=int, default=int(os.environ.get("PARITY_ND_PORT", DEFAULT_ND_PORT)))
        p.add_argument("--project", default=os.environ.get("PARITY_PROJECT", "parity"))
        p.add_argument("--settle", type=float, default=10.0, help="seconds to wait after Octo answers")

    def record_opts(p):
        p.add_argument("--out", required=True)
        p.add_argument("--alt-port", type=int, default=None, help="the alt Octo's main port (default --port + 1)")
        p.add_argument("--until-patience", type=int, default=3,
                       help="after this many timed-out waits in one corpus file, its other waits get at most 20 s")
        p.add_argument("--alt-admin-port", type=int, default=None, help="its admin port (default --port + 2)")
        p.add_argument("--only", help="glob over <corpus-file>/<request-name>, or several, comma-separated")
        p.add_argument("--shard", help="record this shard of the plan (shards.json): its files and setup requests")
        p.add_argument("--shards", default=None, help="the shard plan (default parity/shards.json)")
        p.add_argument("--waits-from", default=None,
                       help="a recording of the reference build (C#): each wait it met gets "
                            f"max({WAIT_FLOOR_MS // 1000} s, {WAIT_FACTOR} x its time there), and its time "
                            f"+ {WAIT_JOB_EXTRA_MS // 1000} s when that was a second or more, at most")
        p.add_argument("--timeout", type=float, default=60.0)
        p.add_argument("--image-label", default=None, help="label stored in meta.json (default: --image)")
        p.add_argument("-v", "--verbose", action="store_true")

    p = sub.add_parser("up")
    up_opts(p)
    p.set_defaults(func=cmd_up)

    p = sub.add_parser("down")
    p.add_argument("--project", default=os.environ.get("PARITY_PROJECT", "parity"))
    p.set_defaults(func=cmd_down)

    p = sub.add_parser("record")
    p.add_argument("--target", required=True)
    p.add_argument("--project", default=os.environ.get("PARITY_PROJECT", "parity"), help="for exec requests")
    record_opts(p)
    p.set_defaults(func=cmd_record)

    p = sub.add_parser("diff")
    p.add_argument("a")
    p.add_argument("b")
    p.add_argument("--mode", choices=["structural", "bytes"], default="structural")
    p.add_argument("--allowlist", default=DEFAULT_ALLOWLIST)
    p.add_argument("--intentional", default=INTENTIONAL, help="reviewed intentional diffs (intentional-diffs.json)")
    p.add_argument("--report")
    p.add_argument("--groups-out", help="write {group: [request ids with diffs]} as JSON")
    p.add_argument("--summary", action="store_true", help="print the summary and the group table only")
    p.add_argument("--only", help="as record's --only: file names or globs over <file>/<request>, comma-separated")
    p.set_defaults(func=cmd_diff)

    p = sub.add_parser("digest", help="print the hash of the corpus, stubs, fixtures and compose file")
    p.set_defaults(func=lambda _args: print(inputs_digest()))

    p = sub.add_parser("merge", help="join shard recordings into one recording, in corpus order")
    p.add_argument("run_dir", help="the merged recording (its *.json files are replaced)")
    p.add_argument("shard_dirs", nargs="+", help="the shards' recordings (record or run --shard)")
    p.add_argument("--partial", action="store_true", help="merge even when corpus files are missing")
    p.set_defaults(func=cmd_merge)

    p = sub.add_parser("shards", help="the shard plan: check it, show it, or print its hash")
    p.add_argument("action", choices=["check", "show", "digest"])
    p.add_argument("--shards", default=None, help="the plan (default parity/shards.json)")
    p.add_argument("--times", default=None, help="a recording whose fileSeconds give each shard's length")
    p.set_defaults(func=cmd_shards)

    p = sub.add_parser("show", help="print recorded entries matching a glob")
    p.add_argument("recording")
    p.add_argument("glob")
    p.add_argument("--max", type=int, default=1500)
    p.set_defaults(func=cmd_show)

    p = sub.add_parser("run")
    up_opts(p)
    record_opts(p)
    p.add_argument("--keep", action="store_true", help="leave the containers running")
    p.set_defaults(func=cmd_run)

    args = ap.parse_args()
    if getattr(args, "image_label", "unset") is None:
        args.image_label = getattr(args, "image", None) or args.target
    args.func(args)


if __name__ == "__main__":
    main()
