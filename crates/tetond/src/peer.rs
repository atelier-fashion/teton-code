//! Peer process ancestry: did this connection come out of the daemon's own
//! process tree?
//!
//! REQ-569 ADR-A makes *process ancestry* the primary mechanism for BR-4 — a
//! connection whose process descends from this daemon may never attach — on the
//! grounds that it keys on where the process came from rather than on what it
//! happens to do, and that it works identically in unsigned dev builds and
//! signed release builds. No code-signature or keychain machinery belongs here;
//! both were considered and rejected in ADR-A.
//!
//! ADR-B splits the implementation in two, and the module keeps that split
//! visible:
//!
//! - a **platform seam** — [`ParentOf`], one `pid -> ppid` lookup, answered by
//!   `sysctl(KERN_PROC_PID)` on macOS and `/proc/<pid>/status` on Linux;
//! - a **pure policy** — [`is_descendant_of`], which walks parent links over
//!   plain `i32`s and so is table-testable on every platform, against a fake
//!   tree, with no real processes involved (LESSON-499: push the decision out
//!   of the part CI cannot exercise).
//!
//! LESSON-433 governs the seam: cfg-gated platform code verified on one OS is
//! false confidence, so each arm has its own real-syscall test and CI runs the
//! suite on both a macOS and a Linux runner.
//!
//! **This module answers a question; it does not ask it.** The policy lives at
//! the callers: TASK-106 made [`Ancestry`] terminal at `session/attach`, the
//! `monitor` declaration and `attach/consent`, and decided that
//! [`Ancestry::Indeterminate`] costs a connection exactly what
//! [`Ancestry::Descendant`] does (`crate::server`'s
//! `ConnState::may_hold_session_access`).
//!
//! Known limits, recorded rather than hidden (ADR-A):
//!
//! - **The chain is one shell word away from being broken, and the word is
//!   model-supplied.** The `shell` tool runs `sh -c <command>`, so
//!   `helper >/dev/null 2>&1 &` or `setsid helper` orphans a grandchild that
//!   reparents to `launchd`/`init` and classifies [`Ancestry::NotDescendant`]
//!   with full client rights. This is not a hypothetical "a child that
//!   double-forks": it is one token in a command the model writes. The `shell`
//!   tool kills its whole process group **on the timeout arm only**, so neither
//!   form is reached on an ordinary completion. Killing the group on every arm
//!   was tried and reverted (REQ-569 re-verify, R4): it destroyed work a command
//!   backgrounded on purpose, signalled a pgid whose leader had already been
//!   reaped, and still did not reach `setsid`, which leaves the group outright.
//! - PID reuse is a narrow race between `connect(2)` and the walk.
//!
//! Neither is chased with a heuristic here — a "reparented to init" rule would
//! also catch a legitimate CLI whose terminal closed.

/// The answer to "did `peer` come out of `ancestor`'s process tree?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// `peer` is `ancestor`, or is reachable from it by parent links.
    Descendant,
    /// The parent chain was walked to its root without meeting `ancestor`.
    NotDescendant,
    /// The question could not be answered: a lookup failed, the chain cycled,
    /// or the depth cap was reached.
    ///
    /// This is deliberately **not** a synonym for [`Ancestry::NotDescendant`],
    /// and the decision must never be modelled as a `bool` or an `Option`: on
    /// this seam, "I could not tell" collapsing into "not a descendant" is
    /// exactly the shape of a fail-open. The caller has to name its policy.
    Indeterminate,
}

/// The one platform-specific fact the ancestry decision needs.
///
/// `None` means *no answer* — the process exited, the kernel would not say, or
/// the platform has no way to ask. It never means "this process has no parent";
/// a root process reports pid 1 (or 0), which is a value, not an absence.
pub trait ParentOf {
    /// Returns `pid`'s parent process id, or `None` when the kernel will not say.
    fn parent_of(&self, pid: i32) -> Option<i32>;
}

/// How far [`is_descendant_of`] walks before giving up.
///
/// This bounds the walk; it does not express policy. Real chains are a handful
/// of links deep, so a cap this size is only ever reached by a chain that is
/// already lying to us — which is why reaching it yields
/// [`Ancestry::Indeterminate`] rather than an answer.
pub const MAX_ANCESTRY_DEPTH: usize = 64;

/// Walks `peer`'s parent links looking for `ancestor`.
///
/// Pure over `lookup`: given the same `pid -> ppid` map it returns the same
/// answer on every platform, which is what makes the decision table-testable
/// where the syscalls are not (ADR-B).
///
/// A process is reported as a descendant of *itself*. The walk starts at
/// `peer`, and the gate this feeds asks "did this connection come out of the
/// daemon's own process tree" — the daemon's own process is the root of that
/// tree, so answering [`Ancestry::NotDescendant`] there would let the one
/// process the question is really about straight through.
///
/// Termination is guaranteed twice over: an already-visited pid ends the walk
/// as a cycle, and `max_depth` bounds it regardless. A hostile or corrupt
/// chain therefore cannot hang the daemon; it can only produce
/// [`Ancestry::Indeterminate`].
#[must_use]
pub fn is_descendant_of(
    peer: i32,
    ancestor: i32,
    lookup: &dyn ParentOf,
    max_depth: usize,
) -> Ancestry {
    // pid 0 is the kernel's own sentinel and negatives are not pids at all;
    // neither end of the question is answerable.
    if peer <= 0 || ancestor <= 0 {
        return Ancestry::Indeterminate;
    }
    if peer == ancestor {
        return Ancestry::Descendant;
    }

    let mut seen = Vec::with_capacity(max_depth.min(16));
    seen.push(peer);
    let mut current = peer;

    for _ in 0..max_depth {
        let Some(parent) = lookup.parent_of(current) else {
            // The chain broke under us — the process exited mid-walk, or the
            // kernel would not answer. We do not know, and we say so.
            return Ancestry::Indeterminate;
        };
        if parent == ancestor {
            return Ancestry::Descendant;
        }
        if parent <= 1 {
            // The top of the tree (`launchd`/`init`, or pid 0 above it),
            // reached without meeting `ancestor`. Checked *after* the equality
            // test above so that `ancestor == 1` still answers `Descendant`.
            return Ancestry::NotDescendant;
        }
        if seen.contains(&parent) {
            // A real process tree cannot contain a cycle, so a chain that loops
            // is a chain we cannot trust to answer the question at all.
            return Ancestry::Indeterminate;
        }
        seen.push(parent);
        current = parent;
    }

    Ancestry::Indeterminate
}

/// The kernel's own answer to [`ParentOf`].
///
/// The syscalls live behind this one type so that everything above it is plain
/// data (ADR-B).
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelParentOf;

impl ParentOf for KernelParentOf {
    fn parent_of(&self, pid: i32) -> Option<i32> {
        if pid <= 0 {
            return None;
        }
        sys::parent_of(pid)
    }
}

#[cfg(target_vendor = "apple")]
use darwin as sys;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
use unsupported as sys;

/// macOS/iOS: `sysctl([CTL_KERN, KERN_PROC, KERN_PROC_PID, pid])`, over
/// `struct kinfo_proc` as the system headers declare it.
///
/// `libc` 0.2 exposes `CTL_KERN`, `KERN_PROC` and `KERN_PROC_PID` for Apple
/// targets but **not** `struct kinfo_proc`, so the layout is declared here
/// field for field from the SDK headers: `struct kinfo_proc` and its inner
/// `struct eproc` from `<sys/sysctl.h>`, `struct extern_proc` from
/// `<sys/proc.h>`, `struct _pcred` / `struct _ucred` from `<sys/sysctl.h>`, and
/// `struct vmspace` from `<sys/vm.h>`. Only `kp_proc.p_pid` and
/// `kp_eproc.e_ppid` are ever read; every other field exists to place those two
/// at the right offset.
///
/// A wrong layout would be a wrong *answer*, not a compile error, so two
/// runtime checks stand behind it: the kernel must write exactly
/// `size_of::<KinfoProc>()` bytes, and the `p_pid` in the record it wrote must
/// be the pid we asked about. Either mismatch yields `None` — no fabricated
/// parent ever leaves this module.
#[cfg(target_vendor = "apple")]
mod darwin {
    use std::ffi::{c_char, c_int, c_short, c_uchar, c_uint, c_ushort, c_void};
    use std::mem::MaybeUninit;

    /// `struct extern_proc` — `<sys/proc.h>`.
    #[repr(C)]
    #[allow(dead_code)]
    struct ExternProc {
        /// `union { struct { struct proc *__p_forw, *__p_back; }; struct timeval; }`
        /// — both arms are 16 bytes at 8-byte alignment.
        p_un: [*mut c_void; 2],
        p_vmspace: *mut c_void,
        p_sigacts: *mut c_void,
        p_flag: c_int,
        p_stat: c_char,
        /// Read as a layout self-check; see the module docs.
        p_pid: libc::pid_t,
        p_oppid: libc::pid_t,
        p_dupfd: c_int,
        user_stack: *mut c_char,
        exit_thread: *mut c_void,
        p_debugger: c_int,
        /// `boolean_t` — 32 bits on every Apple target.
        sigwait: c_int,
        p_estcpu: c_uint,
        p_cpticks: c_int,
        /// `fixpt_t`.
        p_pctcpu: u32,
        p_wchan: *mut c_void,
        p_wmesg: *mut c_char,
        p_swtime: c_uint,
        p_slptime: c_uint,
        p_realtimer: libc::itimerval,
        p_rtime: libc::timeval,
        /// `u_quad_t`.
        p_uticks: u64,
        p_sticks: u64,
        p_iticks: u64,
        p_traceflag: c_int,
        p_tracep: *mut c_void,
        p_siglist: c_int,
        p_textvp: *mut c_void,
        p_holdcnt: c_int,
        /// `sigset_t` — `__uint32_t` on Darwin.
        p_sigmask: u32,
        p_sigignore: u32,
        p_sigcatch: u32,
        p_priority: c_uchar,
        p_usrpri: c_uchar,
        p_nice: c_char,
        /// `MAXCOMLEN + 1` (`<sys/param.h>`).
        p_comm: [c_char; 17],
        p_pgrp: *mut c_void,
        p_addr: *mut c_void,
        p_xstat: c_ushort,
        p_acflag: c_ushort,
        p_ru: *mut c_void,
    }

    /// `struct _pcred` — `<sys/sysctl.h>`.
    #[repr(C)]
    #[allow(dead_code)]
    struct Pcred {
        pc_lock: [c_char; 72],
        pc_ucred: *mut c_void,
        p_ruid: libc::uid_t,
        p_svuid: libc::uid_t,
        p_rgid: libc::gid_t,
        p_svgid: libc::gid_t,
        p_refcnt: c_int,
    }

    /// `struct _ucred` — `<sys/sysctl.h>`; `NGROUPS` is 16
    /// (`<sys/syslimits.h>`).
    #[repr(C)]
    #[allow(dead_code)]
    struct Ucred {
        cr_ref: i32,
        cr_uid: libc::uid_t,
        cr_ngroups: c_short,
        cr_groups: [libc::gid_t; 16],
    }

    /// `struct vmspace` — `<sys/vm.h>`, where the fields are already declared as
    /// opaque placeholders.
    #[repr(C)]
    #[allow(dead_code)]
    struct Vmspace {
        dummy: i32,
        dummy2: *mut c_char,
        dummy3: [i32; 5],
        dummy4: [*mut c_char; 3],
    }

    /// `struct eproc` — the inner struct of `kinfo_proc` (`<sys/sysctl.h>`).
    #[repr(C)]
    #[allow(dead_code)]
    struct Eproc {
        e_paddr: *mut c_void,
        e_sess: *mut c_void,
        e_pcred: Pcred,
        e_ucred: Ucred,
        e_vm: Vmspace,
        /// The whole point of the struct.
        e_ppid: libc::pid_t,
        e_pgid: libc::pid_t,
        e_jobc: c_short,
        e_tdev: libc::dev_t,
        e_tpgid: libc::pid_t,
        e_tsess: *mut c_void,
        /// `WMESGLEN + 1`.
        e_wmesg: [c_char; 8],
        /// `segsz_t`.
        e_xsize: i32,
        e_xrssize: c_short,
        e_xccount: c_short,
        e_xswrss: c_short,
        e_flag: i32,
        /// `COMAPT_MAXLOGNAME` — the header's own spelling.
        e_login: [c_char; 12],
        e_spare: [i32; 4],
    }

    /// `struct kinfo_proc` — `<sys/sysctl.h>`.
    #[repr(C)]
    #[allow(dead_code)]
    struct KinfoProc {
        kp_proc: ExternProc,
        kp_eproc: Eproc,
    }

    /// Reads `pid`'s parent from the kernel.
    pub fn parent_of(pid: i32) -> Option<i32> {
        let mut mib: [c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
        let mut record = MaybeUninit::<KinfoProc>::zeroed();
        let wanted = std::mem::size_of::<KinfoProc>();
        let mut len = wanted;
        // SAFETY: `mib` is a live array of exactly the `namelen` we pass; the
        // out-buffer is live, writable storage of `len` bytes owned by this
        // frame; `len` is live storage the kernel updates with the byte count
        // it wrote; and a null new-value pointer with a zero length is how
        // `sysctl(3)` is told this is a query, not a write. All four borrows
        // end when the call returns.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as c_uint,
                record.as_mut_ptr().cast::<c_void>(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        // A vanished pid is the ordinary case here: `sysctl` succeeds and
        // reports zero bytes written rather than failing. A short or long
        // record means our declared layout no longer describes the kernel's.
        if rc != 0 || len != wanted {
            return None;
        }
        // SAFETY: `sysctl` returned 0 having written exactly `wanted` bytes —
        // the full `KinfoProc` — so every field is initialized.
        let record = unsafe { record.assume_init() };
        // Layout self-check: the record must be about the process we asked
        // about. If the fields ever shift, this fails closed instead of
        // handing back some neighbouring field as a parent pid.
        if record.kp_proc.p_pid != pid {
            return None;
        }
        Some(record.kp_eproc.e_ppid)
    }

    #[cfg(test)]
    mod tests {
        use super::{ExternProc, KinfoProc};

        #[test]
        fn kinfo_proc_layout_matches_the_system_headers() {
            // Ground truth taken from the macOS SDK headers on an LP64 Apple
            // target (`sizeof` / `offsetof` compiled against `<sys/sysctl.h>`);
            // `boolean_t` differs in signedness between arm64 and x86_64 but
            // not in width, so both Apple architectures agree on these numbers.
            //
            // Drift here would otherwise surface only as `parent_of` quietly
            // returning `None` for every pid — a guard that silently stops
            // guarding. This says so out loud instead.
            assert_eq!(std::mem::size_of::<KinfoProc>(), 648, "sizeof(kinfo_proc)");
            assert_eq!(
                std::mem::size_of::<ExternProc>(),
                296,
                "sizeof(extern_proc)"
            );
            assert_eq!(
                std::mem::offset_of!(KinfoProc, kp_proc.p_pid),
                40,
                "offsetof(kinfo_proc, kp_proc.p_pid)"
            );
            assert_eq!(
                std::mem::offset_of!(KinfoProc, kp_eproc.e_ppid),
                560,
                "offsetof(kinfo_proc, kp_eproc.e_ppid)"
            );
        }
    }
}

/// Linux: the `PPid:` line of `/proc/<pid>/status`.
#[cfg(target_os = "linux")]
mod linux {
    /// Reads `pid`'s parent from procfs.
    ///
    /// procfs is the documented interface for this, so the Linux arm needs no
    /// `unsafe` at all — it is a file read plus
    /// [`super::ppid_from_proc_status`]. A missing or unreadable file is a
    /// vanished process: `None`, not a guess.
    pub fn parent_of(pid: i32) -> Option<i32> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        super::ppid_from_proc_status(&status)
    }
}

/// Pulls the `PPid:` value out of a `/proc/<pid>/status` blob.
///
/// Deliberately **outside** the Linux `cfg`: the file read is the part that
/// needs procfs, but the parse is the part with any judgement in it, and a
/// developer on macOS can only run the arms their own machine compiles.
/// Extracting it means the parse is exercised in every test run on every
/// platform, and CI's Linux runner is then confirming the file read rather than
/// being the first thing ever to execute the parser (LESSON-433).
#[cfg(any(target_os = "linux", test))]
fn ppid_from_proc_status(status: &str) -> Option<i32> {
    status
        .lines()
        // `strip_prefix` anchors at the start of the line, which is what keeps
        // `TracerPid:` from answering a question about `PPid:`.
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse::<i32>().ok())
}

/// Every other target: no answer.
#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
mod unsupported {
    /// REQ-569 ships to macOS and Linux, both of which answer this properly.
    /// Anywhere else the honest report is "cannot tell", which the policy layer
    /// turns into [`super::Ancestry::Indeterminate`] — never a silent "not a
    /// descendant".
    pub fn parent_of(_pid: i32) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `pid -> ppid` map standing in for the kernel, so the policy can be
    /// tested exhaustively without spawning anything (ADR-B).
    struct FakeTree(HashMap<i32, i32>);

    impl FakeTree {
        fn new(links: &[(i32, i32)]) -> Self {
            Self(links.iter().copied().collect())
        }
    }

    impl ParentOf for FakeTree {
        fn parent_of(&self, pid: i32) -> Option<i32> {
            self.0.get(&pid).copied()
        }
    }

    /// One row of the ancestry table: case name, the `pid -> ppid` links the
    /// fake tree is built from, the peer, the ancestor asked about, the depth
    /// budget, and the expected answer.
    type Case = (
        &'static str,
        &'static [(i32, i32)],
        i32,
        i32,
        usize,
        Ancestry,
    );

    #[test]
    fn ancestry_table() {
        let cases: &[Case] = &[
            (
                "a direct child is a descendant",
                &[(200, 100)],
                200,
                100,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Descendant,
            ),
            (
                "a deep chain is still a descendant",
                &[(500, 400), (400, 300), (300, 200), (200, 100)],
                500,
                100,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Descendant,
            ),
            (
                "a sibling under the same root is not a descendant",
                &[(200, 1), (300, 1)],
                300,
                200,
                MAX_ANCESTRY_DEPTH,
                Ancestry::NotDescendant,
            ),
            (
                "a chain that reaches pid 1 without meeting the ancestor ends",
                &[(400, 300), (300, 200), (200, 1)],
                400,
                999,
                MAX_ANCESTRY_DEPTH,
                Ancestry::NotDescendant,
            ),
            (
                "pid 1 as the ancestor is matched, not swallowed by the root check",
                &[(200, 1)],
                200,
                1,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Descendant,
            ),
            (
                "a process is a descendant of itself",
                &[],
                100,
                100,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Descendant,
            ),
            (
                "a cycle is unanswerable, not a refusal",
                &[(200, 300), (300, 200)],
                200,
                100,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Indeterminate,
            ),
            (
                "a chain longer than the cap is unanswerable",
                &[(500, 400), (400, 300), (300, 200), (200, 100)],
                500,
                100,
                2,
                Ancestry::Indeterminate,
            ),
            (
                "a lookup that fails mid-walk is unanswerable",
                &[(400, 300)],
                400,
                100,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Indeterminate,
            ),
            (
                "a peer with no pid at all is unanswerable",
                &[(200, 100)],
                0,
                100,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Indeterminate,
            ),
            (
                "an ancestor with no pid at all is unanswerable",
                &[(200, 100)],
                200,
                0,
                MAX_ANCESTRY_DEPTH,
                Ancestry::Indeterminate,
            ),
        ];

        for (case, links, peer, ancestor, max_depth, expected) in cases {
            let tree = FakeTree::new(links);
            let got = is_descendant_of(*peer, *ancestor, &tree, *max_depth);
            assert_eq!(got, *expected, "{case}");
        }
    }

    #[test]
    fn a_cycle_terminates_without_help_from_the_depth_cap() {
        // The cap alone would end the walk, which would make the cycle case
        // above pass even with no cycle detection at all. Removing the cap is
        // what makes this test able to fail: with an unbounded budget, a loop
        // that is not detected hangs the suite rather than reporting.
        let tree = FakeTree::new(&[(200, 300), (300, 400), (400, 200)]);
        assert_eq!(
            is_descendant_of(200, 100, &tree, usize::MAX),
            Ancestry::Indeterminate
        );
    }

    #[test]
    fn indeterminate_is_not_a_refusal() {
        // The one conflation this module exists to prevent (AC-3): a broken
        // chain must not read as "not a descendant", because a gate that treats
        // the two alike fails open.
        assert_ne!(Ancestry::Indeterminate, Ancestry::NotDescendant);
        let broken = FakeTree::new(&[]);
        let intact = FakeTree::new(&[(200, 1)]);
        assert_eq!(
            is_descendant_of(200, 100, &broken, MAX_ANCESTRY_DEPTH),
            Ancestry::Indeterminate
        );
        assert_eq!(
            is_descendant_of(200, 100, &intact, MAX_ANCESTRY_DEPTH),
            Ancestry::NotDescendant
        );
    }

    #[test]
    fn proc_status_ppid_is_parsed() {
        // A real `/proc/<pid>/status` prefix, tab-separated as the kernel
        // writes it. `TracerPid:` comes first on purpose: it ends in the same
        // four characters as the line we want, and a prefix match that was not
        // anchored to the line start would take it.
        let status = "Name:\tbash\n\
                      State:\tS (sleeping)\n\
                      Tgid:\t3515\n\
                      Pid:\t3515\n\
                      TracerPid:\t99\n\
                      PPid:\t3452\n\
                      Uid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(ppid_from_proc_status(status), Some(3452));

        // pid 1's parent really is reported as 0; that is a value, not an
        // absence, and the policy layer treats it as the top of the tree.
        assert_eq!(ppid_from_proc_status("PPid:\t0\n"), Some(0));

        // No answer beats a wrong answer.
        assert_eq!(ppid_from_proc_status("Name:\tbash\nPid:\t3515\n"), None);
        assert_eq!(ppid_from_proc_status("PPid:\tnot-a-number\n"), None);
        assert_eq!(ppid_from_proc_status(""), None);
    }

    /// Asserts the real, platform-specific [`ParentOf`] against a second,
    /// independent kernel answer (`getppid(2)`).
    ///
    /// Shared by the two `#[cfg]`-gated tests below so that macOS and Linux run
    /// the *same* assertions against their *own* syscall arm (LESSON-433:
    /// verifying cfg-gated code on one platform is false confidence).
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    fn assert_kernel_parent_matches_getppid() {
        let me = i32::try_from(std::process::id()).expect("a pid fits in i32");
        // SAFETY: `getppid` takes no arguments, touches no memory we own, and
        // cannot fail.
        let real_parent = unsafe { libc::getppid() };

        assert_eq!(
            KernelParentOf.parent_of(me),
            Some(real_parent),
            "the seam must report the same parent the kernel does"
        );

        // The pure policy over the real lookup: this process trivially descends
        // from the process that spawned it.
        assert_eq!(
            is_descendant_of(me, real_parent, &KernelParentOf, MAX_ANCESTRY_DEPTH),
            Ancestry::Descendant
        );

        // A pid that cannot exist yields no answer rather than a fabricated
        // one. On macOS this is the case where `sysctl` *succeeds* and writes
        // nothing; the length check is what catches it.
        assert_eq!(KernelParentOf.parent_of(i32::MAX), None);
        assert_eq!(KernelParentOf.parent_of(0), None);
        assert_eq!(KernelParentOf.parent_of(-1), None);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn macos_kernel_parent_of_is_the_real_parent() {
        assert_kernel_parent_matches_getppid();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_kernel_parent_of_is_the_real_parent() {
        assert_kernel_parent_matches_getppid();
    }
}
