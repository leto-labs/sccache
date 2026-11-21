# Fleet build health

Run from the sccache checkout on devbox every two hours:

```sh
taskset -c 0-15,32-47 leto/health/collect.sh
```

The collector runs the local report and sends the same Python script over
stdin to worker-01 and worker-02 with Tailscale SSH as `leovigna`. Each host
needs Python 3.11+, Git, GNU coreutils, util-linux and access to the sccache
user unit. The workers need no copy of this checkout. Run from an account
with non-interactive Tailscale SSH access to both workers.

Each collection writes timestamped `leto-build-health-fleet/1` JSON and Markdown
to `~/data/core/build-health/` and updates `latest.json` and `latest.md`.
Core reads `latest.md`. A failed collection leaves the other hosts' reports
available and gives the affected host a warning row. The headline miss cause
is the most frequent miss classification across the host's 24-hour log window.
Use `--out-dir` or `--timeout` to override the collector defaults.

For a single host:

```sh
python3 leto/health/health-report.py --format markdown --hours 24
python3 leto/health/health-report.py --help
```

Host JSON uses schema `leto-build-health/1`. The only daemon request is
`--show-stats --stats-format=json` on port 4226 by default. The hit rates
divide hits by hits plus misses since the server started. The stats client
gets an empty configuration and an unwritable fallback cache path so a
disconnected client cannot initialize the shared cache. The report reads the
host TOML separately. It records the installed binary's version and SHA-256
along with the server-reported version when available.

Cache fill uses the server's tracked, potentially weighted size and cap when
available. A full cache sits at its cap by design, so the report warns only
when the tracked size exceeds the cap by more than 1%. "On disk" divides the
physical footprint (btrfs extents when compsize ran, else allocated bytes) by
the cap and warns above 110%: reflink weights estimate compressed size, so a
btrfs cache can hold far more logical bytes than its cap. The disk columns record GNU `du` apparent and allocated bytes.
On btrfs, passwordless `sudo -n compsize` adds extent figures when it finishes
within 60 seconds. The two-level cache entry count has a two-second budget.
Both cache walks and incremental `du` run at idle CPU and I/O priority.

The miss window includes the current log and its `.1` archive. Each component
kind or extern counts at most once per miss. In the pre-warm section, age is
time since the last successful warm using the `last-warmed` marker when
available. Commit lag compares that commit with the canonical local branch
in the project's config.

Incremental discovery checks `target/<profile>/incremental` and
`target/<triple>/<profile>/incremental`. With `--fast`, or when discovery
finds more than 100 worktrees, the report samples the 20 newest worktree
targets by checkout or target mtime. It estimates each project's worktree
totals from that project's sample and measures canonical checkouts directly.
An unsampled project has an unknown total. Use `--no-fast` to disable sampling.
The incremental disk measurements have a shared 90-second budget.

## Weekly miss-log rotation

`MissExplain::record` opens the active file in append mode for every record,
writes it, and closes the handle when it returns.
[Writer code](../../src/miss_explain.rs#L268-L289)

`--rotate` renames the active file to `.1` and creates an empty current file
if the writer has not already recreated it. An append already holding an open
descriptor finishes in the archived inode. Later appends open the current path.
Each rotation replaces the previous archive. A missing or empty active log
leaves the archive in place.

Core can install the provided systemd user units on each host as the
sccache user:

```sh
install -Dm755 leto/health/health-report.py ~/.local/bin/leto-health-report
install -Dm644 leto/health/systemd/leto-misses-rotate.service ~/.config/systemd/user/leto-misses-rotate.service
install -Dm644 leto/health/systemd/leto-misses-rotate.timer ~/.config/systemd/user/leto-misses-rotate.timer
systemctl --user daemon-reload
systemctl --user enable --now leto-misses-rotate.timer
```

The service reads the same miss log path as the report. If the daemon
overrides the configured path, supply `--miss-log` in the service command.

Run the fixture suite with `python3 -B -m unittest discover -v -s leto/health`.

## Fast and deep modes

The default run is fast and bounded, for the 2-hourly check:
- `--show-stats` (15 s limit), `/proc` reads, `df` of the cache filesystem and pre-warm `status.json`;
- the last 8 MiB of the current miss log (`--miss-log-tail`), so the miss window can start later than `--hours`;
- no du, compsize, cache walk or incremental-session scan;
- a host whose memory PSI `some avg10` exceeds 10% (`--max-memory-pressure`) is skipped and reported as "skipped: under pressure";
- the host script ends itself after 25 s (`--time-limit`), and `collect.sh` gives each host 30 s.

`--deep` (`collect.sh --deep`) adds du, compsize, the entry count, cache entries written in the last hour, the full miss logs and the incremental-session scan. It refuses to run when the 1-minute load exceeds half the CPUs or memory PSI exceeds 2%, runs at nice 19 and idle I/O priority, and stops after 900 s. Run it at most once a day.
