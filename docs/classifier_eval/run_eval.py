#!/usr/bin/env python3
"""
Throwaway evaluation harness (ships nothing) for the classifier-routing research.

Compares, on the SAME labeled phrasing set:
  A) the EMBEDDING router  — nearest-exemplar by cosine over qwen2.5:3b /api/embed
  B) the LLM baseline      — the REAL classifier prompt from prompts.rs via /api/generate

Reports: per-engine accuracy (overall + by bucket), latency, out-of-scope
rejection, and the margin distribution (correct vs incorrect) so we can see
whether a margin threshold is trustworthy. Also tries mean-centering the
embeddings (a standard fix for decoder-model embeddings) and re-measures.

Usage:  python3 run_eval.py            # both engines
        python3 run_eval.py --embed    # embeddings only (fast, no LLM)
Requires: Ollama running with qwen2.5:3b. Pure stdlib.
"""
import json, math, re, sys, time, urllib.request, os

HERE = os.path.dirname(os.path.abspath(__file__))
OLLAMA = "http://localhost:11434"
MODEL = os.environ.get("EVAL_MODEL", "qwen2.5:3b")
PROMPTS_RS = os.path.join(HERE, "..", "..", "gitrust", "src", "prompts.rs")

# ---------------------------------------------------------------- ollama calls
def _post(path, body):
    req = urllib.request.Request(OLLAMA + path,
        data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=120).read())

def embed(text):
    return _post("/api/embed", {"model": MODEL, "input": text})["embeddings"][0]

# ---------------------------------------------------------------- vector math
def cos(a, b):
    d = sum(x*y for x, y in zip(a, b))
    na = math.sqrt(sum(x*x for x in a)); nb = math.sqrt(sum(y*y for y in b))
    return d / (na*nb + 1e-9)

def mean_vec(vs):
    n = len(vs); return [sum(v[i] for v in vs)/n for i in range(len(vs[0]))]

def sub(a, m): return [x-y for x, y in zip(a, m)]

# ---------------------------------------------------------------- load data
def load(name):
    with open(os.path.join(HERE, name)) as f:
        return json.load(f)

EVAL = load("eval_set.json")["cases"]
EXEMPLARS = load("exemplars.json")["ops"]

# ---------------------------------------------------------------- prompt (real)
def read_rust_raw_string(var):
    """Pull a `pub static VAR ... = ... r#"..."#` block out of prompts.rs."""
    src = open(PROMPTS_RS).read()
    i = src.index(var)
    start = src.index('r#"', i) + 3
    end = src.index('"#', start)
    return src[start:end]

SYSTEM_PROMPT = read_rust_raw_string("SYSTEM_PROMPT")
CLASSIFIER_PROMPT = read_rust_raw_string("CLASSIFIER_PROMPT")

CLASSIFIER_SCHEMA = {
    "type": "object",
    "properties": {
        "reasoning": {"type": "string", "minLength": 10, "maxLength": 200},
        "intent": {"type": "string", "enum": ["read_only", "scoped_write", "feature_pipeline", "clarify"]},
        "op": {"type": "string"},
        "feature_hint": {"type": "string"},
        "count": {"type": "integer"},
        "confidence": {"type": "number"},
        "question": {"type": "string"},
    },
    "required": ["reasoning", "intent", "confidence"],
}

def llm_classify(query):
    prompt = f"{CLASSIFIER_PROMPT}\n\nCurrent repo state:\n(clean repo, on branch main)\n\nUser query: {query}"
    body = {"model": MODEL, "prompt": prompt, "system": SYSTEM_PROMPT,
            "format": CLASSIFIER_SCHEMA, "stream": False,
            "options": {"temperature": 0.0, "top_k": 1, "top_p": 0.1, "num_ctx": 2048, "num_predict": 512}}
    t = time.time()
    resp = _post("/api/generate", body)
    dt = time.time() - t
    try:
        ri = json.loads(resp["response"])
    except Exception:
        return {"op": "__parse_error__", "conf": 0.0}, dt
    return {"intent": ri.get("intent", ""), "op": ri.get("op", ""),
            "conf": float(ri.get("confidence", 0.0))}, dt

# Map the LLM's (intent, op, conf) onto OUR label vocabulary so the comparison
# is fair. This mirrors build_intent_type's routing decisions.
def llm_to_label(r):
    if r["op"] == "__parse_error__":
        return "__parse_error__"
    if r["conf"] < 0.6 or r["intent"] == "clarify":
        return "__reject__"
    op = r["op"]
    if r["intent"] == "feature_pipeline" and op == "push":
        return "scoped_push"
    # feature_pipeline sync -> sync ; else fall through to op name
    return op if op else "__reject__"

# ---------------------------------------------------------------- embed router
def build_router(center=False):
    # embed every exemplar; optionally mean-center against the whole exemplar cloud
    op_ex = {op: [embed(p) for p in ph] for op, ph in EXEMPLARS.items()}
    mean = None
    if center:
        allv = [v for vs in op_ex.values() for v in vs]
        mean = mean_vec(allv)
        op_ex = {op: [sub(v, mean) for v in vs] for op, vs in op_ex.items()}
    return op_ex, mean

def route(query, op_ex, mean):
    qv = embed(query)
    if mean is not None:
        qv = sub(qv, mean)
    scored = sorted(((max(cos(qv, v) for v in vs), op) for op, vs in op_ex.items()), reverse=True)
    (s1, top1), (s2, top2) = scored[0], scored[1]
    return top1, s1, top2, s2, s1 - s2

# ---------------------------------------------------------------- eval loop
def evaluate(center):
    op_ex, mean = build_router(center=center)
    rows = []
    for c in EVAL:
        top1, s1, top2, s2, margin = route(c["q"], op_ex, mean)
        rows.append({**c, "pred": top1, "s1": s1, "margin": margin})
    return rows

def acc(rows, pred_key="pred"):
    ok = sum(1 for r in rows if r[pred_key] == r["op"])
    return ok, len(rows)

def by_bucket(rows, pred_key="pred"):
    b = {}
    for r in rows:
        bk = r["bucket"]; b.setdefault(bk, [0, 0])
        b[bk][1] += 1
        if r[pred_key] == r["op"]:
            b[bk][0] += 1
    return b

# ---------------------------------------------------------------- main
def main():
    embed_only = "--embed" in sys.argv
    print(f"eval set: {len(EVAL)} cases · exemplar ops: {len(EXEMPLARS)}\n")

    print("=== EMBEDDING ROUTER (raw) ===")
    raw = evaluate(center=False)
    ok, n = acc(raw); print(f"overall: {ok}/{n} = {100*ok/n:.0f}%")
    print("=== EMBEDDING ROUTER (mean-centered) ===")
    cen = evaluate(center=True)
    ok2, _ = acc(cen); print(f"overall: {ok2}/{n} = {100*ok2/n:.0f}%")

    best = cen if ok2 >= ok else raw
    label = "mean-centered" if ok2 >= ok else "raw"
    print(f"\n--- using {label} for detail ---")
    print("by bucket:")
    for bk, (o, t) in sorted(by_bucket(best).items()):
        print(f"  {bk:16} {o}/{t}")

    # margin separation: does margin distinguish correct from wrong?
    cor = [r["margin"] for r in best if r["pred"] == r["op"]]
    wr  = [r["margin"] for r in best if r["pred"] != r["op"]]
    def stats(xs): return (min(xs), sum(xs)/len(xs), max(xs)) if xs else (0,0,0)
    print(f"\nmargin (correct):   min/mean/max = {tuple(round(x,3) for x in stats(cor))}")
    print(f"margin (incorrect): min/mean/max = {tuple(round(x,3) for x in stats(wr))}")

    print("\nmisses (embedding, best variant):")
    for r in best:
        if r["pred"] != r["op"]:
            print(f"  {r['q'][:42]:44} exp={r['op']:14} got={r['pred']:14} margin={r['margin']:+.3f}")

    if embed_only:
        return

    print("\n=== LLM BASELINE (real prompt, qwen2.5:3b) ===")
    llm_rows = []; lat = []
    for c in EVAL:
        r, dt = llm_classify(c["q"]); lat.append(dt)
        llm_rows.append({**c, "pred": llm_to_label(r)})
        mark = "ok" if llm_to_label(r) == c["op"] else "MISS"
        print(f"  [{mark:4}] {c['q'][:42]:44} exp={c['op']:14} got={llm_to_label(r):14} {dt:4.1f}s")
    ok3, _ = acc(llm_rows)
    print(f"\nLLM overall: {ok3}/{n} = {100*ok3/n:.0f}%")
    print("LLM by bucket:")
    for bk, (o, t) in sorted(by_bucket(llm_rows).items()):
        print(f"  {bk:16} {o}/{t}")
    lat.sort()
    print(f"\nLLM latency: median={lat[len(lat)//2]:.1f}s  min={lat[0]:.1f}s  max={lat[-1]:.1f}s")

    # embedding latency sample
    t = time.time(); [embed("push my changes") for _ in range(5)]; edt = (time.time()-t)/5
    print(f"embedding latency (avg of 5): {edt*1000:.0f} ms/query")

    print("\n=== HEADLINE ===")
    print(f"  routing accuracy:  LLM {100*ok3/n:.0f}%   →   embeddings {100*max(ok,ok2)/n:.0f}%")
    print(f"  latency:           LLM ~{lat[len(lat)//2]:.0f}s   →   embeddings ~{edt*1000:.0f} ms  (~{lat[len(lat)//2]/edt:.0f}x faster)")

if __name__ == "__main__":
    main()
