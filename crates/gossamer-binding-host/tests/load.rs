//! Binding libraries built from the fixture crates, loaded and called
//! through a VM.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use gossamer_binding_host::{HostError, Signatures};
use gossamer_interp::Vm;
use gossamer_interp::value::{RuntimeError, SmolStr, Value};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Builds the fixture crate `name` as a shared library and answers its path.
fn build(name: &str) -> PathBuf {
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/binding-host-fixtures")
        .join(name);
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--quiet", "--manifest-path"])
        .arg(fixture(name).join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target)
        .status()
        .expect("cargo runs");
    assert!(status.success(), "building fixture {name}");
    target.join("release").join(format!(
        "{}{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

/// The probe library and its signatures, installed once for this process.
fn probe() -> &'static (PathBuf, Signatures) {
    static PROBE: OnceLock<(PathBuf, Signatures)> = OnceLock::new();
    PROBE.get_or_init(|| {
        let path = build("host_probe");
        let signatures =
            gossamer_binding_host::read_library_signatures(&path).expect("probe loads");
        gossamer_binding_host::install(&[(path.clone(), signatures.clone())]);
        (path, signatures)
    })
}

fn call(name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
    probe();
    Vm::new().call(name, args)
}

fn text(s: &str) -> Value {
    Value::String(SmolStr::from_str(s))
}

#[test]
fn a_library_states_every_item_and_its_wire_symbol() {
    let (_, signatures) = probe();
    let module = signatures
        .modules
        .iter()
        .find(|m| m.path == "host_probe")
        .expect("host_probe module");
    let double = module
        .items
        .iter()
        .find(|i| i.name == "double")
        .expect("double");
    assert_eq!(double.wire_symbol, "gos_binding_host_probe__double");
    assert!(signatures.modules.iter().any(|m| m.path == "Counter"));
}

#[test]
fn scalars_and_strings_cross_both_ways() {
    assert!(matches!(
        call("host_probe::double", vec![Value::Int(21)]),
        Ok(Value::Int(42))
    ));
    let Ok(Value::String(s)) = call("host_probe::greet", vec![text("world")]) else {
        panic!("greet answers a string");
    };
    assert_eq!(s.as_str(), "hello, world");
}

#[test]
fn bytes_maps_tuples_and_options_cross() {
    let bytes = Value::ByteVec(Arc::new(vec![1, 2, 3]));
    let Ok(Value::ByteVec(back)) = call("host_probe::reverse", vec![bytes]) else {
        panic!("reverse answers bytes");
    };
    assert_eq!(back.as_slice(), &[3, 2, 1]);

    let mut map = gossamer_interp::value::dense_map_with_capacity(2);
    map.insert(
        gossamer_interp::value::MapKey::Str(SmolStr::from_str("a")),
        Value::Int(2),
    );
    map.insert(
        gossamer_interp::value::MapKey::Str(SmolStr::from_str("b")),
        Value::Int(5),
    );
    let map = Value::Map(Arc::new(parking_lot::Mutex::new(map.into())));
    assert!(matches!(
        call("host_probe::total", vec![map]),
        Ok(Value::Int(7))
    ));

    let Ok(Value::Tuple(pair)) = call("host_probe::pair", vec![Value::Int(9)]) else {
        panic!("pair answers a tuple");
    };
    assert!(matches!(pair[0], Value::Int(9)));
    assert!(matches!(&pair[1], Value::String(s) if s.as_str() == "9"));

    let Ok(Value::Variant(some)) = call("host_probe::maybe", vec![Value::Int(4)]) else {
        panic!("maybe answers a variant");
    };
    assert_eq!(some.name.to_string(), "Some");
}

#[test]
fn an_opaque_handle_keeps_its_state_in_the_library() {
    let handle = call("Counter::new", Vec::new()).expect("new");
    assert!(matches!(
        call("Counter::inc", vec![handle.clone()]),
        Ok(Value::Int(1))
    ));
    assert!(matches!(
        call("Counter::inc", vec![handle]),
        Ok(Value::Int(2))
    ));
}

#[test]
fn a_rust_panic_is_a_gossamer_panic_naming_the_item() {
    let err = call("host_probe::explode", vec![text("no window")]).unwrap_err();
    let RuntimeError::Panic(message) = err else {
        panic!("expected a panic, got {err:?}");
    };
    assert_eq!(message, "host_probe::explode: no window");
}

#[test]
fn a_wrong_argument_count_is_refused_before_the_call() {
    let err = call("host_probe::double", Vec::new()).unwrap_err();
    assert!(
        matches!(
            err,
            RuntimeError::Arity {
                expected: 1,
                found: 0
            }
        ),
        "{err:?}"
    );
}

#[test]
fn a_library_built_for_another_abi_major_is_refused() {
    let path = build("old_abi");
    let err = gossamer_binding_host::read_library_signatures(&path).unwrap_err();
    assert!(
        matches!(err, HostError::Abi { library, .. } if library == 2 << 16),
        "{err}"
    );
    assert!(err.to_string().contains("binding ABI 2.0"), "{err}");
}

#[test]
fn a_library_whose_signatures_changed_is_refused_before_any_call() {
    let (path, signatures) = probe();
    let mut stale = signatures.clone();
    stale.signature_hash = "0000000000000000".to_string();
    for module in &mut stale.modules {
        module.path = format!("stale_{}", module.path);
    }
    gossamer_binding_host::install(&[(path.clone(), stale)]);
    let err = Vm::new()
        .call("stale_host_probe::double", vec![Value::Int(1)])
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("changed since its signatures were recorded"),
        "{err}"
    );
}
