from collections import defaultdict


def daily_minutes(entries) -> dict:
    """{date: minutes}, attributed to the day the work started."""
    out = defaultdict(int)
    for e in entries:
        out[e.day] += e.minutes
    return dict(sorted(out.items()))


def project_minutes(entries) -> dict:
    out = defaultdict(int)
    for e in entries:
        out[e.project] += e.minutes
    return dict(sorted(out.items()))


def hours(minutes: int) -> str:
    return f"{minutes / 60:.2f}"


def render(entries) -> str:
    lines = ["by day:"]
    lines += [f"  {d.isoformat()}  {hours(m)}h" for d, m in daily_minutes(entries).items()]
    lines.append("by project:")
    lines += [f"  {p:<10} {hours(m)}h" for p, m in project_minutes(entries).items()]
    return "\n".join(lines)
