"""dev-todo-web: start the agent's server with /app/start.sh, exercise the CRUD API over HTTP and
check the agreed page elements. Standard library only."""

import json
import os
import re
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from html.parser import HTMLParser

sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, finish

PORT = 8000
BASE = f"http://127.0.0.1:{PORT}"


def listeners(port):
    """pids holding a listening socket on `port` (from /proc, no lsof/fuser in the image)."""
    inodes = set()
    for f in ("/proc/net/tcp", "/proc/net/tcp6"):
        try:
            for line in open(f).read().splitlines()[1:]:
                p = line.split()
                if p[3] == "0A" and int(p[1].rsplit(":", 1)[1], 16) == port:
                    inodes.add(p[9])
        except OSError:
            pass
    pids = set()
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            for fd in os.listdir(f"/proc/{pid}/fd"):
                link = os.readlink(f"/proc/{pid}/fd/{fd}")
                if link.startswith("socket:[") and link[8:-1] in inodes:
                    pids.add(int(pid))
        except OSError:
            pass
    return pids - {os.getpid()}


def free_port():
    for pid in listeners(PORT):
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    time.sleep(0.5)


def wait_port(timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", PORT), timeout=1).close()
            return True
        except OSError:
            time.sleep(0.3)
    return False


def req(method, path, body=None):
    """(status, content-type, parsed JSON or raw text)"""
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(BASE + path, data=data, method=method)
    if data is not None:
        r.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=10) as resp:
            status, ctype, raw = (
                resp.status,
                resp.headers.get("Content-Type", ""),
                resp.read(),
            )
    except urllib.error.HTTPError as e:
        status, ctype, raw = e.code, e.headers.get("Content-Type", ""), e.read()
    except Exception as e:  # connection refused, timeout, malformed response
        return None, "", repr(e)
    text = raw.decode("utf-8", "replace")
    try:
        return status, ctype, json.loads(text) if text.strip() else None
    except ValueError:
        return status, ctype, text


def is_todo(t, title=None, done=None):
    return (
        isinstance(t, dict)
        and isinstance(t.get("id"), int)
        and not isinstance(t.get("id"), bool)
        and t["id"] > 0
        and isinstance(t.get("title"), str)
        and isinstance(t.get("done"), bool)
        and (title is None or t["title"] == title)
        and (done is None or t["done"] is done)
    )


class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.tags = []  # (tag, attrs dict)
        self.scripts = []  # inline script text
        self.srcs = []
        self._in_script = False

    def handle_starttag(self, tag, attrs):
        a = {k: (v or "") for k, v in attrs}
        self.tags.append((tag, a))
        if tag == "script":
            self._in_script = True
            if a.get("src"):
                self.srcs.append(a["src"])

    def handle_endtag(self, tag):
        if tag == "script":
            self._in_script = False

    def handle_data(self, data):
        if self._in_script:
            self.scripts.append(data)

    def has(self, tag, **want):
        return any(
            t == tag and all(a.get(k) == v for k, v in want.items())
            for t, a in self.tags
        )


log = Log()
c = common_checks(log)
details = {}

start = "/app/start.sh"
os.makedirs("/logs/verifier", exist_ok=True)
c["start.sh exists"] = os.path.isfile(start)
free_port()  # a server the agent left running would answer for start.sh otherwise
server = None
if c["start.sh exists"]:
    server = subprocess.Popen(
        ["bash", start],
        cwd="/app",
        stdout=open("/logs/verifier/server.log", "w"),
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
c["server listens on 8000 within 30s"] = c["start.sh exists"] and wait_port()

api = {}
if c["server listens on 8000 within 30s"]:
    s, ct, body = req("GET", "/api/todos")
    api["empty list"] = s == 200 and body == [] and "application/json" in ct
    s, ct, a = req("POST", "/api/todos", {"title": "  buy milk  "})
    api["POST returns 201 and the to-do (trimmed, done false)"] = (
        s == 201 and is_todo(a, "buy milk", False) and "application/json" in ct
    )
    s, _, b = req("POST", "/api/todos", {"title": "write report"})
    api["second POST"] = s == 201 and is_todo(b, "write report", False)
    ida = a["id"] if is_todo(a) else 10**6
    idb = b["id"] if is_todo(b) else 10**6 + 1
    api["ids are distinct"] = ida != idb
    s, _, body = req("GET", "/api/todos")
    api["list has both in creation order"] = (
        s == 200
        and isinstance(body, list)
        and [t.get("id") for t in body if isinstance(t, dict)] == [ida, idb]
    )
    s, _, _ = req("POST", "/api/todos", {"title": "   "})
    s2, _, _ = req("POST", "/api/todos", {})
    s3, _, _ = req("POST", "/api/todos", {"title": 5})
    api["POST rejects empty/missing/non-string title with 400"] = (s, s2, s3) == (
        400,
        400,
        400,
    )
    s, _, body = req("PATCH", f"/api/todos/{ida}", {"done": True})
    api["PATCH done"] = s == 200 and is_todo(body, "buy milk", True)
    s, _, body = req("PATCH", f"/api/todos/{ida}", {"title": "buy oat milk"})
    api["PATCH title keeps done"] = s == 200 and is_todo(body, "buy oat milk", True)
    s, _, _ = req("PATCH", f"/api/todos/{ida}", {"done": "yes"})
    s2, _, _ = req("PATCH", f"/api/todos/{ida}", {"title": ""})
    api["PATCH rejects bad done/empty title with 400"] = (s, s2) == (400, 400)
    s, _, _ = req("PATCH", "/api/todos/999999", {"done": True})
    api["PATCH unknown id is 404"] = s == 404
    s, _, body = req("GET", "/api/todos")
    api["list reflects the update"] = (
        s == 200
        and isinstance(body, list)
        and len(body) == 2
        and is_todo(body[0], "buy oat milk", True)
    )
    s, _, body = req("DELETE", f"/api/todos/{idb}")
    api["DELETE returns 204 with empty body"] = s == 204 and body is None
    s, _, _ = req("DELETE", f"/api/todos/{idb}")
    api["DELETE again is 404"] = s == 404
    s, _, c3 = req("POST", "/api/todos", {"title": "third"})
    api["ids are not reused"] = s == 201 and is_todo(c3) and c3["id"] not in (ida, idb)
    s, _, body = req("GET", "/api/todos")
    api["final list"] = (
        s == 200
        and isinstance(body, list)
        and [t.get("title") for t in body if isinstance(t, dict)]
        == ["buy oat milk", "third"]
    )

    s, ct, html = req("GET", "/")
    page_ok = s == 200 and "text/html" in ct and isinstance(html, str)
    api["GET / is text/html"] = page_ok
    p = Page()
    if page_ok:
        p.feed(html)
    js = "\n".join(p.scripts)
    for src in p.srcs:
        if re.match(r"^(https?:)?//", src):
            continue  # external scripts are not ours to check
        s2, _, text = req("GET", "/" + src.lstrip("./").lstrip("/"))
        if s2 == 200 and isinstance(text, str):
            js += "\n" + text
    api["page: <input id=new-todo>"] = p.has("input", id="new-todo")
    api["page: <button id=add-todo>"] = p.has("button", id="add-todo")
    api["page: <ul id=todo-list>"] = p.has("ul", id="todo-list")
    api["page: <span id=todo-count>"] = p.has("span", id="todo-count")
    api["page script uses /api/todos"] = "/api/todos" in js
    api["page script renders data-id, toggle and delete"] = all(
        k in js for k in ("data-id", "toggle", "delete")
    ) or ("dataset.id" in js and "toggle" in js and "delete" in js)
    api["page script sends POST, PATCH and DELETE"] = all(
        m in js for m in ("POST", "PATCH", "DELETE")
    )

if server is not None:
    try:
        os.killpg(server.pid, signal.SIGKILL)
    except OSError:
        pass
free_port()

c.update(api)
details["api_checks_passed"] = f"{sum(api.values())}/{len(api)}" if api else "0/0"
finish(all(c.values()), details, c)
