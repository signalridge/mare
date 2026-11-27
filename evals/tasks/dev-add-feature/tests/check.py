"""dev-add-feature: hidden coupon tests, the original suite (pristine copies), and the agent's own
tests (they must pass on the new code and fail on the original commit)."""

import filecmp
import os
import shutil
import subprocess
import sys

sys.path.insert(0, "/tests")
from mare_log import Log, all_pass, common_checks, finish, run_pytest

APP = "/app"
WORK = "/tmp/verify"

log = Log()
c = common_checks(log)
shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK)

# hidden tests and the original suite run from copies outside /app (no agent conftest, no edits)
shutil.copytree("/tests/hidden", f"{WORK}/hidden")
shutil.copytree("/tests/orig", f"{WORK}/orig")
hidden = run_pytest([f"{WORK}/hidden", f"--rootdir={WORK}/hidden"], WORK, APP, "hidden")
orig = run_pytest([f"{WORK}/orig", f"--rootdir={WORK}/orig"], WORK, APP, "original")
c["hidden coupon tests pass"] = all_pass(hidden, 13)
c["original tests still pass"] = all_pass(orig, 10)
c["original test files unchanged"] = all(
    filecmp.cmp(f"/tests/orig/{f}", f"{APP}/tests/{f}", shallow=False)
    if os.path.exists(f"{APP}/tests/{f}")
    else False
    for f in os.listdir("/tests/orig")
)

# the agent's tests: present, at least 5, green on the new code ...
mine = f"{APP}/tests/test_coupons.py"
c["tests/test_coupons.py exists"] = os.path.isfile(mine)
own = (
    run_pytest(["tests/test_coupons.py"], APP, APP, "own")
    if os.path.isfile(mine)
    else None
)
c["own tests: >= 5, all pass"] = bool(own) and all_pass(own, 5)

# ... and red on the original (root) commit: they exercise the feature
before = None
if os.path.isfile(mine):
    base = f"{WORK}/base"
    os.makedirs(base)
    arch = subprocess.run(
        f"git -c safe.directory='*' -C {APP} archive $(git -c safe.directory='*' -C {APP} rev-list --max-parents=0 HEAD | tail -1) | tar -x -C {base}", shell=True, capture_output=True
    )
    if arch.returncode == 0 and os.path.isdir(f"{base}/shopcart"):
        os.makedirs(f"{base}/tests", exist_ok=True)
        shutil.copyfile(mine, f"{base}/tests/test_coupons.py")
        before = run_pytest(
            ["tests/test_coupons.py", f"--rootdir={base}"],
            base,
            base,
            "own-on-original",
        )
c["own tests fail on the original code"] = bool(before) and (
    before["failed"] + before["errors"] > 0 or before["rc"] != 0
)

details = {
    "hidden": f"{hidden['passed']}/{hidden['total']}",
    "original": f"{orig['passed']}/{orig['total']}",
    "own": f"{own['passed']}/{own['total']}" if own else "-",
}
finish(all(c.values()), details, c)
