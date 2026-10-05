/// Main-thread stack a Windows executable reserves, for compiled programs and
/// for the `gos` binary that runs them on the bytecode VM. A PE's default
/// reserve is 1 MiB against the 8 MiB main-thread stack Linux and macOS give a
/// process, and a Win64 frame is larger than its System V counterpart (32
/// bytes of shadow space per call, more callee-saved registers), so the
/// reserve is doubled again to reach the same recursion depth. The OS commits
/// the pages only as the stack grows into them.
pub(crate) const WINDOWS_MAIN_STACK_RESERVE: u64 = 16 * 1024 * 1024;
