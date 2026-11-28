# Leto FUSE3 stream jobserver

`leto-jobserver` is a standalone, unprivileged Linux FUSE3 stream jobserver
replacing the legacy FIFO + external PSI governor + sccache private pipe.

It speaks Linux FUSE 7.38 directly over `/dev/fuse` and `/usr/bin/fusermount3`
with `FOPEN_DIRECT_IO | FOPEN_NONSEEKABLE | FOPEN_STREAM | FOPEN_PARALLEL_DIRECT_WRITES`
(no `libfuse3-dev`, no root, no `/dev/cuse`).

## Key properties

- **Leak reclamation on handle close:** `jobserver-rs` opens `fifo:PATH` twice
  (read and write descriptors). `leto-jobserver` groups open handles by process
  identity `(tgid, starttime)` (resolved at `FUSE_OPEN`, or a synthetic per-fh
  owner when `pid == 0` across PID namespaces) and reclaims all tokens held by
  that owner when the kernel sends the final `FUSE_RELEASE` for its open file
  descriptions (for example when the process exits or is `SIGKILL`ed). Note: if
  a process opens the token file without `O_CLOEXEC` and forks a child that
  keeps the descriptor open, the kernel defers `FUSE_RELEASE` until the child
  also closes it; `jobserver-rs` opens with `O_CLOEXEC`.
- **Arrival-ordered fairness & per-TGID cap:** blocking readers (`rustc`) and
  poll clients (`jobserver-rs`, GNU `make`) share one arrival-ordered queue with
  short-lived poll reservations (50 ms) so poll clients do not starve behind
  blocking readers. `--max-per-owner N` (or live `set-cap N`) caps concurrent
  tokens per process without head-of-line blocking uncapped waiters or spinning
  `FUSE_POLL`.
- **Graceful SIGTERM drain:** on `SIGTERM`/`SIGINT`, the daemon unlinks its
  `--path` symlink (preventing new opens) and continues serving existing open
  handles until `open_fhs == 0` or `--drain-timeout-secs` (default 300 s)
  elapses. A second signal forces immediate exit. Operators should still prefer
  restarting at `open_fhs == 0`.
- **Built-in PSI I/O throttling:** samples `/proc/pressure/io` (`full avg10`)
  with a moving average, hysteresis (`--psi-high` / `--psi-low`), and
  `--psi-max-withhold-fraction`, logging `memory some avg10` on transitions.
  Repeated PSI read errors release withheld tokens step-by-step.
- **Control & observability:** local `0600` Unix socket (verified with
  `SO_PEERCRED`, protected by an exclusive `<socket>.lock` `flock`) serving
  JSON `status`, `resize`, and `set_max_per_owner`, consumed by
  `leto/health/health-report.py`.

## Known stream-file semantics

- **Cross-process write-back:** a token written back by a different process
  than the one that acquired it is counted as `excess_writes_total` and ignored
  (preventing pool inflation); the acquirer's token is reclaimed when its own
  handles close.
- **`SA_RESTART` on parked reads:** Linux FUSE delivers `FUSE_INTERRUPT` on
  signals and returns `EINTR` even when `SA_RESTART` is set; standard jobserver
  clients (`jobserver-rs`, GNU `make`) retry `EINTR` in a loop.
- **`RLIMIT_FSIZE=0`:** because the stream node has mode `S_IFREG`, writing a
  token under `RLIMIT_FSIZE=0` fails with `EFBIG`; the token is reclaimed on
  process exit.

## Usage

```sh
leto-jobserver serve \
  --tokens 48 \
  --mount-dir "$XDG_RUNTIME_DIR/leto-jobserver" \
  --path "$XDG_RUNTIME_DIR/build-jobserver" \
  --socket "$XDG_RUNTIME_DIR/leto-jobserver.sock" \
  --max-per-owner 16 \
  --drain-timeout-secs 300

leto-jobserver status
leto-jobserver resize 64
leto-jobserver set-cap 12
```

Pair with `sccache` by setting `[server] jobserver_fifo = "/run/user/1000/build-jobserver"`
(or `SCCACHE_SERVER_JOBSERVER_FIFO`) so `sccache` uses an in-process admission
semaphore and forwards `--jobserver-auth=fifo:...` to `rustc` without acquiring
from the host pool.
