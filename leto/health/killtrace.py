# Read-only /proc sampler: which callers make the shared sccache daemon kill a
# running compiler (its client went away mid-compile, so kill_on_drop fired)?
# Usage: python3 -I - DURATION_S OUT_PREFIX < killtrace.py
import json, os, subprocess, sys, time
from collections import Counter

DURATION, OUT = float(sys.argv[1]), sys.argv[2]
TICK = 0.5
ME = os.getuid()


def read(path, mode="rb"):
    try:
        with open(path, mode) as f:
            return f.read()
    except OSError:
        return None


def cmdline(pid):
    raw = read(f"/proc/{pid}/cmdline")
    return [a.decode(errors="replace") for a in raw.split(b"\0")[:-1]] if raw else []


def key_of(args, pid):
    """(cwd, crate, extra-filename) of a rustc command line, or None."""
    crate = extra = outdir = None
    for i, a in enumerate(args):
        if a == "--crate-name" and i + 1 < len(args):
            crate = args[i + 1]
        elif a == "--out-dir" and i + 1 < len(args):
            outdir = args[i + 1]
        elif a.startswith("extra-filename="):
            extra = a.split("=", 1)[1]
        elif a == "-C" and i + 1 < len(args) and args[i + 1].startswith("extra-filename="):
            extra = args[i + 1].split("=", 1)[1]
    if not crate:
        return None, None
    try:
        cwd = os.readlink(f"/proc/{pid}/cwd")
    except OSError:
        cwd = "?"
    return (cwd, crate, extra or ""), outdir


def ppid(pid):
    raw = read(f"/proc/{pid}/stat")
    if not raw:
        return None
    return int(raw.rsplit(b")", 1)[1].split()[1])


def ancestors(pid, depth=10):
    chain = []
    p = ppid(pid)
    while p and p > 1 and len(chain) < depth:
        args = cmdline(p)
        comm = (read(f"/proc/{p}/comm") or b"?").decode().strip()
        chain.append({"comm": comm, "cmd": " ".join(args)[:160]})
        p = ppid(p)
    return chain


def daemon_pid():
    out = subprocess.run(["systemctl", "--user", "show", "sccache", "-p", "MainPID", "--value"],
                         capture_output=True, text=True).stdout.strip()
    return int(out) if out.isdigit() and out != "0" else None


def children(pid):
    kids = set()
    try:
        for t in os.listdir(f"/proc/{pid}/task"):
            raw = read(f"/proc/{pid}/task/{t}/children")
            if raw:
                kids.update(int(x) for x in raw.split())
    except OSError:
        pass
    return kids


def produced(outdir, crate, extra, since):
    if not outdir:
        return None
    stems = (f"lib{crate}{extra}.", f"{crate}{extra}")
    try:
        for name in os.listdir(outdir):
            if name.startswith(stems):
                try:
                    if os.stat(os.path.join(outdir, name)).st_mtime >= since - 1:
                        return True
                except OSError:
                    pass
    except OSError:
        return None
    return False


def stats():
    try:
        out = subprocess.run([os.path.expanduser("~/.cargo/bin/sccache"), "--show-stats", "--stats-format=json"],
                             capture_output=True, text=True, timeout=15,
                             env={**os.environ, "SCCACHE_SERVER_PORT": "4226"}).stdout
        q = json.loads(out)["stats"]["compile_queue"]
        return {"running": q["running"], "waiting": q["token_wait"]["active"],
                "in_use": q.get("tokens_in_use"), "stalled_s": (q.get("stalled_for") or {}).get("secs")}
    except Exception as e:  # noqa: BLE001
        return {"error": str(e)[:80]}


events = open(OUT + ".jsonl", "a")
def emit(kind, **fields):
    events.write(json.dumps({"t": time.time(), "kind": kind, **fields}) + "\n")
    events.flush()

compilers = {}  # pid -> {key, outdir, start}
clients = {}    # pid -> {key, chain}
daemon = daemon_pid()
end = time.time() + DURATION
next_stats = 0.0
emit("start", daemon=daemon)
while time.time() < end:
    now = time.time()
    if now >= next_stats:
        emit("stats", clients=len(clients), compilers=len(compilers), **stats())
        next_stats = now + 10
        d = daemon_pid()
        if d != daemon:
            emit("daemon_restart", old=daemon, new=d)
            daemon, compilers = d, {}
    kids = children(daemon) if daemon else set()
    for pid in kids - compilers.keys():
        args = cmdline(pid)
        key, outdir = key_of(args, pid)
        compilers[pid] = {"key": key, "outdir": outdir, "start": now, "exe": os.path.basename(args[0]) if args else "?"}
    live_clients = set()
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        pid = int(name)
        if pid == daemon:
            continue
        if pid in clients:
            live_clients.add(pid)
            continue
        try:
            if os.stat(f"/proc/{pid}").st_uid != ME or not os.readlink(f"/proc/{pid}/exe").endswith("/sccache"):
                continue
        except OSError:
            continue
        args = cmdline(pid)
        key, _ = key_of(args, pid)
        if key:
            clients[pid] = {"key": key, "chain": ancestors(pid), "seen": now}
            live_clients.add(pid)
    by_key = {c["key"]: pid for pid, c in clients.items() if pid in live_clients}
    for pid in list(compilers):
        if pid in kids:
            continue
        c = compilers.pop(pid)
        if not c["key"]:
            continue
        client = by_key.get(c["key"])
        gone = [p for p, cl in clients.items() if cl["key"] == c["key"] and p not in live_clients]
        out = produced(c["outdir"], c["key"][1], c["key"][2], c["start"])
        if client is None and gone and out is False:
            cl = clients[gone[0]]
            emit("killed", crate=c["key"][1], cwd=c["key"][0], ran_s=round(now - c["start"], 1),
                 exe=c["exe"], chain=cl["chain"])
        else:
            emit("ended", crate=c["key"][1], produced=out, client_alive=client is not None,
                 ran_s=round(now - c["start"], 1))
    for pid in [p for p in clients if p not in live_clients]:
        cl = clients.pop(pid)
        if not any(c["key"] == cl["key"] for c in compilers.values()):
            continue
        # The client went away while its compiler still runs: kill_on_drop is about to fire.
        emit("client_gone_mid_compile", crate=cl["key"][1], cwd=cl["key"][0], chain=cl["chain"])
    time.sleep(TICK)

events.close()
kills, ended, sigs = 0, 0, Counter()
for line in open(OUT + ".jsonl"):
    e = json.loads(line)
    if e["kind"] in ("killed", "client_gone_mid_compile"):
        kills += e["kind"] == "client_gone_mid_compile"
        comms = [a["comm"] for a in e["chain"]]
        sigs[" < ".join(comms[:6])] += e["kind"] == "client_gone_mid_compile"
    elif e["kind"] == "ended":
        ended += 1
print(json.dumps({"compilers_ended": ended, "clients_gone_mid_compile": kills,
                  "top_ancestor_chains": sigs.most_common(8)}, indent=1))
