#!/usr/bin/env python3
"""
Score a Kev server on the held-out dev split (kev_dev.jsonl) and optionally fit
a calibration temperature (ships nothing).

Writes every row's FULL probability distribution, so a temperature can be
applied offline: p_T ∝ p_1^(1/T) when the server ran at T=1.

Guards (both learned the hard way, docs/kev_migration_research.md §10):
  * --expect-run must match the run the server reports, so a stale server on
    the port can never be scored as a different model (the Colab step-4 bug).
  * --fit-temperature refuses unless the server reports temperature 1.0 (a fit
    on already-tempered probabilities would be relative, not absolute).

Usage:
  python3 run_kev_dev.py --expect-run jaredpalmer/kev-0.8b@v1.0 --tag stock
  python3 run_kev_dev.py --expect-run runs/gitrust-v1 --tag ft-T1 --fit-temperature
"""
import json, math, os, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))

def arg(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default

URL = arg("--url", "http://127.0.0.1:8009")
EXPECT = arg("--expect-run")
TAG = arg("--tag", "dev")
FIT = "--fit-temperature" in sys.argv

def post(body):
    req = urllib.request.Request(URL + "/v1/systemone", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=300).read())

def tempered(probs, T):
    logs = {k: math.log(max(v, 1e-12)) / T for k, v in probs.items()}
    m = max(logs.values()); z = sum(math.exp(v - m) for v in logs.values())
    return {k: math.exp(v - m) / z for k, v in logs.items()}

def nll(rows, T):
    return -sum(math.log(max(tempered(r["probs"], T)[r["gold"]], 1e-12)) for r in rows) / len(rows)

def ece(rows, T, bins=10):
    b = [[0, 0.0, 0] for _ in range(bins)]
    for r in rows:
        p = tempered(r["probs"], T); top = max(p, key=p.get)
        i = min(int(p[top] * bins), bins - 1)
        b[i][0] += 1; b[i][1] += p[top]; b[i][2] += top == r["gold"]
    return sum(abs(c - k) for n, c, k in b if n) / len(rows) if rows else 0.0

def main():
    m = json.loads(urllib.request.urlopen(URL + "/v1/models").read())["models"][0]
    print(f"server run={m['run']} temperature={m['temperature']}")
    if EXPECT and not m["run"].rstrip("/").endswith(EXPECT.rstrip("/")):
        sys.exit(f"REFUSING: server is serving {m['run']}, expected {EXPECT}")
    if FIT and abs(m["temperature"] - 1.0) > 1e-6:
        sys.exit("REFUSING: --fit-temperature needs the server at KEV_TEMPERATURE=1.0")

    rows = []; t0 = time.time()
    for line in open(os.path.join(HERE, "kev_dev.jsonl")):
        r = json.loads(line); q = r["questions"]["op"]; gold = q.pop("label")
        kind = "in_progress" if "something_else" in q["criteria"] else "routing"
        a = post({"model": "kev-latest", "state": r["state"], "questions": {"op": q}})["answers"]["op"]
        rows.append({"q": r["state"]["user_request"], "state": r["state"]["repo_state"], "kind": kind,
                     "gold": gold, "pred": a["choice"], "probs": a["probabilities"]})
    dt = time.time() - t0
    for kind in ("routing", "in_progress", None):
        rs = [r for r in rows if kind is None or r["kind"] == kind]
        ok = sum(r["pred"] == r["gold"] for r in rs)
        print(f"  {kind or 'ALL':12} {ok}/{len(rs)} = {100*ok/len(rs):.1f}%")
    print(f"  ({dt/len(rows)*1000:.0f} ms/record)")

    out = {"run": m["run"], "server_temperature": m["temperature"], "rows": rows}
    if FIT:
        grid = [0.25 * (16 ** (i / 120)) for i in range(121)]  # 0.25 .. 4, log grid (Kev's own range)
        T = min(grid, key=lambda t: nll(rows, t))
        print(f"\nfitted temperature T={T:.3f} (min dev NLL over 0.25..4)")
        for t in (1.0, T):
            print(f"  T={t:.2f}: NLL={nll(rows, t):.3f}  ECE={ece(rows, t):.3f}")
        out["fitted_temperature"] = T
    else:
        print(f"  NLL={nll(rows, 1.0):.3f} ECE={ece(rows, 1.0):.3f} (at the served temperature)")
    path = os.path.join(HERE, f"kev_dev_results_{TAG}.json")
    json.dump(out, open(path, "w"), indent=1)
    print(f"rows → {path}")

if __name__ == "__main__":
    main()
