//! DoS-surface limits on JSON, XML, and YAML parsers. Each parser
//! has a process-wide `max_depth` and `max_size` cap (`set_max_depth`
//! / `set_max_size`); the parse functions reject inputs that exceed
//! either before allocating proportional memory.

#![allow(missing_docs)]

use parking_lot::Mutex;

static LIMITS_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn json_rejects_oversize_input() {
    let _g = LIMITS_LOCK.lock();
    gossamer_std::json::set_max_size(64);
    let payload = format!("\"{}\"", "a".repeat(128));
    let err = gossamer_std::json::parse(&payload).expect_err("parse should reject oversize input");
    assert!(
        err.message.contains("max_size"),
        "expected max_size message, got {err:?}",
    );
    gossamer_std::json::set_max_size(usize::MAX);
}

#[test]
fn json_rejects_deep_nesting() {
    let _g = LIMITS_LOCK.lock();
    gossamer_std::json::set_max_depth(8);
    let mut deep = String::new();
    for _ in 0..20 {
        deep.push('[');
    }
    for _ in 0..20 {
        deep.push(']');
    }
    let err = gossamer_std::json::parse(&deep).expect_err("parse should reject too-deep nesting");
    assert!(
        err.message.contains("max_depth") || err.message.contains("depth"),
        "expected depth message, got {err:?}",
    );
    gossamer_std::json::set_max_depth(128);
}

#[test]
fn json_accepts_shallow_input() {
    let _g = LIMITS_LOCK.lock();
    let v = gossamer_std::json::parse("[1, 2, 3]").expect("shallow array parses");
    assert!(matches!(v, gossamer_std::json::Value::Array(_)));
}

#[test]
fn xml_rejects_oversize_input() {
    let _g = LIMITS_LOCK.lock();
    gossamer_std::encoding::xml::set_max_size(64);
    let payload = format!("<root>{}</root>", "a".repeat(200));
    let err = gossamer_std::encoding::xml::parse(&payload)
        .expect_err("xml parse should reject oversize input");
    assert!(
        err.message().contains("max_size"),
        "expected max_size in xml error, got {err:?}",
    );
    gossamer_std::encoding::xml::set_max_size(usize::MAX);
}

#[test]
fn xml_rejects_deep_nesting() {
    let _g = LIMITS_LOCK.lock();
    gossamer_std::encoding::xml::set_max_depth(8);
    let mut deep = String::new();
    for _ in 0..20 {
        deep.push_str("<a>");
    }
    for _ in 0..20 {
        deep.push_str("</a>");
    }
    let err = gossamer_std::encoding::xml::parse(&deep)
        .expect_err("xml parse should reject too-deep nesting");
    assert!(
        err.message().contains("max_depth") || err.message().contains("depth"),
        "expected xml depth message, got {err:?}",
    );
    gossamer_std::encoding::xml::set_max_depth(128);
}

#[test]
fn xml_accepts_shallow_input() {
    let _g = LIMITS_LOCK.lock();
    let n = gossamer_std::encoding::xml::parse("<root><a>hi</a></root>").expect("parses");
    assert_eq!(n.name(), Some("root"));
}

#[test]
fn yaml_rejects_oversize_input() {
    let _g = LIMITS_LOCK.lock();
    gossamer_std::encoding::yaml::set_max_size(64);
    let payload = "key: ".to_string() + &"a".repeat(200);
    let err = gossamer_std::encoding::yaml::parse(&payload)
        .expect_err("yaml parse should reject oversize input");
    assert!(
        err.message.contains("max_size"),
        "expected yaml max_size message, got {err:?}",
    );
    gossamer_std::encoding::yaml::set_max_size(usize::MAX);
}

#[test]
fn yaml_rejects_deep_nesting() {
    let _g = LIMITS_LOCK.lock();
    gossamer_std::encoding::yaml::set_max_depth(4);
    let payload = "a: [[[[[[[[x]]]]]]]]\n";
    let err = gossamer_std::encoding::yaml::parse(payload)
        .expect_err("yaml parse should reject too-deep nesting");
    assert!(
        err.message.contains("max_depth") || err.message.contains("depth"),
        "expected yaml depth message, got {err:?}",
    );
    gossamer_std::encoding::yaml::set_max_depth(128);
}

#[test]
fn yaml_accepts_shallow_input() {
    let _g = LIMITS_LOCK.lock();
    let v = gossamer_std::encoding::yaml::parse("name: ada\nage: 30\n").expect("parses");
    assert!(v.get("name").is_some());
}
