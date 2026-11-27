import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

content = open("/app/task.txt").read() if os.path.exists("/app/task.txt") else None
c["task.txt is value=after"] = content == "value=after\n"
c["read before and after"] = len(log.named("read")) >= 2
finish(all(c.values()), {"content": content}, c)
