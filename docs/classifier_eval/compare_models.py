#!/usr/bin/env python3
"""
Compare candidate SLMs as the GiTrust *classifier*, on the SAME labeled eval set
and the SAME real prompt (from prompts.rs). This isolates the model's routing
quality — no fast-path, no embedding router — so we can decide if a swap is
worth it vs. qwen2.5:3b.

For each model: run the real classifier prompt over every eval case, map the
JSON to our label vocabulary (same mapping as run_eval.py), score, and time.

Usage:  python3 compare_models.py qwen2.5:3b qwen3:4b phi4-mini
Requires each model already pulled (`ollama pull <m>`). Ollama up.
"""
import json, os, sys, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
OLLAMA = "http://localhost:11434"
PROMPTS_RS = os.path.join(HERE, "..", "..", "gitrust", "src", "prompts.rs")
EVAL = json.load(open(os.path.join(HERE, "eval_set.json")))["cases"]

def _post(path, body):
    req = urllib.request.Request(OLLAMA + path,
        data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=180).read())

def unload(model):
    """Evict a model from memory (keep_alive=0). On an 8 GB machine two 2.5 GB
    models must NOT be resident at once, or the second model's latency is
    inflated by swap — an unfair comparison. Call between models."""
    try:
        _post("/api/generate", {"model": model, "prompt": " ", "keep_alive": 0, "stream": False})
    except Exception:
        pass
    time.sleep(3)  # let the OS reclaim the pages before the next model loads

def read_rust_raw_string(var):
    src = open(PROMPTS_RS).read()
    i = src.index(var); start = src.index('r#"', i) + 3; end = src.index('"#', start)
    return src[start:end]

SYSTEM_PROMPT = read_rust_raw_string("SYSTEM_PROMPT")
CLASSIFIER_PROMPT = read_rust_raw_string("CLASSIFIER_PROMPT")
SCHEMA = {"type": "object", "properties": {
    "reasoning": {"type": "string"}, "intent": {"type": "string"},
    "op": {"type": "string"}, "feature_hint": {"type": "string"},
    "count": {"type": "integer"}, "confidence": {"type": "number"},
    "question": {"type": "string"}}, "required": ["reasoning", "intent", "confidence"]}

# MEMORY-SAFE knobs (a 4B model at 2048 ctx OOM-crashed an 8 GB machine). Smaller
# context = smaller KV cache; capped output; single model resident (unload between).
NUM_CTX = int(os.environ.get("EVAL_NUM_CTX", "1024"))
NUM_PREDICT = int(os.environ.get("EVAL_NUM_PREDICT", "256"))
CALL_PAUSE = float(os.environ.get("EVAL_CALL_PAUSE", "0.5"))  # let pages reclaim

def classify(model, query):
    prompt = f"{CLASSIFIER_PROMPT}\n\nCurrent repo state:\n(clean repo, on branch main)\n\nUser query: {query}"
    body = {"model": model, "prompt": prompt, "system": SYSTEM_PROMPT, "format": SCHEMA,
            "stream": False, "options": {"temperature": 0.0, "top_k": 1, "top_p": 0.1,
            "num_ctx": NUM_CTX, "num_predict": NUM_PREDICT}}
    # Reasoning models (qwen3, and others that support it) default to emitting a
    # hidden <think> block that (a) burns the token budget → empty response, and
    # (b) is huge/slow on 8 GB. GiTrust is a classifier, not a reasoner — disable
    # thinking so we compare like-for-like with qwen2.5 (which has no think mode).
    # Ollama ignores `think` for models that don't support it, so it's safe to
    # send for all.
    body["think"] = False
    t = time.time()
    try:
        resp = _post("/api/generate", body)
        dt = time.time() - t
        ri = json.loads(resp["response"])
    except Exception as e:
        time.sleep(CALL_PAUSE)
        return {"op": "__parse_error__", "conf": 0.0, "intent": ""}, time.time() - t
    time.sleep(CALL_PAUSE)
    return {"intent": ri.get("intent", ""), "op": ri.get("op", ""),
            "conf": float(ri.get("confidence", 0.0))}, dt

def to_label(r):
    if r["op"] == "__parse_error__": return "__parse_error__"
    if r["conf"] < 0.6 or r["intent"] == "clarify": return "__reject__"
    if r["intent"] == "feature_pipeline" and r["op"] == "push": return "scoped_push"
    return r["op"] or "__reject__"

# Optional subset for the memory-safe run: fewer sustained calls. Picks the
# variant buckets (the ones we care about) + a few unseen/safety/out-of-scope.
def subset(cases, n):
    if n <= 0 or n >= len(cases):
        return cases
    keep = [c for c in cases if c["bucket"].startswith("variant")]
    extra = [c for c in cases if not c["bucket"].startswith("variant")]
    return (keep + extra)[:n]

def run(model, cases):
    # Warm up: the FIRST call to a cold model pays the load cost (~seconds). Time
    # it separately so per-query latency reflects steady state, not model load.
    t0 = time.time()
    classify(model, "status")
    load_s = time.time() - t0
    ok = 0; parse_fail = 0; lat = []; misses = []
    for c in cases:
        r, dt = classify(model, c["q"]); lat.append(dt)
        lab = to_label(r)
        if lab == "__parse_error__": parse_fail += 1
        if lab == c["op"]:
            ok += 1
        else:
            misses.append((c["q"], c["op"], lab, c["bucket"]))
    n = len(cases); lat.sort()
    return {"model": model, "ok": ok, "n": n, "parse_fail": parse_fail,
            "median_lat": lat[len(lat)//2], "load_s": load_s, "misses": misses}

def main():
    models = sys.argv[1:] or ["qwen2.5:3b"]
    cases = subset(EVAL, int(os.environ.get("EVAL_SUBSET", "0")))
    results = []
    print(f"cases: {len(cases)}  ctx: {NUM_CTX}  num_predict: {NUM_PREDICT}", flush=True)
    for m in models:
        print(f"\n=== {m} ===", flush=True)
        # Defensive: make sure NOTHING else is resident before we load this model.
        for other in models:
            if other != m:
                unload(other)
        res = run(m, cases); results.append(res)
        print(f"  accuracy: {res['ok']}/{res['n']} = {100*res['ok']//res['n']}%"
              f"   parse-fails: {res['parse_fail']}"
              f"   median latency: {res['median_lat']:.1f}s   (load: {res['load_s']:.1f}s)")
        from collections import Counter
        bm = Counter(x[3] for x in res["misses"])
        print(f"  misses by bucket: {dict(bm)}")
        # Fairness on 8 GB: evict this model before the next loads.
        print("  unloading…", flush=True)
        unload(m)

    print("\n================ SUMMARY ================")
    print(f"{'model':16} {'acc':>6} {'parse-fail':>11} {'median-lat':>11}")
    for r in results:
        print(f"{r['model']:16} {100*r['ok']//r['n']:>5}% {r['parse_fail']:>11} {r['median_lat']:>10.1f}s")

if __name__ == "__main__":
    main()
