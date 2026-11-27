import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

reads = log.named("read")
c["the read of missing.txt failed"] = any("missing.txt" in x["raw"] and (log.result(x) or {}).get("is_error") for x in reads)
c["data/real.txt was read"] = any("real.txt" in x["raw"] and log.result(x) and not log.result(x)["is_error"] for x in reads)
c["the answer states 42"] = says_number(final_text(), "42")
finish(all(c.values()), {"answer": final_text()[-200:]}, c)
