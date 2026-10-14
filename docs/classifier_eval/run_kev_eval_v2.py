#!/usr/bin/env python3
"""
Score a Kev server on eval_set_v2.json (ships nothing) — the held-out set the
project owner wrote from situation prompts on 2026-10-14. Never train on it.

Model level, the way the binary asks:
  * normal cases → the main 35-op question, in a clean and a dirty repo state;
  * in-progress cases → the family's in-progress question (continue / skip /
    abort + something_else) in that family's state line; for a
    `something_else` gold the main question is then asked too (followup_accept).

A case is right when Kev's top option is in its `accept` list. Also reported:
the shipped policy's view (would it route at 0.5 / 0.6 destructive, refuse at
0.9, or ask?), wrong routes that survive it, and the score without phrasings
that occur verbatim in the training data ("seen").

Usage:  python3 run_kev_eval_v2.py --expect-run runs/gitrust-v1 --tag ft
"""
import json, os, sys, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from kev_ops import OPS, INSTRUCTIONS, DESTRUCTIVE, IN_PROGRESS  # noqa: E402

def arg(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default

URL = arg("--url", "http://127.0.0.1:8009")
MIN, DMIN, RMIN = 0.5, 0.6, 0.9

def post(body):
    req = urllib.request.Request(URL + "/v1/systemone", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=300).read())

def rank(q, state, instructions, criteria):
    a = post({"model": "kev-latest", "state": {"user_request": q, "repo_state": state},
              "questions": {"op": {"type": "choice", "instructions": instructions,
                                   "criteria": criteria}}})["answers"]["op"]
    return sorted(a["probabilities"].items(), key=lambda kv: -kv[1])

def policy(ranked):
    top, p = ranked[0]
    if top == "__reject__":
        return "reject" if p >= RMIN else "ask"
    return "route" if p >= (DMIN if top in DESTRUCTIVE else MIN) else "ask"

def norm(s):
    return " ".join(s.lower().strip(" .?!").replace('"', "").split())

def main():
    m = json.loads(urllib.request.urlopen(URL + "/v1/models").read())["models"][0]
    expect = arg("--expect-run")
    if expect and not m["run"].rstrip("/").endswith(expect.rstrip("/")):
        sys.exit(f"REFUSING: server is serving {m['run']}, expected {expect}")
    print(f"server run={m['run']} T={m['temperature']:.3f}")
    doc = json.load(open(os.path.join(HERE, "eval_set_v2.json")))
    seen = set()
    for f in ("kev_train.jsonl", "kev_dev.jsonl"):
        for line in open(os.path.join(HERE, f)):
            seen.add(norm(json.loads(line)["state"]["user_request"]))

    rows = []
    for c in doc["cases"]:
        states = (["normal_clean", "normal_dirty"] if c["situation"] == "normal"
                  else [c["situation"]])
        for sk in states:
            state = doc["states"][sk]
            if c["situation"] == "normal":
                r = rank(c["q"], state, INSTRUCTIONS, OPS)
                follow = None
            else:
                fam = IN_PROGRESS["families"][c["situation"]]
                crit = dict(fam); crit[IN_PROGRESS["other"][0]] = IN_PROGRESS["other"][1]
                r = rank(c["q"], state, IN_PROGRESS["instructions"], crit)
                follow = rank(c["q"], state, INSTRUCTIONS, OPS) if "something_else" in c["accept"] else None
            top, p = r[0]
            ok = top in c["accept"]
            if follow is not None:
                ok = ok and follow[0][0] in doc["followup_accept"][c["id"]]
            pol = policy(r)
            rows.append({"id": c["id"], "q": c["q"], "state": sk, "bucket": c["bucket"],
                         "accept": c["accept"], "top": top, "p": round(p, 3),
                         "top2": r[1][0], "p2": round(r[1][1], 3), "ok": ok, "policy": pol,
                         "follow": follow[0][0] if follow else None,
                         "seen_in_training": norm(c["q"]) in seen})
            flag = "✓" if ok else "✗"
            print(f"  {flag} {c['id']} {sk:12} {c['q'][:52]!r:56} → {top} {p:.2f} [{pol}]"
                  + (f" then {follow[0][0]}" if follow else "")
                  + ("" if ok else f"   (want {'/'.join(c['accept'])})"))

    def score(rs):
        return f"{sum(r['ok'] for r in rs)}/{len(rs)}"
    print()
    for sk in ("normal_clean", "normal_dirty"):
        rs = [r for r in rows if r["state"] == sk and r["bucket"] != "gap"]
        unseen = [r for r in rs if not r["seen_in_training"]]
        routed = [r for r in rs if r["policy"] == "route"]
        print(f"{sk:13} top-1 {score(rs)}  (unseen-only {score(unseen)})  "
              f"policy: route {len(routed)} ({sum(r['ok'] for r in routed)} right), "
              f"ask {sum(r['policy']=='ask' for r in rs)}, reject {sum(r['policy']=='reject' for r in rs)}")
        wrong = [r for r in routed if not r["ok"]] + \
                [r for r in rs if r["policy"] == "reject" and not r["ok"]]
        for r in wrong:
            print(f"     WRONG {r['policy']}: {r['q']!r} → {r['top']} {r['p']}")
        buckets = sorted({r["bucket"] for r in rs})
        print("     by bucket: " + "  ".join(f"{b} {score([r for r in rs if r['bucket']==b])}" for b in buckets))
    ip = [r for r in rows if r["bucket"] == "in_progress"]
    print(f"in-progress   top-1 {score(ip)}  (unseen-only {score([r for r in ip if not r['seen_in_training']])})")
    gap = [r for r in rows if r["bucket"] == "gap"]
    print("gap (commit+push, no single op): " + ", ".join(f"{r['state'][7:]}:{r['top']} {r['p']}" for r in gap))

    tag = arg("--tag", "v2")
    out = os.path.join(HERE, f"kev_results_v2_{tag}.json")
    json.dump({"run": m["run"], "temperature": m["temperature"], "rows": rows}, open(out, "w"), indent=1)
    print(f"rows → {out}")

if __name__ == "__main__":
    main()
