#!/usr/bin/env python3
"""
Build Kev fine-tuning data for GiTrust routing (ships nothing).

Writes kev_train.jsonl + kev_dev.jsonl (Kev's labelled-request format) from
hand-written phrasings per op. The label policy mirrors prompts.rs: undo-family
language (even multi-commit) → `undo` (the gate decides), throw-away language →
`cleanup`, never reset.

HOLD-OUT DISCIPLINE — eval_set.json is never trained on:
  * topics / branch names / counts used by the eval set are banned from slots
    (login, docs, api, schema, auth · dev, hotfix · 3, three, few)
  * any phrasing that equals an eval query, or whose content-word Jaccard with
    one is >= 0.6, is dropped and reported
  * the dev split is grouped by source phrasing (no phrasing in both splits)

Usage:  python3 make_kev_train.py   → prints counts + every dropped near-duplicate
"""
import json, os, random, re

from kev_ops import OPS, INSTRUCTIONS, IN_PROGRESS

HERE = os.path.dirname(os.path.abspath(__file__))
EVAL_Q = [c["q"] for c in json.load(open(os.path.join(HERE, "eval_set.json")))["cases"]]
rng = random.Random(7)

TOPICS = ["payments", "billing", "search", "checkout page", "profile", "settings", "export",
          "onboarding", "notifications", "dashboard", "cart", "invoice", "upload", "analytics",
          "signup form", "email templates", "rate limiter", "cache", "logging", "i18n"]
BRANCHES = ["feature-x", "release-2.1", "staging", "bugfix/navbar", "experiment", "qa",
            "feat/search", "v2", "develop", "spike-cache"]
COUNTS = ["2", "two", "4", "four", "5", "five", "couple of", "several"]
FILES = ["app.py", "main.rs", "index.html", "package.json", "config.yaml", "styles.css",
         "Makefile", "server.js", "notes.txt", "CHANGELOG.md"]
URLS = ["git@github.com:me/proj.git", "https://github.com/acme/site.git",
        "https://gitlab.com/team/tool.git"]
MSGS = ["fix typo", "add search", "wip", "bump version", "refactor parser"]
SHAS = ["abc123", "9f2e7d1", "HEAD~2", "4c1a0be"]
REMOTE_BRANCHES = ["origin/develop", "origin/release-2.1", "upstream/main"]
REMOTES = ["upstream", "fork", "backup"]

P = {
 "status": ["what's changed", "show me what's modified", "git status", "what files did i touch",
            "is anything uncommitted", "show the working tree state", "any pending changes?",
            "what's staged right now", "which files are dirty", "status of my repo please"],
 "log": ["show the commit log", "list the recent commits", "what commits have been made",
         "commit history please", "git log", "show the last ten commits", "who committed recently",
         "show past commits on this branch"],
 "diff": ["show me the diff", "what did i change in {file}", "show line by line changes",
          "diff my working copy", "what exactly changed in the code", "git diff"],
 "branches": ["which branch am i on", "show branches", "what branches exist", "current branch?",
              "list all the branches", "what branch is checked out", "show local and remote branches",
              "branch"],
 "config": ["what's my git email", "show my git username", "which name will my commits use",
            "print the git config"],
 "show": ["show the last commit", "details of the latest commit", "what's in commit {sha}",
          "show me commit {sha}", "what did the most recent commit change", "inspect HEAD"],
 "fetch": ["check if the remote has anything new", "is my branch behind", "am i in sync with origin",
           "anything new upstream?", "fetch the remote", "has anyone pushed since i last checked",
           "fetch", "check the upstream for new commits"],
 "merge_preview": ["would a pull conflict with my work", "check for conflicts before i pull",
                   "is it safe to merge the remote changes", "predict merge conflicts",
                   "do my edits clash with what's on origin", "preview what a merge would do"],
 "add_commit": ["commit all of it", "stage and commit everything", "commit my work",
                "commit the {file}", "add everything and commit", "record all my changes in a commit",
                "make a commit with every change", "commit {file} and {file2}",
                "commit all with message '{msg}'", "check in all my edits"],
 "commit": ["commit what's staged", "commit the staged changes", "commit", "just commit the index",
            "commit only what i already added", "commit staged files with message '{msg}'"],
 "push": ["push my commits", "upload my branch", "send my commits to the remote", "publish this branch",
          "push it", "push to origin", "push my code up", "push everything i've committed",
          "share my commits with the team", "push the current branch"],
 "scoped_push": ["push the {topic} changes", "push only the {topic} files", "commit and push the {topic} work",
                 "ship the {topic} fixes", "send up just the {topic} stuff", "push everything related to {topic}",
                 "push my {topic} edits but nothing else", "commit and push only the {topic} feature"],
 "branch": ["create a branch named {branch}", "make branch {branch}", "new branch {branch}",
            "start a {branch} branch", "branch off as {branch}", "open a fresh branch called {branch}"],
 "checkout": ["switch to {branch}", "go to the {branch} branch", "check out {branch}", "move over to {branch}",
              "hop onto {branch}", "change branch to {branch}", "take me to {branch}"],
 "pull": ["pull", "pull from the remote", "grab the newest commits", "download the latest changes",
          "update from origin", "pull down my teammate's work", "bring in the remote commits",
          "git pull"],
 "sync": ["merge main into this branch", "bring main into my branch", "update my branch with main",
          "catch my feature branch up with main", "get main's changes into my branch",
          "sync my branch with {branch}", "keep my branch current with main"],
 "undo": ["undo my last commit", "take back that commit", "undo that", "uncommit",
          "i didn't mean to commit that", "undo the last {count} commits", "undo my previous commit",
          "that commit was a mistake, undo it", "take back my last {count} commits", "undo the commit"],
 "amend": ["amend the commit", "edit the last commit message", "add a forgotten file to the previous commit",
           "change the message of my latest commit to '{msg}'", "fix a typo in my last commit message",
           "amend with message '{msg}'", "put {file} into the previous commit"],
 "squash": ["squash the last {count} commits", "merge my last {count} commits into one",
            "squash those commits together", "turn my last {count} commits into a single commit",
            "squash my recent commits", "fold the last {count} commits together"],
 "revert": ["revert commit {sha}", "create a commit that undoes {sha}", "revert the bad commit but keep history",
            "revert {sha}", "add a commit reversing {sha}", "back out {sha} safely for the team"],
 "unstage": ["unstage everything", "unstage {file}", "remove {file} from the index",
             "i staged the wrong file", "unstage all files", "don't include {file} in the commit, unstage it"],
 "stash": ["stash this", "put my changes aside for now", "save my work temporarily without committing",
           "stash my edits", "shelve everything so i can switch branches", "git stash"],
 "cleanup": ["discard all my uncommitted changes", "reset my working directory", "wipe my local changes",
             "get rid of untracked files", "start over from a clean tree", "nuke my edits",
             "revert all my file changes", "delete everything i haven't committed"],
 "discard_local": ["drop my local commits and match origin", "reset my branch to the remote",
                   "abandon the commits i haven't pushed", "make my branch identical to origin",
                   "throw out my unpushed commits", "hard reset to origin"],
 "force_push": ["force push", "push --force", "overwrite origin with my branch", "force the push through",
                "push with force-with-lease", "replace the remote history with mine"],
 "init": ["create a repo here", "initialize git", "start version control in this folder",
          "make this directory a git repository", "git init"],
 "add_remote": ["add origin {url}", "set the remote to {url}", "connect this repo to {url}",
                "add a remote called upstream at {url}"],
 "find_lost_work": ["i can't find my commit", "where did my work go", "recover my deleted branch",
                    "i lost changes after a reset", "help me find a commit that disappeared",
                    "my stash vanished, can you find it"],
 "remotes": ["list my remotes", "which remotes are configured", "show the remote urls",
             "where does origin point", "git remote -v"],
 "stash_pop": ["bring back my stash", "unstash my changes", "restore what i stashed",
               "apply my last stash and drop it", "get my shelved work back"],
 "pull_rebase": ["pull with rebase", "rebase onto the remote", "pull --rebase",
                 "put my commits on top of origin", "replay my work on top of the latest remote"],
 "set_upstream": ["make this branch track {remote_branch}", "set the upstream to {remote_branch}",
                  "track the remote branch", "link this branch to origin", "set upstream for this branch"],
 "remove_remote": ["remove the upstream remote", "delete remote {remote}", "drop the {remote} remote",
                   "unset the remote", "get rid of the remote called {remote}"],
 "delete_repo": ["delete this repository", "remove the .git folder", "undo git init",
                 "stop tracking this folder with git", "get rid of the git repo here"],
 "__reject__": ["what's 2+2", "tell me a joke", "book a flight to denver", "translate hello to french",
                "how tall is everest", "order a pizza", "play some music", "summarize this article",
                "what's the capital of peru", "set an alarm for 7am", "explain quantum physics",
                "who won the game last night", "write a haiku about rain", "compose an email to my boss",
                "how do i bake bread", "recommend a movie", "what's the stock price of apple",
                "is it going to rain tomorrow", "hello there", "how are you doing today"],
}
STATES = ["clean repo, on branch main", "on branch feature-x, 2 files modified",
          "on branch main, 1 commit ahead of origin", "on branch develop, 3 untracked files",
          "on branch main, behind origin by 4 commits", "on branch qa, 1 staged file"]

STOP = set("the a an my me to of i is it this that and for with on in please all".split())
def toks(s): return {w for w in re.findall(r"[a-z0-9']+", s.lower()) if w not in STOP}
def jacc(a, b):
    A, B = toks(a), toks(b)
    return len(A & B) / len(A | B) if A | B else 1.0

def fill(t):
    f1, f2 = rng.sample(FILES, 2)
    return t.format(topic=rng.choice(TOPICS), branch=rng.choice(BRANCHES), count=rng.choice(COUNTS),
                    file=f1, file2=f2, url=rng.choice(URLS), msg=rng.choice(MSGS), sha=rng.choice(SHAS),
                    remote_branch=rng.choice(REMOTE_BRANCHES), remote=rng.choice(REMOTES))

def variants(q):
    out = {q, q.capitalize()}
    if not q.endswith("?"): out.add(q + " please")
    out.add("can you " + q.rstrip("?") + "?")
    return out

# In-progress control (asked only when the repo is mid-operation). Labels are
# the family's ops or `something_else`; the state carries the in-progress line.
IP = {
 "rebase_continue": ["continue", "keep going with the rebase", "i fixed the conflicts, go on",
                     "resume", "conflicts are resolved, carry on"],
 "rebase_skip": ["skip this one", "skip this commit and keep rebasing", "drop the commit it stopped on"],
 "rebase_abort": ["abort", "cancel the rebase", "get me out of this rebase", "stop rebasing, go back",
                  "undo this rebase"],
 "merge_abort": ["abort the merge", "cancel this merge", "get me out of this merge", "back out of the merge"],
 "cherry_pick_continue": ["continue the cherry-pick", "conflicts fixed, finish the cherry pick", "carry on"],
 "cherry_pick_abort": ["cancel the cherry-pick", "abort", "stop the cherry pick and go back"],
 "revert_continue": ["continue the revert", "resolved it, finish reverting", "keep going"],
 "revert_abort": ["abort the revert", "cancel reverting", "stop the revert"],
}
IP_OTHER = ["show status", "which files have conflicts", "show me the diff", "what branch am i on",
            "show the log", "what's going on", "list branches"]
IP_STATES = {"rebase": "on branch feature-x, rebase in progress (2/5)",
             "merge": "on branch main, merge in progress (conflicts in 2 file(s))",
             "cherry_pick": "on branch main, cherry-pick in progress",
             "revert": "on branch main, revert in progress"}

def in_progress_records():
    out = []
    other = IN_PROGRESS["other"][0]
    for fam, opts in IN_PROGRESS["families"].items():
        crit = {o: d for o, d in opts}
        crit[other] = IN_PROGRESS["other"][1]
        labelled = [(q, o) for o, _ in opts for q in IP[o]] + [(q, other) for q in IP_OTHER]
        for q, label in labelled:
            for v in sorted(variants(q))[:2]:
                out.append({"state": {"user_request": v, "repo_state": IP_STATES[fam]},
                            "questions": {"op": {"type": "choice", "instructions": IN_PROGRESS["instructions"],
                                                 "criteria": crit, "label": label}}})
    return out

def main():
    assert set(P) == set(OPS), set(OPS) ^ set(P)
    groups, dropped = [], []
    for op, templates in P.items():
        for t in templates:
            for k in range(3 if "{" in t else 1):  # slot templates get 3 fills
                q = fill(t)
                near = max(EVAL_Q, key=lambda e: jacc(q, e))
                if q.lower() in {e.lower() for e in EVAL_Q} or jacc(q, near) >= 0.6:
                    dropped.append((q, near, round(jacc(q, near), 2))); continue
                groups.append((op, sorted(variants(q))))
    rng.shuffle(groups)
    n_dev = len(groups) * 15 // 100
    def records(gs):
        for op, qs in gs:
            for q in qs:
                yield {"state": {"user_request": q, "repo_state": rng.choice(STATES)},
                       "questions": {"op": {"type": "choice", "instructions": INSTRUCTIONS,
                                            "criteria": OPS, "label": op}}}
    ip = in_progress_records(); rng.shuffle(ip)
    n_ip_dev = len(ip) * 15 // 100
    for name, gs, extra in (("kev_dev.jsonl", groups[:n_dev], ip[:n_ip_dev]),
                            ("kev_train.jsonl", groups[n_dev:], ip[n_ip_dev:])):
        rows = list(records(gs)) + extra
        rng.shuffle(rows)
        with open(os.path.join(HERE, name), "w") as f:
            for r in rows: f.write(json.dumps(r) + "\n")
        print(f"{name}: {len(rows)} records ({len(gs)} routing phrasings + {len(extra)} in-progress)")
    print(f"dropped {len(dropped)} phrasings as eval near-duplicates:")
    for q, e, j in dropped: print(f"  {q!r:50} ~ {e!r} ({j})")

if __name__ == "__main__":
    main()
