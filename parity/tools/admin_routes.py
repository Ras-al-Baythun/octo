#!/usr/bin/env python3
"""The admin API's routes, read from the C# controllers, and the sign-in corpus built from them.

  admin_routes.py list            print every /api/admin route: method, path, [AdminOpen], [AdminAppCall]
  admin_routes.py write           rewrite corpus/11-signin.json and corpus/99-signed-out-writes.json
  admin_routes.py check           exit 1 when a C# admin route is missing from those two files

The gate (AdminSignInGate.cs) stands in front of every /api/admin route, so the corpus asks
each one signed out (401 with signIn, or the open answer) and each GET signed in. A route added
to the C# app later shows up in `check` until the corpus is written again. Stdlib only.

Signed-out writes run last (99-...): a build without the gate would carry them out, so they come
after everything else, and the restart is the very last request.
"""
from __future__ import annotations

import glob
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PARITY = os.path.dirname(HERE)
REPO = os.path.dirname(PARITY)
CONTROLLERS = os.path.join(REPO, "octo", "Controllers")
SIGNIN = os.path.join(PARITY, "corpus", "11-signin.json")
WRITES = os.path.join(PARITY, "corpus", "99-signed-out-writes.json")

VERB_RE = re.compile(r'\[Http(Get|Post|Put|Delete|Patch)(?:\("([^"]*)"\))?')
CLASS_ROUTE_RE = re.compile(r'^\[Route\("([^"]*)"\)\]')


def routes() -> list[dict]:
    """Every action route under api/admin: {method, path, open, appCall, file, action}."""
    out = []
    for path in sorted(glob.glob(os.path.join(CONTROLLERS, "*.cs"))):
        prefix = ""
        attrs: list[str] = []
        with open(path, encoding="utf-8") as fh:
            lines = [line.strip() for line in fh]
        for i, line in enumerate(lines):
            m = CLASS_ROUTE_RE.match(line)
            if m and any("class " in x for x in lines[i + 1:i + 4]):
                prefix = m.group(1)
                attrs = []
                continue
            if line.startswith("["):
                attrs.append(line)
                continue
            if not line or line.startswith("//") or line.startswith("///"):
                continue
            if re.match(r"(public|private|internal|protected)\b.*\(", line) and attrs:
                action = re.search(r"(\w+)\s*\(", line).group(1)
                is_open = any("AdminOpen" in a for a in attrs)
                app = next((a for a in attrs if "AdminAppCall" in a), None)
                for a in attrs:
                    for verb, template in VERB_RE.findall(a):
                        template = template or ""
                        if template.startswith("~/") or template.startswith("/"):
                            full = template.lstrip("~/")
                        elif template:
                            full = f"{prefix}/{template}" if prefix else template
                        else:
                            full = prefix
                        if not full.startswith("api/admin"):
                            continue
                        out.append({
                            "method": verb.upper(), "path": "/" + full, "open": is_open,
                            "appCall": None if app is None else ("admin" if "true" in app else "any"),
                            "file": os.path.basename(path), "action": action,
                        })
            attrs = []
    return out


# Values for route parameters: unknown ids, so a signed-in GET answers its own not-found.
PARAMS = {"{search}": "parity-no-such-search", "{id}": "parity-no-such-id"}

# GETs left out of the signed-in sweep, with the reason (the signed-out pass still asks them).
SIGNED_IN_SKIP = {
    "/api/admin/discover-servers": "scans the network; the sandbox's addresses change every run",
    "/api/admin/sonic": "the Sounds alike worker's first look is 2 minutes after start, so the state "
                        "here depends on how fast the run got this far; 15-radio reads it once settled",
}


# The backlog group behind each admin feature (docs/rust-port/BACKLOG.md), for the signed-in
# requests: a diff there is the feature's, while the signed-out ones are the gate's (J). Routes
# the Rust port already had at 2026.10.04 are "base".
GROUP_BY_PREFIX = [
    ("/api/admin/imports", "H"),
    ("/api/admin/downloads/log", "D"),
    ("/api/admin/find", "D"),
    ("/api/admin/trash", "F"),
    ("/api/admin/health", "R"),
    ("/api/admin/explicit", "P"),
    ("/api/admin/radio-outcomes", "L"),
    ("/api/admin/sonic", "L"),
    ("/api/admin/activity", "N"),
    ("/api/admin/ambient", "N"),
    ("/api/admin/auth/recovery", "J"),
    ("/api/admin/browse/session", "J"),
    ("/api/admin/settings", "I"),
]


def group_of(path: str) -> str:
    for prefix, group in GROUP_BY_PREFIX:
        if path == prefix or path.startswith(prefix + "/"):
            return group
    return "base"


# Compare options for the signed-in GETs whose answers C# does not keep in one order from run to
# run: the lossy list (Navidrome's order), and Library health's song table (filled in parallel).
COMPARE = {
    "/api/admin/lossy": {"unordered": True},
    "/api/admin/health": {"anyKeyOrder": True},
}


# The same for the signed-out GETs: a build without the gate answers them, and the network scan's
# addresses change every run.
COMPARE_OUT = {
    "/api/admin/discover-servers": {"ignoreKeys": ["servers"]},
}


def fill(path: str) -> str:
    for k, v in PARAMS.items():
        path = path.replace(k, v)
    return path


def slug(method: str, path: str) -> str:
    s = path.removeprefix("/api/admin").strip("/") or "root"
    s = re.sub(r"\{(\w+)\}", r"\1", s)
    return f"{method.lower()}-{re.sub(r'[^a-z0-9]+', '-', s.lower()).strip('-')}"


def signin_corpus(rows: list[dict]) -> dict:
    reqs: list[dict] = []
    gets = [r for r in rows if r["method"] == "GET"]
    # Every GET signed out: 401 with signIn, or the open answer.
    for r in gets:
        reqs.append({"name": f"out-{slug('GET', r['path'])}", "path": fill(r["path"]), "session": None,
                     **COMPARE_OUT.get(r["path"], {})})
    # Unknown paths: the gate refuses them too, signed out (nothing tells a stranger what exists),
    # and they are Octo's own 404 once signed in.
    reqs += [
        {"name": "out-unknown-get", "path": "/api/admin/parity-not-a-route", "session": None},
        {"name": "out-unknown-nested", "path": "/api/admin/a/b/c", "session": None},
        {"name": "out-unknown-post", "method": "POST", "path": "/api/admin/parity-not-a-route", "session": None,
         "headers": {"X-Octo-Admin": "1"}, "body": {"json": {}}},
        {"name": "out-admin-prefix-lookalike", "path": "/api/administrator", "session": None},
        {"name": "in-unknown-get", "path": "/api/admin/parity-not-a-route"},
        {"name": "in-unknown-post", "method": "POST", "path": "/api/admin/parity-not-a-route",
         "headers": {"X-Octo-Admin": "1"}, "body": {"json": {}}},
    ]
    # Sign-in states other than a cookie.
    reqs += [
        {"name": "out-bogus-cookie", "path": "/api/admin/settings", "headers": {"Cookie": "octo_browse=not-a-session"}},
        {"name": "out-bogus-header", "path": "/api/admin/settings", "headers": {"X-Octo-Browse-Token": "not-a-session"}},
        {"name": "in-header-token", "path": "/api/admin/config-sources", "session": None,
         "headers": {"X-Octo-Browse-Token": "{{admin_session}}"}},
        {"name": "in-cookie-refreshed", "path": "/api/admin/browse/session"},
        {"name": "recovery-session", "path": "/api/admin/browse/session", "session": "recovery"},
        {"name": "recovery-settings", "path": "/api/admin/config-sources", "session": "recovery"},
        {"name": "recovery-lyrics-refused", "path": "/api/admin/lyrics/choices", "session": "recovery"},
        {"name": "recovery-wrong-code", "method": "POST", "path": "/api/admin/auth/recovery", "session": None,
         "headers": {"X-Octo-Admin": "1"}, "body": {"json": {"code": "parity-wrong-code"}}},
        {"name": "recovery-no-body", "method": "POST", "path": "/api/admin/auth/recovery", "session": None,
         "headers": {"X-Octo-Admin": "1"}, "body": {"json": {}}},
    ]
    # The calls the Octo app signs with its own Subsonic sign-in ([AdminAppCall]).
    for r in rows:
        if not r["appCall"] or r["method"] != "GET":
            continue
        base = slug("GET", r["path"])
        for auth in ("p", "t", "listener", "bad"):
            reqs.append({"name": f"app-{auth}-{base}", "path": fill(r["path"]), "session": None, "auth": auth})
    # The second Octo: sign-in off, the dashboard on a port of its own.
    reqs += [
        {"name": "alt-main-ping", "server": "alt", "path": "/rest/ping", "auth": "p", "f": "json"},
        {"name": "alt-main-admin-api", "server": "alt", "path": "/api/admin/status", "session": None},
        {"name": "alt-main-admin-page", "server": "alt", "path": "/admin/", "session": None},
        {"name": "alt-main-root", "server": "alt", "path": "/", "session": None},
        {"name": "alt-admin-ping", "server": "alt-admin", "path": "/rest/ping", "auth": "p", "f": "json"},
        {"name": "alt-admin-root", "server": "alt-admin", "path": "/", "session": None},
        {"name": "alt-admin-page", "server": "alt-admin", "path": "/admin/", "session": None},
        {"name": "alt-admin-index", "server": "alt-admin", "path": "/admin/index.html", "session": None},
        {"name": "alt-admin-session", "server": "alt-admin", "path": "/api/admin/browse/session", "session": None},
        {"name": "alt-admin-settings", "server": "alt-admin", "path": "/api/admin/config-sources", "session": None},
        {"name": "alt-admin-status", "server": "alt-admin", "path": "/api/admin/status", "session": None},
        {"name": "alt-admin-write-needs-header", "server": "alt-admin", "method": "POST",
         "path": "/api/admin/clear-metadata-cache", "session": None},
        {"name": "alt-admin-write", "server": "alt-admin", "method": "POST", "path": "/api/admin/clear-metadata-cache",
         "session": None, "headers": {"X-Octo-Admin": "1"}},
    ]
    # Every GET signed in (the features behind the gate; the groups' own files go deeper).
    for r in gets:
        if r["path"] in SIGNED_IN_SKIP:
            continue
        reqs.append({"name": f"in-{slug('GET', r['path'])}", "path": fill(r["path"]), "backlog": group_of(r["path"]),
                     **COMPARE.get(r["path"], {})})
    return {
        "group": "admin-sign-in",
        "description": ("Written by parity/tools/admin_routes.py from the C# controllers: every /api/admin GET "
                        "signed out (401 with signIn, or the open answer) and signed in, unknown admin paths, "
                        "cookie, header and recovery sign-ins, the app's Subsonic sign-in on [AdminAppCall] "
                        "routes, and the second Octo with the sign-in off and the dashboard on its own port. "
                        "Signed-out writes are in 99-signed-out-writes.json. Do not edit by hand: change the "
                        "tool and run it again."),
        "defaults": {"auth": "none", "session": "admin", "backlog": "J"},
        "skipped": {k: v for k, v in SIGNED_IN_SKIP.items()},
        "requests": reqs,
    }


def writes_corpus(rows: list[dict]) -> dict:
    reqs = []
    last = []
    for r in rows:
        if r["method"] == "GET":
            continue
        req = {"name": f"out-{slug(r['method'], r['path'])}", "method": r["method"], "path": fill(r["path"]),
               "session": None, "headers": {"X-Octo-Admin": "1"}, "body": {"json": {}}}
        (last if r["path"] == "/api/admin/restart" else reqs).append(req)
    return {
        "group": "admin-signed-out-writes",
        "description": ("Written by parity/tools/admin_routes.py: every /api/admin write signed out, with the "
                        "X-Octo-Admin header so the request guard lets it through to the sign-in gate. C# answers "
                        "401 (or the open endpoint's answer). A build without the gate would carry them out, so "
                        "this file runs last and the restart is the very last request."),
        "defaults": {"auth": "none", "backlog": "J"},
        "requests": reqs + last,
    }


def dump(path: str, doc: dict) -> None:
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("{\n")
        keys = [k for k in doc if k != "requests"]
        for k in keys:
            fh.write(f'  {json.dumps(k)}: {json.dumps(doc[k], ensure_ascii=False)},\n')
        fh.write('  "requests": [\n')
        fh.write(",\n".join("    " + json.dumps(r, ensure_ascii=False) for r in doc["requests"]))
        fh.write("\n  ]\n}\n")


def main() -> None:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    rows = routes()
    if cmd == "list":
        for r in rows:
            flags = (" open" if r["open"] else "") + (f" app:{r['appCall']}" if r["appCall"] else "")
            print(f"{r['method']:<6} {r['path']}{flags}  ({r['file']} {r['action']})")
        print(f"{len(rows)} routes", file=sys.stderr)
    elif cmd == "write":
        dump(SIGNIN, signin_corpus(rows))
        dump(WRITES, writes_corpus(rows))
        print(f"wrote {SIGNIN} and {WRITES} from {len(rows)} routes")
    elif cmd == "check":
        have = set()
        for path in (SIGNIN, WRITES):
            with open(path, encoding="utf-8") as fh:
                for req in json.load(fh)["requests"]:
                    have.add((req.get("method", "GET"), req["path"]))
        missing = [r for r in rows if (r["method"], fill(r["path"])) not in have]
        for r in missing:
            print(f"missing: {r['method']} {r['path']} ({r['file']} {r['action']})")
        if missing:
            print("run parity/tools/admin_routes.py write", file=sys.stderr)
            sys.exit(1)
        print(f"all {len(rows)} admin routes are in the sign-in corpus")
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
