//! Memory-leak gate matrix.
//!
//! Each shape allocates heap memory inside a loop that runs many iterations.
//! If the per-iteration heap is reclaimed at scope end, peak RSS stays bounded
//! regardless of the iteration count; if it leaks, RSS grows with N and blows
//! past the cap. Every shape is gated three ways: peak RSS under the cap, no
//! reference-counted object left at exit (`RC_LIVE_AT_EXIT`), and the leak
//! ledger at the compiled baseline - one counter family alone misses leaks
//! the others see. The caught-panic shape is the one documented exemption:
//! compiled code does not reclaim the values of frames a contained panic
//! unwinds, and the gate pins that to the exact counts so it cannot grow.
//!
//! Run just this gate: `cargo test -p gossamer-cli --test leak_matrix -- --nocapture`

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

/// Peak RSS below this is "bounded"; a per-iteration leak at these iteration
/// counts pushes well past it (millions of small heap objects retained).
const CAP_KB: u64 = 60_000;

/// Shapes whose per-iteration heap MUST be reclaimed. Grows as phases land.
/// Phase 0: only the RC-managed control. Strings/Vec/Map/payloads are added by
/// their phases.
/// What a shape may leave behind at exit, beyond the runtime's own baseline.
struct Allowance {
    /// Live `String`s the leak ledger may report.
    strings: u64,
    /// Live `Vec`s the leak ledger may report.
    vecs: u64,
}

/// The compiled baseline: the runtime keeps one string and one vector.
const BASELINE: Allowance = Allowance {
    strings: 1,
    vecs: 1,
};

/// Shapes whose residue is documented rather than reclaimed, with the exact
/// counts the gate holds them to.
fn allowance(name: &str) -> Allowance {
    match name {
        // Forty contained panics, each unwinding a frame that holds two
        // strings in a vector: compiled code does not reclaim them (SPEC
        // section 8.5), and these counts must not grow.
        "caught_panic_frames" => Allowance {
            strings: 81,
            vecs: 41,
        },
        _ => BASELINE,
    }
}

/// (name, source). N is baked into each source, sized so a leak clears the cap.
const SHAPES: &[(&str, &str)] = &[
    (
        "closure_captured_map_escapes_through_call",
        r#"
struct Lookup { find: Fn(String) -> i64 }
enum Widget { Coll(Lookup) }

fn wrap(l: Lookup) -> Widget { Widget::Coll(l) }

fn view(i: i64) -> Widget {
    let mut index: Map<String, i64> = Map::new()
    index[f"k{i}"] = i
    let mut seen = Set::new()
    seen.insert(i)
    wrap(Lookup { find: |key: String| index.get(key).unwrap_or(-1) + seen.len() })
}

fn main() {
    let mut total = 0
    for i in 0..20000 {
        match view(i) {
            Widget::Coll(l) => total += (l.find)(f"k{i}"),
        }
    }
    println(f"{total}")
}
"#,
    ),
    (
        "lent_element_to_capturing_callee",
        r#"
struct Icon { rgba: Vec<u8> }
struct Entry { tags: Vec<String>, image: Option<Icon> }

fn tagged(entry: Entry, wanted: Vec<String>) -> bool {
    wanted.iter().any(|w| entry.tags.contains(w))
}

fn main() {
    let entries = #[Entry { tags: #["a"], image: Some(Icon { rgba: #[1u8, 2u8] }) }, Entry { tags: #["b"], image: Some(Icon { rgba: #[3u8] }) }]
    let mut hits = 0
    for round in 0..20000 {
        hits += entries.filter(|e| tagged(e, #["b"])).len()
    }
    println(f"{hits}")
}
"#,
    ),
    (
        "conditional_value_stored_in_element_field",
        r#"
struct Node { items: Vec<String> }
struct Deco { child: W, grow: i64 }
enum W { Empty, Field(Node), Decorated(Deco), Stack(Vec<W>) }
struct Win { key: String, root: W, toasts: Vec<String>, dirty: bool }
struct Sess { windows: Vec<Win>, count: i64 }

fn view(n: i64) -> W {
    W::Decorated(Deco { child: W::Field(Node { items: #[f"item {n}"] }), grow: 1 })
}

impl Sess {
    fn note(&mut self) { self.count += 1 }

    fn rebuild(&mut self, w: i64) {
        let v = view(self.count)
        let toasts = self.windows[w].toasts
        self.windows[w].root = if toasts.is_empty() { v } else { W::Stack(#[v, W::Empty]) }
        self.note()
        self.windows[w].dirty = true
    }
}

fn main() {
    let mut s = Sess { windows: #[Win { key: "main", root: W::Empty, toasts: #[], dirty: false }], count: 0 }
    for i in 0..20000 {
        s.rebuild(0)
    }
    println(f"{s.count}")
}
"#,
    ),
    (
        "closure_one_word_struct_capture",
        r#"
struct Order { order: Vec<i64> }

impl Order {
    fn source_of(&self, i: i64) -> i64 { if i < 0 || i >= self.order.len() { -1 } else { self.order[i] } }
}

struct List { count: i64, row: Fn(i64) -> i64 }

fn view(order: Order) -> List {
    let row = |i: i64| order.source_of(i)
    List { count: order.order.len(), row: row }
}

fn main() {
    let mut total = 0
    for i in 0..20000 {
        let l = view(Order { order: #[i, i + 1, i + 2] })
        let f = l.row
        total += l.count + f(0) + f(2)
    }
    println(f"{total}")
}
"#,
    ),
    (
        "io_memory_streams",
        r#"
use std::io

fn round(i: i64) -> i64 {
    let src = io::string_reader(f"payload {i}")
    let sink = io::buffer_writer()
    let teed = io::tee_reader(io::limit_reader(src, 5), sink)
    let r, w = io::pipe()
    io::write(w, "x")
    io::close_writer(w)
    let all = io::multi_reader(#[teed, r])
    io::drain(all).len() + io::contents(sink).len()
}

fn main() {
    let mut total = 0
    for i in 0..20000 { total += round(i) }
    println(f"{total}")
}
"#,
    ),
    (
        "aggregate_element_streams",
        r#"
struct Entry { name: String, n: i64 }

fn main() {
    let mut total = 0
    for round in 0..2000 {
        let pairs: Vec<(i64, String)> = (0..50).map(|i| (i, f"p{i}")).collect()
        let entries: Vec<Entry> = (0..50).map(|i| Entry { name: f"e{i}", n: i + round }).collect()
        let names: Vec<String> = (0..50).map(|i| f"n{i}").collect()
        for (u, s) in names.iter().enumerate() {
            total += u + s.byte_len()
        }
        for (k, s) in pairs.iter() {
            total += k + s.byte_len()
        }
        for (u, (k, s)) in pairs.iter().enumerate() {
            total += u + k + s.byte_len()
        }
        total += entries.iter().filter(|e| e.n % 2 == 0).count()
        total += entries.iter().map(|e| e.name.byte_len()).sum()
        match entries.iter().find(|e| e.n == round + 7) {
            Some(e) => total += e.name.byte_len()
            None => {}
        }
        let kept: Vec<Entry> = entries.iter().filter(|e| e.n % 3 == 0).collect()
        total += kept.len()
        total += pairs.iter().chain(pairs.iter()).count()
    }
    println(f"{total}")
}
"#,
    ),
    (
        "spawn_shared_read",
        r"
fn rows(n: i64) -> Vec<Vec<f64>> {
    let mut out: Vec<Vec<f64>> = #[]
    for i in 0..n { out.push(#[i as f64; n]) }
    out
}
fn band(m: Vec<Vec<f64>>, lo: i64, hi: i64) -> f64 {
    let mut s = 0.0
    for i in lo..hi { s += m[i][0] }
    s
}
fn fan(m: Vec<Vec<f64>>) -> f64 {
    let mut total = 0.0
    let joined = cohort {
        let mut hs = #[]
        for w in 0..8 {
            let lo = 25 * w
            hs.push(spawn(|| band(m, lo, lo + 25)))
        }
        for h in hs { total += h.join().unwrap_or(0.0) }
    }
    if joined.is_err() { return -1.0 }
    total
}
fn main() {
    let m = rows(200)
    let mut total = 0.0
    for _ in 0..2000 { total += fan(m) }
    println(total)
}
",
    ),
    (
        "vec_of_option_structs",
        r#"
struct Pt { x: i64, label: String }
fn fill(n: i64) -> i64 {
    let mut keep: Vec<Option<Pt>> = #[]
    for i in 0..n { keep.push(Some(Pt { x: i, label: f"label-{i}" })) }
    keep.len()
}
fn main() {
    let mut total = 0
    let mut round = 0
    while round < 2000 {
        total += fill(500)
        round += 1
    }
    println(total)
}
"#,
    ),
    (
        "returned_tuple_option_fields",
        r#"
struct Span { file: u32, start: u32, end: u32 }
struct Node { id: u32, span: Span, label: String }
fn split(i: i64) -> (Vec<i64>, Option<Node>, String) {
    let node = Node { id: 1, span: Span { file: 0, start: 1, end: 2 }, label: f"node-{i}" }
    (#[i], Some(node), f"tail-{i}")
}
fn main() {
    let mut total = 0
    let mut i = 0
    while i < 400000 {
        let xs, node, tail = split(i)
        if let Some(n) = node { total += n.label.len() }
        total += xs.len() + tail.len()
        i += 1
    }
    println(total)
}
"#,
    ),
    (
        "enum_carrier_payloads",
        r#"
struct Pt { x: i64, label: String }
enum Kind {
    Let { name: Pt, init: Option<Pt> },
    Text(Option<String>),
    Mixed(Result<String, i64>),
    Blobby(Result<Pt, String>),
}
fn build(i: i64) -> Vec<Kind> {
    #[
        Kind::Let { name: Pt { x: i, label: f"n{i}" }, init: Some(Pt { x: i, label: f"i{i}" }) },
        Kind::Text(Some(f"t{i}")),
        Kind::Mixed(Ok(f"m{i}")),
        Kind::Blobby(Err(f"e{i}")),
    ]
}
fn weigh(k: Kind) -> i64 {
    match k {
        Kind::Let { name, init } => match init { Some(p) => name.x + p.label.len(), None => 0 },
        Kind::Text(t) => t.unwrap_or("").len(),
        Kind::Mixed(r) => match r { Ok(s) => s.len(), Err(n) => n },
        Kind::Blobby(r) => match r { Ok(p) => p.x, Err(e) => e.len() },
    }
}
fn main() {
    let mut total = 0
    let mut i = 0
    while i < 200000 {
        for k in build(i) { total += weigh(k) }
        i += 1
    }
    println(total)
}
"#,
    ),
    (
        "carrier_field_unwrap_or",
        r#"
struct S { n: i64, t: Option<String> }
enum K { Text(Option<String>), Count(i64) }
fn weigh(s: S) -> i64 {
    let t = s.t
    t.unwrap_or("").len()
}
fn weigh_kind(k: K) -> i64 {
    match k {
        K::Text(t) => t.unwrap_or("").len(),
        K::Count(n) => n,
    }
}
fn main() {
    let mut total = 0
    let mut i = 0
    while i < 200000 {
        for s in #[S { n: i, t: Some(f"t{i}") }] { total += weigh(s) }
        for k in #[K::Text(Some(f"k{i}")), K::Count(i)] { total += weigh_kind(k) }
        i += 1
    }
    println(total)
}
"#,
    ),
    (
        "enum_tree_control",
        r#"
enum Tree { Node(i64, Tree, Tree), Leaf }
fn build(d: i64) -> Tree {
    if d == 0 { Tree::Leaf } else { Tree::Node(d, build(d - 1), build(d - 1)) }
}
fn count(t: Tree) -> i64 {
    match t { Tree::Node(v, l, r) => v + count(l) + count(r), Tree::Leaf => 0 }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 200000 {
        let t = build(12)
        total += count(t)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        // Shared subtrees decrement to a non-zero count on every release, the
        // case a cycle collector buffers; an expression tree cannot form a
        // cycle, so its nodes are never buffered and the tree frees by count.
        "acyclic_tree_shared_subtrees",
        r#"
enum Expr { Num(i64), Add(Expr, Expr), Mul(Expr, Expr) }
fn grow(base: Expr, d: i64) -> Expr {
    if d == 0 { base } else { Expr::Add(grow(base, d - 1), Expr::Mul(base, Expr::Num(d))) }
}
fn eval(e: Expr) -> i64 {
    match e {
        Expr::Num(n) => n,
        Expr::Add(a, b) => eval(a) + eval(b),
        Expr::Mul(a, b) => eval(a) * eval(b),
    }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 200000 {
        let shared = Expr::Add(Expr::Num(i), Expr::Num(2))
        total +%= eval(grow(shared, 6))
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        // A recursive struct can close a cycle, so its nodes stay cycle
        // candidates and the collector still reclaims each round's pair.
        "struct_cycle_still_collected",
        r#"
use std::runtime
struct Node { id: i64, next: Option<Node> }
fn main() {
    let mut total: i64 = 0
    let mut round: i64 = 0
    while round < 200000 {
        let mut a = Node { id: round, next: None }
        let mut b = Node { id: round * 3, next: None }
        b.next = Some(a)
        a.next = Some(b)
        total += a.id
        if round % 1000 == 0 { runtime::collect_cycles() }
        round += 1
    }
    runtime::collect_cycles()
    println("{}", total)
}
"#,
    ),
    (
        "transient_string",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let s = format("value-{}", i)
        total += s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "returned_string",
        r#"
fn make(i: i64) -> String { format("value-{}", i) }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let s = make(i)
        total += s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_struct",
        r#"
struct Holder { s: String }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let h = Holder { s: format("value-{}", i) }
        total += h.s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_enum",
        r#"
enum E { S(String), N }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let e = E::S(format("value-{}", i))
        match e { E::S(s) => total += s.len(), E::N => {} }
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_option",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 4000000 {
        let o: Option<String> = Some(format("value-{}", i))
        match o { Some(s) => total += s.len(), None => {} }
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "string_in_vec",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 1500000 {
        let mut v: Vec<String> = Vec::from([])
        v.push(format("key-{}", i))
        v.push(format("val-{}", i))
        total += v[0].len() + v[1].len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "nested_vec_string",
        r#"
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 1000000 {
        let mut outer: Vec<Vec<String>> = Vec::from([])
        let mut inner: Vec<String> = Vec::from([])
        inner.push(format("value-{}", i))
        outer.push(inner)
        total += outer[0][0].len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        // 0.18.1: a `String` nested inside a by-value sub-struct was never
        // released when the outer struct died (the per-field RC teardown
        // walked only the outer struct's direct fields), so RSS grew with N.
        // The teardown now recurses into by-value sub-structs, with matching
        // recursive retains at every sub-aggregate copy / extract / `..base`
        // site so the nested share is freed exactly once.
        "string_in_nested_struct",
        r#"
struct Inner { name: String, tag: String }
struct Outer { inner: Inner, id: i64 }
fn make(i: i64) -> i64 {
    let o = Outer { inner: Inner { name: format("n-{}", i), tag: format("t-{}", i) }, id: i }
    o.id
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 3000000 {
        total += make(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        // A container field takes a table of its own from the answer a call
        // hands the struct literal, so the frame that built that answer keeps
        // its release: without it every table a loop ever built stayed live.
        "map_in_struct_in_vec",
        r#"
struct Holder { m: Map<i64, i64> }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 400000 {
        let v = #[Holder { m: {1: 1} }]
        total += v[0].m.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "set_in_struct_in_vec",
        r#"
struct Holder { s: Set<i64> }
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 400000 {
        let v = #[Holder { s: #{1, 2, 3} }]
        let w = v.clone()
        total += v[0].s.len() + w[0].s.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "weak_in_vec",
        r#"
enum Node { Leaf(i64), Pair(Node, Node) }
fn fan(k: i64) -> i64 {
    let n = Node::Leaf(k)
    let mut ws = #[]
    ws.push(n.downgrade())
    ws.push(n.downgrade())
    ws.len()
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 400000 {
        total += fan(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "weak_fanout_past_the_header",
        r#"
enum Node { Leaf(i64), Pair(Node, Node) }
fn fan(k: i64) -> i64 {
    let n = Node::Leaf(k)
    let mut ws = #[]
    for _ in 0..300 { ws.push(n.downgrade()) }
    ws.len()
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 2000 {
        total += fan(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "weak_in_map_and_option",
        r#"
enum Node { Leaf(i64), Pair(Node, Node) }
struct Holder { w: Weak<Node>, tag: i64 }
fn fan(k: i64) -> i64 {
    let n = Node::Leaf(k)
    let mut m = Map::new()
    m[k] = n.downgrade()
    let mut hm = Map::new()
    hm[k] = Holder { w: n.downgrade(), tag: k }
    let o = Some(n.downgrade())
    let mut hs = #[Holder { w: n.downgrade(), tag: k }]
    hs.push(Holder { w: n.downgrade(), tag: k })
    if o.is_some() { m.len() + hm.len() + hs.len() } else { 0 }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 200000 {
        total += fan(i)
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "lent_carrier_payloads",
        r#"
use std::errors
fn mk(n: i64) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::from([])
    for i in 0..n { v.push(i as u8) }
    v
}
fn q(r: Result<Vec<u8>, errors::Error>) -> Result<i64, errors::Error> {
    let v = r?
    Ok(v.len())
}
fn keep(r: Result<Vec<u8>, errors::Error>) -> Vec<u8> {
    match r {
        Ok(v) => v,
        Err(_) => Vec::from([]),
    }
}
fn main() {
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 300000 {
        match q(Ok(mk(8))) { Ok(n) => total += n, Err(_) => {} }
        total += keep(Ok(mk(4))).len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "returned_tables",
        r#"
fn pass_map(m: Map<i64, i64>) -> Map<i64, i64> { m }
fn pass_set(s: Set<i64>) -> Set<i64> { s }
fn pass_deque(d: Deque<i64>) -> Deque<i64> { d }
fn pass_heap(h: MinHeap<i64>) -> MinHeap<i64> { h }
fn pick(m: Map<i64, i64>, fresh: bool) -> Map<i64, i64> {
    if fresh { {1: 1} } else { m }
}
fn main() {
    let m = {1: 10, 2: 20}
    let s = #{1, 2, 3}
    let mut d = Deque::new()
    d.push_back(1)
    let h = MinHeap::from([5, 3, 9])
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 100000 {
        let a = pass_map(m)
        let b = pass_set(s)
        let c = pass_deque(d)
        let e = pass_heap(h)
        let p = pick(m, i % 2 == 0)
        let lit = {1: (2, "x")}
        total += a.len() + b.len() + c.len() + e.len() + p.len() + lit.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "borrowed_tables",
        r#"
struct State { s: Set<i64>, d: Deque<i64> }
impl State {
    fn set(&self) -> Set<i64> { self.s }
}
fn first(v: Vec<Set<i64>>) -> Set<i64> { v[0] }
fn value_of(m: Map<i64, Set<i64>>) -> Set<i64> { m[1] }
fn unwrap_deque(o: Option<Deque<i64>>) -> Deque<i64> {
    match o {
        Some(d) => d,
        None => Deque::new(),
    }
}
fn local_field() -> Set<i64> {
    let st = State { s: #{1}, d: Deque::new() }
    st.s
}
fn main() {
    let st = State { s: #{1, 2}, d: Deque::new() }
    let vs = #[#{7}]
    let ms = {1: #{3}}
    let od = Some(st.d)
    let mut total: i64 = 0
    let mut i: i64 = 0
    while i < 100000 {
        let a = st.set()
        let b = first(vs)
        let c = value_of(ms)
        let d = unwrap_deque(od)
        let e = local_field()
        let read = || st.s
        let f = read()
        let mut held = Vec::new()
        held.push(Some(#{i}))
        total += a.len() + b.len() + c.len() + d.len() + e.len() + f.len() + held.len()
        i += 1
    }
    println("{}", total)
}
"#,
    ),
    (
        "arena_held_handles",
        r"
enum Holder { Has(Map<i64, i64>), Nothing }
fn main() {
    let mut t = 0
    for i in 0..2000 {
        arena {
            let mut m: Map<i64, i64> = Map::new()
            m.insert(i, i)
            let v = #[m]
            let mut s = Set::new()
            s.insert(i)
            let sets = #[s]
            let mut q: Deque<i64> = Deque::new()
            q.push_back(i)
            let qs = #[q]
            let mut m2: Map<i64, i64> = Map::new()
            m2.insert(i, i + 1)
            let h = Holder::Has(m2)
            let held = match h { Holder::Has(x) => x.len(), Holder::Nothing => 0 }
            t += v.len() + sets.len() + qs.len() + held
        }
    }
    println(t)
}
",
    ),
    (
        "auto_region_map_containers",
        r#"
enum Box2 { Has(Map<i64, i64>), Empty }
fn size(b: Box2) -> i64 {
    match b { Box2::Has(m) => m.len(), Box2::Empty => 0 }
}
fn main() {
    let mut t = 0
    for i in 0..2000 {
        let mut m: Map<i64, i64> = Map::new()
        m.insert(i, i)
        t += size(Box2::Has(m))
    }
    for i in 0..2000 {
        let mut v: Vec<Map<i64, i64>> = Vec::new()
        v.push(Map::new())
        let label = f"{i}"
        t += v.len() + label.len()
    }
    println(t)
}
"#,
    ),
    (
        "arena_held_json_and_iterators",
        r#"
use std::encoding::json
fn main() {
    let mut t = 0
    for i in 0..400000 {
        arena {
            let doc = json::parse(f"{{\"a\": {i}}}").unwrap()
            let docs = #[doc]
            let xs = #[1, 2, 3]
            let it = xs.iter().map(|x| x + i)
            let its = #[it]
            t += docs.len() + its.len()
        }
    }
    println(t)
}
"#,
    ),
    (
        "caught_panic_frames",
        r#"
fn work(fail: bool) -> i64 {
    let s = "x".repeat(10000)
    let xs = #[s, s]
    if fail { panic("boom {}", xs.len()) }
    xs.len()
}
fn main() {
    let mut errors = 0
    for i in 0..40 {
        let h = spawn(|| work(true))
        match h.join() {
            Ok(_) => {}
            Err(_) => { errors += 1 }
        }
    }
    println("{}", errors)
}
"#,
    ),
    (
        "variant_option_struct_layout",
        r#"
struct Ty { id: i64, name: String }
enum Stmt { Let { name: String, ty: Option<Ty>, n: i64 }, Empty }
fn empty_let(n: i64) -> Stmt { Stmt::Let { name: "e", ty: None, n: n } }
fn typed_let(n: i64) -> Stmt {
    let ty = Some(Ty { id: n, name: "t" + n.to_string() })
    Stmt::Let { name: "l", ty: ty, n: n }
}
fn weigh(s: Stmt) -> i64 {
    match s {
        Stmt::Let { ty, n, .. } => match ty { Some(t) => t.id + n, None => n },
        Stmt::Empty => 0,
    }
}
fn main() {
    let mut total = 0
    for i in 0..200000 {
        total += weigh(empty_let(i)) + weigh(typed_let(i))
    }
    println(total)
}
"#,
    ),
    (
        "option_string_field_through_mut",
        r#"
struct P { pos: i64, current: Option<String> }
fn work(p: &mut P, name: String) {
    let outer = p.current
    p.current = Some(name)
    p.pos += 1
    p.current = outer
}
fn main() {
    let mut p = P { pos: 0, current: None }
    for i in 0..200000 {
        work(&mut p, "trait" + i.to_string())
    }
    println(p.pos)
}
"#,
    ),
    (
        "argument_copy_in_loop",
        r#"
struct Seg { name: String, generics: Vec<i64> }
struct PathExpr { segments: Vec<Seg> }
struct Attribute { path: PathExpr, tokens: Option<String> }
fn attr_is_named(a: Attribute, name: String) -> bool {
    a.path.segments.len() == 1 && a.path.segments[0].name == name
}
fn has_word(attrs: Vec<Attribute>, name: String) -> bool {
    for a in attrs {
        if attr_is_named(a, name) { return true }
    }
    false
}
fn main() {
    let mut hits = 0
    for i in 0..100000 {
        let a = Attribute { path: PathExpr { segments: #[Seg { name: "n" + i.to_string(), generics: #[] }] }, tokens: None }
        let b = Attribute { path: PathExpr { segments: #[Seg { name: "test", generics: #[] }] }, tokens: None }
        if has_word(#[a, b], "nope") { hits += 1 }
    }
    println(hits)
}
"#,
    ),
    (
        "mut_payload_copy_on_write",
        r#"
enum Node { List(Vec<i64>), Name(String), Pair(Node, Node) }
fn grow(n: &mut Node) {
    match n {
        Node::List(v) => v.push(1),
        Node::Name(s) => s.push_str("!"),
        Node::Pair(a, b) => {
            grow(a)
            grow(b)
        }
    }
}
fn size(n: Node) -> i64 {
    match n {
        Node::List(v) => v.len(),
        Node::Name(s) => s.len(),
        Node::Pair(a, b) => size(a) + size(b),
    }
}
fn main() {
    let mut total = 0
    for i in 0..100000 {
        let mut n = Node::Pair(Node::List(#[i]), Node::Name("x" + i.to_string()))
        let kept = n
        grow(&mut n)
        total += size(n) - size(kept)
    }
    println(total)
}
"#,
    ),
    (
        "fixed_array_of_blob_carriers",
        r"
fn fill(base: i64) -> i64 {
    let mut xs: [Option<(i64, i64)>; 8] = [None; 8]
    let mut i = 0
    while i < 8 {
        xs[i] = Some((base + i, base))
        i += 1
    }
    let mut total = 0
    for k in 0..8 {
        match xs[k] {
            Some((a, b)) => total += a - b
            None => total += 1000
        }
    }
    total
}
fn main() {
    let mut total = 0
    for r in 0..40000 { total += fill(r) }
    println(total)
}
",
    ),
    (
        "vec_of_blob_carriers_read_by_index",
        r"
fn fill(base: i64) -> i64 {
    let mut xs: Vec<Option<(i64, i64)>> = #[None; 8]
    let mut i = 0
    while i < 8 {
        xs[i] = Some((base + i, base))
        i += 1
    }
    let mut total = 0
    for k in 0..8 {
        match xs[k] {
            Some((a, b)) => total += a - b
            None => total += 1000
        }
    }
    total
}
fn main() {
    let mut total = 0
    for r in 0..40000 { total += fill(r) }
    println(total)
}
",
    ),
    (
        "regioned_loop_boxes_packed_struct",
        r#"
struct Span { start: u32, end: u32 }
struct Expr { id: u32, span: Span, kind: Kind }
enum Kind { Lit(String), Bin { lhs: Expr, rhs: Expr } }
fn mk(id: u32, kind: Kind) -> Expr {
    Expr { id: id, span: Span { start: id, end: id + 1 }, kind: kind }
}
fn condition(n: i64) -> (Vec<i64>, Option<Expr>) {
    let e = mk(1, Kind::Bin { lhs: mk(2, Kind::Lit("i")), rhs: mk(3, Kind::Lit("n")) })
    if n % 2 == 0 {
        return (#[n], None)
    }
    (#[], Some(e))
}
fn main() {
    let mut total = 0
    for n in 0..60000 {
        let rest, cond = condition(n)
        match cond {
            Some(e) => total += e.id as i64
            None => total += rest.len()
        }
    }
    println(total)
}
"#,
    ),
    (
        "arena_node_holds_outside_values",
        r#"
enum Tree {
    Leaf(String),
    Items(Vec<i64>),
    Node(Tree, Tree),
}
fn size(t: Tree) -> i64 {
    match t {
        Tree::Leaf(s) => s.len(),
        Tree::Items(v) => v.len(),
        Tree::Node(a, b) => size(a) + size(b),
    }
}
fn wrap(t: Tree, s: String, v: Vec<i64>) -> i64 {
    let mut total = 0
    arena {
        let n = Tree::Node(t, Tree::Node(Tree::Leaf(s), Tree::Items(v)))
        total = size(n)
    }
    total
}
fn main() {
    let mut s = String::new()
    s += "outside"
    let leaf = Tree::Leaf(s)
    let mut t = String::new()
    t += "str"
    let v = #[1, 2, 3]
    let mut k = 0
    for i in 0..100000 { k += wrap(leaf, t, v) }
    println(k)
}
"#,
    ),
    (
        "option_and_then_lends_its_payload",
        r#"
fn pick(m: Map<String, String>, i: i64) -> i64 {
    let found = m.get("k").and_then(|h| if i % 2 == 0 { Some(h) } else { None })
    found.unwrap_or("").len()
}
fn main() {
    let mut m = Map::new()
    let mut v = String::new()
    v += "a lookup payload long enough to count"
    m["k"] = v
    let mut total = 0
    for i in 0..200000 { total += pick(m, i) }
    println(total)
}
"#,
    ),
];

fn gnu_time_ok() -> bool {
    if !Path::new("/usr/bin/time").exists() {
        return false;
    }
    Command::new("/usr/bin/time")
        .arg("-v")
        .arg("true")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stderr).contains("Maximum resident set size"))
}

fn build_release(dir: &Path, name: &str, source: &str) -> Option<PathBuf> {
    let src = dir.join(format!("{name}.gos"));
    std::fs::write(&src, source).unwrap();
    let out = Command::new(gos_bin())
        .arg("build")
        .arg("--release")
        .arg(&src)
        .output()
        .expect("spawn gos build");
    if !out.status.success() {
        return None;
    }
    let bin = dir
        .join("target")
        .join("release")
        .join(format!("{name}{}", env::consts::EXE_SUFFIX));
    bin.exists().then_some(bin)
}

/// What a run left behind: peak RSS, reference-counted objects still live,
/// and the leak ledger's live `String` and `Vec` counts with the rest of the
/// ledger line.
struct Residue {
    rss_kb: u64,
    rc_live: u64,
    strings: u64,
    vecs: u64,
    ledger: String,
}

fn ledger_field(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&format!("{name}=")))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

fn measure(bin: &Path) -> Residue {
    let out = Command::new("/usr/bin/time")
        .arg("-v")
        .arg(bin)
        .env("GOS_RC_DEBUG", "1")
        .env("GOS_LEAK_LEDGER", "1")
        .output()
        .expect("spawn /usr/bin/time");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "binary {} failed: {stderr}",
        bin.display()
    );
    let rss_kb = stderr
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Maximum resident set size (kbytes):")
                .and_then(|rest| rest.trim().parse().ok())
        })
        .unwrap_or_else(|| panic!("no RSS line for {}", bin.display()));
    let rc_live = stderr
        .lines()
        .find_map(|line| line.strip_prefix("RC_LIVE_AT_EXIT="))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX);
    let ledger = stderr
        .lines()
        .find(|line| line.starts_with("LEAK LEDGER"))
        .unwrap_or("")
        .to_string();
    let others_clear = ["aggr", "rc", "map", "set", "deque"]
        .iter()
        .all(|name| ledger_field(&ledger, name) == 0);
    Residue {
        rss_kb,
        rc_live,
        strings: ledger_field(&ledger, "str"),
        vecs: if others_clear {
            ledger_field(&ledger, "vec")
        } else {
            u64::MAX
        },
        ledger,
    }
}

#[test]
fn leak_matrix_report() {
    if !gnu_time_ok() {
        // Linux CI is where this gate is required, so a missing measurement
        // there is a failure rather than a skip.
        assert!(
            !(cfg!(target_os = "linux") && env::var_os("CI").is_some()),
            "GNU /usr/bin/time -v is required for the leak gate on Linux CI"
        );
        eprintln!("skipping: GNU /usr/bin/time -v not available");
        return;
    }
    let dir = env::temp_dir().join(format!("gos-leak-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut failures = Vec::new();
    eprintln!("\n  leak matrix (cap {CAP_KB} KB)");
    for (name, source) in SHAPES {
        let Some(bin) = build_release(&dir, name, source) else {
            eprintln!("  {name:<28} BUILD FAIL");
            failures.push(format!("{name}: build failed"));
            continue;
        };
        let residue = measure(&bin);
        let allowed = allowance(name);
        let ok = residue.rss_kb < CAP_KB
            && residue.rc_live == 0
            && residue.strings <= allowed.strings
            && residue.vecs <= allowed.vecs;
        eprintln!(
            "  {name:<28} {:>8} KB  rc_live={}  {}  {}",
            residue.rss_kb,
            residue.rc_live,
            residue.ledger,
            if ok { "ok" } else { "LEAK" }
        );
        if !ok {
            failures.push(format!(
                "{name}: rss {} KB, rc_live {}, {}",
                residue.rss_kb, residue.rc_live, residue.ledger
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "leaking shapes:\n{}",
        failures.join("\n")
    );
}
