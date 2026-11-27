import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

import xml.etree.ElementTree as ET
svg_root = None
try:
    tag = ET.parse("/app/result.svg").getroot().tag
    svg_root = tag.rsplit("}", 1)[-1]
except Exception as e:
    svg_root = f"unparseable: {e}"
c["result.svg parses as XML with root <svg>"] = svg_root == "svg"
c["the answer states 21"] = says_number(final_text(), "21")
size = os.path.getsize("/app/result.svg") if os.path.exists("/app/result.svg") else 0
finish(all(c.values()), {"svg root": svg_root, "svg bytes": size, "answer": final_text()[-400:]}, c)
