//! Every shape a binding answers or takes, for the tier-parity walk: a tier
//! that marshals one differently, or leaks one, prints a different line.

use std::collections::HashMap;

use gossamer_binding::native::{BindingAbi, GosDynVariant};
use gossamer_binding::{
    BindingCallback, Bytes, DynValue, FromGos, GosError, GosStruct, PersistentCallback, SigType,
    ToGos, Type, Value, VariantArm, register_module,
};

/// Callables kept between calls.
static KEPT: std::sync::Mutex<Vec<PersistentCallback>> = std::sync::Mutex::new(Vec::new());

#[derive(Default, Clone, GosStruct)]
pub struct Probe {
    pub name: String,
    pub score: f64,
    pub on: bool,
    pub count: i64,
}

/// A declared arm set: the program reads it as the enum spelling the same
/// arms, `Shape`.
pub struct Shape(DynValue);

impl SigType for Shape {
    const TYPE: Type = <Shape as BindingAbi>::TYPE;
}

impl ToGos for Shape {
    fn to_gos(self) -> Value {
        self.0.to_gos()
    }
}

impl FromGos for Shape {
    fn from_gos(value: &Value) -> gossamer_binding::RuntimeResult<Self> {
        Ok(Self(DynValue::from_gos(value)?))
    }
}

impl BindingAbi for Shape {
    type Input = *const GosDynVariant;
    type Output = *mut GosDynVariant;
    const TYPE: Type = Type::Variant(&[
        VariantArm {
            name: "Pair",
            payload: &[Type::I64, Type::Bool],
        },
        VariantArm {
            name: "Label",
            payload: &[Type::String],
        },
    ]);

    unsafe fn from_input(input: *const GosDynVariant) -> Self {
        Self(unsafe { <DynValue as BindingAbi>::from_input(input) })
    }

    fn to_output(self) -> *mut GosDynVariant {
        <DynValue as BindingAbi>::to_output(self.0)
    }
}

fn int_of(value: Value) -> i64 {
    match value {
        Value::Int(n) => n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

register_module!(
    name: wire_probe,
    doc: "Every wire shape.",

    fn scalars(n: i64, x: f64, b: bool, c: char) -> String {
        format!("{n} {x} {b} {c}")
    }

    fn text(s: String) -> String {
        format!("<{s}>")
    }

    fn ints(xs: Vec<i64>) -> Vec<i64> {
        xs.iter().map(|x| x * 10).collect()
    }

    fn floats(xs: Vec<f64>) -> Vec<f64> {
        xs.iter().map(|x| x / 2.0).collect()
    }

    fn flags(xs: Vec<bool>) -> Vec<bool> {
        xs.iter().map(|x| !x).collect()
    }

    fn words(xs: Vec<String>) -> Vec<String> {
        xs.iter().map(|x| x.to_uppercase()).collect()
    }

    fn grid(n: i64) -> Vec<Vec<i64>> {
        (0..n).map(|i| (0..=i).collect()).collect()
    }

    fn maybe_int(n: i64) -> Option<i64> {
        (n > 0).then_some(n)
    }

    fn maybe_text(n: i64) -> Option<String> {
        (n > 0).then(|| format!("n{n}"))
    }

    fn maybe_float(n: i64) -> Option<f64> {
        (n > 0).then_some(n as f64 + 0.5)
    }

    fn maybe_words(n: i64) -> Option<Vec<String>> {
        (n > 0).then(|| (0..n).map(|i| format!("w{i}")).collect())
    }

    fn parse(s: String) -> Result<i64, String> {
        s.parse::<i64>().map_err(|e| e.to_string())
    }

    fn parse_float(s: String) -> Result<f64, GosError> {
        Ok(s.parse::<f64>()?)
    }

    fn split(s: String) -> Result<Vec<String>, GosError> {
        if s.is_empty() {
            return Err(GosError::new("empty").wrap("splitting"));
        }
        Ok(s.split(',').map(str::to_string).collect())
    }

    fn checked(ok: bool) -> Result<(), GosError> {
        if ok { Ok(()) } else { Err(GosError::new("refused")) }
    }

    fn chunk(b: Bytes) -> Result<Bytes, GosError> {
        let mut bytes = b.into_inner();
        bytes.reverse();
        Ok(Bytes::new(bytes))
    }

    fn tally(words: Vec<String>) -> HashMap<String, i64> {
        let mut out = HashMap::new();
        for word in words {
            *out.entry(word).or_insert(0) += 1;
        }
        out
    }

    fn rename(m: HashMap<String, String>) -> HashMap<String, String> {
        m.into_iter().map(|(k, v)| (v, k)).collect()
    }

    fn labels(n: i64) -> HashMap<i64, String> {
        (0..n).map(|i| (i, format!("L{i}"))).collect()
    }

    fn pair(n: i64) -> (i64, String) {
        (n * 2, format!("p{n}"))
    }

    fn triple(t: (i64, String, bool)) -> (i64, String, bool) {
        (t.0 + 1, t.1.repeat(2), !t.2)
    }

    fn probe(p: Probe) -> Probe {
        Probe {
            name: format!("{}!", p.name),
            score: p.score * 2.0,
            on: !p.on,
            count: p.count + 1,
        }
    }

    fn maybe_pair(n: i64) -> Option<(i64, String)> {
        (n > 0).then(|| (n, format!("m{n}")))
    }

    fn parse_maybe(s: String) -> Result<Option<i64>, String> {
        if s.is_empty() {
            return Ok(None);
        }
        s.parse::<i64>().map(Some).map_err(|e| e.to_string())
    }

    fn span(s: String) -> Result<(i64, i64), String> {
        let (a, b) = s.split_once('-').ok_or_else(|| format!("no dash in {s}"))?;
        Ok((a.len() as i64, b.len() as i64))
    }

    fn blobs(n: i64) -> Vec<Bytes> {
        (0..n).map(|i| Bytes(vec![b'a' + i as u8; i as usize + 1])).collect()
    }

    fn blob_sizes(bs: Vec<Bytes>) -> Vec<i64> {
        bs.iter().map(|b| b.0.len() as i64).collect()
    }

    fn probes(n: i64) -> Vec<Probe> {
        (0..n)
            .map(|i| Probe {
                name: format!("p{i}"),
                score: i as f64 / 2.0,
                on: i % 2 == 0,
                count: i * 10,
            })
            .collect()
    }

    fn total_count(ps: Vec<Probe>) -> i64 {
        ps.iter().map(|p| p.count).sum()
    }

    fn maybe_probe(n: i64) -> Option<Probe> {
        (n > 0).then(|| Probe { name: format!("q{n}"), score: 0.5, on: true, count: n })
    }

    fn pairs(n: i64) -> Vec<(i64, String)> {
        (0..n).map(|i| (i, format!("v{i}"))).collect()
    }

    fn pair_sum(ps: Vec<(i64, i64)>) -> i64 {
        ps.iter().map(|(a, b)| a * b).sum()
    }

    fn maybe_ints(xs: Vec<i64>) -> Vec<Option<i64>> {
        xs.into_iter().map(|x| (x != 0).then_some(x)).collect()
    }

    fn deep(n: i64) -> Option<Option<i64>> {
        match n {
            0 => None,
            1 => Some(None),
            _ => Some(Some(n)),
        }
    }

    fn dynamic(pick: i64) -> DynValue {
        match pick {
            0 => DynValue::List(vec![DynValue::Int(1), DynValue::String("two".to_string())]),
            1 => DynValue::Tagged {
                name: "Pair".to_string(),
                payload: vec![DynValue::Int(3), DynValue::Bool(true)],
            },
            _ => DynValue::Nil,
        }
    }

    fn shape(pick: i64) -> Shape {
        Shape(if pick == 0 {
            DynValue::Tagged {
                name: "Pair".to_string(),
                payload: vec![DynValue::Int(3), DynValue::Bool(true)],
            }
        } else {
            DynValue::Tagged {
                name: "Label".to_string(),
                payload: vec![DynValue::String("tag".to_string())],
            }
        })
    }

    cb_fn apply(d, f: BindingCallback, x: i64) -> i64 {
        int_of(f.invoke(d, vec![Value::Int(x)]).expect("callback"))
    }

    cb_fn keep(d, f: PersistentCallback) -> i64 {
        let _ = d;
        let mut kept = KEPT.lock().expect("kept");
        kept.push(f);
        i64::try_from(kept.len()).unwrap_or(i64::MAX)
    }

    cb_fn fire(d, x: i64) -> i64 {
        let kept: Vec<PersistentCallback> = KEPT.lock().expect("kept").clone();
        kept.iter()
            .map(|f| int_of(f.invoke(d, vec![Value::Int(x)]).expect("kept callback")))
            .sum()
    }

    fn forget() -> i64 {
        let mut kept = KEPT.lock().expect("kept");
        let n = kept.len();
        kept.clear();
        i64::try_from(n).unwrap_or(i64::MAX)
    }
);
