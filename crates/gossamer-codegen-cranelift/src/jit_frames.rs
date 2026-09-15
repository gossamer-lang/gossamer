//! Names the in-process JIT frames on a thread's stack for a panic report.
//!
//! JIT code keeps no per-call bookkeeping. Each function's unwind rules are
//! registered with the platform unwinder, and its code range and source
//! positions with this registry, so a report walks the real machine stack and
//! names the frames it finds on it.

#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::isa::unwind::UnwindInfo;
use gossamer_lex::Span;
use gossamer_mir::InlineChain;
use parking_lot::RwLock;

/// One JIT frame found on the stack.
#[derive(Debug, Clone)]
pub struct JitFrame {
    /// The body's source name.
    pub function: Arc<str>,
    /// Where in the source the frame stands, when its instruction carries a
    /// position.
    pub span: Option<Span>,
}

/// What compiling one function recorded for the registry.
pub(crate) struct FunctionFrames {
    pub(crate) id: cranelift_module::FuncId,
    pub(crate) name: Arc<str>,
    pub(crate) code_len: u32,
    pub(crate) unwind: Option<UnwindInfo>,
    /// Code-offset ranges `(start, end, index into spans)`, sorted by start.
    pub(crate) positions: Vec<(u32, u32, u32)>,
    /// Each position's span and the inlined calls its code came through.
    pub(crate) spans: Vec<(Span, InlineChain)>,
}

struct Registered {
    start: usize,
    end: usize,
    name: Arc<str>,
    positions: Vec<(u32, u32, u32)>,
    spans: Vec<(Span, InlineChain)>,
    owner: u64,
}

/// Every registered function, sorted by code address.
static REGISTRY: RwLock<Vec<Registered>> = parking_lot::const_rwlock(Vec::new());
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// Keeps one artifact's frames known to the unwinder and the registry for as
/// long as its code can run.
pub(crate) struct FrameRegistration {
    owner: u64,
    _unwind: Option<platform::Unwind>,
}

impl Drop for FrameRegistration {
    fn drop(&mut self) {
        REGISTRY.write().retain(|entry| entry.owner != self.owner);
    }
}

/// Registers `functions`, each paired with the address its code was placed
/// at.
pub(crate) fn register(
    isa: &dyn TargetIsa,
    functions: Vec<(usize, FunctionFrames)>,
) -> FrameRegistration {
    let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    let unwind = platform::register(
        isa,
        functions
            .iter()
            .filter_map(|(start, frames)| Some((*start, frames.code_len, frames.unwind.as_ref()?))),
    );
    let mut registry = REGISTRY.write();
    for (start, frames) in functions {
        registry.push(Registered {
            start,
            end: start + frames.code_len as usize,
            name: frames.name,
            positions: frames.positions,
            spans: frames.spans,
            owner,
        });
    }
    registry.sort_by_key(|entry| entry.start);
    FrameRegistration {
        owner,
        _unwind: unwind,
    }
}

/// The JIT frames on the calling thread's stack, innermost first. Empty when
/// no JIT code is registered or the unwinder cannot reach any.
#[must_use]
pub fn active_frames() -> Vec<JitFrame> {
    let registry = REGISTRY.read();
    if registry.is_empty() {
        return Vec::new();
    }
    let mut frames = Vec::new();
    backtrace::trace(|frame| {
        // Every JIT frame the walk reaches is suspended at a call, so its
        // address is the return address one past the call instruction.
        let address = (frame.ip() as usize).saturating_sub(1);
        frames_at(&registry, address, &mut frames);
        true
    });
    frames
}

/// Pushes the frames the code at `address` stands for, innermost first: the
/// body's own, preceded by one per call whose callee an inliner placed there.
fn frames_at(registry: &[Registered], address: usize, out: &mut Vec<JitFrame>) {
    let index = registry.partition_point(|entry| entry.start <= address);
    let Some(entry) = index.checked_sub(1).and_then(|index| registry.get(index)) else {
        return;
    };
    if address >= entry.end {
        return;
    }
    let Ok(offset) = u32::try_from(address - entry.start) else {
        return;
    };
    let at = entry
        .positions
        .partition_point(|(start, _, _)| *start <= offset);
    let position = at
        .checked_sub(1)
        .and_then(|at| entry.positions.get(at))
        .filter(|(_, end, _)| offset < *end)
        .and_then(|(_, _, index)| entry.spans.get(*index as usize));
    let Some((span, chain)) = position else {
        out.push(JitFrame {
            function: Arc::clone(&entry.name),
            span: None,
        });
        return;
    };
    let Some(chain) = chain.as_deref().filter(|chain| !chain.is_empty()) else {
        out.push(JitFrame {
            function: Arc::clone(&entry.name),
            span: Some(*span),
        });
        return;
    };
    // The innermost callee stands at the position itself; each outer
    // function stands at the call that led inward.
    let mut span = *span;
    for (depth, frame) in chain.iter().enumerate().rev() {
        out.push(JitFrame {
            function: Arc::from(frame.function.as_str()),
            span: Some(span),
        });
        span = frame.call;
        if depth == 0 {
            out.push(JitFrame {
                function: Arc::clone(&entry.name),
                span: Some(span),
            });
        }
    }
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod platform {
    use cranelift_codegen::gimli::RunTimeEndian;
    use cranelift_codegen::gimli::write::{Address, EhFrame, EndianVec, FrameTable};
    use cranelift_codegen::isa::TargetIsa;
    use cranelift_codegen::isa::unwind::UnwindInfo;

    unsafe extern "C" {
        fn __register_frame(entry: *const u8);
        fn __deregister_frame(entry: *const u8);
    }

    /// An `.eh_frame` image the unwinder reads for as long as it is
    /// registered.
    pub(super) struct Unwind {
        bytes: Box<[u8]>,
        /// Offsets into `bytes` of every entry handed to the unwinder.
        registered: Vec<usize>,
    }

    pub(super) fn register<'a>(
        isa: &dyn TargetIsa,
        functions: impl Iterator<Item = (usize, u32, &'a UnwindInfo)>,
    ) -> Option<Unwind> {
        let cie = isa.create_systemv_cie()?;
        let mut table = FrameTable::default();
        let cie_id = table.add_cie(cie);
        let mut any = false;
        for (start, _, info) in functions {
            if let UnwindInfo::SystemV(info) = info {
                table.add_fde(cie_id, info.to_fde(Address::Constant(start as u64)));
                any = true;
            }
        }
        if !any {
            return None;
        }
        let mut eh_frame = EhFrame(EndianVec::new(RunTimeEndian::default()));
        table.write_eh_frame(&mut eh_frame).ok()?;
        let mut bytes = eh_frame.0.into_vec();
        // A zero length ends the section for an unwinder that walks it whole.
        bytes.extend_from_slice(&[0; 4]);
        let bytes = bytes.into_boxed_slice();
        let mut registered = Vec::new();
        // libgcc takes the whole section at once; libunwind takes one frame
        // description entry per call and rejects the leading CIE.
        if cfg!(any(
            all(target_os = "linux", target_env = "gnu"),
            target_os = "freebsd"
        )) {
            // SAFETY: `bytes` is a complete, zero-terminated `.eh_frame`
            // image that stays alive until the matching deregistration.
            unsafe { __register_frame(bytes.as_ptr()) };
            registered.push(0);
        } else {
            let end = bytes.len() - 4;
            let mut offset = 0;
            while offset < end {
                let Some(length) = bytes
                    .get(offset..offset + 4)
                    .and_then(|word| <[u8; 4]>::try_from(word).ok())
                    .map(u32::from_ne_bytes)
                else {
                    break;
                };
                if offset != 0 {
                    // SAFETY: `offset` starts one FDE inside the live image.
                    unsafe { __register_frame(bytes.as_ptr().add(offset)) };
                    registered.push(offset);
                }
                offset += length as usize + 4;
            }
        }
        Some(Unwind { bytes, registered })
    }

    impl Drop for Unwind {
        fn drop(&mut self) {
            for &offset in self.registered.iter().rev() {
                // SAFETY: each offset was registered from this same image,
                // which is still alive.
                unsafe { __deregister_frame(self.bytes.as_ptr().add(offset)) };
            }
        }
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod platform {
    use cranelift_codegen::isa::TargetIsa;
    use cranelift_codegen::isa::unwind::UnwindInfo;

    /// `RUNTIME_FUNCTION`: a code range and its `UNWIND_INFO`, each an offset
    /// from the table's base address.
    #[repr(C)]
    struct RuntimeFunction {
        begin: u32,
        end: u32,
        unwind_info: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn RtlAddFunctionTable(
            function_table: *const RuntimeFunction,
            entry_count: u32,
            base_address: u64,
        ) -> u8;
        fn RtlDeleteFunctionTable(function_table: *const RuntimeFunction) -> u8;
    }

    /// A function table and the unwind records it points into, both alive
    /// for as long as the table is registered.
    pub(super) struct Unwind {
        table: Box<[RuntimeFunction]>,
        _records: Box<[u8]>,
    }

    // SAFETY: the table and records are plain bytes never written after
    // registration; the unwinder only reads them.
    unsafe impl Send for Unwind {}
    unsafe impl Sync for Unwind {}

    pub(super) fn register<'a>(
        _isa: &dyn TargetIsa,
        functions: impl Iterator<Item = (usize, u32, &'a UnwindInfo)>,
    ) -> Option<Unwind> {
        let functions: Vec<(
            usize,
            u32,
            &cranelift_codegen::isa::unwind::winx64::UnwindInfo,
        )> = functions
            .filter_map(|(start, len, info)| match info {
                UnwindInfo::WindowsX64(info) => Some((start, len, info)),
                _ => None,
            })
            .collect();
        if functions.is_empty() {
            return None;
        }
        // Each record is padded to a four-byte boundary, as the loader
        // aligns them in an image.
        let mut offsets = Vec::with_capacity(functions.len());
        let mut size = 0usize;
        for (_, _, info) in &functions {
            offsets.push(size);
            size += info.emit_size().next_multiple_of(4);
        }
        let mut records = vec![0u8; size].into_boxed_slice();
        for ((_, _, info), &offset) in functions.iter().zip(&offsets) {
            info.emit(&mut records[offset..offset + info.emit_size()]);
        }
        let records_start = records.as_ptr() as usize;
        let base = functions
            .iter()
            .map(|(start, _, _)| *start)
            .chain(std::iter::once(records_start))
            .min()?;
        let relative = |address: usize| u32::try_from(address - base).ok();
        let mut table = Vec::with_capacity(functions.len());
        for ((start, len, _), &offset) in functions.iter().zip(&offsets) {
            table.push(RuntimeFunction {
                begin: relative(*start)?,
                end: relative(*start + *len as usize)?,
                unwind_info: relative(records_start + offset)?,
            });
        }
        let table = table.into_boxed_slice();
        // SAFETY: every entry's offsets lie within the live code and record
        // allocations measured from `base`, and both outlive the
        // registration, which the matching delete ends.
        let added = unsafe {
            RtlAddFunctionTable(
                table.as_ptr(),
                u32::try_from(table.len()).ok()?,
                base as u64,
            )
        };
        (added != 0).then_some(Unwind {
            table,
            _records: records,
        })
    }

    impl Drop for Unwind {
        fn drop(&mut self) {
            // SAFETY: the table was registered by `register` and is alive.
            unsafe { RtlDeleteFunctionTable(self.table.as_ptr()) };
        }
    }
}

// The unwinder on the remaining targets takes its tables in a form this
// module does not build, so their reports name the frames the host tracks.
#[cfg(not(any(
    all(unix, any(target_arch = "x86_64", target_arch = "aarch64")),
    all(windows, target_arch = "x86_64")
)))]
mod platform {
    use cranelift_codegen::isa::TargetIsa;
    use cranelift_codegen::isa::unwind::UnwindInfo;

    pub(super) struct Unwind;

    pub(super) fn register<'a>(
        _isa: &dyn TargetIsa,
        _functions: impl Iterator<Item = (usize, u32, &'a UnwindInfo)>,
    ) -> Option<Unwind> {
        None
    }
}
