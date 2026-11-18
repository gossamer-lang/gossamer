//! Binary-heap push and pop over whole signed words, written inline.
//!
//! A `MinHeap` or `MaxHeap` whose element is an integer or a tuple of up to
//! four integers orders its elements word by word. The runtime sifts such a
//! heap with plain word moves; this lowering writes that same sift at the call
//! site, so a hot loop that feeds and drains a heap pays no call and no
//! descriptor dispatch per operation. The header checks the runtime makes
//! before its own word path guard the inline one: when any fails (a full
//! buffer, an element stored another way), the call goes to the runtime.

use std::fmt::Write as _;

use gossamer_mir::{ConstValue, Operand};

use super::Lowerer;

/// `GosVec` header offsets, fixed by the runtime's C ABI.
const LEN: u64 = 0;
const CAP: u64 = 8;
const ELEM_BYTES: u64 = 16;
const ELEM_KIND: u64 = 20;
const DATA: u64 = 24;

/// Element kinds whose words are stored as they are.
const PLAIN_KINDS: [u8; 2] = [0, 18];

/// The ordering descriptor tag of a signed word, and of a nested tuple.
const DESC_TAG_INT: u8 = 0;
const TUPLE_TAG_NESTED: u8 = gossamer_abi::TUPLE_TAG_NESTED;

/// Which heap operation a runtime call is.
#[derive(Clone, Copy)]
pub(crate) enum HeapOp {
    Push,
    PopInto,
}

/// A heap call the lowering writes inline.
#[derive(Clone, Copy)]
pub(crate) struct HeapInline {
    op: HeapOp,
    /// Whether the heap keeps its greatest element at the root.
    max: bool,
    /// Words per element.
    words: usize,
}

impl HeapInline {
    /// The inline form of a call to `symbol` with `args`, when the call is a
    /// push or pop whose descriptor orders whole signed words.
    pub(crate) fn of(symbol: &str, args: &[Operand]) -> Option<Self> {
        let (op, max, tags) = match symbol {
            "gos_rt_bheap_min_push_desc" => (HeapOp::Push, false, args.get(2)?),
            "gos_rt_bheap_max_push_desc" => (HeapOp::Push, true, args.get(2)?),
            "gos_rt_bheap_min_pop_desc_into" => (HeapOp::PopInto, false, args.get(1)?),
            "gos_rt_bheap_max_pop_desc_into" => (HeapOp::PopInto, true, args.get(1)?),
            _ => return None,
        };
        if args.len() != 3 {
            return None;
        }
        let Operand::Const(ConstValue::Str(text)) = tags else {
            return None;
        };
        let bytes: Vec<u8> = text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).ok())
            .collect::<Option<_>>()?;
        let words = match bytes.as_slice() {
            [DESC_TAG_INT, ..] => 1,
            [TUPLE_TAG_NESTED, arity, fields @ ..]
                if fields.len() >= usize::from(*arity)
                    && fields[..usize::from(*arity)]
                        .iter()
                        .all(|&t| t == DESC_TAG_INT) =>
            {
                usize::from(*arity)
            }
            _ => return None,
        };
        (1..=4).contains(&words).then_some(Self { op, max, words })
    }

    /// Index of the vec argument and of the element (push) or answer slot
    /// (pop) among the call's arguments.
    pub(crate) fn operand_indices(self) -> (usize, usize) {
        match self.op {
            HeapOp::Push => (0, 1),
            HeapOp::PopInto => (0, 2),
        }
    }
}

impl Lowerer<'_> {
    /// Writes `heap`'s operation on the vec at `vec` with the element or
    /// answer slot at `slot`, falling back to `call` (the runtime call, as an
    /// instruction without its result name) when the inline path does not
    /// apply. Answers the name holding a pop's status, as the runtime's.
    pub(crate) fn emit_heap_inline(
        &mut self,
        heap: HeapInline,
        vec: &str,
        slot: &str,
        call: &str,
    ) -> Option<String> {
        let w = heap.words;
        let width = w * 8;
        let elem_ty = format!("[{w} x i64]");
        let len = self.heap_header_load(vec, LEN, "i64");
        let elem_bytes = self.heap_header_load(vec, ELEM_BYTES, "i32");
        let kind = self.heap_header_load(vec, ELEM_KIND, "i8");
        let data = self.heap_header_load(vec, DATA, "ptr");
        let room = self.fresh();
        match heap.op {
            HeapOp::Push => {
                let cap = self.heap_header_load(vec, CAP, "i64");
                writeln!(self.out, "  {room} = icmp slt i64 {len}, {cap}").unwrap();
            }
            HeapOp::PopInto => {
                writeln!(self.out, "  {room} = icmp sgt i64 {len}, 0").unwrap();
            }
        }
        let sized = self.fresh();
        writeln!(self.out, "  {sized} = icmp eq i32 {elem_bytes}, {width}").unwrap();
        let mut plain = self.fresh();
        writeln!(
            self.out,
            "  {plain} = icmp eq i8 {kind}, {}",
            PLAIN_KINDS[0]
        )
        .unwrap();
        for k in &PLAIN_KINDS[1..] {
            let this = self.fresh();
            let either = self.fresh();
            writeln!(self.out, "  {this} = icmp eq i8 {kind}, {k}").unwrap();
            writeln!(self.out, "  {either} = or i1 {plain}, {this}").unwrap();
            plain = either;
        }
        let present = self.fresh();
        writeln!(self.out, "  {present} = icmp ne ptr {data}, null").unwrap();
        let mut fast = room;
        for cond in [sized, plain, present] {
            let both = self.fresh();
            writeln!(self.out, "  {both} = and i1 {fast}, {cond}").unwrap();
            fast = both;
        }
        let inline = self.fresh_label("heap_inline");
        let slow = self.fresh_label("heap_call");
        let join = self.fresh_label("heap_join");
        writeln!(self.out, "  br i1 {fast}, label %{inline}, label %{slow}").unwrap();
        writeln!(self.out, "{inline}:").unwrap();
        let _ = self.emit_vec_mutation_bump(vec);
        let inline_end = match heap.op {
            HeapOp::Push => self.emit_heap_push(heap, vec, slot, &len, &data, &elem_ty),
            HeapOp::PopInto => self.emit_heap_pop(heap, vec, slot, &len, &data, &elem_ty),
        };
        writeln!(self.out, "  br label %{join}").unwrap();
        writeln!(self.out, "{slow}:").unwrap();
        let status = match heap.op {
            HeapOp::Push => {
                writeln!(self.out, "  {call}").unwrap();
                None
            }
            HeapOp::PopInto => {
                let answered = self.fresh();
                writeln!(self.out, "  {answered} = {call}").unwrap();
                Some(answered)
            }
        };
        writeln!(self.out, "  br label %{join}").unwrap();
        writeln!(self.out, "{join}:").unwrap();
        status.map(|answered| {
            let merged = self.fresh();
            writeln!(
                self.out,
                "  {merged} = phi i64 [0, %{inline_end}], [{answered}, %{slow}]"
            )
            .unwrap();
            merged
        })
    }

    /// Stores the element at `elem` past the last and sifts it up. Answers
    /// the label the inline path ends in.
    fn emit_heap_push(
        &mut self,
        heap: HeapInline,
        vec: &str,
        elem: &str,
        len: &str,
        data: &str,
        elem_ty: &str,
    ) -> String {
        let grown = self.fresh();
        writeln!(self.out, "  {grown} = add i64 {len}, 1").unwrap();
        writeln!(self.out, "  store i64 {grown}, ptr {vec}").unwrap();
        let held = self.heap_load_words(elem, heap.words);
        let entry = self.fresh_label("heap_up_entry");
        let up = self.fresh_label("heap_up");
        let test = self.fresh_label("heap_up_test");
        let shift = self.fresh_label("heap_up_shift");
        let done = self.fresh_label("heap_up_done");
        writeln!(self.out, "  br label %{entry}").unwrap();
        writeln!(self.out, "{entry}:").unwrap();
        writeln!(self.out, "  br label %{up}").unwrap();
        writeln!(self.out, "{up}:").unwrap();
        let at = self.fresh();
        let parent = self.fresh();
        writeln!(
            self.out,
            "  {at} = phi i64 [{len}, %{entry}], [{parent}, %{shift}]"
        )
        .unwrap();
        let above_root = self.fresh();
        writeln!(self.out, "  {above_root} = icmp ne i64 {at}, 0").unwrap();
        writeln!(
            self.out,
            "  br i1 {above_root}, label %{test}, label %{done}"
        )
        .unwrap();
        writeln!(self.out, "{test}:").unwrap();
        let less_one = self.fresh();
        writeln!(self.out, "  {less_one} = sub i64 {at}, 1").unwrap();
        writeln!(self.out, "  {parent} = lshr i64 {less_one}, 1").unwrap();
        let parent_addr = self.heap_slot_addr(data, elem_ty, &parent);
        let above = self.heap_load_words(&parent_addr, heap.words);
        // The parent moves down when the held element outranks it.
        let moves = if heap.max {
            self.heap_words_less(&above, &held)
        } else {
            self.heap_words_less(&held, &above)
        };
        writeln!(self.out, "  br i1 {moves}, label %{shift}, label %{done}").unwrap();
        writeln!(self.out, "{shift}:").unwrap();
        let at_addr = self.heap_slot_addr(data, elem_ty, &at);
        self.heap_store_words(&at_addr, &above);
        writeln!(self.out, "  br label %{up}").unwrap();
        writeln!(self.out, "{done}:").unwrap();
        let home = self.heap_slot_addr(data, elem_ty, &at);
        self.heap_store_words(&home, &held);
        done
    }

    /// Moves the root to `out` and sifts the last element down from the
    /// root. Answers the label the inline path ends in.
    fn emit_heap_pop(
        &mut self,
        heap: HeapInline,
        vec: &str,
        out: &str,
        len: &str,
        data: &str,
        elem_ty: &str,
    ) -> String {
        let last = self.fresh();
        writeln!(self.out, "  {last} = sub i64 {len}, 1").unwrap();
        writeln!(self.out, "  store i64 {last}, ptr {vec}").unwrap();
        let root = self.heap_load_words(data, heap.words);
        let tail_addr = self.heap_slot_addr(data, elem_ty, &last);
        let held = self.heap_load_words(&tail_addr, heap.words);
        self.heap_store_words(out, &root);
        let entry = self.fresh_label("heap_down_entry");
        let down = self.fresh_label("heap_down");
        let left_label = self.fresh_label("heap_down_left");
        let right_label = self.fresh_label("heap_down_right");
        let pick = self.fresh_label("heap_down_pick");
        let shift = self.fresh_label("heap_down_shift");
        let settle = self.fresh_label("heap_down_settle");
        let done = self.fresh_label("heap_down_done");
        let remains = self.fresh();
        writeln!(self.out, "  {remains} = icmp ne i64 {last}, 0").unwrap();
        writeln!(self.out, "  br i1 {remains}, label %{entry}, label %{done}").unwrap();
        writeln!(self.out, "{entry}:").unwrap();
        writeln!(self.out, "  br label %{down}").unwrap();
        writeln!(self.out, "{down}:").unwrap();
        let at = self.fresh();
        let best = self.fresh();
        writeln!(
            self.out,
            "  {at} = phi i64 [0, %{entry}], [{best}, %{shift}]"
        )
        .unwrap();
        let doubled = self.fresh();
        let left = self.fresh();
        writeln!(self.out, "  {doubled} = shl i64 {at}, 1").unwrap();
        writeln!(self.out, "  {left} = add i64 {doubled}, 1").unwrap();
        let has_left = self.fresh();
        writeln!(self.out, "  {has_left} = icmp ult i64 {left}, {last}").unwrap();
        writeln!(
            self.out,
            "  br i1 {has_left}, label %{left_label}, label %{settle}"
        )
        .unwrap();
        writeln!(self.out, "{left_label}:").unwrap();
        let left_addr = self.heap_slot_addr(data, elem_ty, &left);
        let left_words = self.heap_load_words(&left_addr, heap.words);
        let right = self.fresh();
        writeln!(self.out, "  {right} = add i64 {left}, 1").unwrap();
        let has_right = self.fresh();
        writeln!(self.out, "  {has_right} = icmp ult i64 {right}, {last}").unwrap();
        writeln!(
            self.out,
            "  br i1 {has_right}, label %{right_label}, label %{pick}"
        )
        .unwrap();
        writeln!(self.out, "{right_label}:").unwrap();
        let right_addr = self.heap_slot_addr(data, elem_ty, &right);
        let right_words = self.heap_load_words(&right_addr, heap.words);
        let right_wins = if heap.max {
            self.heap_words_less(&left_words, &right_words)
        } else {
            self.heap_words_less(&right_words, &left_words)
        };
        let chosen_index = self.fresh();
        writeln!(
            self.out,
            "  {chosen_index} = select i1 {right_wins}, i64 {right}, i64 {left}"
        )
        .unwrap();
        let chosen: Vec<String> = left_words
            .iter()
            .zip(&right_words)
            .map(|(l, r)| {
                let word = self.fresh();
                writeln!(
                    self.out,
                    "  {word} = select i1 {right_wins}, i64 {r}, i64 {l}"
                )
                .unwrap();
                word
            })
            .collect();
        writeln!(self.out, "  br label %{pick}").unwrap();
        writeln!(self.out, "{pick}:").unwrap();
        writeln!(
            self.out,
            "  {best} = phi i64 [{left}, %{left_label}], [{chosen_index}, %{right_label}]"
        )
        .unwrap();
        let best_words: Vec<String> = left_words
            .iter()
            .zip(&chosen)
            .map(|(l, c)| {
                let word = self.fresh();
                writeln!(
                    self.out,
                    "  {word} = phi i64 [{l}, %{left_label}], [{c}, %{right_label}]"
                )
                .unwrap();
                word
            })
            .collect();
        // The child moves up when it outranks the held element.
        let moves = if heap.max {
            self.heap_words_less(&held, &best_words)
        } else {
            self.heap_words_less(&best_words, &held)
        };
        writeln!(self.out, "  br i1 {moves}, label %{shift}, label %{settle}").unwrap();
        writeln!(self.out, "{shift}:").unwrap();
        let at_addr = self.heap_slot_addr(data, elem_ty, &at);
        self.heap_store_words(&at_addr, &best_words);
        writeln!(self.out, "  br label %{down}").unwrap();
        writeln!(self.out, "{settle}:").unwrap();
        let home = self.heap_slot_addr(data, elem_ty, &at);
        self.heap_store_words(&home, &held);
        writeln!(self.out, "  br label %{done}").unwrap();
        writeln!(self.out, "{done}:").unwrap();
        done
    }

    fn heap_header_addr(&mut self, vec: &str, offset: u64) -> String {
        if offset == 0 {
            return vec.to_string();
        }
        let addr = self.fresh();
        writeln!(
            self.out,
            "  {addr} = getelementptr i8, ptr {vec}, i64 {offset}"
        )
        .unwrap();
        addr
    }

    fn heap_header_load(&mut self, vec: &str, offset: u64, ty: &str) -> String {
        let addr = self.heap_header_addr(vec, offset);
        let value = self.fresh();
        writeln!(self.out, "  {value} = load {ty}, ptr {addr}").unwrap();
        value
    }

    fn heap_slot_addr(&mut self, data: &str, elem_ty: &str, index: &str) -> String {
        let addr = self.fresh();
        writeln!(
            self.out,
            "  {addr} = getelementptr {elem_ty}, ptr {data}, i64 {index}"
        )
        .unwrap();
        addr
    }

    fn heap_load_words(&mut self, addr: &str, words: usize) -> Vec<String> {
        (0..words)
            .map(|k| {
                let at = self.fresh();
                let word = self.fresh();
                writeln!(self.out, "  {at} = getelementptr i64, ptr {addr}, i64 {k}").unwrap();
                writeln!(self.out, "  {word} = load i64, ptr {at}").unwrap();
                word
            })
            .collect()
    }

    fn heap_store_words(&mut self, addr: &str, words: &[String]) {
        for (k, word) in words.iter().enumerate() {
            let at = self.fresh();
            writeln!(self.out, "  {at} = getelementptr i64, ptr {addr}, i64 {k}").unwrap();
            writeln!(self.out, "  store i64 {word}, ptr {at}").unwrap();
        }
    }

    /// Whether `a` orders before `b`, word by word as signed integers.
    fn heap_words_less(&mut self, a: &[String], b: &[String]) -> String {
        let last = a.len() - 1;
        let mut less = self.fresh();
        writeln!(self.out, "  {less} = icmp slt i64 {}, {}", a[last], b[last]).unwrap();
        for k in (0..last).rev() {
            let same = self.fresh();
            let below = self.fresh();
            let decided = self.fresh();
            writeln!(self.out, "  {same} = icmp eq i64 {}, {}", a[k], b[k]).unwrap();
            writeln!(self.out, "  {below} = icmp slt i64 {}, {}", a[k], b[k]).unwrap();
            writeln!(
                self.out,
                "  {decided} = select i1 {same}, i1 {less}, i1 {below}"
            )
            .unwrap();
            less = decided;
        }
        less
    }
}
