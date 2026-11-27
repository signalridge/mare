import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

bash = log.named("bash")
sep = len(bash) >= 2 and any("b.txt" in x["raw"] and "cd " not in x["raw"] and "sub" not in x["raw"] for x in bash)
c["b.txt written by a separate call without cd or path"] = sep
c["sub/a.txt and sub/b.txt exist, ./b.txt doesn't"] = os.path.exists("/app/sub/a.txt") and os.path.exists("/app/sub/b.txt") and not os.path.exists("/app/b.txt")
finish(all(c.values()), {}, c)
