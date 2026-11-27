# Jobserver pool swap on a confirmed stall (leto-4ehq)

Status: design, not implemented.

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

Rejected in review:
- Repairing the pipe in place. Tokens in flight, and tokens that surviving
  descendants still hold, cannot be told from lost ones, so a refill can push
  the pool above capacity, and that can compound.
- Not killing the compilers of disconnected clients. A cancelled compile can
  finish after a newer one and leave a stale artifact.
- Exiting the daemon on a stall. A cache restore that outlasts the drain can
  overwrite a local retry, and a client-side preprocessor lookup fails
  (leto-vk2f).

## Idea

Never repair a pipe. On a confirmed stall, give new compiles a fresh pipe with
full capacity, and abandon the old one. Every token goes back to the pipe it
came from, so each pipe's accounting stays consistent. Nothing has to tell a
lost token from a borrowed one.

## Design

1. **Pool generations.** `jobserver::Client` keeps
   `current: Arc<Mutex<Arc<Pool>>>`. Each `Pool` holds its own
   `jobserver::Client`, its helper thread, its request channel, its capacity
   and a generation number. Clones of `Client`, held by `ProcessCommandCreator`
   and the service, share `current`. A `tokio::sync::watch<u64>` announces the
   generation.

2. **Acquire, then configure from the same pool.**
   - `acquire_for_command` reads the current pool, then waits for either:
     - its helper hands over a token: the result is a `CommandToken` that
       keeps `Arc<Pool>`; or
     - the generation changes: drop the request and retry on the new pool,
       keeping the same `TokenUse`, so the wait time and stats carry over.
   - `AsyncCommand::spawn` today calls `configure` before it acquires. It will
     acquire first, then `token.configure(&mut cmd)`, so a child always gets
     the fds of the pool its token came from.
   - Callers that spawn without sharing are unchanged.

3. **Swap trigger.** The existing 5 s stall check (`log_stalls`) swaps when
   `stalled_for >= 120 s`: `running == 0` and `waiting > 0`, as one unbroken
   stretch timed on queue transitions. It does this under the queue lock, and
   checks the condition again under that lock.
   - Swap: build `Pool::new(capacity)` (a pipe plus a helper thread). If that
     fails, for example from fd exhaustion, log an error and keep the old pool.
   - Store it in `current` and bump the generation.
   - Log an error naming the old pool's tokens in use and its age.
   - Count `pool_swaps` in the stats.

4. **The old pool.** It is dropped when its last `Arc` goes. Holders of that
   `Arc`:
   - `CommandToken`s still out. A compile admitted just before the swap runs
     on the old pool and returns its token there.
   - The old helper thread, which a cancelled request may still use for one
     read.

   `HelperThread`'s Drop interrupts and joins its thread. That runs from
   `spawn_blocking`, never on an async worker. Once the daemon has closed its
   fds, the pipe lives on only in surviving descendants, whose tokens go back
   into a pipe nobody reads.

5. **Gauge and stats.**
   - `tokens_in_use` reads the current pool.
   - `pool_swaps: u64` is appended to the stats wire after `stalled_for`, and
     is absent from older daemons.
   - The health report warns when `pool_swaps` grows: the leak rate. The
     runbook restart is still there if swaps loop.

## Why it is safe

- **No accounting race.** Nothing reads a pipe's counts to decide how many
  tokens to write. A swap only redirects future acquisitions.
- **Bounded overrun.** At the swap nothing runs (`running == 0`). Extra
  parallelism comes from two sources:
  - descendants of killed compilers still holding old tokens, which run
    anyway;
  - compiles admitted on the old pool in the instant before the swap.

  Neither grows across swaps. An old pool is never refilled, and each swap
  needs a new 120 s stall.
- **No compile is cut off,** and no client sees a disconnect. Waiters move to
  the new pool, and the daemon keeps running.
- **No artifact risk.** Kill-on-drop is unchanged, so outputs are written only
  by live compiles, exactly as today.

## Limits

- Fresh tokens can oversubscribe the CPUs while an orphan that holds many old
  tokens keeps running. That is the same as today without the swap, where
  those processes run too.
- A partial leak (pool short, but never fully stalled) is not repaired. It
  costs throughput until the next stall or restart. A lower trigger, such as
  swapping when tokens in use stay above half of capacity with nothing running
  for 10 min, would cover it with the same mechanism. Proposed as a later
  setting, off at first.
- Linux and macOS only, where the pool is a private pipe. Fifo and Windows
  pools are unchanged.

## Tests

- Unit (paused time):
  - with leaked tokens, a waiter stalls for 120 s, the swap happens, and the
    waiter gets a token from the new pool;
  - a token taken before the swap returns to the old pool, and the new pool
    stays at capacity;
  - the generation retry keeps the `TokenUse` wait time.
- Spawn: a child configured after a swap gets the new pool's fds. A child
  spawned before it keeps the old ones.
- A real-child reproduction: a child takes a token from the pipe and is killed
  while holding it, until the pool stalls. The swap restores throughput, and
  the old pipe's later returns do not reach the new one.
- Wire: `pool_swaps` appended, and -10, -11 and -12 frames read both ways.
- No self-exit, so the run loop is unchanged apart from the existing stall
  task.
