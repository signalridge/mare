import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

amb = any((log.result(x) or {}).get("is_error") and "ambiguous_edit" in (log.result(x) or {}).get("text", "") for x in log.named("edit"))
content = open("/app/config.txt").read()
c["an edit returned ambiguous_edit"] = amb
c["only the second occurrence changed"] = content == "[a]\nenabled=false\n[b]\nenabled=true\n"
finish(all(c.values()), {"content": content}, c)
