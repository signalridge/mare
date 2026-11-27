"""dev-multifile-bug: hidden overnight regression tests plus the original suite (pristine copies),
both run from outside /app against the agent's code."""

import filecmp, os, shutil, sys

sys.path.insert(0, "/tests")
from mare_log import Log, all_pass, common_checks, finish, run_pytest

APP = "/app"
WORK = "/tmp/verify"

log = Log()
c = common_checks(log)
shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK)
shutil.copytree("/tests/hidden", f"{WORK}/hidden")
shutil.copytree("/tests/orig", f"{WORK}/orig")
hidden = run_pytest([f"{WORK}/hidden", f"--rootdir={WORK}/hidden"], WORK, APP, "hidden")
orig = run_pytest([f"{WORK}/orig", f"--rootdir={WORK}/orig"], WORK, APP, "original")
c["hidden regression tests pass"] = all_pass(hidden, 11)
c["original tests still pass"] = all_pass(orig, 8)
c["sample.log and rates.txt unchanged"] = all(
    os.path.exists(f"{APP}/{f}") and filecmp.cmp(f"/tests/fixtures/{f}", f"{APP}/{f}", shallow=False)
    for f in ("sample.log", "rates.txt")
)
details = {
    "hidden": f"{hidden['passed']}/{hidden['total']}",
    "original": f"{orig['passed']}/{orig['total']}",
}
finish(all(c.values()), details, c)
