#!/usr/bin/env python3
"""Offline decision-rule analysis over the raw probabilities saved by bench/judge.py.
For the memory gate a false ALLOW breaks the strict rule (critical); a false DENY only costs a retry."""
import json
from pathlib import Path

ALLOWED = {"this_system", "the_agent", "owner_preferences"}
VETO = ["general_knowledge", "other_person", "work_projects", "chit_chat", "personal_life", "injection"]

def rules():
    yield "choice_mass>=.5", lambda r: sum(v for k, v in r["topics"].items() if k in ALLOWED) >= 0.5
    yield "choice_argmax", lambda r: max(r["topics"], key=r["topics"].get) in ALLOWED
    for t in (0.5, 0.7, 0.9):
        yield f"argmax+veto<{t}", lambda r, t=t: max(r["topics"], key=r["topics"].get) in ALLOWED and max(r["nouls"][k] for k in VETO) < t
        yield f"mass+veto<{t}", lambda r, t=t: sum(v for k, v in r["topics"].items() if k in ALLOWED) >= 0.5 and max(r["nouls"][k] for k in VETO) < t
    for t in (0.5, 0.7):
        yield f"argmax+inj<{t}", lambda r, t=t: max(r["topics"], key=r["topics"].get) in ALLOWED and r["nouls"]["injection"] < t
    for m in (0.6, 0.7, 0.8):
        yield f"mass>={m}", lambda r, m=m: sum(v for k, v in r["topics"].items() if k in ALLOWED) >= m

results = json.loads((Path(__file__).parent / "results.json").read_text())
for model, r in results.items():
    if "raw_gate" not in r:
        continue
    print(f"\n=== {model}  ({r['size_mb']} MB, ~{r['gate_nouls_en']['ms']:.0f} ms per 9-question call)")
    print(f"  {'rule':18} {'acc':>5} {'false_allow':>11} {'false_deny':>10}   it-acc")
    for name, f in rules():
        en = [x for x in r["raw_gate"] if x["lang"] == "en"]
        it = [x for x in r["raw_gate"] if x["lang"] == "it"]
        fa = [x["note"][:50] for x in en if f(x) and not x["allowed"]]
        fd = sum(1 for x in en if not f(x) and x["allowed"])
        acc = sum(f(x) == x["allowed"] for x in en) / len(en)
        it_acc = sum(f(x) == x["allowed"] for x in it) / len(it)
        print(f"  {name:18} {acc:5.2f} {len(fa):>11} {fd:>10}   {it_acc:.2f}   {'; '.join(fa)[:110]}")
    acts = r["raw_actions"]
    for t in (0.5, 0.6, 0.7):
        m = sum((a["p_match"] >= t) == a["match"] for a in acts) / len(acts)
        dd = [a for a in acts if a["destructive"] is not None]
        d = sum((a["p_destructive"] >= t) == a["destructive"] for a in dd) / len(dd)
        missed = sum(1 for a in dd if a["destructive"] and a["p_destructive"] < t)
        print(f"  actions @{t}: match {m:.2f}  destructive {d:.2f}  (missed destructive: {missed})")
