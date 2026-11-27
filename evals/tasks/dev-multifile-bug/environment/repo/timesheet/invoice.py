from collections import defaultdict

from .clock import parse_hhmm


def parse_rates(lines) -> dict:
    """"<project> <cents per hour>" lines -> {project: cents_per_hour}."""
    rates = {}
    for line in lines:
        body = line.split("#", 1)[0].strip()
        if body:
            project, cents = body.split()
            rates[project] = int(cents)
    return rates


def billable_minutes(entry) -> int:
    # billed on the clock times as written in the log (seconds are never billed)
    return parse_hhmm(entry.end_text) - parse_hhmm(entry.start_text)


def amounts(entries, rates) -> dict:
    """{project: cents}, rounded half up to whole cents per project. Unknown projects: KeyError."""
    minutes = defaultdict(int)
    for e in entries:
        minutes[e.project] += billable_minutes(e)
    return {p: (m * rates[p] + 30) // 60 for p, m in sorted(minutes.items())}
