import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

bg = any("background job" in (log.result(x) or {}).get("text", "") for x in log.named("bash"))
content = open("/app/slow.txt").read().strip() if os.path.exists("/app/slow.txt") else None
c["the command moved to the background (job id in a result)"] = bg
c["slow.txt == done"] = content == "done"
finish(all(c.values()), {"content": content}, c)
