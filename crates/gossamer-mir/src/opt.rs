#![forbid(unsafe_code)]

include!("opt/entry.rs");
include!("opt/reserve.rs");
include!("opt/inline.rs");
include!("opt/simple_passes.rs");
include!("opt/rc_cleanup.rs");
include!("opt/loop_versioning.rs");
include!("opt/pop_into.rs");
include!("opt/tests.rs");
