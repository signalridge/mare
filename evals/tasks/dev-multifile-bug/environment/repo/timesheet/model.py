import datetime as dt
from dataclasses import dataclass


@dataclass(frozen=True)
class Entry:
    day: dt.date            # the day the work started
    start: dt.datetime
    end: dt.datetime
    project: str
    note: str = ""

    @property
    def start_text(self) -> str:
        return self.start.strftime("%H:%M")

    @property
    def end_text(self) -> str:
        return self.end.strftime("%H:%M")

    @property
    def minutes(self) -> int:
        return int((self.end - self.start).total_seconds() // 60)
