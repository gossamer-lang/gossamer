//! Landing pads for bodies that register deferred expressions.
//!
//! MIR gives such a body two pad blocks, reached from a probe switch at
//! entry: a cleanup pad that runs the pending expressions of the region the
//! fault left (MIR's `__gos_unwind_region` local) and resumes unwinding, and
//! a note pad that records a panic one of those expressions raised and goes
//! on with the next. Here the probe's edge is dropped and every call in the
//! body is given an edge to a pad.
//!
//! A call in a pad's own code is a catching call: the runtime makes it
//! through a thunk under `catch_unwind`, which ends the nested panic as Rust
//! expects, and the call answers whether one unwound, on which the code
//! branches to the note pad. A call in the body's own code is an `invoke`
//! whose unwind edge reaches the cleanup pad's `landingpad`, where the
//! unwinder is DWARF-based. Windows unwinds Rust panics through SEH, whose
//! landing pads Rust's personality does not serve, so there those calls are
//! catching calls too and the pad resumes the fault the runtime holds.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use gossamer_mir::{Body, Local, Operand};

use super::{BuildError, Lowerer, declare_rt};

/// The alloca holding the exception a cleanup pad caught, for `resume`.
pub(crate) const EXCEPTION_SLOT: &str = "%gos.unwind.exc";

/// The alloca a catching call passes its callee, arguments, and results in.
const CATCHING_BUFFER: &str = "%gos.unwind.ctx";

/// The bytes each value takes in a catching call's buffer, enough for any
/// scalar, carrier, or vector a call passes.
const SLOT_BYTES: usize = 16;

/// How a body's own calls reach its cleanup pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PadEntry {
    /// Through an `invoke` and the pad's `landingpad`.
    Landing,
    /// Through a catching call and a branch.
    Catching,
}

impl PadEntry {
    /// The entry the target unwinds Rust panics through.
    pub(crate) fn for_target() -> Self {
        // `GOS_LLVM_UNWIND=catching` selects the Windows lowering on any
        // target, so it can be run where SEH is not available.
        let forced = std::env::var("GOS_LLVM_UNWIND").is_ok_and(|value| value == "catching");
        if forced || crate::emit::target_is_windows() {
            Self::Catching
        } else {
            Self::Landing
        }
    }
}

/// Where a body's landing pads are.
#[derive(Debug, Clone)]
pub(crate) struct UnwindPlan {
    /// The local the entry probe switches on.
    pub(crate) probe: Local,
    /// The block a fault in the body's own code lands in.
    pub(crate) cleanup_pad: u32,
    /// The block a fault in a pad's code lands in.
    pub(crate) note_pad: u32,
    /// The blocks a pad's code runs in, the pads included.
    pub(crate) pad_code: HashSet<u32>,
    /// How the body's own calls reach the cleanup pad.
    pub(crate) entry: PadEntry,
}

impl UnwindPlan {
    /// The pads of `body`, when it has them.
    pub(crate) fn of(body: &Body) -> Option<Self> {
        let pads = gossamer_mir::UnwindPads::of(body)?;
        Some(Self {
            probe: pads.probe,
            cleanup_pad: pads.cleanup_pad.as_u32(),
            note_pad: pads.note_pad.as_u32(),
            pad_code: pads.pad_code.iter().map(|block| block.as_u32()).collect(),
            entry: PadEntry::for_target(),
        })
    }
}

impl Lowerer<'_> {
    /// Opens a pad block, when `block` is one.
    pub(crate) fn emit_landing_pad(&mut self, block: u32) {
        let Some(plan) = self.unwind.clone() else {
            return;
        };
        if block == plan.cleanup_pad {
            declare_rt(&mut self.runtime_refs, "gos_rt_unwind_begin");
            if plan.entry == PadEntry::Landing {
                let caught = self.fresh();
                writeln!(self.out, "  {caught} = landingpad {{ ptr, i32 }} cleanup").unwrap();
                writeln!(
                    self.out,
                    "  store {{ ptr, i32 }} {caught}, ptr {EXCEPTION_SLOT}"
                )
                .unwrap();
            }
            writeln!(self.out, "  call void @gos_rt_unwind_begin()").unwrap();
        } else if block == plan.note_pad {
            declare_rt(&mut self.runtime_refs, "gos_rt_unwind_note");
            writeln!(self.out, "  call void @gos_rt_unwind_note()").unwrap();
        }
    }

    /// Lowers `Terminator::Resume`: the frame's pad has run its deferred
    /// expressions, and the fault it caught continues to the callers.
    pub(crate) fn lower_resume(&mut self) {
        declare_rt(&mut self.runtime_refs, "gos_rt_unwind_end");
        writeln!(self.out, "  call void @gos_rt_unwind_end()").unwrap();
        let entry = self
            .unwind
            .as_ref()
            .map_or(PadEntry::Landing, |plan| plan.entry);
        match entry {
            PadEntry::Landing => {
                let exception = self.fresh();
                writeln!(
                    self.out,
                    "  {exception} = load {{ ptr, i32 }}, ptr {EXCEPTION_SLOT}"
                )
                .unwrap();
                writeln!(self.out, "  resume {{ ptr, i32 }} {exception}").unwrap();
            }
            PadEntry::Catching => {
                declare_rt(&mut self.runtime_refs, "gos_rt_unwind_resume");
                writeln!(self.out, "  call void @gos_rt_unwind_resume()").unwrap();
                writeln!(self.out, "  unreachable").unwrap();
            }
        }
    }

    /// Whether `discriminant` is the entry probe, whose switch lowers to
    /// its default: the pads are reached only by unwinding.
    pub(crate) fn is_unwind_probe(&self, discriminant: &Operand) -> bool {
        matches!(
            (discriminant, &self.unwind),
            (Operand::Copy(place), Some(plan))
                if place.local == plan.probe && place.projection.is_empty()
        )
    }

    /// Gives every call in the lowered body an edge to its pad, recording
    /// the catching thunks and runtime entries the rewritten body uses.
    pub(crate) fn connect_pads(&mut self, plan: &UnwindPlan) -> Result<(), BuildError> {
        let connected = connect_calls(&self.out, plan).ok_or(BuildError::InternalLoweringBug(
            "a call in a body with landing pads is not in a shape a catching call reads",
        ))?;
        self.out = connected.text;
        if connected.catching {
            for helper in ["gos_rt_unwind_try_call", "gos_rt_unwind_try_call_in_pad"] {
                declare_rt(&mut self.runtime_refs, helper);
            }
        }
        self.runtime_refs.extend(connected.thunks);
        Ok(())
    }
}

/// A lowered body with every call connected to its pad.
struct Connected {
    text: String,
    /// The catching thunks the body calls through, as module-level text.
    thunks: Vec<String>,
    /// Whether the body makes any catching call.
    catching: bool,
}

/// One `call` line, split into the parts a catching call reassembles.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallLine<'a> {
    /// The value the call defines, as `%name`.
    result: Option<&'a str>,
    /// Everything between `call` and the callee: calling convention, return
    /// attributes, and the return or function type.
    head: &'a str,
    /// The type the call answers, `void` for none.
    ret: &'a str,
    /// The callee, `@name` or `%pointer`.
    callee: &'a str,
    /// Each argument's type with its attributes, and its value.
    args: Vec<(&'a str, &'a str)>,
    /// What follows the argument list before any metadata.
    attrs: &'a str,
    /// The trailing metadata attachments, from `, !` on.
    meta: &'a str,
}

/// The index just past the bracket group opening at `open`, which `text`
/// starts with.
fn close_of(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    for (index, ch) in text[open..].char_indices() {
        match ch {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '(' | '{' | '[' | '<' => depth += 1,
            ')' | '}' | ']' | '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The length of the type `text` starts with.
fn type_len(text: &str) -> Option<usize> {
    match text.chars().next()? {
        '{' | '[' | '<' => close_of(text, 0),
        _ => Some(text.find([' ', ',', ')']).unwrap_or(text.len())),
    }
}

/// Splits `list` at its top-level commas.
fn split_top_level(list: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut start = 0;
    for (index, ch) in list.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '(' | '{' | '[' | '<' => depth += 1,
            ')' | '}' | ']' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(list[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    let last = list[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

/// The parameter attributes an argument may carry between its type and
/// its value.
const ARG_ATTRIBUTES: &[&str] = &[
    "noundef",
    "nonnull",
    "noalias",
    "nocapture",
    "readonly",
    "writeonly",
    "readnone",
    "signext",
    "zeroext",
    "inreg",
    "returned",
    "nofree",
    "nest",
    "immarg",
    "align",
];

/// Splits one argument into its type with attributes, and its value.
fn split_arg(arg: &str) -> Option<(&str, &str)> {
    let mut end = type_len(arg)?;
    loop {
        let rest = arg[end..].trim_start();
        let offset = arg.len() - rest.len();
        let word_len = rest.find([' ', '(']).unwrap_or(rest.len());
        let word = &rest[..word_len];
        let attribute = ARG_ATTRIBUTES.contains(&word)
            || rest[word_len..].starts_with('(')
                && word.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        if !attribute {
            return Some((arg[..end].trim_end(), rest));
        }
        end = if rest[word_len..].starts_with('(') {
            offset + close_of(rest, word_len)?
        } else if word == "align" {
            let digits = rest[word_len..].trim_start();
            offset + (rest.len() - digits.len()) + digits.find(' ').unwrap_or(digits.len())
        } else {
            offset + word_len
        };
    }
}

impl<'a> CallLine<'a> {
    /// Parses a trimmed `call` line.
    fn parse(line: &'a str) -> Option<Self> {
        let (result, rest) = match line.split_once(" = ") {
            Some((name, rest)) if name.starts_with('%') && !name.contains(' ') => {
                (Some(name), rest)
            }
            _ => (None, line),
        };
        let rest = rest.strip_prefix("tail ").unwrap_or(rest);
        let rest = rest.strip_prefix("call ")?;
        let callee_at = rest.char_indices().find_map(|(index, ch)| {
            if ch != '@' && ch != '%' {
                return None;
            }
            let after = &rest[index + 1..];
            let len = if let Some(quoted) = after.strip_prefix('"') {
                quoted.find('"')? + 2
            } else {
                after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || "_.$-".contains(c)))
                    .unwrap_or(after.len())
            };
            after[len..]
                .starts_with('(')
                .then_some((index, index + 1 + len))
        })?;
        let (start, end) = callee_at;
        let head = rest[..start].trim_end();
        let callee = &rest[start..end];
        let args_end = close_of(rest, end)?;
        let args = split_top_level(&rest[end + 1..args_end - 1])
            .into_iter()
            .map(split_arg)
            .collect::<Option<Vec<_>>>()?;
        let tail = &rest[args_end..];
        let (attrs, meta) = match tail.find(", !") {
            Some(at) => (&tail[..at], &tail[at..]),
            None => (tail, ""),
        };
        // An explicit function type (`i64 (ptr, ...)`) names the return type
        // before its parameter list.
        let ret_head = if head.ends_with(')') {
            let open = head.rfind(" (")?;
            head[..open].trim_end()
        } else {
            head
        };
        let ret = match ret_head.chars().last()? {
            '}' | ']' | '>' => {
                let open_char = match ret_head.chars().last()? {
                    '}' => '{',
                    ']' => '[',
                    _ => '<',
                };
                &ret_head[ret_head.rfind(open_char)?..]
            }
            _ => ret_head.rsplit(' ').next()?,
        };
        Some(Self {
            result,
            head,
            ret,
            callee,
            args,
            attrs,
            meta,
        })
    }

    /// The name of the direct callee, without its sigil or quotes.
    fn direct_name(&self) -> Option<&'a str> {
        self.callee
            .strip_prefix('@')
            .map(|name| name.trim_matches('"'))
    }

    /// The catching thunk for this call's signature, as a module-level
    /// definition, and its name.
    fn thunk(&self) -> (String, String) {
        let mut signature = String::new();
        let _ = write!(signature, "{}|{}", self.head, self.attrs.trim());
        for (ty, _) in &self.args {
            let _ = write!(signature, "|{ty}");
        }
        let name = format!("@gos.unwind.thunk.{:016x}", fnv1a(signature.as_bytes()));
        let mut text = format!(
            "define internal void {name}(ptr %ctx) {{\nentry:\n  %callee = load ptr, ptr %ctx\n"
        );
        let mut call_args = Vec::with_capacity(self.args.len());
        for (index, (ty, _)) in self.args.iter().enumerate() {
            let offset = (1 + index) * SLOT_BYTES;
            let value_ty = &ty[..type_len(ty).unwrap_or(ty.len())];
            let _ = writeln!(
                text,
                "  %p{index} = getelementptr inbounds i8, ptr %ctx, i64 {offset}"
            );
            let _ = writeln!(text, "  %a{index} = load {value_ty}, ptr %p{index}");
            call_args.push(format!("{ty} %a{index}"));
        }
        let call = format!(
            "call {} %callee({}){}",
            self.head,
            call_args.join(", "),
            self.attrs
        );
        if self.ret == "void" {
            let _ = writeln!(text, "  {call}");
        } else {
            let offset = (1 + self.args.len()) * SLOT_BYTES;
            let _ = writeln!(text, "  %r = {call}");
            let _ = writeln!(
                text,
                "  %rp = getelementptr inbounds i8, ptr %ctx, i64 {offset}"
            );
            let _ = writeln!(text, "  store {} %r, ptr %rp", self.ret);
        }
        text.push_str("  ret void\n}");
        (text, name)
    }

    /// The words of a catching call's buffer this call needs.
    fn words(&self) -> usize {
        1 + self.args.len() + usize::from(self.ret != "void")
    }
}

/// The 64-bit FNV-1a hash of `bytes`, naming a thunk by its signature the
/// same way in every build.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Writes a catching call for `call`, which branches to `bb{pad}` when a
/// fault unwinds out of the callee and continues at label `resumed`.
fn write_catching_call(
    out: &mut String,
    call: &CallLine<'_>,
    thunk: &str,
    shim: &str,
    pad: u32,
    resumed: &str,
    id: u32,
) {
    let ctx = CATCHING_BUFFER;
    let _ = writeln!(out, "  store ptr {}, ptr {ctx}", call.callee);
    for (index, (ty, value)) in call.args.iter().enumerate() {
        let offset = (1 + index) * SLOT_BYTES;
        let value_ty = &ty[..type_len(ty).unwrap_or(ty.len())];
        let _ = writeln!(
            out,
            "  %gos.cc.{id}.p{index} = getelementptr inbounds i8, ptr {ctx}, i64 {offset}"
        );
        let _ = writeln!(out, "  store {value_ty} {value}, ptr %gos.cc.{id}.p{index}");
    }
    let _ = writeln!(
        out,
        "  %gos.cc.{id}.st = call i32 @{shim}(ptr {thunk}, ptr {ctx}){}",
        call.meta
    );
    let _ = writeln!(out, "  %gos.cc.{id}.f = icmp ne i32 %gos.cc.{id}.st, 0");
    let _ = writeln!(
        out,
        "  br i1 %gos.cc.{id}.f, label %bb{pad}, label %{resumed}"
    );
    let _ = writeln!(out, "{resumed}:");
    if let Some(result) = call.result {
        let offset = (1 + call.args.len()) * SLOT_BYTES;
        let _ = writeln!(
            out,
            "  %gos.cc.{id}.rp = getelementptr inbounds i8, ptr {ctx}, i64 {offset}"
        );
        let _ = writeln!(out, "  {result} = load {}, ptr %gos.cc.{id}.rp", call.ret);
    }
}

/// Rewrites every call in a lowered body with landing pads so a fault in it
/// reaches a pad: one in a pad's code, the note pad through a catching call;
/// one in the body's own code, the cleanup pad through an `invoke` or a
/// catching call, as `plan.entry` says. `text` is the whole `define`.
fn connect_calls(text: &str, plan: &UnwindPlan) -> Option<Connected> {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    // The block each original label's terminator ends up in once its calls
    // split it, for the `phi`s that name the label as a predecessor.
    let mut last_segment: HashMap<String, String> = HashMap::new();
    let mut current_label = String::new();
    let mut current_class: Option<u32> = None;
    let mut next = 0u32;
    let mut thunks: Vec<String> = Vec::new();
    let mut buffer_words = 0usize;
    for line in text.lines() {
        if let Some(label) = line.strip_suffix(':')
            && !label.starts_with(' ')
            && !label.contains(' ')
        {
            current_label = label.to_string();
            if let Some(id) = label.strip_prefix("bb").and_then(|n| n.parse::<u32>().ok()) {
                current_class = Some(id);
            }
            last_segment.insert(current_label.clone(), current_label.clone());
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let trimmed = line.trim_start();
        let is_call = (trimmed.starts_with("call ")
            || trimmed.starts_with("tail call ")
            || (trimmed.starts_with('%')
                && (trimmed.contains(" = call ") || trimmed.contains(" = tail call "))))
            && !trimmed.contains("musttail")
            && !trimmed.contains(" asm ");
        let (Some(block), true) = (current_class, is_call) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let pad_block = block == plan.cleanup_pad || block == plan.note_pad;
        let call = CallLine::parse(trimmed);
        let skip = pad_block
            || call.as_ref().is_some_and(|call| {
                call.direct_name().is_some_and(|name| {
                    name.starts_with("llvm.") || name.starts_with("gos_rt_unwind_")
                })
            });
        if skip {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let in_pad = plan.pad_code.contains(&block);
        let resumed = format!("inv.{next}");
        let id = next;
        next += 1;
        if in_pad || plan.entry == PadEntry::Catching {
            let call = call?;
            let (thunk_text, thunk_name) = call.thunk();
            thunks.push(thunk_text);
            buffer_words = buffer_words.max(call.words());
            let (shim, pad) = if in_pad {
                ("gos_rt_unwind_try_call_in_pad", plan.note_pad)
            } else {
                ("gos_rt_unwind_try_call", plan.cleanup_pad)
            };
            write_catching_call(&mut out, &call, &thunk_name, shim, pad, &resumed, id);
        } else {
            let rewritten = trimmed
                .replacen("tail call ", "invoke ", 1)
                .replacen("call ", "invoke ", 1);
            let (core, meta) = match rewritten.find(", !") {
                Some(at) => (&rewritten[..at], &rewritten[at..]),
                None => (rewritten.as_str(), ""),
            };
            writeln!(
                out,
                "  {core} to label %{resumed} unwind label %bb{}{meta}",
                plan.cleanup_pad
            )
            .unwrap();
            writeln!(out, "{resumed}:").unwrap();
        }
        last_segment.insert(current_label.clone(), resumed);
    }
    if buffer_words > 0 {
        // The buffer is one per frame, allocated once at entry: calls in a
        // frame are made one at a time.
        let alloca = format!(
            "  {CATCHING_BUFFER} = alloca [{} x i8], align 16\n",
            buffer_words * SLOT_BYTES
        );
        if let Some(at) = out.find("\nentry:\n") {
            out.insert_str(at + "\nentry:\n".len(), &alloca);
        }
    }
    // A predecessor named by a `phi` is the block its terminator is in.
    let renamed: HashMap<&String, &String> = last_segment
        .iter()
        .filter(|(label, last)| label != last)
        .collect();
    let text = if renamed.is_empty() {
        out
    } else {
        let mut fixed = String::with_capacity(out.len());
        for line in out.lines() {
            if line.contains(" = phi ") {
                let mut rewritten = line.to_string();
                for (label, last) in &renamed {
                    rewritten =
                        rewritten.replace(&format!(", %{label} ]"), &format!(", %{last} ]"));
                }
                fixed.push_str(&rewritten);
            } else {
                fixed.push_str(line);
            }
            fixed.push('\n');
        }
        fixed
    };
    Some(Connected {
        text,
        catching: !thunks.is_empty(),
        thunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_line_splits_into_its_parts() {
        let call = CallLine::parse(
            "%t4 = call noalias ptr @\"gos_rt_vec_new\"(i64 %t2, { i64, ptr } %t3) #2, !dbg !7",
        )
        .expect("parses");
        assert_eq!(call.result, Some("%t4"));
        assert_eq!(call.head, "noalias ptr");
        assert_eq!(call.ret, "ptr");
        assert_eq!(call.callee, "@\"gos_rt_vec_new\"");
        assert_eq!(call.args, vec![("i64", "%t2"), ("{ i64, ptr }", "%t3")]);
        assert_eq!(call.attrs, " #2");
        assert_eq!(call.meta, ", !dbg !7");
    }

    #[test]
    fn an_indirect_call_with_an_explicit_type_keeps_its_return_type() {
        let call =
            CallLine::parse("call i64 (ptr, ...) %fn_ptr(ptr noundef nonnull align 8 %a, i128 7)")
                .expect("parses");
        assert_eq!(call.ret, "i64");
        assert_eq!(call.callee, "%fn_ptr");
        assert_eq!(
            call.args,
            vec![("ptr noundef nonnull align 8", "%a"), ("i128", "7")]
        );
    }

    #[test]
    fn an_argument_keeps_a_parenthesised_attribute_with_its_type() {
        assert_eq!(
            split_arg("ptr byval({ i64, i64 }) align 8 %v"),
            Some(("ptr byval({ i64, i64 }) align 8", "%v"))
        );
        assert_eq!(split_arg("i64 0"), Some(("i64", "0")));
    }

    #[test]
    fn a_void_call_with_no_arguments_parses() {
        let call = CallLine::parse("call preserve_mostcc void @gos_rt_tick()").expect("parses");
        assert_eq!(call.ret, "void");
        assert!(call.args.is_empty());
        assert_eq!(call.words(), 1);
    }
}
