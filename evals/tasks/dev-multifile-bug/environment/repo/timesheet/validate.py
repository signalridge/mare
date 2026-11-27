import datetime as dt

from .clock import parse_hhmm, split_span


class InvalidLine(ValueError):
    pass


def check_line(line: str) -> None:
    """Raise InvalidLine when a log line is malformed. Blank lines and comments are fine."""
    body = line.split("#", 1)[0].strip()
    if not body:
        return
    parts = body.split()
    if len(parts) < 3:
        raise InvalidLine(f"expected '<date> <start>-<end> <project>': {line!r}")
    try:
        dt.date.fromisoformat(parts[0])
    except ValueError:
        raise InvalidLine(f"bad date {parts[0]!r}") from None
    try:
        start, end = (parse_hhmm(t) for t in split_span(parts[1]))
    except ValueError as e:
        raise InvalidLine(str(e)) from None
    if end <= start:
        raise InvalidLine(f"end is not after start: {parts[1]!r}")
