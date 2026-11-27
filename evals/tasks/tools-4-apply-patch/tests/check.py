import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

one = any(all(s in x["raw"] for s in ("Add File: new.txt", "Update File: keep.txt", "Delete File: remove.txt")) for x in log.named("apply_patch"))
rd = lambda p: open(p).read().strip() if os.path.exists(p) else None
c["one apply_patch call did all three"] = one
c["new.txt == hello"] = rd("/app/new.txt") == "hello"
c["keep.txt == new"] = rd("/app/keep.txt") == "new"
c["remove.txt deleted"] = not os.path.exists("/app/remove.txt")
finish(all(c.values()), {}, c)
