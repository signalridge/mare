import datetime as dt
import subprocess
import sys

import pytest

from timesheet import InvalidLine, check_line, parse_line, parse_lines
from timesheet import invoice, report

NIGHT = "2026-03-02 22:00-02:00 acme release night"


def test_check_accepts_overnight():
    check_line(NIGHT)
    check_line("2026-03-02 23:59-00:00 acme")


@pytest.mark.parametrize("line", ["2026-03-02 09:00-09:00 acme", "2026-03-02 00:00-00:00 acme"])
def test_check_still_rejects_zero_length(line):
    with pytest.raises(InvalidLine):
        check_line(line)


def test_parse_overnight_entry():
    e = parse_line(NIGHT)
    assert e.day == dt.date(2026, 3, 2)
    assert e.start == dt.datetime(2026, 3, 2, 22, 0)
    assert e.end == dt.datetime(2026, 3, 3, 2, 0)
    assert e.minutes == 240
    assert e.note == "release night"


def test_parse_until_midnight():
    e = parse_line("2026-12-31 23:30-00:00 acme")
    assert e.end == dt.datetime(2027, 1, 1, 0, 0)
    assert e.minutes == 30


def test_daytime_unchanged():
    e = parse_line("2026-03-02 09:00-12:30 acme")
    assert e.end == dt.datetime(2026, 3, 2, 12, 30)
    assert e.minutes == 210


def test_report_attributes_to_start_day():
    entries = parse_lines([NIGHT, "2026-03-03 10:00-11:15 globex"])
    assert report.daily_minutes(entries) == {dt.date(2026, 3, 2): 240, dt.date(2026, 3, 3): 75}
    assert report.project_minutes(entries) == {"acme": 240, "globex": 75}


def test_invoice_overnight():
    entries = parse_lines([NIGHT, "2026-03-05 23:45-00:20 globex"])
    assert invoice.billable_minutes(entries[0]) == 240
    assert invoice.billable_minutes(entries[1]) == 35
    assert invoice.amounts(entries, {"acme": 12000, "globex": 9000}) == {"acme": 48000, "globex": 5250}


def run(*args):
    return subprocess.run([sys.executable, "-m", "timesheet", *args], cwd="/app", capture_output=True, text=True)


def test_cli_check_sample():
    p = run("check", "sample.log")
    assert p.returncode == 0 and "ok" in p.stdout


def test_cli_report_sample():
    p = run("report", "sample.log")
    assert p.returncode == 0
    assert "2026-03-02  11.00h" in p.stdout
    assert "2026-03-03  1.25h" in p.stdout


def test_cli_invoice_sample():
    p = run("invoice", "sample.log", "rates.txt")
    assert p.returncode == 0
    assert "$900.00" in p.stdout and "$427.50" in p.stdout
