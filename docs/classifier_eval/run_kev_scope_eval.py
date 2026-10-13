#!/usr/bin/env python3
"""
Throwaway probe (ships nothing): can Kev do "Problem B", the resolver's
file-relatedness judgment ("which files belong to the login changes?")?

Fixtures are copied verbatim from the acceptance suite's M-04 variants
(GiTrust-test/tests.manifest.sh), where qwen2.5:3b was over-inclusive on M-04a
(api_client pulled into "login") and under-inclusive on M-04b (README left out
of "docs"). One `noul` question per modified file: path + new content in the
state, topic in the question. Kev reports P(yes); we print it per file so the
"uncertain" band the paper proposes can be read off directly.

Usage:  python3 run_kev_scope_eval.py [--url http://127.0.0.1:8009]
"""
import json, sys, time, urllib.request

URL = sys.argv[sys.argv.index("--url") + 1] if "--url" in sys.argv else "http://127.0.0.1:8009"

# (variant, topic, {path: content}, set of files that belong to the topic)
FIXTURES = [
    ("M-04a", "login", {"login.txt": "login page", "auth.txt": "login helper",
                        "readme.txt": "unrelated", "api_client.txt": "api thing"},
     {"login.txt", "auth.txt"}),
    ("M-04b", "docs", {"README.md": "readme docs", "docs/guide.md": "guide docs",
                       "login.txt": "login page", "schema.sql": "schema def"},
     {"README.md", "docs/guide.md"}),
    ("M-04c", "api", {"api_client.txt": "api client", "api_routes.txt": "api routes",
                      "login.txt": "login page", "README.md": "readme"},
     {"api_client.txt", "api_routes.txt"}),
    ("M-04d", "schema", {"schema.sql": "users schema", "migrations.sql": "orders schema",
                         "api_client.txt": "api client", "README.md": "readme"},
     {"schema.sql", "migrations.sql"}),
]

def ask(topic, files):
    # One request per variant: each file is its own noul question over a shared
    # state listing every changed file (so Kev sees the whole change set once).
    state = {"user_request": f"push the {topic} changes",
             "changed_files": [{"path": p, "new_content": c} for p, c in files.items()]}
    qs = {f"f{i}": {"type": "noul",
                    "instructions": f"Should the changed file `{p}` be included in a commit "
                                    f"of only the '{topic}' changes?"}
          for i, p in enumerate(files)}
    body = {"model": "kev-latest", "state": state, "questions": qs}
    req = urllib.request.Request(URL + "/v1/systemone", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    t = time.time(); r = json.loads(urllib.request.urlopen(req, timeout=300).read())
    probs = {p: r["answers"][f"f{i}"]["noul"] for i, p in enumerate(files)}
    return probs, r.get("latency_ms"), (time.time() - t) * 1000

def main():
    m = json.loads(urllib.request.urlopen(URL + "/v1/models").read())["models"][0]
    print(f"server: run={m['run']} temperature={m['temperature']:.3f}")
    expect = sys.argv[sys.argv.index("--expect-run") + 1] if "--expect-run" in sys.argv else None
    if expect and not m["run"].rstrip("/").endswith(expect.rstrip("/")):
        sys.exit(f"REFUSING: server is serving {m['run']}, expected {expect}")
    hi, lo = 0.7, 0.3  # include ≥ hi, exclude ≤ lo, else "uncertain" (user decides)
    tot = right = unc = wrong = 0
    for vid, topic, files, gold in FIXTURES:
        probs, mms, wms = ask(topic, files)
        print(f"\n{vid}  topic='{topic}'   ({mms} ms model)")
        for p, pr in probs.items():
            g = p in gold
            band = "include" if pr >= hi else "exclude" if pr <= lo else "uncertain"
            ok = (band == "include" and g) or (band == "exclude" and not g)
            tot += 1; right += ok; unc += band == "uncertain"
            wrong += band != "uncertain" and not ok
            print(f"  {'✓' if ok else ('?' if band == 'uncertain' else '✗')} {p:16} P(yes)={pr:.2f} "
                  f"→ {band:9} (gold: {'related' if g else 'unrelated'})")
        # binary split at 0.5 as well, to compare with a forced related/unrelated call
        b_ok = all((probs[p] >= 0.5) == (p in gold) for p in files)
        print(f"  forced 0.5 split exact-match: {'yes' if b_ok else 'no'}")
    print(f"\nbanded ({lo}/{hi}): {right}/{tot} confidently right, {unc} uncertain (→ user), "
          f"{wrong} confidently WRONG")

if __name__ == "__main__":
    main()
