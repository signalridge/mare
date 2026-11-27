# timesheet

Turns plain-text time logs into daily reports and invoices.

```
# date       span         project   [note]
2026-03-02   09:00-12:30  acme      kickoff
2026-03-02   13:30-17:00  globex
```

```
python -m timesheet check   log.txt              # validate every line
python -m timesheet report  log.txt              # hours per day and per project
python -m timesheet invoice log.txt rates.txt    # amount per project (rates: "<project> <cents per hour>")
```

Run the tests with `python -m pytest`.
