# Jobserver pool swap after killed compilers (leto-4ehq)

Status: design, revised after one review round (2026-11-27). Not implemented.

## Problem

The daemon runs compilers against one private jobserver pipe
(`jobserver::Client::new(cpus)`, src/jobserver.rs). Each compile holds one
daemon token (`CommandToken`), and rustc takes more tokens from the pipe for
codegen. It also lends out its implicit token. A compiler killed while it
holds tokens never returns them:
- `kill_on_drop` when its client disconnects;
- the OOM killer.

The pool shrinks until no compile can start. With about 22 Markets-rs lanes
on worker-01 (36 tokens), 14 tokens were lost about 5 h after a restart
(2026-11-27). The only recovery today is a SIGTERM restart (build-bench
runbook), which Core applies every 2 h.

There is a second problem. `kill_on_drop` kills only the direct child. A
linker or build helper that the killed rustc started keeps running, keeps its
tokens, and can still write its output after a retry has written the same
path. Restarts have the same exposure.

Rejected in earlier review rounds:
- Repairing the pipe in place. Tokens in flight, and tokens that surviving
  descendants hold, cannot be told from lost ones.
- Not killing the compilers of disconnected clients. This leaves a stale
  artifact.
- A daemon self-exit. A cache restore can outlast the drain, and a
  client-side lookup fails (leto-vk2f).

## Design

### 0. Kill the whole compiler process group (prerequisite)

Spawn compilers in their own process group with `CommandExt::process_group(0)`,
stable since 1.64 and below our MSRV, which keeps `posix_spawn`. Replace
`kill_on_drop` with a guard. If the child has not been reaped, it sends
`SIGKILL` to `-pgid`, then lets tokio reap the child.

After that, nothing a killed compiler started survives, except processes that
call `setsid` themselves, which compilers and linkers do not. So nothing holds
old tokens, and nothing writes outputs late. This alone removes the
stale-output exposure, with or without a swap.

### 1. Pool generations (Linux only at first)

Split each generation into two parts:
- `Pipe`: a `jobserver::Client`. `Acquired` tokens and `configure` hold it.
- `Helper`: a `HelperThread` and its request channel. Only the `Client`
  holds it.

`Client` keeps `current: Mutex<Generation { id, pipe: Arc<Pipe>, helper: Arc<Helper> }>`
and a `watch<u64>` of the id.

On macOS, jobserver creates the pipe with `pipe()` followed by `fcntl`, so a
concurrent fork can inherit it. macOS keeps a single pool.

### 2. Atomic admission

`acquire_for_command` loops with one `TokenUse` for the whole wait. Each pass:
1. Subscribe to the id first, then snapshot the generation.
2. Request a token from that generation's helper.
3. Wait for either the token or a change of id.
4. On a token: take `current`'s mutex. If the id is unchanged, mark the
   command running (`TokenUse::acquired`) inside that critical section and
   return a `CommandToken` holding the token and its `Arc<Pipe>`. Otherwise
   drop the token, which returns it to its own old pipe, and loop again.
5. On a change of id: drop the request and loop again. If the old helper later
   fills the dropped oneshot, that token goes back to the old pipe.

The swap publishes under the same mutex, so no command can be admitted on a
stale generation after the swap.

Waiters lose their FIFO position at a swap. That is acceptable: swaps are rare
and every waiter re-requests at once.

### 3. Spawn order

The order becomes: acquire, then configure the child with the token's `Pipe`
(only on the existing private-sharing branch), then spawn. The `Arc<Pipe>`
lives through `spawn()` because `pre_exec` captures raw fds.

The rest stays as it is:
- the FIFO paths: a forwarded named FIFO still takes a daemon admission token;
  `from_env_fifo` takes none but is still configured;
- non-sharing spawns;
- the spawn guard, which still covers only the OS spawn, never the token wait.

### 4. Triggers and swap

The existing 5 s stall check evaluates the triggers off the run loop. It
never returns from its `select!` branch, on success or on failure.

Two triggers, each with its own state, not `stalled_for`:
- **Hard stall:** the current pipe is empty, `running == 0` and
  `waiting > 0`, continuously for 120 s within the current generation.
- **Idle deficit:** `running == 0` and `waiting == 0`, and the pipe is short
  of capacity by at least a quarter, by the same count, for 10 min within the
  current generation. This covers the partial loss on worker-01, where 22 of
  36 slots are left and progress continues. It can be switched off with
  `[server] jobserver_swap = "stall"`, and `"off"` disables both. The default
  is "stall,idle".

Why a false positive is harmless:
- After the process-group kill, at `running == 0` nothing outside the daemon
  holds pipe tokens. A deficit can only be lost tokens, or a token in flight
  in our own helper.
- An unneeded swap only redirects future acquisitions to a full new pool.
  Tokens in flight return to the old pipe.
- Nothing is ever written into a pipe by count.

The swap itself:
1. On a trigger, build a candidate (a pipe and a helper) in `spawn_blocking`,
   outside every lock.
2. Under `current`'s mutex, check the trigger condition and the id again, then
   publish.
3. If creation fails, for example from fd exhaustion, log an error, keep the
   old pool, and back off for 10 min.

### 5. Retiring the old helper

At the swap, the old `Helper` is taken out of `current`. Its request channel
is closed, so it serves no new requests, and it is handed to a dedicated
retirement thread: a plain `std::thread`, never a tokio worker. That thread
drops it, which interrupts and joins the helper.

jobserver gives up the join after about 1 s and detaches a stuck helper.
Retirements that were not confirmed are counted. Above 4, swapping stops,
with an error log and a health warning. Recovery then falls back to the
runbook restart.

The old `Pipe` closes when its last token is dropped.

### 6. Gauge, stats, logs

- `tokens_in_use` samples the current generation's pipe. `CompileQueue` holds
  a handle to `current`, not a pipe clone.
- Each swap logs at error level: the generation, the trigger, the old pipe's
  tokens in use, and its age.
- `pool_swaps: Option<u64>` and `retire_pending: Option<u64>` are appended to
  the stats wire after `stalled_for`, keeping the -11 placeholder. They are
  absent from older daemons.
- The health report warns when swaps rise. A swap count measures recoveries,
  not the leak, so the report keeps showing tokens in use with nothing
  running.

## Bounds

- **No accounting race.** No decision writes tokens by count.
- **No compounding overrun.** A process-group kill leaves no holders of old
  tokens, and an old pipe is never refilled. Compiles admitted on the old
  generation just before a swap finish normally and return their tokens there.
- **No cut-off compile, no client error.** Waiters move to the new pool, and
  the daemon keeps running.
- **Fd and thread use stay bounded:** at most one live helper per generation
  that still has holders, plus at most 4 unconfirmed retirements.

## Tests

- **Process group:**
  - a compiler that starts a grandchild, a "linker" that sleeps and then
    writes a file, is killed by drop; the grandchild is gone and never writes;
  - normal exit is unaffected.
- **Admission races (paused time, with injected hooks):**
  - a swap published between the snapshot and the subscribe;
  - a token and the swap ready at once;
  - a cancellation after delivery.
  In each case: exactly one admission, the old token back in the old pipe, and
  one `TokenUse` throughout.
- **Retirement:** repeated swaps with an empty old pipe; a helper that does
  not exit; the cap reached.
- **Triggers:**
  - a hard stall with real killed children (a child takes a token from the
    pipe and is killed, repeatedly) recovers;
  - a partial loss swaps at idle;
  - no swap while a compile runs;
  - creation failure keeps the old pool;
  - shutdown during preparation.
- **Spawn:**
  - a child after a swap reads tokens from the new pipe's advertised fds;
  - both FIFO paths and a non-sharing child are unchanged;
  - the spawn-guard scope is unchanged.
- **Wire:** `pool_swaps` and `retire_pending` both ways against -10, -11 and
  -12, for both response kinds and without a gauge.

## Rollout

Ship the process-group kill first, as its own release, because it is smaller
and independently valuable. Then the swap, with `jobserver_swap = "stall"` on
worker-01 for a day, then "stall,idle", then the other hosts. Each step needs
a review round.
