import re

_HHMM = re.compile(r"^([01]\d|2[0-3]):([0-5]\d)$")


def parse_hhmm(text: str) -> int:
    """"09:30" -> 570 (minutes since midnight). Raises ValueError on bad input."""
    m = _HHMM.match(text)
    if not m:
        raise ValueError(f"bad time {text!r}")
    return int(m.group(1)) * 60 + int(m.group(2))


def split_span(span: str) -> tuple[str, str]:
    """"09:00-12:30" -> ("09:00", "12:30")."""
    start, sep, end = span.partition("-")
    if not sep:
        raise ValueError(f"bad span {span!r}")
    return start, end
