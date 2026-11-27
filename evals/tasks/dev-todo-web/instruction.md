Build a small to-do web application in the current directory (`/app`, empty right now). Any language and framework is fine, but nothing is installed for you later: the image has Python 3.12 (standard library); whatever else you use must be installed by you now, and the app must run offline.

Create an executable `/app/start.sh` that starts the server in the foreground. It must listen on `0.0.0.0` port `8000`. Keep the to-dos in memory (no persistence needed).

HTTP API (all request and response bodies are JSON with `Content-Type: application/json`, except `GET /` and the 204 response):

| Method and path | Request body | Success | Errors |
|---|---|---|---|
| `GET /api/todos` | – | `200`, a JSON array of all to-dos in creation order | – |
| `POST /api/todos` | `{"title": "..."}` | `201`, the new to-do | `400` when `title` is missing, not a string, or empty after trimming |
| `PATCH /api/todos/<id>` | any of `{"title": "...", "done": true/false}` | `200`, the updated to-do | `404` unknown id; `400` for an empty title or a non-boolean `done` |
| `DELETE /api/todos/<id>` | – | `204`, empty body | `404` unknown id |

A to-do is `{"id": <integer>, "title": "<string>", "done": <boolean>}`. Ids are positive integers, unique, and never reused; a new to-do has `"done": false`. Titles are stored trimmed.

The page (`GET /` returns `200` with `Content-Type: text/html`) must contain:

- `<input id="new-todo">` for the title of a new to-do,
- `<button id="add-todo">` that creates it through `POST /api/todos`,
- `<ul id="todo-list">` filled from `GET /api/todos`, one `<li data-id="<id>">` per to-do, each with a checkbox (`<input type="checkbox" class="toggle">`) that sends `PATCH` with `done`, and a button with class `delete` that sends `DELETE`,
- `<span id="todo-count">` showing how many to-dos are not done.

The JavaScript may be inline or in a separate file served by the same server (`<script src="...">`); it must use the `/api/todos` endpoints. Check your server with real requests before you finish, then stop any server you started.
