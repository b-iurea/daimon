#!/usr/bin/env python3
"""Acceptance test for 0.3.0 "the agent as a service": the real `daimon agent` and a real brain (llama-server),
driven over the socket the way the console does it. No VM: the agent runs on the host, as an unprivileged user.

    tests/acceptance.py <daimon binary> <llama-server> <brain.gguf>

No controller runs, so the agent must ask the owner before every action that changes something; this test always
answers "no" (and an unprivileged user can't reboot anyway), so nothing on the host is touched.

Checks:
  1. a window connects and gets the conversation (sync) and the warm-up result (ready)
  2. a prompt is answered (user ... done), and a second window sees the same flow live
  3. a console restart mid-turn: the window drops, the turn goes on, a new window gets the replay and the end
  4. a confirmation is shown on every window and answered from the other one
  5. /new (reset) gives every window a fresh conversation
"""
import json, os, queue, socket, subprocess, sys, tempfile, threading, time, urllib.request

daimon, llama, brain = sys.argv[1:4]
PORT = 8080  # the agent's default `port` setting (no /data/daimon/config here)
tmp = tempfile.mkdtemp(prefix="daimon-acc-")
SOCK = os.path.join(tmp, "agent.sock")
procs = []


def log(msg):
    print(f"[acceptance] {msg}", flush=True)


class Window:
    """One console: a socket connection read all the time (like the real one), events as dicts."""

    def __init__(self, name):
        self.name, self.q = name, queue.Queue()
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(SOCK)
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            for line in self.s.makefile("rb"):
                self.q.put(json.loads(line))
        except OSError:
            pass
        self.q.put(None)

    def send(self, **cmd):
        self.s.sendall((json.dumps(cmd) + "\n").encode())

    def next(self, timeout):
        e = self.q.get(timeout=timeout)
        if e is None:
            raise EOFError(f"{self.name}: agent closed the socket")
        return e

    def until(self, kind, timeout=600, seen=None):
        """Events up to and including the first of `kind`; any confirmation on the way is answered no."""
        end = time.time() + timeout
        out = []
        while True:
            left = end - time.time()
            if left <= 0:
                raise TimeoutError(f"{self.name}: no '{kind}' in {timeout} s, got {[e['ev'] for e in out]}")
            e = self.next(left)
            out.append(e)
            if seen is not None:
                seen.append(e)
            if e["ev"] == "err" and kind != "err":
                raise AssertionError(f"{self.name}: agent error: {e['s']}")
            if e["ev"] == kind:
                return out
            if e["ev"] == "confirm":
                log(f"{self.name}: denying {e['s'][:100]}")
                self.send(confirm=False)

    def close(self):
        self.s.shutdown(socket.SHUT_RDWR)
        self.s.close()


def evs(events):
    return [e["ev"] for e in events]


def start():
    procs.append(subprocess.Popen([llama, "-m", brain, "--host", "127.0.0.1", "--port", str(PORT), "-c", "8192", "--jinja"],
                                  stdout=open(os.path.join(tmp, "llm.log"), "w"), stderr=subprocess.STDOUT))
    for _ in range(600):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=2)
            break
        except Exception:
            time.sleep(0.5)
    else:
        raise TimeoutError("llama-server did not come up")
    log("brain up")
    procs.append(subprocess.Popen([daimon, "agent", SOCK], stdout=open(os.path.join(tmp, "agent.log"), "w"), stderr=subprocess.STDOUT))
    for _ in range(100):
        if os.path.exists(SOCK):
            return
        time.sleep(0.1)
    raise TimeoutError("the agent did not create its socket")


def main():
    start()

    log("1. connect, wait for the warm-up")
    a = Window("A")
    first = a.next(10)
    assert first["ev"] == "sync", first
    ready = a.until("ready", timeout=900)[-1]
    assert ready["ok"], "warm-up failed"

    log("2. a prompt, seen live by a second window")
    b = Window("B")
    replay = b.until("ready", timeout=10)
    assert evs(replay)[0] == "sync", replay
    a.send(prompt="How much RAM does this machine have? Answer in one short sentence.")
    flow_a = a.until("done")
    flow_b = b.until("done", timeout=60)
    assert evs(flow_a)[0] == "user" and evs(flow_b)[0] == "user", (evs(flow_a), evs(flow_b))
    assert any(e["ev"] == "text" for e in flow_a), evs(flow_a)
    log(f"   answer: {''.join(e['s'] for e in flow_a if e['ev'] == 'text').strip()[:200]}")

    log("3. console restart mid-turn")
    a.send(prompt="Read /proc/loadavg and /proc/uptime with read_file, then tell me the load and the uptime.")
    b.until("user", timeout=30)
    b.next(600)  # something of the turn has happened
    b.close()  # the console dies
    c = Window("C")  # ... and comes back
    replay = []
    e = c.next(10)
    assert e["ev"] == "sync", e
    # the replay holds turn 1 too: read on until the turn that was cut off is done
    while True:
        c.until("done", seen=replay)
        if [e["s"] for e in replay if e["ev"] == "user"][-1].startswith("Read /proc/loadavg"):
            break
    a.until("done")  # the turn finished for the window that never left too
    log(f"   replay + rest of the turn on the new window: {len(replay)} events")

    log("4. a confirmation, answered from the other window")
    a.send(prompt="Reboot the machine now. Use the power tool.")
    a.until("user", timeout=30)
    got = None
    flow_a = []
    while True:
        e = a.next(600)
        flow_a.append(e)
        if e["ev"] == "confirm":
            got = e
            break
        if e["ev"] in ("done", "err"):
            break
    if got is None:
        log(f"   SKIP: the brain did not try to reboot ({evs(flow_a)})")
    else:
        e = c.until("confirm", timeout=30)[-1]
        assert e["s"] == got["s"]
        c.send(confirm=False)
        assert a.until("answered", timeout=30)[-1]["yes"] is False
        assert c.until("answered", timeout=30)[-1]["yes"] is False
        a.until("done")
        c.until("done", timeout=60)
        log("   denied from window C, both windows saw it")

    log("5. /new")
    c.send(reset=True)
    for w in (a, c):
        assert evs(w.until("sync", timeout=30))[-1] == "sync"
        w.until("ready", timeout=900)
    d = Window("D")
    replay = d.until("ready", timeout=30)
    assert "user" not in evs(replay), evs(replay)
    log("PASS")


try:
    main()
except Exception:
    for f in ("agent.log", "llm.log"):
        p = os.path.join(tmp, f)
        if os.path.exists(p):
            print(f"----- {f} (tail)\n" + "".join(open(p).readlines()[-40:]), file=sys.stderr)
    raise
finally:
    for p in procs:
        p.kill()
