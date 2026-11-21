#![allow(missing_docs)]

//! What `gos check` reports when a program draws more than one kind of
//! finding. The editor reports the same set (pinned in the language
//! server's own tests).

mod common;

use common::{gos_check_str, stderr};

#[test]
fn a_warning_is_reported_beside_a_comptime_failure() {
    let out = gos_check_str(
        "comptime fn boom() -> i64 { panic(\"no\") }\nconst N: i64 = comptime { boom() }\nfn add(a: i64, b: i64) {\n    a + b\n}\nfn main() {\n    add(1, 2)\n    println(\"{}\", N)\n}\n",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("warning[GL0055]"), "{err}");
    assert!(err.contains("error[GX0005]"), "{err}");
}

#[test]
fn a_type_error_is_reported_before_the_comptime_fold_runs() {
    let out = gos_check_str(
        "comptime fn boom() -> i64 { panic(\"no\") }\nconst N: i64 = comptime { boom() }\nfn main() {\n    let s: String = 5\n    println(\"{}\", N)\n}\n",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("error[GT0001]"), "{err}");
    assert!(!err.contains("GX0005"), "{err}");
}
