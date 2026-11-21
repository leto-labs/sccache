# FairFill nextest integration

The devbox runner is built from sccache commit `4c75a082`. Set `LETO_TEST_CACHE=off` to opt out.

For an existing integration, update the runner path in `scripts/cargo.sh`.

```diff
-  leto_test_runner="$HOME/.local/opt/leto-test-runner-36a65426/bin/leto-test-runner"
+  leto_test_runner="$HOME/.local/opt/leto-test-runner-4c75a082/bin/leto-test-runner"
```

For a fresh integration, apply the patch below.

```diff
diff --git a/scripts/cargo.sh b/scripts/cargo.sh
--- a/scripts/cargo.sh
+++ b/scripts/cargo.sh
@@ -19,6 +19,16 @@
   test | nextest | bench) unset T3CODE_THREAD_ID ;;
 esac

+# Collect test-result evidence during nextest runs when the runner is installed.
+if [[ ${1:-} == nextest && ${LETO_TEST_CACHE:-} != off ]]; then
+  leto_test_runner="$HOME/.local/opt/leto-test-runner-4c75a082/bin/leto-test-runner"
+  if [[ -x $leto_test_runner ]]; then
+    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="$leto_test_runner"
+    export LETO_TEST_CACHE_WORKSPACE="$PWD" LETO_TEST_CACHE_PROJECT=fairfill
+    export LETO_TEST_CACHE_ENV="${LETO_TEST_CACHE_ENV-FAIRFILL_*,FOUNDRY_*,RUST_*,PROPTEST_*}"
+  fi
+fi
+
 # Host-wide build slots: parallel lanes share 32 build CPUs, so at most
 # FAIRFILL_BUILD_SLOTS (default 6, 0 disables) heavy cargo commands run at once.
 # Two more "urgent" slots exist for FAIRFILL_BUILD_URGENT=1: the parent's merge
```

## Bumping the runner

FairFill pins the runner path in two files, and both must change together:
- `scripts/cargo.sh` (`leto_test_runner=...`);
- `scripts/test_cargo_wrapper.py`, whose `TestCacheRunnerTests` build a fake home with the same versioned path.

Send both changes in one message. The 4c75a082 bump (2026-11-22) failed 2 of those tests until the second file was updated.
