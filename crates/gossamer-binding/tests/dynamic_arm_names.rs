//! A variant name a binding builds at run time crosses intact, however many
//! distinct names it builds.

use gossamer_binding::Value;
use gossamer_binding::conv::{DynValue, FromGos, ToGos};

#[test]
fn every_runtime_built_arm_name_round_trips() {
    for i in 0..2000 {
        let value = DynValue::Tagged {
            name: format!("Item-{i}"),
            payload: vec![DynValue::Int(i)],
        };
        let lowered: Value = value.clone().to_gos();
        assert_eq!(DynValue::from_gos(&lowered).unwrap(), value);
    }
}
