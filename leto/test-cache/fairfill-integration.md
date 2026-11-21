# FairFill nextest integration

The patch selects the devbox installation built from sccache commit `36a65426`. Set `LETO_TEST_CACHE=off` to opt out.

```diff
diff --git a/scripts/cargo.sh b/scripts/cargo.sh
--- a/scripts/cargo.sh
+++ b/scripts/cargo.sh
@@ -19,6 +19,16 @@
   test | nextest | bench) unset T3CODE_THREAD_ID ;;
 esac

+# Collect test-result evidence during nextest runs when the runner is installed.
+if [[ ${1:-} == nextest && ${LETO_TEST_CACHE:-} != off ]]; then
+  leto_test_runner="$HOME/.local/opt/leto-test-runner-36a65426/bin/leto-test-runner"
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
