#![forbid(unsafe_code)]

include!("opt/entry.rs");
include!("opt/unbox_carriers.rs");
include!("opt/reserve.rs");
include!("opt/inline.rs");
include!("opt/simple_passes.rs");
include!("opt/rc_cleanup.rs");
include!("opt/share_transfer.rs");
include!("opt/loop_versioning.rs");
include!("opt/row_tables.rs");
include!("opt/payload_views.rs");
include!("opt/byte_append_loops.rs");
include!("opt/pop_into.rs");
include!("opt/tests.rs");
