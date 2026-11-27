import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

edits = [x for x in log.calls if x["name"] in ("edit", "write")]
refused = any((log.result(x) or {}).get("is_error") and "not_observed" in (log.result(x) or {}).get("text", "") for x in edits)
then_ok = any(log.result(x) and not log.result(x)["is_error"] for x in edits)
content = open("/app/notes.txt").read()
c["an edit was refused with not_observed"] = refused
c["an edit succeeded afterwards"] = then_ok
c["notes.txt changed"] = content == "name=box\ncolor=blue\n"
finish(all(c.values()), {"content": content}, c)
