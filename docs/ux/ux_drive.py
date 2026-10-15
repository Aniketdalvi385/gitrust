#!/usr/bin/env python3
"""UX harness: drive the gitrust binary in a real pseudo-terminal and save what
the user would see (docs/ux/README.md explains the audit it was built for).

Each scenario = a fixture repo (clean / dirty / mid-rebase / none) + argv + a
list of steps (wait for text on screen → snapshot → send keys). pyte renders the
terminal; cursor-position queries (crossterm needs them) are answered by writing
the reply back to the pty. The binary runs with HOME pointed at a scratch dir
(its own ~/.gitrust.toml, kev_autostart off) so your real config is untouched.

Needs a Kev server on 127.0.0.1:8009 for model-routed scenarios
(docs/classifier_eval/kevctl.sh start runs/gitrust-v2), except `kev_down`
(uses a dead port) and `cold_start` (needs KEV_DIR, starts its own server).

Usage (from this folder):
  uv run --with pyte python ux_drive.py                 # every scenario → screens/after/
  uv run --with pyte python ux_drive.py undo_gate repl  # some
  UX_OUT=/tmp/x GITRUST_BIN=… uv run --with pyte python ux_drive.py
"""
import fcntl, os, pty, select, shutil, signal, struct, subprocess, sys, tempfile, termios, time

import pyte

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
BIN = os.environ.get("GITRUST_BIN", os.path.join(ROOT, "gitrust", "target", "release", "gitrust"))
OUT = os.environ.get("UX_OUT", os.path.join(HERE, "screens", "after"))
# The binary runs with HOME pointed at a scratch dir; keep the real caches.
REAL_UV_CACHE = os.environ.get("UV_CACHE_DIR", os.path.expanduser("~/.cache/uv"))
REAL_HF_HOME = os.environ.get("HF_HOME", os.path.expanduser("~/.cache/huggingface"))
KEV_DIR = os.environ.get("KEV_DIR", "")
COLS, ROWS = 100, 50
ENTER, DOWN, UP, ESC, CTRL_C, SPACE, TAB = b"\r", b"\x1b[B", b"\x1b[A", b"\x1b", b"\x03", b" ", b"\t"


class TtyScreen(pyte.Screen):
    fd = None

    def write_process_input(self, data):
        if self.fd is not None:
            os.write(self.fd, data.encode())


def screen_text(screen):
    lines = [l.rstrip() for l in screen.display]
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def run(argv, cwd, env, steps, timeout=90):
    """steps: list of (wait_text or None, keys or None, label)."""
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execvpe(argv[0], argv, env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    screen = TtyScreen(COLS, ROWS)
    screen.fd = fd
    stream = pyte.ByteStream(screen)
    snaps, raw = [], b""
    deadline = time.time() + timeout
    exited, status = False, None

    def pump(t):
        nonlocal raw, exited, status
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    data = b""
                if not data:
                    exited = True
                    return
                raw += data
                stream.feed(data)
            if not exited:
                p, st = os.waitpid(pid, os.WNOHANG)
                if p:
                    exited, status = True, st
                    # drain
                    try:
                        while True:
                            r, _, _ = select.select([fd], [], [], 0.1)
                            if not r:
                                break
                            data = os.read(fd, 65536)
                            if not data:
                                break
                            raw += data
                            stream.feed(data)
                    except OSError:
                        pass
                    return

    for wait_text, keys, label in steps:
        if wait_text:
            while time.time() < deadline and wait_text not in screen_text(screen) and not exited:
                pump(0.2)
        pump(0.6)
        snaps.append((label + ("" if not wait_text or wait_text in screen_text(screen)
                               else f"   [!! timed out waiting for {wait_text!r}]"), screen_text(screen)))
        if keys and not exited:
            os.write(fd, keys)
    while time.time() < deadline and not exited:
        pump(0.3)
    if not exited:
        os.kill(pid, signal.SIGKILL)
        snaps.append(("!! killed at timeout", screen_text(screen)))
    else:
        if status is None:
            try:
                _, status = os.waitpid(pid, 0)
            except ChildProcessError:
                status = 0
    snaps.append((f"final (exit={os.waitstatus_to_exitcode(status) if status is not None else '?'})",
                  screen_text(screen)))
    os.close(fd)
    return snaps


# ---------------------------------------------------------------- fixtures
def sh(cwd, *cmd, check=True):
    return subprocess.run(cmd, cwd=cwd, check=check, capture_output=True, text=True)


def base_repo(root, dirty=True, remote=True):
    os.makedirs(root, exist_ok=True)
    bare = os.path.join(root, "origin.git")
    work = os.path.join(root, "work")
    if remote:
        sh(root, "git", "init", "-q", "--bare", "-b", "main", bare)
        sh(root, "git", "clone", "-q", bare, work)
    else:
        sh(root, "git", "init", "-q", "-b", "main", work)
    sh(work, "git", "config", "user.email", "dev@example.invalid")
    sh(work, "git", "config", "user.name", "Dev")
    files = {
        "src/login.rs": "pub fn login(user: &str) -> bool {\n    !user.is_empty()\n}\n",
        "src/auth.rs": "pub fn check_token(t: &str) -> bool { t.len() > 8 }\n",
        "src/api_client.rs": "pub fn get(url: &str) -> String { url.to_string() }\n",
        "README.md": "# Demo app\n\nA demo.\n",
    }
    for p, c in files.items():
        os.makedirs(os.path.dirname(os.path.join(work, p)) or work, exist_ok=True)
        open(os.path.join(work, p), "w").write(c)
    sh(work, "git", "add", ".")
    sh(work, "git", "commit", "-qm", "Initial commit")
    for i, msg in enumerate(["Add login form", "Fix token check", "Update README"]):
        open(os.path.join(work, "CHANGELOG.md"), "a").write(f"- {msg}\n")
        sh(work, "git", "add", ".")
        sh(work, "git", "commit", "-qm", msg)
    if remote:
        sh(work, "git", "push", "-q", "-u", "origin", "main")
    if dirty:
        open(os.path.join(work, "src/login.rs"), "a").write("\npub fn logout() {}\n")
        open(os.path.join(work, "src/auth.rs"), "a").write("\npub fn refresh_login_token() {}\n")
        open(os.path.join(work, "README.md"), "a").write("\n## Usage\nRun it.\n")
        open(os.path.join(work, "notes.txt"), "w").write("scratch notes\n")
    return work


def rebase_conflict_repo(root):
    work = base_repo(root, dirty=False)
    sh(work, "git", "checkout", "-q", "-b", "feature")
    for i in (1, 2):
        open(os.path.join(work, "src/login.rs"), "a").write(f"// feature {i}\n")
        sh(work, "git", "commit", "-qam", f"Feature change {i}")
    sh(work, "git", "checkout", "-q", "main")
    open(os.path.join(work, "src/login.rs"), "a").write("// main change\n")
    sh(work, "git", "commit", "-qam", "Main change")
    sh(work, "git", "checkout", "-q", "feature")
    sh(work, "git", "rebase", "main", check=False)
    return work


def env_for(home):
    os.makedirs(home, exist_ok=True)
    open(os.path.join(home, ".gitrust.toml"), "w").write(
        'kev_url = "http://127.0.0.1:8009"\nkev_autostart = false\nkev_idle_minutes = 0\n')
    return {"HOME": home, "PATH": os.environ["PATH"], "TERM": "xterm-256color",
            "GIT_CONFIG_GLOBAL": "/dev/null", "LANG": "en_US.UTF-8", "COLUMNS": str(COLS),
            "LINES": str(ROWS), "UV_CACHE_DIR": REAL_UV_CACHE, "HF_HOME": REAL_HF_HOME}


# ---------------------------------------------------------------- scenarios
S = {}
def scenario(name, repo="dirty"):
    def deco(f):
        S[name] = (repo, f)
        return f
    return deco

@scenario("status")
def _(): return [BIN, "status"], [(None, None, "after run")]

@scenario("history_kev")
def _(): return [BIN, "show me the history"], [("Understanding", None, "while Kev thinks"), (None, None, "after run")]

@scenario("unsure_menu")
def _(): return [BIN, "put my work up on github"], [("?", None, "menu shown"), (None, ESC, "after Esc")]

@scenario("menu_digit")
def _(): return [BIN, "put my work up on github"], [("Esc cancel", b"2", "menu"), (None, None, "after pressing 2")]

@scenario("menu_arrows", repo="clean")
def _(): return [BIN, "undo"], [("Esc cancel", DOWN, "menu"), (None, DOWN, "after ↓"), (None, ESC, "after ↓↓"), (None, None, "after Esc")]

@scenario("maybe_not_git")
def _(): return [BIN, "where am i right now"], [("?", None, "menu shown"), (None, DOWN + ENTER, "pick 2nd")]

@scenario("refusal")
def _(): return [BIN, "make me a sandwich"], [(None, None, "after run")]

@scenario("missing_branch_name")
def _(): return [BIN, "make me a new branch"], [("Name", b"feat/Login Page (v2)" + ENTER, "prompt shown"), ("suggested", ENTER, "suggestion"), (None, None, "after")]

@scenario("commit_all")
def _(): return [BIN, "commit everything"], [("message", b"Add logout" + ENTER, "message prompt"),
                                             ("[Enter] run", ENTER, "plan"),
                                             ("Done", None, "done")]

@scenario("scoped_push")
def _(): return [BIN, "push the login changes"], [("Stage which files", ENTER, "file picker"),
                                                  ("Commit message", b"Add logout flow" + ENTER, "message"),
                                                  ("[Enter] run", ENTER, "plan"),
                                                  ("Done", None, "done")]

@scenario("commit_push_kev")
def _(): return [BIN, "commit all my work and push it"], [(None, None, "first screen"), (None, b"Ship it" + ENTER, "message"), (None, ENTER, "run")]

@scenario("undo_gate", repo="clean")
def _(): return [BIN, "undo"], [("?", None, "undo gate"), (None, ESC, "Esc")]

@scenario("cleanup_gate")
def _(): return [BIN, "clean up my repo"], [("Esc cancel", ENTER, "cleanup gate"), ("[Enter] run", ENTER, "plan"), ("Done", None, "done")]

@scenario("rebase_continue", repo="rebase")
def _(): return [BIN, "continue"], [(None, None, "after run")]

@scenario("rebase_status", repo="rebase")
def _(): return [BIN, "whats conflicting"], [(None, None, "after run")]

@scenario("plan_cancel")
def _(): return [BIN, "commit everything"], [("message", b"wip" + ENTER, "message prompt"), ("[Enter] run", b"q", "plan"), (None, None, "after q")]

@scenario("help")
def _(): return [BIN, "help"], [(None, None, "after run")]

@scenario("clap_help")
def _(): return [BIN, "--help"], [(None, None, "after run")]

@scenario("repl")
def _(): return [BIN], [("gitrust>", b"status" + ENTER, "repl start"), ("Show status", b"exit" + ENTER, "status"),
                        (None, None, "exit")]

@scenario("yes_unsure")
def _(): return [BIN, "--yes", "put my work up on github"], [(None, None, "after run")]

@scenario("force_push", repo="clean")
def _(): return [BIN, "force push"], [("?", None, "gate"), (None, CTRL_C, "ctrl-c")]

@scenario("find_lost", repo="clean")
def _(): return [BIN, "i lost a commit help"], [(None, None, "screen"), (None, ESC, "esc")]

@scenario("not_a_repo", repo="none")
def _(): return [BIN, "status"], [(None, None, "after run")]

@scenario("rebase_plain_status", repo="rebase")
def _(): return [BIN, "status"], [(None, None, "after run")]

@scenario("empty_branch_enter")
def _(): return [BIN, "make me a new branch"], [("Name", ENTER, "prompt"), (None, None, "after empty enter"), (None, ESC, "esc")]

@scenario("nothing_to_commit", repo="clean")
def _(): return [BIN, "commit everything"], [(None, None, "after run")]

@scenario("cold_start")
def _(): return [BIN, "show me the history"], [("Starting", None, "cold start begins"), (" · 1", None, "about 10 s in"), ("Show history", None, "answered")]

@scenario("kev_down")
def _(): return [BIN, "show me the history"], [(None, None, "after run")]


def main():
    names = [a for a in sys.argv[1:] if not a.startswith("--")] or list(S)
    os.makedirs(OUT, exist_ok=True)
    base = tempfile.mkdtemp(prefix="gitrust-ux-")
    for name in names:
        repo_kind, f = S[name]
        root = os.path.join(base, name)
        if repo_kind == "dirty":
            work = base_repo(root, dirty=True)
        elif repo_kind == "clean":
            work = base_repo(root, dirty=False)
        elif repo_kind == "rebase":
            work = rebase_conflict_repo(root)
        else:
            work = root
            os.makedirs(work, exist_ok=True)
        env = env_for(os.path.join(root, "home"))
        if name == "cold_start":
            open(os.path.join(root, "home", ".gitrust.toml"), "w").write(
                f'kev_url = "http://127.0.0.1:8009"\nkev_dir = "{KEV_DIR}"\n'
                'kev_run = "runs/gitrust-v2"\nkev_autostart = true\nkev_idle_minutes = 2\n')
        if name == "kev_down":
            open(os.path.join(root, "home", ".gitrust.toml"), "w").write(
                'kev_url = "http://127.0.0.1:8999"\nkev_autostart = false\n')
        argv, steps = f()
        snaps = run(argv, work, env, steps)
        with open(os.path.join(OUT, f"{name}.txt"), "w") as fh:
            fh.write(f"$ gitrust {' '.join(repr(a) for a in argv[1:])}\n")
            for label, text in snaps:
                fh.write(f"\n----- {label} -----\n{text}\n")
        print(f"== {name}")
        print(open(os.path.join(OUT, f"{name}.txt")).read())


if __name__ == "__main__":
    main()
