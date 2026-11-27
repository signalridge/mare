The current directory (`/app`) is `timesheet`, a small Python package that turns time logs into reports and invoices (see `README.md`; tests run with `python -m pytest`). A user filed this bug:

> **Overnight shifts break everything.** Our release night was logged as
> `2026-03-02   22:00-02:00  acme      release night` (it is in `sample.log`).
> `python -m timesheet check sample.log` says `line 4: end is not after start`, and `report` and `invoice` refuse the whole file.
> A span whose end time is earlier than its start time means the work ended the next day, so this line is 4 hours of work.

Fix it so that overnight spans work everywhere:

- `check` accepts them (a span whose end equals its start is still invalid);
- the parsed `Entry` has `end` on the next day (for the line above: `start` 2026-03-02 22:00, `end` 2026-03-03 02:00, `minutes` 240, `day` 2026-03-02);
- `report` counts the whole span on the day the work started;
- `invoice` bills the right number of minutes.

With the fix, `python -m timesheet report sample.log` shows 11.00h on 2026-03-02 and 1.25h on 2026-03-03, and `python -m timesheet invoice sample.log rates.txt` shows `$900.00` for acme and `$427.50` for globex. Keep the existing behavior and tests otherwise unchanged; they must still pass. Adding a regression test is welcome.
