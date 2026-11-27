import datetime as dt

from .clock import split_span
from .model import Entry
from .validate import check_line


def parse_line(line: str) -> Entry | None:
    """One log line -> Entry (None for blank lines and comments)."""
    check_line(line)
    body = line.split("#", 1)[0].strip()
    if not body:
        return None
    date_s, span, project, *note = body.split()
    day = dt.date.fromisoformat(date_s)
    start_s, end_s = split_span(span)
    start = dt.datetime.combine(day, dt.time.fromisoformat(start_s))
    end = dt.datetime.combine(day, dt.time.fromisoformat(end_s))
    return Entry(day, start, end, project, " ".join(note))


def parse_lines(lines) -> list[Entry]:
    return [e for e in (parse_line(l) for l in lines) if e is not None]
