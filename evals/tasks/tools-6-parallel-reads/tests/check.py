import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

text = final_text()
batch = log.max_batch("read")
c["first lines reported"] = all(s in text for s in ("alpha first", "bravo first", "charlie first"))
# the parallel batch is reported (the rate across attempts), not a failure condition
finish(all(c.values()), {"max reads in one message": batch, "parallel": batch >= 3}, c)
