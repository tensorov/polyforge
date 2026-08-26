//! T2 kill-tests: the set-once executor selection guard.
//!
//! This file is a dedicated test PROCESS on purpose: `init_executor` pins
//! the process-global `EXECUTOR_KIND` permanently (set-once CAS), so any
//! test that flips it must not share a process with tests expecting another
//! selection. Each `tests/*.rs` file compiles to its own binary, isolating
//! the flip from the lib unit-test binary and from other integration files.
//!
//! Kills runner.rs init_executor guard mutant (`prev == code` -> always
//! true): with that mutation, a conflicting re-selection would be silently
//! accepted as idempotent instead of rejected.

use polyforge_toolrunner::{init_executor, ExecutorKind};

#[test]
fn set_once_selection_rejects_conflicting_reinit() {
    init_executor(ExecutorKind::Process).expect("first process selection");
    init_executor(ExecutorKind::Process).expect("repeat of the same kind stays idempotent");

    let err = init_executor(ExecutorKind::Sandbox)
        .expect_err("conflicting re-selection must be rejected");

    #[cfg(feature = "sandbox-mock")]
    {
        // With the feature compiled in, Sandbox passes the feature gate and
        // dies at the set-once check, which names the recorded kind.
        assert_eq!(
            err, "executor already initialized to process",
            "set-once guard must pin the recorded kind: {err}"
        );
    }
    #[cfg(not(feature = "sandbox-mock"))]
    {
        // Without the feature the fail-closed gate rejects Sandbox before
        // any state is written; the exact message is part of the contract.
        assert_eq!(err, "sandbox executor requires feature sandbox-mock");
    }

    init_executor(ExecutorKind::Process)
        .expect("the recorded selection must survive the rejection untouched");
}
