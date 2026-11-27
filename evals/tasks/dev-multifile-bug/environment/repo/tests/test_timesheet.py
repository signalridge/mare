import datetime as dt

import pytest

from timesheet import InvalidLine, check_line, parse_hhmm, parse_line, parse_lines
from timesheet import invoice, report

LOG = """\
# comment
2026-03-02 09:00-12:30 acme kickoff
2026-03-02 13:30-17:00 globex

2026-03-03 10:00-11:15 globex review   # trailing comment
""".splitlines()


def test_parse_hhmm():
    assert parse_hhmm("00:00") == 0
    assert parse_hhmm("09:30") == 570
    assert parse_hhmm("23:59") == 1439
    for bad in ("24:00", "9:30", "09:60", "x"):
        with pytest.raises(ValueError):
            parse_hhmm(bad)


def test_parse_line():
    e = parse_line("2026-03-02 09:00-12:30 acme kickoff meeting")
    assert e.day == dt.date(2026, 3, 2)
    assert e.start == dt.datetime(2026, 3, 2, 9, 0)
    assert e.end == dt.datetime(2026, 3, 2, 12, 30)
    assert (e.project, e.note, e.minutes) == ("acme", "kickoff meeting", 210)
    assert parse_line("   # nothing") is None


@pytest.mark.parametrize(
    "line",
    ["2026-03-02 09:00 acme", "2026-13-02 09:00-10:00 acme", "2026-03-02 09:00-25:00 acme", "2026-03-02 09:00-10:00"],
)
def test_check_line_rejects(line):
    with pytest.raises(InvalidLine):
        check_line(line)


def test_report():
    entries = parse_lines(LOG)
    assert report.daily_minutes(entries) == {dt.date(2026, 3, 2): 420, dt.date(2026, 3, 3): 75}
    assert report.project_minutes(entries) == {"acme": 210, "globex": 285}
    assert "2026-03-02  7.00h" in report.render(entries)


def test_invoice():
    entries = parse_lines(LOG)
    rates = invoice.parse_rates(["acme 12000", "globex 9000  # discounted"])
    assert invoice.amounts(entries, rates) == {"acme": 42000, "globex": 42750}
