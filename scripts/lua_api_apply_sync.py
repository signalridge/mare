"""Static lint forbidding yielding calls and async wrappers inside built-in plugin `apply`."""

from __future__ import annotations

import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lua_api_runner import ROOT, ast_grep_matches, isolated_temp_dir

_BUILTIN_APPLY_YIELDING_CALLS = frozenset(
    {
        "coroutine.yield",
        "cordis.await",
        "cordis.tick",
        "mare.task.sleep",
        "mare.task.yield",
        "mare.task.select",
        "mare.task.timeout",
        "mare.fs.read",
        "mare.fs.write",
        "mare.fs.list",
        "mare.fs.stat",
        "mare.fs.mkdir",
        "mare.fs.remove",
        "mare.fs.git_dir",
        "mare.fs.watch",
        "mare.proc.run",
        "mare.http.request",
        "mare.http.stream",
        "ui.select",
        "ui.confirm",
        "ui.input",
        "ui.ask",
        "mare.ui.select",
        "mare.ui.confirm",
        "mare.ui.input",
        "mare.ui.ask",
    }
)

# Strictly shrinking static lint exceptions for built-in plugin entry files whose `apply` still
# reaches a yielding API within the same file (directly or via an intra-file helper).
#
# Relationship with `DEFERRED_IO_BUILTINS` in
# `crates/mare-cli/tests/suite/plugins/builtin_apply_budgets.rs`:
# - This table covers static single-file yields.
# - `DEFERRED_IO_BUILTINS` covers cross-module yields observed at runtime during full boot.
#
# Both lists are now empty (0 exceptions) and enforce a one-way ratchet: any new entry or stale
# exception fails the lint/test.
_BUILTIN_APPLY_YIELD_EXCEPTIONS: dict[tuple[str, str], int] = {}

_CALLEE_PREFIX_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_.:]*(?:\s*\.\s*[A-Za-z_][A-Za-z0-9_]*)*)")
_SIMPLE_IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_LOCAL_FN_NAME_RE = re.compile(
    r"^(?:local\s+function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(|local\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*function\s*\()"
)


def _unwrap_parens(expr: str) -> str:
    expr = expr.strip()
    while expr.startswith("(") and expr.endswith(")"):
        depth = 0
        wrapped = True
        for index, ch in enumerate(expr):
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0 and index < len(expr) - 1:
                    wrapped = False
                    break
        if not wrapped or depth != 0:
            break
        expr = expr[1:-1].strip()
    return expr


def _call_callee(call_text: str) -> str:
    head = call_text.lstrip()
    if head.startswith("("):
        depth = 0
        for idx, ch in enumerate(head):
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0:
                    return _unwrap_parens(head[: idx + 1])
    match = _CALLEE_PREFIX_RE.match(head)
    if not match:
        return ""
    return re.sub(r"\s+", "", match.group(1))


def _yielding_api_name(callee: str) -> str | None:
    if callee in _BUILTIN_APPLY_YIELDING_CALLS:
        return callee
    if callee.endswith(":await"):
        return ":await()"
    return None


def _containing_fn(
    span: tuple[int, int],
    all_fns: list[tuple[int, int]],
) -> tuple[int, int] | None:
    """Return the innermost function span strictly containing `span`, or None if top-level."""
    s_start, s_end = span
    enclosing = [fn for fn in all_fns if fn != span and fn[0] <= s_start and s_end <= fn[1]]
    if not enclosing:
        return None
    return min(enclosing, key=lambda fn: fn[1] - fn[0])


def builtin_apply_no_yield_violations(
    files: list[Path],
    root: Path = ROOT,
    exceptions: dict[tuple[str, str], int] | None = None,
) -> list[str]:
    """Forbid yielding calls (direct or via intra-file helpers) and async wrappers in built-in `apply`."""
    if not files:
        return []

    active_exceptions = _BUILTIN_APPLY_YIELD_EXCEPTIONS if exceptions is None else exceptions

    def run_pattern(pattern: str, selector: str | None = None) -> list[dict]:
        command = ["ast-grep", "run", "--lang", "lua", "--pattern", pattern, "--json=compact"]
        if selector:
            command += ["--selector", selector]
        result = subprocess.run(
            [*command, *map(str, files)],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        return ast_grep_matches(result.returncode, result.stdout, result.stderr, "builtin apply yield scanner")

    def scan_inline(rules_yaml: str) -> list[dict]:
        result = subprocess.run(
            ["ast-grep", "scan", "--inline-rules", rules_yaml, "--json=compact", *map(str, files)],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        return ast_grep_matches(result.returncode, result.stdout, result.stderr, "builtin apply AST scanner")

    def rel_key(raw_file: str) -> str:
        path = Path(raw_file)
        path = path if path.is_absolute() else root / path
        return path.relative_to(root).as_posix()

    scanned_rels = {rel_key(str(path)) for path in files}
    errors: list[tuple[tuple[str, int, int], str]] = []
    seen_pos: set[tuple[str, int, int]] = set()
    exception_counts: Counter[tuple[str, str]] = Counter()

    # 1. `apply = cordis.async(...)` (or `cordis.future.AsyncFunction.spawn(...)`) is forbidden.
    for pattern in (
        "return { apply = cordis.async($$$ARGS) }",
        "return { apply = cordis.future.AsyncFunction.spawn($$$ARGS) }",
    ):
        for match in run_pattern(pattern, "field"):
            rel = rel_key(match["file"])
            start = match["range"]["start"]
            key = (rel, start["line"], start["column"])
            if key in seen_pos:
                continue
            seen_pos.add(key)
            errors.append(
                (
                    key,
                    (
                        f"{rel}:{start['line'] + 1}:{start['column'] + 1}: builtin-apply-no-yield: "
                        f"built-in plugin `apply` must be synchronous (do not wrap with async)"
                    ),
                )
            )

    # 2. Collect `apply = function(...) ... end` byte spans.
    apply_field_spans: dict[str, list[tuple[int, int]]] = {}
    for match in run_pattern("return { apply = function($$$ARGS) $$$BODY end }", "field"):
        rel = rel_key(match["file"])
        offset = match["range"]["byteOffset"]
        apply_field_spans.setdefault(rel, []).append((offset["start"], offset["end"]))

    if apply_field_spans:
        # 3. Collect all function spans and named local helper declarations/assignments.
        fn_spans: dict[str, list[tuple[int, int]]] = {}
        named_fn_decls: dict[str, list[tuple[str, tuple[int, int]]]] = {}
        for match in scan_inline(
            "id: fn-def\nlanguage: lua\nrule:\n  any:\n    - kind: function_definition\n    - kind: function_declaration\n"
        ):
            rel = rel_key(match["file"])
            if rel not in apply_field_spans:
                continue
            offset = match["range"]["byteOffset"]
            span = (offset["start"], offset["end"])
            fn_spans.setdefault(rel, []).append(span)
            m_name = _LOCAL_FN_NAME_RE.match(match.get("text", "").lstrip())
            if m_name:
                name = m_name.group(1) or m_name.group(2)
                named_fn_decls.setdefault(rel, []).append((name, span))

        for match in run_pattern("local $NAME = function($$$ARGS) $$$BODY end"):
            rel = rel_key(match["file"])
            if rel not in apply_field_spans:
                continue
            m_name = _LOCAL_FN_NAME_RE.match(match.get("text", "").lstrip())
            if not m_name:
                continue
            name = m_name.group(1) or m_name.group(2)
            offset = match["range"]["byteOffset"]
            decl_start, decl_end = offset["start"], offset["end"]
            inner_fns = [span for span in fn_spans.get(rel, ()) if decl_start <= span[0] and span[1] <= decl_end]
            if inner_fns:
                outer_fn = max(inner_fns, key=lambda s: s[1] - s[0])
                named_fn_decls.setdefault(rel, []).append((name, outer_fn))

        # Resolve each `apply` field to its own `function_definition` span.
        apply_fn_spans: dict[str, list[tuple[int, int]]] = {}
        for rel, field_spans in apply_field_spans.items():
            all_fns = fn_spans.get(rel, [])
            resolved: list[tuple[int, int]] = []
            for a_start, a_end in field_spans:
                own = [span for span in all_fns if a_start <= span[0] and span[1] <= a_end]
                if own:
                    resolved.append(max(own, key=lambda s: s[1] - s[0]))
            if resolved:
                apply_fn_spans[rel] = resolved

        # Index tracked helpers: top-level helpers and helpers declared directly inside an `apply`.
        helpers_by_scope: dict[tuple[str, tuple[int, int] | None, str], tuple[int, int]] = {}
        helper_info: dict[tuple[str, tuple[int, int]], tuple[tuple[int, int] | None, str]] = {}
        for rel, decls in named_fn_decls.items():
            all_fns = fn_spans.get(rel, [])
            apply_set = set(apply_fn_spans.get(rel, ()))
            for name, fn_span in decls:
                parent = _containing_fn(fn_span, all_fns)
                if parent is None or parent in apply_set:
                    helpers_by_scope[(rel, parent, name)] = fn_span
                    helper_info[(rel, fn_span)] = (parent, name)

        # 4. Collect all direct calls inside each `apply` and each tracked helper.
        direct_calls: dict[tuple[str, tuple[int, int]], list[dict]] = {}
        tracked_fns: set[tuple[str, tuple[int, int]]] = set(helper_info.keys())
        for rel, applies in apply_fn_spans.items():
            for a_span in applies:
                tracked_fns.add((rel, a_span))

        for match in scan_inline("id: fn-call\nlanguage: lua\nrule:\n  kind: function_call\n"):
            rel = rel_key(match["file"])
            if rel not in apply_fn_spans:
                continue
            offset = match["range"]["byteOffset"]
            c_span = (offset["start"], offset["end"])
            owner_fn = _containing_fn(c_span, fn_spans.get(rel, []))
            if owner_fn is None or (rel, owner_fn) not in tracked_fns:
                continue
            direct_calls.setdefault((rel, owner_fn), []).append(match)

        # 5. Propagate yielding reachability across intra-file helpers (fixed-point).
        helper_yields: dict[tuple[str, tuple[int, int]], str] = {}
        helper_edges: dict[tuple[str, tuple[int, int]], list[tuple[str, tuple[int, int]]]] = {}
        for key, (scope, _name) in helper_info.items():
            rel, _fn_span = key
            for call in direct_calls.get(key, ()):
                callee = _call_callee(call.get("text", ""))
                api = _yielding_api_name(callee)
                if api is not None:
                    helper_yields.setdefault(key, api)
                elif _SIMPLE_IDENT_RE.match(callee):
                    target = helpers_by_scope.get((rel, scope, callee))
                    if target is None and scope is not None:
                        target = helpers_by_scope.get((rel, None, callee))
                    if target is not None:
                        helper_edges.setdefault(key, []).append((rel, target))

        changed = True
        while changed:
            changed = False
            for h_key, targets in helper_edges.items():
                if h_key in helper_yields:
                    continue
                for t_key in targets:
                    if t_key in helper_yields:
                        helper_yields[h_key] = helper_yields[t_key]
                        changed = True
                        break

        # 6. Flag yielding calls (direct or via a yielding helper) directly inside each `apply`.
        for rel, applies in apply_fn_spans.items():
            for a_span in applies:
                for call in direct_calls.get((rel, a_span), ()):
                    callee = _call_callee(call.get("text", ""))
                    matched_api = _yielding_api_name(callee)
                    via_helper: str | None = None
                    if matched_api is None and _SIMPLE_IDENT_RE.match(callee):
                        target = helpers_by_scope.get((rel, a_span, callee))
                        if target is None:
                            target = helpers_by_scope.get((rel, None, callee))
                        if target is not None and (rel, target) in helper_yields:
                            matched_api = helper_yields[(rel, target)]
                            via_helper = callee
                    if matched_api is None:
                        continue

                    exc_key = (rel, matched_api)
                    allowed = active_exceptions.get(exc_key, 0)
                    if allowed > 0:
                        exception_counts[exc_key] += 1
                        if exception_counts[exc_key] <= allowed:
                            continue

                    start = call["range"]["start"]
                    pos = (rel, start["line"], start["column"])
                    if pos in seen_pos:
                        continue
                    seen_pos.add(pos)
                    if via_helper is not None:
                        reason = (
                            f"built-in plugin `apply` must not call yielding helper `{via_helper}` "
                            f"(reaches `{matched_api}`); defer async work to `ctx:spawn` or an event handler"
                        )
                    else:
                        reason = (
                            f"built-in plugin `apply` must not call yielding API `{matched_api}` directly; "
                            f"defer async work to `ctx:spawn` or an event handler"
                        )
                    errors.append((pos, f"{rel}:{start['line'] + 1}:{start['column'] + 1}: builtin-apply-no-yield: {reason}"))

    # 7. Fail when any exception for a scanned file is stale (observed fewer times than allowed).
    for (exc_file, exc_api), allowed in sorted(active_exceptions.items()):
        if exc_file not in scanned_rels:
            continue
        observed = exception_counts.get((exc_file, exc_api), 0)
        if observed < allowed:
            errors.append(
                (
                    (exc_file, 0, 0),
                    (
                        f"{exc_file}:1:1: builtin-apply-no-yield: stale exception for `{exc_api}` "
                        f"(allowed {allowed}, observed {observed}); tighten or remove it from "
                        f"_BUILTIN_APPLY_YIELD_EXCEPTIONS"
                    ),
                )
            )

    return [message for _, message in sorted(errors)]


def self_test_builtin_apply_no_yield(base_dir: Path | None = None) -> None:
    """Built-in plugin apply functions must not call yielding APIs (directly or via helpers)."""
    with isolated_temp_dir("mare-builtin-apply-no-yield-test-", base_dir=base_dir) as directory:
        root = Path(directory)
        probe = root / "crates/mare-plugin/preset/plugins/probe.lua"
        async_probe = root / "crates/mare-plugin/preset/plugins/async_probe.lua"
        helper_probe = root / "crates/mare-plugin/preset/plugins/helper_probe.lua"
        clean_probe = root / "crates/mare-plugin/preset/plugins/clean_probe.lua"
        probe.parent.mkdir(parents=True, exist_ok=True)
        probe.write_text(
            "local function top_level_ok()\n"
            "  mare.task.sleep(1)\n"
            "end\n"
            "return {\n"
            '  name = "probe",\n'
            "  apply = function(ctx)\n"
            "    coroutine.yield()\n"  # 7: violation
            "    cordis.await(fut)\n"  # 8: violation
            "    cordis.tick(host)\n"  # 9: violation
            "    fut:await()\n"  # 10: violation
            "    mare.task.sleep(1)\n"  # 11: violation
            "    mare.task.yield()\n"  # 12: violation
            '    mare.fs.read("a.txt")\n'  # 13: violation
            '    mare.proc.run({ "echo" })\n'  # 14: violation
            "    mare.http.request({})\n"  # 15: violation
            "    ui.select({})\n"  # 16: violation
            "    mare.ui.confirm({})\n"  # 17: violation
            "    -- mare.task.sleep(1)\n"  # 18: comment ok
            '    local s = "mare.fs.read(x)"\n'  # 19: string ok
            '    ctx:on("ev", function()\n'
            '      mare.fs.read("inside-handler.txt")\n'  # 21: nested closure ok
            "      mare.task.sleep(1)\n"  # 22: nested closure ok
            "    end)\n"
            "    ctx:spawn(function()\n"
            '      mare.proc.run({ "git" })\n'  # 25: nested closure ok
            "      fut:await()\n"  # 26: nested closure ok
            "    end)\n"
            "    local function helper()\n"
            "      coroutine.yield()\n"  # 29: nested function ok when not called synchronously
            "    end\n"
            "    return helper, s, top_level_ok\n"
            "  end,\n"
            "}\n"
        )
        async_probe.write_text(
            "return {\n"
            '  name = "async-probe",\n'
            "  apply = cordis.async(function(ctx)\n"  # 3: violation
            "    return ctx\n"
            "  end),\n"
            "}\n"
        )
        helper_probe.write_text(
            "local function leaf_io()\n"
            '  return mare.fs.read("x")\n'
            "end\n"
            "local wrap_io = function()\n"
            "  return leaf_io()\n"
            "end\n"
            "return {\n"
            '  name = "helper-probe",\n'
            "  apply = function(ctx)\n"
            "    local function inner_wait()\n"
            "      mare.task.sleep(1)\n"
            "    end\n"
            "    wrap_io()\n"  # 13: violation via transitive top-level helper
            "    inner_wait()\n"  # 14: violation via local helper inside apply
            "    ctx:spawn(function()\n"
            "      wrap_io()\n"  # 16: ok inside spawned closure
            "      inner_wait()\n"  # 17: ok inside spawned closure
            "    end)\n"
            "  end,\n"
            "}\n"
        )
        clean_probe.write_text(
            "return {\n"
            '  name = "clean-probe",\n'
            "  apply = function() end,\n"
            "}\n"
        )
        violations = builtin_apply_no_yield_violations(
            [probe, async_probe, helper_probe, clean_probe], root=root, exceptions={}
        )
        actual = [f"{Path(err.split(':')[0]).name}:{err.split(':')[1]}" for err in violations]
        expected = [
            "async_probe.lua:3",
            "helper_probe.lua:13",
            "helper_probe.lua:14",
            *(f"probe.lua:{line}" for line in (7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17)),
        ]
        assert actual == expected, f"expected {expected}, got {actual} ({violations})"

        # Stale exception on a scanned file must fail so exceptions ratchet strictly downward.
        stale_violations = builtin_apply_no_yield_violations(
            [clean_probe],
            root=root,
            exceptions={("crates/mare-plugin/preset/plugins/clean_probe.lua", "mare.fs.read"): 1},
        )
        assert len(stale_violations) == 1 and "stale exception" in stale_violations[0], stale_violations
