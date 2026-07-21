//! Socket authentication: filesystem permissions plus a peer-credential check.
//!
//! ADR-002 specifies two layers of socket auth. The first is discretionary: the
//! socket file is created `0600` so only the owning user may `connect(2)`. The
//! second is a defence-in-depth peer-credential check — the daemon reads the
//! connecting process's effective uid from the kernel (`getpeereid`, portable
//! across macOS and Linux) and refuses any peer whose uid differs from the
//! daemon's own. Error text never carries paths, content, or credentials
//! (conventions: privacy in error messages).

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

/// Reads the effective uid of the process on the other end of `stream`.
///
/// The kernel-attested peer credential is obtained per platform: `getpeereid(2)`
/// on macOS/BSD (glibc does **not** provide it), and `getsockopt(SO_PEERCRED)`
/// on Linux. Both report the peer's uid for an `AF_UNIX` stream socket and are
/// not TOCTOU-spoofable (the credential is a property of the connected socket).
///
/// # Errors
///
/// Returns the underlying OS error if the kernel refuses the query.
#[cfg(any(target_os = "macos", target_os = "ios", target_vendor = "apple", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid, open socket descriptor owned by `stream` for the
    // duration of the call, and both out-pointers reference live stack storage.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Linux variant: peer credentials via `getsockopt(SOL_SOCKET, SO_PEERCRED)`,
/// which fills a `ucred { pid, uid, gid }` with the connecting process's
/// credentials as recorded by the kernel at `connect(2)` time.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
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
        Ok(cred.uid)
    } else {
        Err(io::Error::last_os_error())
    }
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
/// # Errors
///
/// Returns [`AuthError::PeerCred`] if the credentials cannot be read, or
/// [`AuthError::Unauthorized`] if the peer is a different user.
pub fn check_peer(stream: &UnixStream) -> Result<u32, AuthError> {
    let peer = peer_uid(stream).map_err(AuthError::PeerCred)?;
    authorize_uid(peer, current_uid())?;
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
}
