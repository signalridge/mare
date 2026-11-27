import json, os, sys
sys.path.insert(0, "/tests")
from mare_log import Log, common_checks, final_text, finish, says_number

log = Log()
c = common_checks(log)

import jsonschema
schema = json.load(open("/app/output_schema.json"))
text = open("/app/model_answer.json").read() if os.path.exists("/app/model_answer.json") else ""
try:
    answer = json.loads(text)
    jsonschema.validate(answer, schema)
    valid = True
except Exception:
    answer, valid = None, False
c["model_answer.json matches output_schema.json"] = valid
# "the model actually read question.png": the read tool, or a command that opens it (e.g. PIL)
png_calls = [x["name"] for x in log.calls if "question.png" in x["raw"]]
c["a tool call read question.png"] = bool(png_calls)
# scores are reported only (the free model's ability varies)
gold = json.load(open("/tests/pompeii-gold.json"))["gold_edges"]
norm = lambda a, b: (a, b) if a <= b else (b, a)
g = {norm(a, b) for a, b in gold}
p = {norm(e["a"], e["b"]) for e in (answer or {}).get("edges", []) if isinstance(e, dict) and "a" in e and "b" in e}
hit = len(p & g)
prec = hit / len(p) if p else 0.0
rec = hit / len(g) if g else 0.0
f1 = 2 * prec * rec / (prec + rec) if prec + rec else 0.0
finish(all(c.values()), {"edges": len(p), "precision": round(prec, 3), "recall": round(rec, 3), "f1": round(f1, 3),
                         "placeholder untouched": text.strip() == '{"edges": []}', "question.png read via": sorted(set(png_calls))}, c)
