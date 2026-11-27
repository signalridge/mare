#!/bin/bash
# reference solution (Harbor oracle agent): validates the verifier, never shown to mare
set -e
cd /app
cat > server.py <<'EOF'
import json, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TODOS, LOCK, NEXT = [], threading.Lock(), [1]
PAGE = open("/app/index.html", "rb").read()


class H(BaseHTTPRequestHandler):
    def send(self, code, obj=None, ctype="application/json"):
        body = b"" if obj is None else (obj if isinstance(obj, bytes) else json.dumps(obj).encode())
        self.send_response(code)
        if code != 204:
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        try:
            v = json.loads(self.rfile.read(n) or b"null")
        except ValueError:
            return None
        return v if isinstance(v, dict) else None

    def find(self):
        try:
            i = int(self.path.rsplit("/", 1)[1])
        except ValueError:
            return None
        return next((t for t in TODOS if t["id"] == i), None)

    def do_GET(self):
        if self.path == "/":
            return self.send(200, PAGE, "text/html; charset=utf-8")
        if self.path == "/api/todos":
            return self.send(200, TODOS)
        self.send(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/api/todos":
            return self.send(404, {"error": "not found"})
        b = self.body() or {}
        t = b.get("title")
        if not isinstance(t, str) or not t.strip():
            return self.send(400, {"error": "title required"})
        with LOCK:
            todo = {"id": NEXT[0], "title": t.strip(), "done": False}
            NEXT[0] += 1
            TODOS.append(todo)
        self.send(201, todo)

    def do_PATCH(self):
        todo = self.find() if self.path.startswith("/api/todos/") else None
        if todo is None:
            return self.send(404, {"error": "not found"})
        b = self.body() or {}
        if "title" in b and (not isinstance(b["title"], str) or not b["title"].strip()):
            return self.send(400, {"error": "bad title"})
        if "done" in b and not isinstance(b["done"], bool):
            return self.send(400, {"error": "bad done"})
        if "title" in b:
            todo["title"] = b["title"].strip()
        if "done" in b:
            todo["done"] = b["done"]
        self.send(200, todo)

    def do_DELETE(self):
        todo = self.find() if self.path.startswith("/api/todos/") else None
        if todo is None:
            return self.send(404, {"error": "not found"})
        TODOS.remove(todo)
        self.send(204)


ThreadingHTTPServer(("0.0.0.0", 8000), H).serve_forever()
EOF
cat > index.html <<'EOF'
<!doctype html>
<html><head><meta charset="utf-8"><title>To-do</title></head>
<body>
<input id="new-todo" placeholder="What needs doing?"> <button id="add-todo">Add</button>
<ul id="todo-list"></ul>
<p><span id="todo-count">0</span> left</p>
<script>
const api = (m, p, b) => fetch(p, {method: m, headers: {"Content-Type": "application/json"},
  body: b === undefined ? undefined : JSON.stringify(b)});
async function load() {
  const todos = await (await fetch("/api/todos")).json();
  const ul = document.getElementById("todo-list");
  ul.innerHTML = "";
  for (const t of todos) {
    const li = document.createElement("li");
    li.setAttribute("data-id", t.id);
    li.innerHTML = '<input type="checkbox" class="toggle"> <span></span> <button class="delete">x</button>';
    li.querySelector("span").textContent = t.title;
    const cb = li.querySelector(".toggle");
    cb.checked = t.done;
    cb.onchange = () => api("PATCH", "/api/todos/" + t.id, {done: cb.checked}).then(load);
    li.querySelector(".delete").onclick = () => api("DELETE", "/api/todos/" + t.id).then(load);
    ul.appendChild(li);
  }
  document.getElementById("todo-count").textContent = todos.filter(t => !t.done).length;
}
document.getElementById("add-todo").onclick = async () => {
  const i = document.getElementById("new-todo");
  if ((await api("POST", "/api/todos", {title: i.value})).ok) i.value = "";
  load();
};
load();
</script>
</body></html>
EOF
printf '#!/bin/sh\nexec python3 /app/server.py\n' > start.sh
chmod +x start.sh
