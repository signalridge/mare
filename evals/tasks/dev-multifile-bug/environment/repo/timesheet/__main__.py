import sys

from . import invoice, report
from .money import format_cents
from .parse import parse_lines
from .validate import InvalidLine, check_line


def main(argv=None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) < 2 or argv[0] not in ("check", "report", "invoice"):
        print(__doc__ or "usage: python -m timesheet check|report|invoice LOG [RATES]", file=sys.stderr)
        return 2
    cmd, log = argv[0], open(argv[1]).read().splitlines()
    try:
        if cmd == "check":
            for n, line in enumerate(log, 1):
                try:
                    check_line(line)
                except InvalidLine as e:
                    print(f"line {n}: {e}")
                    return 1
            print("ok")
        elif cmd == "report":
            print(report.render(parse_lines(log)))
        else:
            rates = invoice.parse_rates(open(argv[2]).read().splitlines())
            for p, cents in invoice.amounts(parse_lines(log), rates).items():
                print(f"{p:<10} {format_cents(cents)}")
    except InvalidLine as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
