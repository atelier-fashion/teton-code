//! Socket authentication: filesystem permissions plus a peer-credential check.
//!
//! ADR-002 specifies two layers of socket auth. The first is discretionary: the
//! socket file is created `0600` so only the owning user may `connect(2)`. The
//! second is a defence-in-depth peer-credential check — the daemon reads the
//! connecting process's effective uid from the kernel (per-platform:
//! `getpeereid(2)` on macOS/BSD, `getsockopt(SO_PEERCRED)` on Linux) and refuses
//! any peer whose uid differs from the daemon's own. Error text never carries
//! paths, content, or credentials (conventions: privacy in error messages).
//!
//! REQ-569 ADR-B adds the peer's **pid** alongside the uid, because process
//! ancestry is what BR-4 keys on ([`crate::peer`]). That addition is strictly
//! additive: the uid is read by the same call as before, the refusal taxonomy
//! is unchanged, and a pid that cannot be read is reported absent rather than
//! turned into a new way to refuse a connection.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use tokio::net::UnixStream;

/// A rejected connection.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The connecting process runs as a different uid than the daemon.
    #[error("peer uid {peer} is not authorized (daemon runs as uid {expected})")]
    Unauthorized {
        /// Effective uid of the connecting process.
        peer: u32,
        /// Effective uid the daemon runs as.
        expected: u32,
    },
    /// The kernel would not report the peer's credentials.
    #[error("could not read peer credentials")]
    PeerCred(#[source] io::Error),
}

/// The kernel-attested identity of the process on the other end of a socket.
///
/// `uid` is what ADR-002's peer-credential check has always compared. `pid` is
/// the REQ-569 addition and feeds the ancestry decision in [`crate::peer`]; no
/// gate reads it yet (TASK-106 does).
///
/// The pid is an `Option` because it is not universally available: macOS and
/// Linux both supply it, but the BSD arm's `getpeereid(2)` reports no pid at
/// all. An absent pid must stay visibly absent — a default of `0` would be a
/// plausible-looking number that an ancestry walk would happily consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Effective uid of the connecting process.
    pub uid: u32,
    /// Process id of the connecting process, when the platform reports one.
    pub pid: Option<i32>,
}

/// Reads the kernel-attested credentials of the process on the other end of
/// `stream`.
///
/// The credentials are obtained per platform: `getpeereid(2)` on macOS/BSD
/// (glibc does **not** provide it), and `getsockopt(SO_PEERCRED)` on Linux.
/// Both report the peer's uid for an `AF_UNIX` stream socket and are not
/// TOCTOU-spoofable (the credential is a property of the connected socket).
///
/// # Errors
///
/// Returns the underlying OS error if the kernel refuses the *uid* query. A pid
/// the platform cannot supply is not an error — it comes back as `None`, so
/// this call refuses exactly the connections it refused before REQ-569.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub fn peer_identity(stream: &UnixStream) -> io::Result<PeerIdentity> {
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid, open socket descriptor owned by `stream` for the
    // duration of the call, and both out-pointers reference live stack storage.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc == 0 {
        Ok(PeerIdentity {
            uid,
            pid: peer_pid(fd),
        })
    } else {
        Err(io::Error::last_os_error())
    }
}

/// macOS/iOS: the peer's pid via `getsockopt(SOL_LOCAL, LOCAL_PEERPID)`.
///
/// `getpeereid(2)` — the call that supplies the uid on this arm — carries no
/// pid, so this is a second, independent query against the same connected
/// socket. Both constants are in `libc` for Apple targets (`SOL_LOCAL`,
/// `LOCAL_PEERPID`, from `<sys/un.h>`); nothing needs declaring locally here.
#[cfg(target_vendor = "apple")]
fn peer_pid(fd: std::os::fd::RawFd) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let wanted = std::mem::size_of::<libc::pid_t>();
    let mut len = wanted as libc::socklen_t;
    // SAFETY: `fd` is a valid, open socket descriptor owned by the caller's
    // `stream`; `pid` and `len` reference live stack storage sized exactly for
    // a `pid_t`, and both borrows end when the call returns.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            std::ptr::addr_of_mut!(pid).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc == 0 && len as usize == wanted && pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// FreeBSD/OpenBSD/NetBSD/DragonFly: no peer-pid option this arm can rely on,
/// so the pid stays absent. REQ-569 ships to macOS and Linux, both of which
/// report one; anywhere else the ancestry decision is simply
/// [`crate::peer::Ancestry::Indeterminate`] and the caller must say what that
/// costs rather than silently getting a "not a descendant".
#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer_pid(_fd: std::os::fd::RawFd) -> Option<i32> {
    None
}

/// Linux variant: peer credentials via `getsockopt(SOL_SOCKET, SO_PEERCRED)`,
/// which fills a `ucred { pid, uid, gid }` with the connecting process's
/// credentials as recorded by the kernel at `connect(2)` time.
///
/// The pid was already in that struct and was simply discarded before REQ-569;
/// this arm needs no second syscall. A kernel that cannot attribute the peer to
/// a process in our pid namespace reports `0`, which is reported as absent.
#[cfg(target_os = "linux")]
pub fn peer_identity(stream: &UnixStream) -> io::Result<PeerIdentity> {
    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is a valid, open socket descriptor owned by `stream`; `cred`
    // and `len` reference live stack storage sized exactly for `ucred`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc == 0 {
        Ok(PeerIdentity {
            uid: cred.uid,
            pid: (cred.pid > 0).then_some(cred.pid),
        })
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Reads the effective uid of the process on the other end of `stream`.
///
/// # Errors
///
/// Returns the underlying OS error if the kernel refuses the query.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(peer_identity(stream)?.uid)
}

/// The daemon's own effective uid.
#[must_use]
pub fn current_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// The authorization predicate: a peer may connect only as the daemon's uid.
///
/// # Errors
///
/// Returns [`AuthError::Unauthorized`] when `peer != expected`.
pub fn authorize_uid(peer: u32, expected: u32) -> Result<(), AuthError> {
    if peer == expected {
        Ok(())
    } else {
        Err(AuthError::Unauthorized { peer, expected })
    }
}

/// Reads and authorizes the peer credentials of a freshly accepted connection.
///
/// Returns the whole [`PeerIdentity`] rather than a bare `u32` so a caller
/// cannot pass a uid where a pid belongs, or the reverse — the two are both
/// small integers describing the same process, which is precisely when a
/// distinct type earns its keep.
///
/// # Errors
///
/// Returns [`AuthError::PeerCred`] if the credentials cannot be read, or
/// [`AuthError::Unauthorized`] if the peer is a different user.
pub fn check_peer(stream: &UnixStream) -> Result<PeerIdentity, AuthError> {
    let peer = peer_identity(stream).map_err(AuthError::PeerCred)?;
    authorize_uid(peer.uid, current_uid())?;
    Ok(peer)
}

/// Restricts a bound socket file to owner-only access (`0600`).
///
/// # Errors
///
/// Returns the OS error if the mode cannot be set.
pub fn secure_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Ensures the socket's parent directory exists and is owner-only (`0700`).
///
/// The load-bearing case is the directory the daemon creates for its own state:
/// creating it with the restrictive mode from the start (rather than the
/// umask-dependent default) closes the brief window in which the socket file
/// could be group/other-*connectable* before its own `0600` lands (REQ-544 L-1)
/// — with a `0700` parent, no other user can even traverse in to reach the
/// socket. A pre-existing directory (e.g. `XDG_RUNTIME_DIR`, already `0700` by
/// spec, or a shared temp dir we may not own) is tightened best-effort but never
/// fails the bind. The peer-credential check ([`check_peer`]) still backstops
/// authorization; this just removes the timing window.
///
/// # Errors
///
/// Returns the OS error only if a directory the daemon needed to create could
/// not be created.
pub fn secure_socket_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if dir.exists() {
        // A directory we did not create: tighten if we can, but do not fail the
        // bind over a parent we may not own.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        return Ok(());
    }
    // Create every missing component owner-only from the start — the window L-1
    // closes.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // Guarantee exactly 0700 regardless of the process umask.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_uid_is_authorized() {
        assert!(authorize_uid(501, 501).is_ok());
    }

    #[test]
    fn a_different_uid_cannot_connect() {
        // The core of the peer-cred rule: a process running as a different uid
        // is rejected. A live cross-uid socket test would need root to spawn a
        // process as another user, so the decision function is tested directly.
        let err = authorize_uid(1000, 501).unwrap_err();
        assert!(matches!(
            err,
            AuthError::Unauthorized {
                peer: 1000,
                expected: 501
            }
        ));
    }

    #[test]
    fn secure_socket_dir_is_owner_only() {
        // REQ-544 L-1: the socket's parent dir is created 0700 so the socket is
        // never briefly group/other-connectable.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "teton-sockdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        secure_socket_dir(&dir).expect("create secure dir");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "socket dir must be owner-only");

        // Tightening a pre-existing looser directory also works.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        secure_socket_dir(&dir).expect("tighten existing dir");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "pre-existing dir must be tightened to 0700");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn peer_uid_of_a_local_socket_is_the_current_uid() {
        let path = std::env::temp_dir().join(format!(
            "teton-auth-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let _client = UnixStream::connect(&path).await.unwrap();
        let server_side = accept.await.unwrap();

        assert_eq!(peer_uid(&server_side).unwrap(), current_uid());
        // A same-user peer therefore passes the full check.
        assert!(check_peer(&server_side).is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn peer_identity_reports_the_connecting_process() {
        // The real-syscall check for the REQ-569 addition, on whichever arm the
        // runner compiles: the client side of this socket is *this* process, so
        // the kernel-attested peer pid must be our own. LESSON-433 — macOS goes
        // through `getsockopt(LOCAL_PEERPID)` and Linux through the `pid` field
        // of `SO_PEERCRED`, and CI runs both.
        let path = std::env::temp_dir().join(format!(
            "teton-auth-pid-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let _client = UnixStream::connect(&path).await.unwrap();
        let server_side = accept.await.unwrap();

        let peer = check_peer(&server_side).expect("a same-user peer is authorized");
        assert_eq!(peer.uid, current_uid());

        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        assert_eq!(
            peer.pid,
            Some(i32::try_from(std::process::id()).expect("a pid fits in i32")),
            "the peer of this socket is this very process"
        );

        let _ = std::fs::remove_file(&path);
    }
}
