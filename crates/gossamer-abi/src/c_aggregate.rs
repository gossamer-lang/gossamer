//! How a C struct passed or returned by value crosses each supported calling
//! convention, decided once here so the LLVM backend, the Cranelift backend,
//! and the bytecode VM's call stubs lower the same call the same way.
//!
//! A [`CLayout`] describes the struct by its size, alignment, and scalar
//! leaves; [`plan_call`] walks a whole signature, tracking the registers each
//! argument takes, and answers how every argument and the result travel:
//! as scalars loaded from (or stored to) the struct's bytes, as a copy on the
//! stack, as a pointer to a copy, or through a hidden result pointer.

/// The calling conventions a by-value struct is lowered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CAbi {
    /// The System V AMD64 ABI (Linux, macOS, and other Unix on x86-64).
    SysV64,
    /// The Microsoft x64 calling convention.
    Win64,
    /// AAPCS64, Linux and Apple alike for the cases a struct reaches.
    Aapcs64,
    /// The RISC-V LP64D convention.
    RiscV64,
}

impl CAbi {
    /// The convention of the target named by `arch` and `os` (Rust's
    /// `target_arch` and `target_os` spellings), or `None` for a target this
    /// release does not lower by-value structs for.
    #[must_use]
    pub fn for_target(arch: &str, os: &str) -> Option<Self> {
        match arch {
            "x86_64" if os == "windows" => Some(Self::Win64),
            "x86_64" => Some(Self::SysV64),
            "aarch64" => Some(Self::Aapcs64),
            "riscv64" => Some(Self::RiscV64),
            _ => None,
        }
    }

    /// The convention of the host running the toolchain.
    #[must_use]
    pub fn host() -> Option<Self> {
        Self::for_target(std::env::consts::ARCH, std::env::consts::OS)
    }
}

/// A scalar inside a struct: its byte offset and C class character (see
/// `gossamer_types::c_class_width`). A union lists every member's leaves, so
/// leaves may overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CLeafClass {
    /// Byte offset from the start of the struct.
    pub offset: u32,
    /// The scalar's class character.
    pub class: char,
}

/// A struct's C layout as far as a calling convention looks at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CLayout {
    /// Size in bytes, a multiple of `align`.
    pub size: u32,
    /// Alignment in bytes.
    pub align: u32,
    /// Every scalar leaf, in offset order.
    pub leaves: Vec<CLeafClass>,
}

/// Bytes a scalar of class `class` occupies.
fn class_width(class: char) -> u32 {
    match class {
        'h' | 'H' => 2,
        'i' | 'I' | 'f' => 4,
        'l' | 'L' | 'd' => 8,
        _ => 1,
    }
}

fn class_is_float(class: char) -> bool {
    matches!(class, 'f' | 'd')
}

fn class_is_signed(class: char) -> bool {
    matches!(class, 'c' | 'h' | 'i' | 'l')
}

impl CLayout {
    /// The spelling a foreign signature carries: `size,align;` then each
    /// leaf as its offset followed by its class character (`16,8;0i,8d`).
    #[must_use]
    pub fn render(&self) -> String {
        let leaves: Vec<String> = self
            .leaves
            .iter()
            .map(|leaf| format!("{}{}", leaf.offset, leaf.class))
            .collect();
        format!("{},{};{}", self.size, self.align, leaves.join(","))
    }

    /// Reads the spelling [`CLayout::render`] writes.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (head, leaves) = text.split_once(';')?;
        let (size, align) = head.split_once(',')?;
        let mut parsed = Vec::new();
        for leaf in leaves.split(',').filter(|leaf| !leaf.is_empty()) {
            let class = leaf.chars().last()?;
            let offset = &leaf[..leaf.len() - class.len_utf8()];
            if class.is_ascii_digit() {
                return None;
            }
            parsed.push(CLeafClass {
                offset: offset.parse().ok()?,
                class,
            });
        }
        Some(Self {
            size: size.parse().ok()?,
            align: align.parse().ok()?,
            leaves: parsed,
        })
    }

    /// Whether two leaves share bytes: a union.
    fn has_overlap(&self) -> bool {
        let mut end = 0u32;
        for leaf in &self.leaves {
            if leaf.offset < end {
                return true;
            }
            end = leaf.offset + class_width(leaf.class);
        }
        false
    }

    /// The homogeneous float aggregate this layout is, if it is one: its
    /// element class (`f` or `d`) and member count (1 to 4), every member
    /// back to back with no padding.
    fn hfa(&self) -> Option<(char, u32)> {
        let first = self.leaves.first()?.class;
        if !class_is_float(first) {
            return None;
        }
        let width = class_width(first);
        let mut offsets: Vec<u32> = Vec::new();
        for leaf in &self.leaves {
            if leaf.class != first || leaf.offset % width != 0 {
                return None;
            }
            if !offsets.contains(&leaf.offset) {
                offsets.push(leaf.offset);
            }
        }
        let count = self.size / width;
        let covered = (0..count).all(|i| offsets.contains(&(i * width)));
        (self.size.is_multiple_of(width)
            && (1..=4).contains(&count)
            && covered
            && offsets.len() == count as usize)
            .then_some((first, count))
    }
}

/// How one register-sized piece of a struct crosses: an integer of `bytes`
/// bytes (1, 2, 4, or 8; `signed` says how a narrow one extends), or a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PieceKind {
    /// An integer of this many bytes.
    Int {
        /// 1, 2, 4, or 8.
        bytes: u8,
        /// Whether a narrow value sign-extends.
        signed: bool,
    },
    /// A `float`.
    F32,
    /// A `double`, or eight bytes of floats travelling in one SIMD register.
    F64,
}

impl PieceKind {
    /// Bytes the piece loads from or stores to the struct.
    #[must_use]
    pub fn bytes(self) -> u32 {
        match self {
            Self::Int { bytes, .. } => u32::from(bytes),
            Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Whether the piece travels in a floating-point register.
    #[must_use]
    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

/// A piece of a struct's bytes, at `offset`, travelling as `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Piece {
    /// Byte offset into the struct.
    pub offset: u32,
    /// How the piece travels.
    pub kind: PieceKind,
}

/// How one argument crosses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ArgPlan {
    /// A scalar, passed as it is.
    Scalar,
    /// The struct's bytes as scalar pieces, in order. `pad_int` and
    /// `pad_float` dummy arguments of each register class come first: they
    /// use up the registers a convention retires when the struct does not
    /// fit in the ones left, so the pieces and every later argument of that
    /// class go to the stack.
    Pieces {
        /// Dummy integer-register arguments before the pieces.
        pad_int: u8,
        /// Dummy float-register arguments before the pieces.
        pad_float: u8,
        /// The pieces.
        pieces: Vec<Piece>,
    },
    /// A copy of the struct on the stack (System V's memory class).
    Memory {
        /// The struct's size.
        size: u32,
        /// The struct's alignment.
        align: u32,
    },
    /// A pointer to a copy of the struct.
    Indirect,
}

/// How the result crosses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RetPlan {
    /// No result.
    Void,
    /// A scalar.
    Scalar,
    /// The struct's bytes as scalar pieces, in order.
    Pieces(Vec<Piece>),
    /// Through a hidden pointer to caller-provided memory, passed before the
    /// other arguments.
    Sret,
}

/// One parameter or result of a C signature, as [`plan_call`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CArg {
    /// A scalar of this class character (`L` for an address).
    Scalar(char),
    /// A struct by value.
    Aggregate(CLayout),
}

/// The whole lowering of one call: each argument's plan, in order, and the
/// result's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallPlan {
    /// One plan per parameter.
    pub params: Vec<ArgPlan>,
    /// The result's plan.
    pub ret: RetPlan,
}

impl CallPlan {
    /// Whether the call passes a hidden result pointer.
    #[must_use]
    pub fn has_sret(&self) -> bool {
        self.ret == RetPlan::Sret
    }
}

/// Registers still free while a signature is walked.
struct Registers {
    int: u8,
    float: u8,
}

/// Plans a call to a function taking `params` and answering `ret` (`None`
/// for `void`) under `abi`.
#[must_use]
pub fn plan_call(abi: CAbi, params: &[CArg], ret: Option<&CArg>) -> CallPlan {
    let (int, float) = match abi {
        CAbi::SysV64 => (6, 8),
        CAbi::Win64 => (4, 4),
        CAbi::Aapcs64 | CAbi::RiscV64 => (8, 8),
    };
    let mut regs = Registers { int, float };
    let ret = match ret {
        None => RetPlan::Void,
        Some(CArg::Scalar(_)) => RetPlan::Scalar,
        Some(CArg::Aggregate(layout)) => plan_ret(abi, layout),
    };
    // A hidden result pointer takes the first integer argument register,
    // except on AArch64, where it has its own (x8).
    if ret == RetPlan::Sret && abi != CAbi::Aapcs64 {
        regs.int = regs.int.saturating_sub(1);
    }
    let params = params
        .iter()
        .map(|param| match param {
            CArg::Scalar(class) => {
                take_scalar(abi, *class, &mut regs);
                ArgPlan::Scalar
            }
            CArg::Aggregate(layout) => plan_arg(abi, layout, &mut regs),
        })
        .collect();
    CallPlan { params, ret }
}

fn take_scalar(abi: CAbi, class: char, regs: &mut Registers) {
    match abi {
        // Win64 assigns one positional slot per argument, whatever its class.
        CAbi::Win64 => {
            regs.int = regs.int.saturating_sub(1);
            regs.float = regs.float.saturating_sub(1);
        }
        // A RISC-V float takes an integer register once the float ones run
        // out.
        CAbi::RiscV64 if class_is_float(class) => {
            if regs.float > 0 {
                regs.float -= 1;
            } else {
                regs.int = regs.int.saturating_sub(1);
            }
        }
        _ if class_is_float(class) => regs.float = regs.float.saturating_sub(1),
        _ => regs.int = regs.int.saturating_sub(1),
    }
}

fn int_piece(offset: u32, bytes: u32) -> Piece {
    let bytes = match bytes {
        0 | 1 => 1,
        2 => 2,
        3 | 4 => 4,
        _ => 8,
    };
    Piece {
        offset,
        kind: PieceKind::Int {
            bytes,
            signed: false,
        },
    }
}

/// System V's eightbyte classification: one piece per eightbyte, a float
/// piece where every leaf in it is a float, or `None` for the memory class.
fn sysv_pieces(layout: &CLayout) -> Option<Vec<Piece>> {
    if layout.size > 16 || layout.size == 0 {
        return None;
    }
    let mut pieces = Vec::new();
    for word in 0..layout.size.div_ceil(8) {
        let start = word * 8;
        let end = (start + 8).min(layout.size);
        let leaves: Vec<&CLeafClass> = layout
            .leaves
            .iter()
            .filter(|leaf| leaf.offset >= start && leaf.offset < end)
            .collect();
        if leaves
            .iter()
            .any(|leaf| leaf.offset + class_width(leaf.class) > end)
        {
            return None;
        }
        if leaves.is_empty() {
            continue;
        }
        // Only the bytes a leaf reaches travel; the rest of the eightbyte is
        // padding.
        let used = leaves
            .iter()
            .map(|leaf| leaf.offset + class_width(leaf.class))
            .max()
            .unwrap_or(start)
            - start;
        if leaves.iter().all(|leaf| class_is_float(leaf.class)) {
            pieces.push(Piece {
                offset: start,
                kind: if used <= 4 {
                    PieceKind::F32
                } else {
                    PieceKind::F64
                },
            });
        } else {
            pieces.push(int_piece(start, used));
        }
    }
    Some(pieces)
}

/// RISC-V's hardware floating-point rule: a struct of one float, two
/// floats, or a float and an integer (after flattening) travels in those
/// registers. `None` when the struct is none of these.
fn riscv_float_pieces(layout: &CLayout) -> Option<Vec<Piece>> {
    if layout.has_overlap() || layout.leaves.is_empty() || layout.leaves.len() > 2 {
        return None;
    }
    if !layout.leaves.iter().any(|leaf| class_is_float(leaf.class)) {
        return None;
    }
    Some(
        layout
            .leaves
            .iter()
            .map(|leaf| Piece {
                offset: leaf.offset,
                kind: match leaf.class {
                    'f' => PieceKind::F32,
                    'd' => PieceKind::F64,
                    class => PieceKind::Int {
                        bytes: u8::try_from(class_width(class)).unwrap_or(8),
                        signed: class_is_signed(class),
                    },
                },
            })
            .collect(),
    )
}

fn count_classes(pieces: &[Piece]) -> (u8, u8) {
    let floats = pieces.iter().filter(|piece| piece.kind.is_float()).count();
    let ints = pieces.len() - floats;
    (
        u8::try_from(ints).unwrap_or(u8::MAX),
        u8::try_from(floats).unwrap_or(u8::MAX),
    )
}

fn plan_arg(abi: CAbi, layout: &CLayout, regs: &mut Registers) -> ArgPlan {
    match abi {
        CAbi::SysV64 => plan_sysv(layout, regs),
        CAbi::Win64 => plan_win64(layout, regs),
        CAbi::Aapcs64 => plan_aapcs64(layout, regs),
        CAbi::RiscV64 => plan_riscv64(layout, regs),
    }
}

fn plan_sysv(layout: &CLayout, regs: &mut Registers) -> ArgPlan {
    match sysv_pieces(layout) {
        Some(pieces) => {
            let (ints, floats) = count_classes(&pieces);
            // A struct that does not fit in the registers left goes to
            // the stack whole, and later arguments may still use them.
            if ints > regs.int || floats > regs.float {
                return ArgPlan::Memory {
                    size: layout.size,
                    align: layout.align,
                };
            }
            regs.int -= ints;
            regs.float -= floats;
            ArgPlan::Pieces {
                pad_int: 0,
                pad_float: 0,
                pieces,
            }
        }
        None => ArgPlan::Memory {
            size: layout.size,
            align: layout.align,
        },
    }
}

fn plan_win64(layout: &CLayout, regs: &mut Registers) -> ArgPlan {
    regs.int = regs.int.saturating_sub(1);
    regs.float = regs.float.saturating_sub(1);
    if matches!(layout.size, 1 | 2 | 4 | 8) {
        ArgPlan::Pieces {
            pad_int: 0,
            pad_float: 0,
            pieces: vec![int_piece(0, layout.size)],
        }
    } else {
        ArgPlan::Indirect
    }
}

fn plan_aapcs64(layout: &CLayout, regs: &mut Registers) -> ArgPlan {
    if let Some((class, count)) = layout.hfa() {
        let width = class_width(class);
        let kind = if class == 'f' {
            PieceKind::F32
        } else {
            PieceKind::F64
        };
        let count8 = u8::try_from(count).unwrap_or(4);
        if count8 <= regs.float {
            regs.float -= count8;
            return ArgPlan::Pieces {
                pad_int: 0,
                pad_float: 0,
                pieces: (0..count)
                    .map(|i| Piece {
                        offset: i * width,
                        kind,
                    })
                    .collect(),
            };
        }
        // On the stack the struct keeps its memory image, carried in
        // eight-byte float pieces once the float registers are gone.
        let pad_float = regs.float;
        regs.float = 0;
        return ArgPlan::Pieces {
            pad_int: 0,
            pad_float,
            pieces: (0..layout.size.div_ceil(8))
                .map(|word| Piece {
                    offset: word * 8,
                    kind: PieceKind::F64,
                })
                .collect(),
        };
    }
    if layout.size > 16 {
        regs.int = regs.int.saturating_sub(1);
        return ArgPlan::Indirect;
    }
    let pieces: Vec<Piece> = (0..layout.size.div_ceil(8))
        .map(|word| int_piece(word * 8, 8))
        .collect();
    let needed = u8::try_from(pieces.len()).unwrap_or(2);
    if needed <= regs.int {
        regs.int -= needed;
        ArgPlan::Pieces {
            pad_int: 0,
            pad_float: 0,
            pieces,
        }
    } else {
        let pad_int = regs.int;
        regs.int = 0;
        ArgPlan::Pieces {
            pad_int,
            pad_float: 0,
            pieces,
        }
    }
}

fn plan_riscv64(layout: &CLayout, regs: &mut Registers) -> ArgPlan {
    if let Some(pieces) = riscv_float_pieces(layout) {
        let (ints, floats) = count_classes(&pieces);
        if ints <= regs.int && floats <= regs.float {
            regs.int -= ints;
            regs.float -= floats;
            return ArgPlan::Pieces {
                pad_int: 0,
                pad_float: 0,
                pieces,
            };
        }
    }
    if layout.size > 16 {
        regs.int = regs.int.saturating_sub(1);
        return ArgPlan::Indirect;
    }
    // The integer convention may split a struct between the last
    // register and the stack, which scalar pieces do on their own.
    let pieces: Vec<Piece> = (0..layout.size.div_ceil(8))
        .map(|word| int_piece(word * 8, 8))
        .collect();
    let needed = u8::try_from(pieces.len()).unwrap_or(2);
    regs.int = regs.int.saturating_sub(needed);
    ArgPlan::Pieces {
        pad_int: 0,
        pad_float: 0,
        pieces,
    }
}

fn plan_ret(abi: CAbi, layout: &CLayout) -> RetPlan {
    match abi {
        CAbi::SysV64 => sysv_pieces(layout).map_or(RetPlan::Sret, RetPlan::Pieces),
        CAbi::Win64 => {
            if matches!(layout.size, 1 | 2 | 4 | 8) {
                RetPlan::Pieces(vec![int_piece(0, layout.size)])
            } else {
                RetPlan::Sret
            }
        }
        CAbi::Aapcs64 => {
            if let Some((class, count)) = layout.hfa() {
                let width = class_width(class);
                let kind = if class == 'f' {
                    PieceKind::F32
                } else {
                    PieceKind::F64
                };
                return RetPlan::Pieces(
                    (0..count)
                        .map(|i| Piece {
                            offset: i * width,
                            kind,
                        })
                        .collect(),
                );
            }
            if layout.size > 16 {
                return RetPlan::Sret;
            }
            RetPlan::Pieces(
                (0..layout.size.div_ceil(8))
                    .map(|word| int_piece(word * 8, 8))
                    .collect(),
            )
        }
        CAbi::RiscV64 => {
            if let Some(pieces) = riscv_float_pieces(layout) {
                return RetPlan::Pieces(pieces);
            }
            if layout.size > 16 {
                return RetPlan::Sret;
            }
            RetPlan::Pieces(
                (0..layout.size.div_ceil(8))
                    .map(|word| int_piece(word * 8, 8))
                    .collect(),
            )
        }
    }
}

/// The bytes `pieces` reach into the struct, from its start: what a staging
/// buffer for them must hold.
#[must_use]
pub fn pieces_extent(pieces: &[Piece]) -> u32 {
    pieces
        .iter()
        .map(|piece| piece.offset + piece.kind.bytes())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(size: u32, align: u32, leaves: &[(u32, char)]) -> CLayout {
        CLayout {
            size,
            align,
            leaves: leaves
                .iter()
                .map(|(offset, class)| CLeafClass {
                    offset: *offset,
                    class: *class,
                })
                .collect(),
        }
    }

    fn agg(l: CLayout) -> CArg {
        CArg::Aggregate(l)
    }

    fn pieces(plan: &ArgPlan) -> Vec<(u32, PieceKind)> {
        match plan {
            ArgPlan::Pieces { pieces, .. } => pieces.iter().map(|p| (p.offset, p.kind)).collect(),
            other => panic!("expected pieces, got {other:?}"),
        }
    }

    const I8: PieceKind = PieceKind::Int {
        bytes: 8,
        signed: false,
    };
    const I4: PieceKind = PieceKind::Int {
        bytes: 4,
        signed: false,
    };

    #[test]
    fn a_layout_round_trips_through_its_spelling() {
        let l = layout(16, 8, &[(0, 'i'), (8, 'd')]);
        assert_eq!(l.render(), "16,8;0i,8d");
        assert_eq!(CLayout::parse(&l.render()), Some(l));
    }

    #[test]
    fn sysv_classifies_eightbytes() {
        let two_floats = layout(8, 4, &[(0, 'f'), (4, 'f')]);
        let plan = plan_call(CAbi::SysV64, &[agg(two_floats)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, PieceKind::F64)]);
        let mixed = layout(16, 8, &[(0, 'i'), (8, 'd')]);
        let plan = plan_call(CAbi::SysV64, &[agg(mixed)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, I4), (8, PieceKind::F64)]);
        let three_ints = layout(12, 4, &[(0, 'i'), (4, 'i'), (8, 'i')]);
        let plan = plan_call(CAbi::SysV64, &[agg(three_ints)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, I8), (8, I4)]);
        let float_and_int = layout(8, 4, &[(0, 'f'), (4, 'i')]);
        let plan = plan_call(CAbi::SysV64, &[agg(float_and_int)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, I8)]);
        let three_floats = layout(12, 4, &[(0, 'f'), (4, 'f'), (8, 'f')]);
        let plan = plan_call(CAbi::SysV64, &[agg(three_floats)], None);
        assert_eq!(
            pieces(&plan.params[0]),
            vec![(0, PieceKind::F64), (8, PieceKind::F32)]
        );
    }

    #[test]
    fn sysv_passes_a_large_struct_in_memory_and_returns_it_through_sret() {
        let big = layout(24, 8, &[(0, 'l'), (8, 'l'), (16, 'l')]);
        let plan = plan_call(CAbi::SysV64, &[agg(big.clone())], Some(&agg(big)));
        assert_eq!(plan.params[0], ArgPlan::Memory { size: 24, align: 8 });
        assert_eq!(plan.ret, RetPlan::Sret);
    }

    #[test]
    fn sysv_sends_a_struct_to_memory_when_the_registers_run_out() {
        let pair = layout(16, 8, &[(0, 'l'), (8, 'l')]);
        let mut params: Vec<CArg> = (0..5).map(|_| CArg::Scalar('l')).collect();
        params.push(agg(pair.clone()));
        params.push(CArg::Scalar('l'));
        let plan = plan_call(CAbi::SysV64, &params, None);
        assert_eq!(plan.params[5], ArgPlan::Memory { size: 16, align: 8 });
        let plan = plan_call(CAbi::SysV64, &[CArg::Scalar('l'), agg(pair)], None);
        assert_eq!(pieces(&plan.params[1]), vec![(0, I8), (8, I8)]);
    }

    #[test]
    fn sysv_returns_a_mixed_struct_in_two_register_classes() {
        let mixed = layout(16, 8, &[(0, 'd'), (8, 'l')]);
        let plan = plan_call(CAbi::SysV64, &[], Some(&agg(mixed)));
        assert_eq!(
            plan.ret,
            RetPlan::Pieces(vec![
                Piece {
                    offset: 0,
                    kind: PieceKind::F64
                },
                Piece {
                    offset: 8,
                    kind: I8
                },
            ])
        );
    }

    #[test]
    fn win64_passes_power_of_two_sizes_in_a_register_and_others_by_pointer() {
        let float = layout(4, 4, &[(0, 'f')]);
        let plan = plan_call(CAbi::Win64, &[agg(float.clone())], Some(&agg(float)));
        assert_eq!(pieces(&plan.params[0]), vec![(0, I4)]);
        assert_eq!(
            plan.ret,
            RetPlan::Pieces(vec![Piece {
                offset: 0,
                kind: I4
            }])
        );
        let twelve = layout(12, 4, &[(0, 'i'), (4, 'i'), (8, 'i')]);
        let plan = plan_call(CAbi::Win64, &[agg(twelve.clone())], Some(&agg(twelve)));
        assert_eq!(plan.params[0], ArgPlan::Indirect);
        assert_eq!(plan.ret, RetPlan::Sret);
    }

    #[test]
    fn aapcs64_passes_hfas_in_float_registers() {
        let quad = layout(32, 8, &[(0, 'd'), (8, 'd'), (16, 'd'), (24, 'd')]);
        let plan = plan_call(CAbi::Aapcs64, &[agg(quad.clone())], Some(&agg(quad)));
        assert_eq!(
            pieces(&plan.params[0]),
            vec![
                (0, PieceKind::F64),
                (8, PieceKind::F64),
                (16, PieceKind::F64),
                (24, PieceKind::F64)
            ]
        );
        assert!(matches!(plan.ret, RetPlan::Pieces(ref p) if p.len() == 4));
        let three = layout(12, 4, &[(0, 'f'), (4, 'f'), (8, 'f')]);
        let plan = plan_call(CAbi::Aapcs64, &[agg(three)], None);
        assert_eq!(
            pieces(&plan.params[0]),
            vec![
                (0, PieceKind::F32),
                (4, PieceKind::F32),
                (8, PieceKind::F32)
            ]
        );
    }

    #[test]
    fn aapcs64_retires_the_float_registers_for_an_hfa_that_does_not_fit() {
        let mut params: Vec<CArg> = (0..6).map(|_| CArg::Scalar('d')).collect();
        params.push(agg(layout(12, 4, &[(0, 'f'), (4, 'f'), (8, 'f')])));
        let plan = plan_call(CAbi::Aapcs64, &params, None);
        match &plan.params[6] {
            ArgPlan::Pieces {
                pad_float, pieces, ..
            } => {
                assert_eq!(*pad_float, 2);
                assert_eq!(pieces.len(), 2);
                assert!(pieces.iter().all(|p| p.kind == PieceKind::F64));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn aapcs64_passes_small_composites_in_words_and_large_ones_by_pointer() {
        let small = layout(12, 4, &[(0, 'i'), (4, 'f'), (8, 'i')]);
        let plan = plan_call(CAbi::Aapcs64, &[agg(small.clone())], Some(&agg(small)));
        assert_eq!(pieces(&plan.params[0]), vec![(0, I8), (8, I8)]);
        let big = layout(24, 8, &[(0, 'l'), (8, 'l'), (16, 'l')]);
        let plan = plan_call(CAbi::Aapcs64, &[agg(big.clone())], Some(&agg(big)));
        assert_eq!(plan.params[0], ArgPlan::Indirect);
        assert_eq!(plan.ret, RetPlan::Sret);
    }

    #[test]
    fn riscv_flattens_float_pairs_and_mixed_pairs() {
        let pair = layout(8, 4, &[(0, 'f'), (4, 'f')]);
        let plan = plan_call(CAbi::RiscV64, &[agg(pair)], None);
        assert_eq!(
            pieces(&plan.params[0]),
            vec![(0, PieceKind::F32), (4, PieceKind::F32)]
        );
        let mixed = layout(16, 8, &[(0, 'd'), (8, 'i')]);
        let plan = plan_call(CAbi::RiscV64, &[agg(mixed)], None);
        assert_eq!(
            pieces(&plan.params[0]),
            vec![
                (0, PieceKind::F64),
                (
                    8,
                    PieceKind::Int {
                        bytes: 4,
                        signed: true
                    }
                )
            ]
        );
        let three = layout(12, 4, &[(0, 'f'), (4, 'f'), (8, 'f')]);
        let plan = plan_call(CAbi::RiscV64, &[agg(three)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, I8), (8, I8)]);
    }

    #[test]
    fn riscv_falls_back_to_integer_registers_when_float_ones_run_out() {
        let mut params: Vec<CArg> = (0..8).map(|_| CArg::Scalar('d')).collect();
        params.push(agg(layout(8, 4, &[(0, 'f'), (4, 'f')])));
        let plan = plan_call(CAbi::RiscV64, &params, None);
        assert_eq!(pieces(&plan.params[8]), vec![(0, I8)]);
    }

    #[test]
    fn a_union_is_classified_by_every_member() {
        let union = layout(8, 8, &[(0, 'd'), (0, 'i')]);
        let plan = plan_call(CAbi::SysV64, &[agg(union.clone())], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, I8)]);
        let floats = layout(8, 8, &[(0, 'd'), (0, 'f')]);
        let plan = plan_call(CAbi::SysV64, &[agg(floats)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, PieceKind::F64)]);
        let plan = plan_call(CAbi::RiscV64, &[agg(union)], None);
        assert_eq!(pieces(&plan.params[0]), vec![(0, I8)]);
    }
}
