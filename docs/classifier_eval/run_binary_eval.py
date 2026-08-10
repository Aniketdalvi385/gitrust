#!/usr/bin/env python3
"""
End-to-end eval of the REAL gitrust binary (fast-path + embedding router + LLM),
so we measure the shipped behaviour, not a Python reimplementation.

For each labeled query it runs `gitrust "<q>"` in a throwaway repo with
GITRUST_DEBUG=1, feeding 'q'/'n' to bail out of any gate, and records:
  * which ENGINE fired (⚡ instant fast-path / ✦ router / ◉ llm), and
  * whether the routed op MATCHES the expected label (parsed from the plan/marker).

This is coarser than the unit tests (it reads stdout), but it's the honest
whole-system number. Compares routing correctness + engine mix. Slow (LLM calls
on abstains). Requires Ollama up.

Usage:  python3 run_binary_eval.py [--fast]   # --fast: skip cases that hit the LLM
"""
import json, os, re, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
GITRUST = os.path.join(HERE, "..", "..", "gitrust", "target", "release", "gitrust")
EVAL = json.load(open(os.path.join(HERE, "eval_set.json")))["cases"]

# Map an expected label to substrings that, if present in the plan/output,
# indicate the right operation was chosen. Coarse but sufficient for routing.
OP_SIGNS = {
    "status": ["On branch", "working tree", "Nothing to commit"],
    "log": ["← HEAD", "commit history", "  ("],
    "branches": ["* ", "branch"],
    "config": ["user", "email"],
    "fetch": ["Fetching", "up to date", "behind", "ahead"],
    "merge_preview": ["conflict", "safe to", "merge"],
    "push": ["push", "Push", "origin"],
    "pull": ["pull", "Pull", "Fetching"],
    "scoped_push": ["Select files", "Related", "stage", "scope"],
    "add_commit": ["stage", "commit", "Commit message"],
    "squash": ["squash", "Squash"],
    "amend": ["amend", "Amend", "force"],
    "undo": ["Undo", "undo", "keep"],
    "cleanup": ["clean", "Discard", "untracked", "trash"],
    "discard_local": ["discard", "reset", "remote"],
    "force_push": ["force", "lease"],
    "checkout": ["switch", "checkout", "branch"],
    "revert": ["revert", "inverse"],
    "unstage": ["unstage", "restore"],
    "__reject__": ["not sure", "rephrase", "manual mode", "clarif"],
}

def make_repo():
    d = tempfile.mkdtemp()
    def g(*a): subprocess.run(["git", "-C", d, *a], capture_output=True)
    g("init", "-q", "-b", "main", ".")
    g("config", "user.email", "t@t.com"); g("config", "user.name", "t")
    open(os.path.join(d, "app.txt"), "w").write("base\n")
    g("add", "."); g("commit", "-qm", "base")
    # a couple of scope-y files so scoped_push has something to resolve
    for f in ("login.txt", "README.md", "api.txt", "schema.sql"):
        open(os.path.join(d, f), "w").write("x\n")
    return d

def run_one(q):
    d = make_repo()
    env = dict(os.environ, GITRUST_DEBUG="1")
    p = subprocess.run([GITRUST, q], cwd=d, env=env, input="q\nn\nno\n",
                       capture_output=True, text=True, timeout=90)
    out = (p.stdout or "") + (p.stderr or "")
    if "instant" in out or "⚡" in out:
        eng = "fast"
    elif "router:" in out or "(router)" in out or "✦" in out:
        eng = "router"
    else:
        eng = "llm"
    return eng, out

def op_ok(expected, out):
    signs = OP_SIGNS.get(expected, [])
    low = out.lower()
    return any(s.lower() in low for s in signs)

def main():
    fast = "--fast" in sys.argv
    rows = []
    for c in EVAL:
        eng, out = run_one(c["q"])
        if fast and eng == "llm":
            continue
        ok = op_ok(c["op"], out)
        rows.append((c["q"], c["op"], c["bucket"], eng, ok))
        print(f"  [{eng:6}] {'ok ' if ok else 'MISS'} {c['q'][:40]:42} exp={c['op']}")
    n = len(rows); good = sum(1 for r in rows if r[4])
    from collections import Counter
    eng_mix = Counter(r[3] for r in rows)
    print(f"\ntotal {n}  correct {good} ({100*good//max(n,1)}%)")
    print(f"engine mix: {dict(eng_mix)}")
    # v0.3.0 variant buckets specifically
    vb = [r for r in rows if r[2].startswith("variant")]
    vg = sum(1 for r in vb if r[4])
    print(f"v0.3.0 variant buckets: {vg}/{len(vb)} correct")

if __name__ == "__main__":
    main()
