# Leto FUSE3 stream jobserver

`leto-jobserver` is a standalone, unprivileged Linux FUSE3 stream jobserver
replacing the legacy FIFO + external PSI governor + sccache private pipe.

It speaks Linux FUSE 7.38 directly over `/dev/fuse` and `/usr/bin/fusermount3`
with `FOPEN_DIRECT_IO | FOPEN_NONSEEKABLE | FOPEN_STREAM | FOPEN_PARALLEL_DIRECT_WRITES`
(no `libfuse3-dev`, no root, no `/dev/cuse`).

## Key properties

- **Immediate leak reclamation:** `jobserver-rs` opens `fifo:PATH` twice (read
  and write descriptors). `leto-jobserver` groups open handles by process TGID
  (resolved once at `FUSE_OPEN`) and unconditionally reclaims all tokens held
  by a TGID when its last `FUSE_RELEASE` arrives (including on `SIGKILL`/OOM).
- **Per-TGID fairness cap:** `--max-per-owner N` (or live `set-cap N`) caps
  concurrent tokens per process without head-of-line blocking uncapped waiters
  or spinning `FUSE_POLL`.
- **Built-in PSI I/O throttling:** samples `/proc/pressure/io` (`full avg10`)
  with a moving average, hysteresis (`--psi-high` / `--psi-low`), and
  `--psi-max-withhold-fraction`, and logs `memory some avg10` alongside
  transitions without external state files.
- **Control & observability:** local `0600` Unix socket (verified with
  `SO_PEERCRED`) serving JSON `status`, `resize`, and `set_max_per_owner`,
  consumed by `leto/health/health-report.py`.

## Usage

```sh
leto-jobserver serve \
  --tokens 48 \
  --mount-dir "$XDG_RUNTIME_DIR/leto-jobserver" \
  --path "$XDG_RUNTIME_DIR/build-jobserver" \
  --socket "$XDG_RUNTIME_DIR/leto-jobserver.sock" \
  --max-per-owner 16

leto-jobserver status
leto-jobserver resize 64
leto-jobserver set-cap 12
```

Pair with `sccache` by setting `[server] jobserver_fifo = "/run/user/1000/build-jobserver"`
(or `SCCACHE_SERVER_JOBSERVER_FIFO`) so `sccache` uses an in-process admission
semaphore and forwards `--jobserver-auth=fifo:...` to `rustc` without acquiring
from the host pool.
