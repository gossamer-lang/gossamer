//! VM-native `&mut self` method tests (destroy-tree-walker).
//!
//! Every program runs entirely through the bytecode [`Vm`].
//! A `&mut self` user method called on a local receiver
//! rides the write-back cell protocol so its mutation of `self`
//! persists in the caller's binding, matching the by-pointer receiver
//! the compiled tiers pass. Receivers that are not direct locals
//! (fields, array elements, temporaries) discard the mutation on the
//! compiled tiers, so the VM matches that here too.

use std::cell::RefCell;

use gossamer_hir::lower_source_file;
use gossamer_interp::{Vm, set_stdout_writer};
use gossamer_lex::SourceMap;
use gossamer_parse::autoderive::parse_with_autoderive;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

thread_local! {
    static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn capture_writer(text: &str) {
    CAPTURED.with(|cell| cell.borrow_mut().push_str(text));
}

fn run_main(source: &str) -> String {
    let mut map = SourceMap::new();
    let file = map.add_file("mut_self.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut vm = Vm::new();
    vm.load(&program, tcx, true).expect("load");
    CAPTURED.with(|cell| cell.borrow_mut().clear());
    let prev = set_stdout_writer(capture_writer);
    let result = vm.call("main", Vec::new());
    set_stdout_writer(prev);
    result.expect("main failed");
    CAPTURED.with(|cell| cell.borrow().clone())
}

#[test]
fn mut_self_mutation_persists_on_local_receiver() {
    let src = r#"
struct Counter { n: i64 }
impl Counter { fn bump(&mut self) { self.n = self.n + 1 } }
fn main() {
    let mut c = Counter { base: 0 }
    c.bump()
    c.bump()
    c.bump()
    println("n={}", c.n)
}
"#;
    assert_eq!(run_main(src), "n=3\n");
}

#[test]
fn mut_self_method_with_args_and_return() {
    let src = r#"
struct Acc { total: i64 }
impl Acc {
    fn add(&mut self, by: i64) -> i64 { self.total = self.total + by
 self.total }
    fn get(&self) -> i64 { self.total }
}
fn main() {
    let mut a = Acc { total: 0 }
    let r1 = a.add(5)
    let r2 = a.add(10)
    println("r1={} r2={} get={}", r1, r2, a.get())
}
"#;
    assert_eq!(run_main(src), "r1=5 r2=15 get=15\n");
}

#[test]
fn immutable_self_method_is_not_regressed() {
    let src = r#"
struct Point { x: i64, y: i64 }
impl Point {
    fn sum(&self) -> i64 { self.x + self.y }
    fn shift(&mut self, d: i64) { self.x = self.x + d
 self.y = self.y + d }
}
fn main() {
    let mut p = Point { x: 1, y: 2 }
    println("before={}", p.sum())
    p.shift(10)
    println("after={}", p.sum())
}
"#;
    assert_eq!(run_main(src), "before=3\nafter=23\n");
}

#[test]
fn deep_non_tail_method_calls_suspend_vm_frames() {
    // A user method resolves through `Op::MethodCall`, not the ordinary
    // `Op::Call` path. Its recursive call must therefore suspend the caller
    // before entering the next bytecode body.
    let src = r#"
struct Counter { base: i64 }
impl Counter {
    fn count(&self, n: i64) -> i64 {
        if n <= 0i64 { return self.base }
        let below = self.count(n - 1i64)
        return below + 1i64
    }
}
fn main() {
    let c = Counter { base: 0i64 }
    println("{}", c.count(1000i64))
}
"#;
    assert_eq!(run_main(src), "1000\n");
}

#[test]
fn mut_self_with_string_field_rc_is_sound() {
    // The receiver carries an RC-managed String; repeated `&mut self`
    // writeback must keep the aggregate intact (no leak / UAF).
    let src = r#"
struct Named { count: i64, label: String }
impl Named { fn tick(&mut self) { self.count = self.count + 1 } }
fn main() {
    let mut it = Named { count: 0, label: "widget" }
    let mut i = 0
    while i < 100 {
        it.tick()
        i = i + 1
    }
    println("{}={}", it.label, it.count)
}
"#;
    assert_eq!(run_main(src), "widget=100\n");
}

#[test]
fn mut_self_writes_through_a_vec_field_indexed_by_usize() {
    // Regression for #110: the method receiver was written back, but a
    // Vec field mutation performed through that receiver was lost.
    let src = r#"
struct Mem { mem: Vec<i64>, pos: usize }
impl Mem {
    fn new(arr: Vec<i64>) -> Self {
        Mem { mem: arr.clone(), pos: 0 }
    }
    fn set_memory(&mut self, offset: usize, value: i64) {
        let pos = self.mem[self.pos + offset] as usize
        self.mem[pos] = value
    }
    fn get_memory(self, index: usize) -> i64 { self.mem[index] }
}
fn main() {
    let mut mem = Mem::new(Vec::from([1, 2, 3, 4]))
    mem.set_memory(1, 9)
    println("{} {}", mem.get_memory(1), mem.get_memory(2))
}
"#;
    assert_eq!(run_main(src), "2 9\n");
}

#[test]
fn mut_self_on_enum_receiver_persists() {
    let src = r#"
enum State { Idle, Running(i64) }
impl State {
    fn advance(&mut self) {
        match self {
            State::Idle => { *self = State::Running(1) }
            State::Running(n) => { *self = State::Running(n + 1) }
        }
    }
    fn value(&self) -> i64 {
        match self {
            State::Idle => 0,
            State::Running(n) => *n,
        }
    }
}
fn main() {
    let mut s = State::Idle
    s.advance()
    s.advance()
    s.advance()
    println("v={}", s.value())
}
"#;
    assert_eq!(run_main(src), "v=3\n");
}
