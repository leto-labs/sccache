# Accounting jobservers (`steve` / `guildmaster`) — evaluation notes

Status: deferred (2026-11-28). Ship `-13` (process-group kill) and `-14`
(`leto-4ehq` private-pool swap) first; keep this record for a future pilot.

## Why evaluated

Standard GNU make jobserver pipes and FIFOs are stateless byte queues: a
process killed (`SIGKILL`, OOM, or `kill_on_drop`) while holding a token byte
never writes it back. Accounting jobservers (`steve` via CUSE character device,
`guildmaster` / NixOS jobserver via per-open-fd tracking) reclaim tokens when
the acquiring process exits, even under `SIGKILL`.

## Compatibility findings (measured 2026-11-28 on Linux `7.0.0-43-generic`)

- **Clients (`cargo`, `rustc` 1.97.1 / 1.99.1, `jobserver` 0.1.35, `GNU Make`
  4.4.1, `sccache`):** accept `--jobserver-auth=fifo:<path>` pointing at a
  character device (`S_ISCHR`), tested live against a PTY. `jobserver` opens
  the path with `OpenOptions` and does not assert `S_ISFIFO`.
- **`nasm-rs` #44 pattern:** none of our workspace build scripts release
  unacquired tokens; accounting servers reject spurious releases cleanly.
- **`steve` Linux TID bug (`steve.cxx:570`):** `steve` calls
  `pidfd_open(tid, 0)` on the `fuse_req_ctx(req)->pid`, which FUSE populates
  with the requesting thread's **TID**, not TGID. On Linux `7.0.0-43-generic`,
  `pidfd_open(non_main_tid, 0)` without `PIDFD_THREAD` fails with `ENOENT`
  (or `EINVAL` on older kernels), breaking multi-threaded clients like `rustc`
  and `cargo`. Any future `steve` pilot requires patching `steve.cxx:570` to
  resolve TGID (or handle `PIDFD_THREAD` / TGID-level accounting).

## The `sccache` admission-token gap and deadlock trap

`steve` reclaims tokens only when the **acquiring process** exits.

- Today the long-lived `sccache` daemon acquires each compile's admission token
  itself, so a killed compiler leaves the daemon holding its admission token
  until `Child`/`CommandToken` drops (which is fine on clean cancellation, but
  does not help if the daemon's own private pipe loses extra codegen tokens
  taken by `rustc`).
- **Deadlock trap:** if the daemon shares a single `steve` device for *both*
  its own admission tokens and the jobserver handed to `rustc` children, a
  per-process cap or reclaim boundary can deadlock when the daemon holds all
  admission tokens waiting on children that try to acquire extra codegen tokens
  from the same pool.
- **Required daemon split (`Semaphore` + `child_jobserver`):** for a future
  `steve` pilot, separate daemon-side admission concurrency (an in-process
  `tokio::sync::Semaphore` sized to the host cap, never leaking across killed
  compilers because `CommandToken` drops inside the daemon) from the
  `child_jobserver` path handed to compilers (`--jobserver-auth=fifo:/dev/steve`
  or per-host device), so compilers acquire/return codegen tokens directly
  against `steve` and the kernel reclaims them on `SIGKILL`.
