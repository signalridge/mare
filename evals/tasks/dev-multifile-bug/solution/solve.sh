#!/bin/bash
# reference solution (Harbor oracle agent): validates the verifier, never shown to mare
set -e
cd /app
python3 - <<'EOF'
def sub(path, old, new):
    s = open(path).read()
    assert old in s, (path, old)
    open(path, "w").write(s.replace(old, new))

sub("timesheet/validate.py", "    if end <= start:\n        raise InvalidLine(f\"end is not after start: {parts[1]!r}\")",
    "    if end == start:\n        raise InvalidLine(f\"empty span: {parts[1]!r}\")")
sub("timesheet/parse.py", "    return Entry(day, start, end, project", "    if end <= start:\n        end += dt.timedelta(days=1)  # overnight\n    return Entry(day, start, end, project")
sub("timesheet/invoice.py", "    return parse_hhmm(entry.end_text) - parse_hhmm(entry.start_text)",
    "    return (parse_hhmm(entry.end_text) - parse_hhmm(entry.start_text)) % (24 * 60)")
EOF
