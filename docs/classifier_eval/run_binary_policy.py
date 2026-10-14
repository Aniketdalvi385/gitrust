#!/usr/bin/env python3
"""
Whole-system eval THROUGH THE RELEASE BINARY (ships nothing): what does GiTrust
actually do with each of the 44 eval queries that the ⚡ fast-path doesn't catch?

Unlike run_kev_eval.py (which scores Kev's raw ranking), this runs
`gitrust --yes "<query>"` in a fresh scratch repo per query, so every
deterministic layer around the model is included — decide's thresholds, the
topic cross-check, the reversed-order re-ask for destructive ops, the softer
"not Git" bar. Outcomes, read from the binary's output:

  acted     — Kev routed on its own (engine marker "(kev · p=…)"); right/wrong
              against the eval label
  rejected  — "not a Git request"
  menu      — a ChoiceGate `--yes` refused (which question says why)
  other     — anything else (printed)

Needs a Kev server ALREADY running (this script never starts one; it runs the
binary with HOME pointed at a scratch dir and kev_autostart=false, so your
real ~/.gitrust.toml and ~/.gitrust/ are untouched).

With `--v2`, runs eval_set_v2.json instead (every case, fast-path hits
included, so the ⚡ layer is measured too); in-progress cases run in a real
scratch repo stopped mid-rebase / mid-cherry-pick on a conflict.

Usage:
  KEV_DIR=… ./kevctl.sh start runs/gitrust-v1
  python3 run_binary_policy.py --expect-run runs/gitrust-v1 [--dirty] [--tag name]
  python3 run_binary_policy.py --expect-run runs/gitrust-v1 --v2 [--dirty] [--tag name]
"""
import json, os, re, subprocess, sys, tempfile, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
BIN = os.path.join(ROOT, "gitrust", "target", "release", "gitrust")
URL = "http://127.0.0.1:8009"

def arg(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default

def git(cwd, *a):
    subprocess.run(["git", *a], cwd=cwd, check=True, capture_output=True)

def scratch_repo(base, dirty):
    repo = tempfile.mkdtemp(dir=base)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "eval@example.invalid")
    git(repo, "config", "user.name", "eval")
    for f, c in (("api_client.txt", "a\n"), ("login.txt", "b\n")):
        open(os.path.join(repo, f), "w").write(c)
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "add files")
    git(repo, "commit", "-q", "--allow-empty", "-m", "second")
    if dirty:
        for f in ("api_client.txt", "login.txt"):
            open(os.path.join(repo, f), "a").write("edit\n")
    return repo

def conflicted_repo(base, kind):
    """A repo stopped on a conflict: 'rebase' (2-commit rebase stops on the 1st)
    or 'cherry_pick'."""
    repo = scratch_repo(base, dirty=False)
    git(repo, "checkout", "-q", "-b", "feature")
    for i in (1, 2):
        open(os.path.join(repo, "login.txt"), "a").write(f"feature {i}\n")
        git(repo, "commit", "-qam", f"feature {i}")
    git(repo, "checkout", "-q", "main")
    open(os.path.join(repo, "login.txt"), "a").write("main change\n")
    git(repo, "commit", "-qam", "main change")
    if kind == "rebase":
        git(repo, "checkout", "-q", "feature")
        r = subprocess.run(["git", "rebase", "main"], cwd=repo, capture_output=True)
    else:
        r = subprocess.run(["git", "cherry-pick", "feature~1"], cwd=repo, capture_output=True)
    assert r.returncode != 0, f"expected a {kind} conflict"
    return repo

def outcome(text):
    m = re.search(r"\(kev: (\S+)=", text)
    kev_top = m.group(1) if m else None
    ip = re.search(r"\(kev in-progress: (\S+)=", text)
    if ip and not m:
        kev_top = ip.group(1)
    if "(fast-path:" in text:
        return "fast", re.search(r"\(fast-path: (.*)\)", text).group(1)[:70]
    if re.search(r"\(kev · p=", text) and "That doesn't look like a Git request" not in text:
        return "acted", kev_top
    if "That doesn't look like a Git request" in text:
        return "rejected", kev_top
    m = re.search(r'cannot answer this choice for you: "([^"]+)"', text)
    if m:
        return "menu: " + m.group(1)[:40], kev_top
    return "other", kev_top

def main():
    m = json.loads(urllib.request.urlopen(URL + "/v1/models").read())["models"][0]
    expect = arg("--expect-run")
    if expect and not m["run"].rstrip("/").endswith(expect.rstrip("/")):
        sys.exit(f"REFUSING: server is serving {m['run']}, expected {expect}")
    dirty = "--dirty" in sys.argv
    v2 = "--v2" in sys.argv
    if v2:
        cases = [(c["q"], c["accept"], c["situation"])
                 for c in json.load(open(os.path.join(HERE, "eval_set_v2.json")))["cases"]]
    else:
        gold = {r["q"]: r["op"] for r in json.load(open(os.path.join(HERE, "kev_results_ft-clean.json")))}
        fast = set(json.load(open(os.path.join(HERE, "fastpath_hits.json"))))
        cases = [(q, [g], "normal") for q, g in gold.items() if q not in fast]
    queries = cases
    print(f"server run={m['run']} T={m['temperature']:.3f}; {len(queries)} "
          f"{'v2' if v2 else 'non-fast-path'} queries; repo {'dirty' if dirty else 'clean'}")

    base = tempfile.mkdtemp(prefix="gitrust-policy-")
    home = os.path.join(base, "home"); os.makedirs(home)
    open(os.path.join(home, ".gitrust.toml"), "w").write(
        f'kev_url = "{URL}"\nkev_autostart = false\nkev_idle_minutes = 0\n')
    env = dict(os.environ, HOME=home, GITRUST_DEBUG="1", NO_COLOR="1",
               GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0")
    rows, tally = [], {}
    for q, accept, situation in queries:
        repo = (scratch_repo(base, dirty) if situation == "normal"
                else conflicted_repo(base, situation))
        p = subprocess.run([BIN, "--yes", q], cwd=repo, env=env, capture_output=True,
                           text=True, timeout=120)
        out = p.stdout + p.stderr
        kind, top = outcome(out)
        g = "/".join(accept)
        if kind == "acted":
            kind = "acted-right" if top in accept else "acted-WRONG"
        elif kind == "rejected":
            kind = "rejected-right" if "__reject__" in accept else "rejected-WRONG"
        tally[kind.split(":")[0] if kind.startswith("menu") else kind] = \
            tally.get(kind.split(":")[0] if kind.startswith("menu") else kind, 0) + 1
        rows.append({"q": q, "gold": g, "situation": situation, "kev_top": top,
                     "outcome": kind, "exit": p.returncode})
        print(f"  {kind[:45]:45} {q[:48]!r:50} gold={g} → {top}")
        if kind == "other":
            print("    " + out.strip().replace("\n", "\n    ")[:400])
    print("\n" + "  ".join(f"{k}={v}" for k, v in sorted(tally.items())))
    tag = arg("--tag", "dirty" if dirty else "clean")
    path = os.path.join(HERE, f"binary_policy_{'v2_' if v2 else ''}{tag}.json")
    json.dump({"run": m["run"], "dirty": dirty, "rows": rows, "tally": tally}, open(path, "w"), indent=1)
    print(f"rows → {path}")

if __name__ == "__main__":
    main()
