#!/usr/bin/env python3
"""
Throwaway evaluation harness (ships nothing): can a Kev decision model
(github.com/jaredpalmer/kev, a local Jev-compatible "System One" model) do
GiTrust's Layer-1 routing better than the two engines measured in the paper?

Same 44-case labeled set as run_eval.py, same strict labels, fast-path bypassed.
Kev is asked ONE `choice` question per query: "which Git operation is this?",
over every op the router knows PLUS an explicit "not a Git request" option. Kev
returns a calibrated probability per option, so besides top-1 accuracy we can
measure the thing neither earlier engine had: does its confidence separate
right answers from wrong ones well enough to ABSTAIN safely?

Reports:
  * top-1 accuracy overall + by bucket (directly comparable to 41% / 64%)
  * out-of-scope rejection (LLM 4/4, embeddings 0/4)
  * selective routing: accuracy/coverage at confidence thresholds, and how many
    DESTRUCTIVE misroutes survive each threshold (the safety-bucket finding)
  * latency (server-reported model time + wall clock)
  * memory: kev server RSS and system free % (watchdog aborts below a floor,
    the guard the paper added after the 4B OOM crash)

Option descriptions describe each op's SEMANTICS; they deliberately do not copy
eval-set queries (the LLM prompt does contain a few, so if anything this is
stricter on Kev than on the LLM).

Usage:  python3 run_kev_eval.py [--url http://127.0.0.1:8009] [--tag name]
                                [--with-ollama]   # keep qwen2.5:3b resident too
                                [--permute]       # also measure option-order sensitivity
                                [--repo-state "on branch main, 2 untracked files"]
                                [--expect-run runs/gitrust-v1]   # refuse if another model answers
Requires: a running `python -m kev.serve`. Pure stdlib.
"""
import json, os, re, statistics, subprocess, sys, threading, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
EVAL = json.load(open(os.path.join(HERE, "eval_set.json")))["cases"]
OLLAMA = "http://localhost:11434"

def arg(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default

URL = arg("--url", "http://127.0.0.1:8009")
TAG = arg("--tag", "kev")
# The repo-state line Kev sees. Confidence is sensitive to it (a dirty tree
# lowered "show me the history" from 0.71 to 0.38), so run both.
REPO_STATE = arg("--repo-state", "clean repo, on branch main")
FREE_FLOOR = 10  # % system free memory; two consecutive readings below → abort

from kev_ops import OPS, DESTRUCTIVE, INSTRUCTIONS

# ------------------------------------------------------------------ plumbing
def post(url, body, timeout=300):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())

def free_pct():
    out = subprocess.run(["memory_pressure"], capture_output=True, text=True).stdout
    m = re.search(r"free percentage:\s*(\d+)%", out)
    return int(m.group(1)) if m else 100

def rss_mb(pattern):
    """Sum of macOS phys_footprint (what Activity Monitor calls Memory) over
    matching processes. RSS undercounts MLX/Metal allocations badly."""
    pids = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True).stdout.split()
    total = 0
    for p in pids:
        out = subprocess.run(["footprint", p], capture_output=True, text=True).stdout
        m = re.search(r"phys_footprint:\s*([\d.]+)\s*(KB|MB|GB)", out)
        if m:
            total += float(m.group(1)) * {"KB": 1/1024, "MB": 1, "GB": 1024}[m.group(2)]
    return int(total)

class Watchdog(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True)
        self.min_free, self.max_rss, self.tripped, self.stop = 100, 0, False, False
    def run(self):
        low = 0
        while not self.stop:
            f = free_pct(); self.min_free = min(self.min_free, f)
            self.max_rss = max(self.max_rss, rss_mb("kev.serve"))
            low = low + 1 if f < FREE_FLOOR else 0
            if low >= 2:
                self.tripped = True
                print(f"\n!! WATCHDOG: free memory {f}% < {FREE_FLOOR}% twice — aborting", flush=True)
                os._exit(3)
            time.sleep(1)

def ask(q, endpoint="/v1/systemone", extra=None):
    body = {"model": "kev-latest",
            "state": {"user_request": q, "repo_state": REPO_STATE},
            "questions": {"op": {"type": "choice", "instructions": INSTRUCTIONS,
                                 "criteria": OPS}}}
    if extra: body.update(extra)
    t = time.time(); r = post(URL + endpoint, body); wall = time.time() - t
    a = r["answers"]["op"]
    probs = a.get("probabilities", {})
    ranked = sorted(probs.items(), key=lambda kv: -kv[1])
    return {"pred": a.get("choice"), "conf": a.get("confidence", 0.0),
            "p1": ranked[0][1] if ranked else 0.0,
            "top2": ranked[1][0] if len(ranked) > 1 else None,
            "p2": ranked[1][1] if len(ranked) > 1 else 0.0,
            "model_ms": r.get("latency_ms"), "wall_ms": wall * 1000}

# ------------------------------------------------------------------ main
def main():
    info = json.loads(urllib.request.urlopen(URL + "/v1/models").read())
    m0 = info["models"][0]
    print(f"server: run={m0['run']} temperature={m0['temperature']:.3f} backend={m0.get('backend')}")
    expect = arg("--expect-run")
    if expect and not m0["run"].rstrip("/").endswith(expect.rstrip("/")):
        sys.exit(f"REFUSING: server is serving {m0['run']}, expected {expect}")
    if "--with-ollama" in sys.argv:
        # Make qwen2.5:3b resident at the production context, as GiTrust would.
        post(OLLAMA + "/api/generate", {"model": "qwen2.5:3b", "prompt": "hi", "stream": False,
                                        "keep_alive": "30m", "options": {"num_ctx": 2048, "num_predict": 1}})
        print("qwen2.5:3b loaded alongside (num_ctx 2048)")

    wd = Watchdog(); wd.start()
    ask("show status")  # warm-up, excluded
    rows = []
    for c in EVAL:
        r = ask(c["q"]); r.update(c); rows.append(r)
        mark = "ok  " if r["pred"] == c["op"] else "MISS"
        print(f"  [{mark}] {c['q'][:40]:42} exp={c['op']:14} got={r['pred']:14} "
              f"p={r['p1']:.2f} conf={r['conf']:.2f} 2nd={r['top2']}({r['p2']:.2f}) {r['model_ms']}ms", flush=True)

    n = len(rows); ok = sum(r["pred"] == r["op"] for r in rows)
    print(f"\n=== {TAG}: top-1 routing {ok}/{n} = {100*ok/n:.0f}%   (paper: LLM 41%, embeddings 64%)")
    buckets = {}
    for r in rows:
        b = buckets.setdefault(r["bucket"], [0, 0]); b[1] += 1; b[0] += r["pred"] == r["op"]
    for bk, (o, t) in sorted(buckets.items()):
        print(f"  {bk:16} {o}/{t}")

    # confidence separation
    cor = [r["p1"] for r in rows if r["pred"] == r["op"]]
    wr = [r["p1"] for r in rows if r["pred"] != r["op"]]
    st = lambda xs: tuple(round(f(xs), 2) for f in (min, statistics.mean, max)) if xs else ()
    print(f"\ntop-prob correct   min/mean/max = {st(cor)}")
    print(f"top-prob incorrect min/mean/max = {st(wr)}")

    # selective routing: route only if p1 >= t, else abstain (→ next layer / gate)
    print("\nselective routing (route if top prob >= t, else abstain):")
    print("  t     coverage  acc-on-routed  destructive-misroutes-surviving")
    for t in (0.0, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9):
        routed = [r for r in rows if r["p1"] >= t]
        acc = sum(r["pred"] == r["op"] for r in routed)
        dmis = [r for r in routed if r["pred"] != r["op"] and
                (r["pred"] in DESTRUCTIVE or r["bucket"] == "safety")]
        print(f"  {t:.1f}   {len(routed):2}/{n}    {acc}/{len(routed) or 1:<3} "
              f"({100*acc/max(len(routed),1):3.0f}%)   {len(dmis)}"
              + (f"  {[(r['q'], r['pred']) for r in dmis]}" if dmis and t >= 0.5 else ""))

    ms = sorted(r["model_ms"] for r in rows if r["model_ms"] is not None)
    wall = sorted(r["wall_ms"] for r in rows)
    print(f"\nlatency: model median {ms[len(ms)//2]} ms (max {ms[-1]}), wall median {wall[len(wall)//2]:.0f} ms"
          f"   (paper: LLM ~9.8 s, embeddings ~283 ms)")
    wd.stop = True
    print(f"memory: kev server peak RSS {wd.max_rss} MB, min system free {wd.min_free}%, "
          f"ollama RSS now {rss_mb('ollama')} MB")

    if "--permute" in sys.argv:
        # Same question under 6 shuffled option orders. A router that flips its
        # answer when options are reordered is guessing, not deciding.
        print("\noption-order sensitivity (/v1/systemone/permute, n_perm=6):")
        unstable = []
        for c in EVAL:
            req = {"model": "kev-latest",
                   "state": {"user_request": c["q"], "repo_state": REPO_STATE},
                   "questions": {"op": {"type": "choice", "instructions": INSTRUCTIONS, "criteria": OPS}}}
            r = post(URL + "/v1/systemone/permute", {"request": req, "question": "op", "n_perm": 6})
            picks = [x["choice"] for x in r["runs"]]
            if not r["argmax_stable"]:
                unstable.append((c["q"], c["op"], sorted(set(picks))))
        print(f"  argmax stable on {n - len(unstable)}/{n} queries")
        for q, exp, picks in unstable:
            print(f"  UNSTABLE {q[:40]:42} exp={exp:14} picks={picks}")

    out = os.path.join(HERE, f"kev_results_{TAG}.json")
    json.dump(rows, open(out, "w"), indent=1)
    print(f"\nrows → {out}")

if __name__ == "__main__":
    main()
