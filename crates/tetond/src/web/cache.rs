//! The on-machine document cache (REQ-563 BR-12, architecture D-3).
//!
//! One file per fetched document, named for the SHA-256 of the **normalized**
//! URL, under `web-cache/` in the daemon's data directory — the same directory
//! the cost ledger's `cost.db` sits in, resolved the same way (the caller passes
//! the base dir; see `Runtime::from_env`). A fresh hit is served with zero
//! egress, which is the whole point: BR-12 makes a cached lookup free of the
//! network, and the taint restriction (BR-13) does not apply to it because there
//! is nothing to restrict.
//!
//! ## Why the file name is a digest and the entry does not hold the URL
//!
//! Content-addressing is the cheap part — a digest gives a flat, collision-free,
//! filesystem-safe name for an arbitrary URL. The part worth stating is what it
//! *avoids*: neither the file name nor the entry body records the URL in the
//! clear. That is the same rule the ledger's schema follows (D-7: no column can
//! hold a full URL), applied to the one other place a lookup leaves a trace.
//! Someone with the URL can confirm it was fetched; nobody gets a browsing
//! history by listing a directory.
//!
//! Nothing here syncs, exports, or egresses. The cache is a directory of files
//! the daemon wrote for itself, `0600` under a `0700` parent, and there is no
//! code path in this module that reads one out to anywhere but the caller.
//!
//! ## Freshness is the *stricter* of two windows
//!
//! An entry records the ttl it was written under, and [`WebCache::get`] requires
//! the entry to be fresh under both that ttl and the cache's currently
//! configured one. The asymmetry is deliberate:
//!
//! - **Lowering** `cache_ttl_secs` (including to `0`) takes effect immediately,
//!   on entries that already exist. A user tightening the window is asking for
//!   less staleness now, not from the next fetch onwards.
//! - **Raising** it never extends an entry beyond the window it was written
//!   under, because that window is what the user was promised when the bytes
//!   were stored.
//!
//! Storing the ttl is what makes the second half possible; without it the
//! recorded fact would be decoration.
//!
//! ## Every failure is a miss
//!
//! [`WebCache::get`] returns `Option`, not `Result`. An unreadable file, a
//! truncated write, an entry written by a future format — each means "this
//! document is not usable from disk", which is precisely a cache miss, and the
//! caller's only sane response to any of them is the one it already has for an
//! absent entry. A `Result` here would be a decision handed upward that has
//! exactly one correct answer.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use teton_core::WebConfig;
use teton_inference::sha256_hex;

use super::normalize_url;

/// The cache directory's name under the daemon data dir — a sibling of
/// `cost.db`, not a child of it.
pub const CACHE_DIR_NAME: &str = "web-cache";

/// One cached document.
///
/// Deliberately **without** a URL field: see the module doc. The reduced text is
/// stored rather than the raw HTML because [`reduce`](super::reduce) is a pure
/// function of the fetched bytes, so re-reducing on every hit could only produce
/// the same string at more cost — and because the raw bytes are the form nobody
/// downstream is allowed to see anyway (BR-10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// The reduced, capped page text — what the lookup tool folds into context.
    pub content: String,
    /// When the document was fetched, in Unix seconds.
    pub fetched_at_secs: u64,
    /// The freshness window in force when this entry was written, in seconds.
    /// Read together with the cache's current window — the stricter wins.
    pub ttl_secs: u64,
}

/// A failure to *write* the cache. Reads have no error surface by design.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The entry could not be written or removed.
    ///
    /// Carries the [`std::io::ErrorKind`] rather than the OS message: the kind
    /// is what a caller can act on, and the message can name a path the daemon
    /// has no business repeating outward.
    #[error("web cache i/o failed: {kind}")]
    Io {
        /// The classified I/O failure.
        kind: std::io::ErrorKind,
    },
    /// The entry could not be serialized.
    #[error("web cache entry could not be encoded")]
    Encode,
}

impl From<std::io::Error> for CacheError {
    fn from(err: std::io::Error) -> Self {
        Self::Io { kind: err.kind() }
    }
}

/// The local, content-addressed document cache.
///
/// Cheap to clone (a path and a number); holds no handle and no lock. Two
/// clones racing on the same URL is safe by construction: writes land through a
/// temporary file and an atomic rename, so a reader sees either the old entry or
/// the new one, never a half-written one.
#[derive(Debug, Clone)]
pub struct WebCache {
    dir: PathBuf,
    ttl_secs: u64,
}

impl WebCache {
    /// A cache rooted at `<base_dir>/web-cache` with `ttl_secs` freshness.
    ///
    /// `base_dir` is the daemon's per-user state directory — the same value
    /// `cost.db` is resolved against, passed in rather than re-derived so the
    /// two stores cannot end up in different places (there is exactly one
    /// resolution of the data dir, and it is the caller's).
    #[must_use]
    pub fn new(base_dir: &Path, ttl_secs: u64) -> Self {
        Self {
            dir: base_dir.join(CACHE_DIR_NAME),
            ttl_secs,
        }
    }

    /// A cache configured from the `[web]` table (BR-12's `cache_ttl_secs`).
    #[must_use]
    pub fn from_config(base_dir: &Path, config: &WebConfig) -> Self {
        Self::new(base_dir, config.cache_ttl_secs)
    }

    /// Whether this cache persists anything at all.
    ///
    /// `cache_ttl_secs = 0` is not "cache forever" and not "cache but always
    /// stale" — it disables the store outright (config.rs spells out why zero
    /// reads that way). A disabled cache never creates its directory, so a user
    /// who turns caching off is not left with an empty directory implying
    /// otherwise.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.ttl_secs != 0
    }

    /// The directory entries live in.
    ///
    /// Uninstall does not need this — `web-cache/` sits inside the state
    /// directory `teton uninstall` already removes wholesale, which is the point
    /// of putting it there rather than somewhere of its own. It is exposed so
    /// the daemon can name and inspect its own store.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The fresh entry for `url`, or `None` for absent, stale, or unreadable.
    ///
    /// Pure with respect to the network: this is a function of the store and the
    /// clock, and no branch of it can reach the transport. That property is what
    /// AC-10 leans on — a cache hit is observably zero-egress because there is
    /// no egress to observe.
    #[must_use]
    pub fn get(&self, url: &str) -> Option<CacheEntry> {
        if !self.is_enabled() {
            return None;
        }
        let entry: CacheEntry =
            serde_json::from_slice(&std::fs::read(self.path_for(url)).ok()?).ok()?;
        if self.is_fresh(&entry, now_secs()) {
            Some(entry)
        } else {
            // Left on disk rather than unlinked: a stale entry is about to be
            // overwritten by the refetch that follows, and deleting it here
            // would add a write to the read path to save a write on the write
            // path.
            None
        }
    }

    /// Store `content` as the current reduction of `url`.
    ///
    /// A disabled cache stores nothing and reports success: "no cache" is a
    /// configuration, not a failure, and a caller that had to distinguish the
    /// two would end up re-implementing [`WebCache::is_enabled`] at every call
    /// site.
    ///
    /// # Errors
    /// [`CacheError`] if the directory cannot be created or the entry cannot be
    /// encoded or written. A caller may reasonably log and continue: a cache
    /// write that fails costs a later refetch and nothing else.
    pub fn put(&self, url: &str, content: &str) -> Result<(), CacheError> {
        if !self.is_enabled() {
            return Ok(());
        }
        let entry = CacheEntry {
            content: content.to_owned(),
            fetched_at_secs: now_secs(),
            ttl_secs: self.ttl_secs,
        };
        let encoded = serde_json::to_vec(&entry).map_err(|_| CacheError::Encode)?;
        prepare_dir(&self.dir)?;
        write_entry(&self.path_for(url), &encoded)
    }

    /// Drop the entry for `url` — the explicit-refresh path (BR-12: "an explicit
    /// user refresh bypasses it").
    ///
    /// Bypassing by *eviction* rather than by a "skip the cache this once" flag
    /// is the difference between a refresh that sticks and one that has to be
    /// repeated: after this, the next lookup misses, refetches, and stores, so
    /// every subsequent reader sees the refreshed document too.
    ///
    /// Removing an entry that is not there succeeds — the caller asked for the
    /// document not to be cached, and it is not. It answers `false` rather than
    /// `true`, because "there was a stale copy and it is gone" and "there was
    /// never a copy" are different answers to *why* the next fetch is live
    /// ([`teton_protocol::methods::WebRefreshOutcome`] keeps them apart, and this
    /// is the only place that can tell them apart).
    ///
    /// Deliberately **not** implemented as "[`Self::get`] first, then remove":
    /// `get` answers `None` for a stale-but-present entry and for a disabled
    /// cache, so a probe would report `false` while this call unlinked a real
    /// file — and two syscalls would leave a race between them. The removal's own
    /// result is the only non-lying source of the answer.
    ///
    /// Unlike [`Self::get`] and [`Self::put`] it does not consult
    /// [`Self::is_enabled`]: a cache turned off after entries were written still
    /// has those files, and a user refreshing one means the file.
    ///
    /// # Errors
    /// [`CacheError::Io`] if the entry exists and cannot be removed.
    pub fn evict(&self, url: &str) -> Result<bool, CacheError> {
        match std::fs::remove_file(self.path_for(url)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// The on-disk path for `url`: the digest of its normalized form.
    fn path_for(&self, url: &str) -> PathBuf {
        self.dir.join(sha256_hex(normalize_url(url).as_bytes()))
    }

    /// Whether `entry` is fresh at `now`, under the stricter of the two windows.
    ///
    /// A `fetched_at_secs` in the future is treated as stale rather than as
    /// eternally fresh. Clocks move backwards (NTP corrections, a laptop waking
    /// in another timezone's opinion of the epoch), and the two readings of an
    /// impossible timestamp are "serve these bytes until the clock catches up"
    /// and "fetch again". Only the second is bounded.
    fn is_fresh(&self, entry: &CacheEntry, now: u64) -> bool {
        let ttl = self.ttl_secs.min(entry.ttl_secs);
        if ttl == 0 || entry.fetched_at_secs > now {
            return false;
        }
        now - entry.fetched_at_secs < ttl
    }
}

/// Seconds since the Unix epoch, or `0` if the clock predates it (which would
/// make every entry stale — the safe reading of a nonsensical clock).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Create the cache directory `0700`, and tighten it if it already exists.
///
/// Mirrors [`crate::auth::secure_socket_dir`] and the weights-dir preparation in
/// [`crate::install`]: the mode is set explicitly rather than inherited from a
/// umask, because a directory holding what the user browsed is not something to
/// leave to the environment's default.
fn prepare_dir(dir: &Path) -> Result<(), CacheError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    // A pre-existing directory may be looser; force it back. Failure here is not
    // fatal (another user's directory cannot be tightened, and the entry write
    // below will fail on its own terms).
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Write `encoded` to `path` as `0600`, atomically.
///
/// The mode is set on the temporary file **before** any content is written and
/// the rename carries it across, which is the only ordering with no window:
/// `set_permissions` after the rename leaves the entry briefly readable under
/// its real name, and `create` alone would honour the umask. Same argument, same
/// shape as `runtime::write_config_atomically`.
fn write_entry(path: &Path, encoded: &[u8]) -> Result<(), CacheError> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    // The pid keeps two daemons (or a test and a daemon) sharing a data dir from
    // interleaving into one temporary file. Within a process, a rename is atomic
    // and the loser simply wins the file back on its next fetch.
    let temp = path.with_extension(format!("{}.part", std::process::id()));
    {
        let mut file = std::fs::File::create(&temp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(encoded)?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(err.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique on-disk base dir for one test (no `tempfile` dependency in this
    /// crate; this is the house pattern — see `cost::ledger::tests::scratch_db`).
    fn scratch_base(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("teton-web-cache-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch base dir");
        dir
    }

    const URL: &str = "https://example.com/docs/page?x=1";

    #[test]
    fn a_fresh_entry_round_trips_without_touching_the_network_layer() {
        let base = scratch_base("fresh");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "reduced page text").expect("put");
        let hit = cache.get(URL).expect("fresh hit");
        assert_eq!(hit.content, "reduced page text");
        assert_eq!(hit.ttl_secs, 900);
    }

    #[test]
    fn an_absent_entry_is_a_miss() {
        let cache = WebCache::new(&scratch_base("absent"), 900);
        assert!(cache.get(URL).is_none());
    }

    /// The cache lives beside the ledger, not inside it and not somewhere else.
    #[test]
    fn entries_live_in_web_cache_beside_the_ledger_db() {
        let base = scratch_base("layout");
        let cache = WebCache::new(&base, 900);
        assert_eq!(cache.dir(), base.join("web-cache"));
        cache.put(URL, "text").expect("put");
        let names: Vec<_> = std::fs::read_dir(cache.dir())
            .expect("read cache dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // A 64-hex-character digest, and nothing that reads as a URL.
        assert_eq!(names[0].len(), 64);
        assert!(names[0].chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Neither the file name nor the body may carry the URL in the clear.
    #[test]
    fn a_stored_entry_records_no_url() {
        let base = scratch_base("no-url");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "text").expect("put");
        for entry in std::fs::read_dir(cache.dir()).expect("read cache dir") {
            let path = entry.expect("entry").path();
            let raw = std::fs::read_to_string(&path).expect("read entry");
            for forbidden in ["example.com", "https", "/docs/page", "x=1"] {
                assert!(
                    !raw.contains(forbidden) && !path.to_string_lossy().contains(forbidden),
                    "cache entry leaked {forbidden}"
                );
            }
        }
    }

    /// Freshness is arithmetic on a recorded timestamp, so it is asserted
    /// directly rather than by sleeping out a real ttl.
    #[test]
    fn an_entry_older_than_the_ttl_is_stale() {
        let cache = WebCache::new(Path::new("/nonexistent"), 60);
        let entry = CacheEntry {
            content: "text".to_owned(),
            fetched_at_secs: 1_000,
            ttl_secs: 60,
        };
        assert!(cache.is_fresh(&entry, 1_000), "just fetched");
        assert!(
            cache.is_fresh(&entry, 1_059),
            "one second inside the window"
        );
        assert!(!cache.is_fresh(&entry, 1_060), "exactly at the window edge");
        assert!(!cache.is_fresh(&entry, 5_000), "long past");
    }

    /// A stale entry on disk reads as a miss end to end, not just in the
    /// predicate.
    #[test]
    fn a_stale_entry_on_disk_is_a_miss() {
        let base = scratch_base("stale");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("dir");
        let entry = CacheEntry {
            content: "old".to_owned(),
            fetched_at_secs: 1,
            ttl_secs: 900,
        };
        write_entry(
            &cache.path_for(URL),
            &serde_json::to_vec(&entry).expect("encode"),
        )
        .expect("write");
        assert!(cache.get(URL).is_none());
    }

    /// Lowering the configured window must retire entries already on disk;
    /// raising it must not resurrect one written under a shorter promise.
    #[test]
    fn freshness_takes_the_stricter_of_the_configured_and_recorded_windows() {
        let entry = CacheEntry {
            content: "text".to_owned(),
            fetched_at_secs: 1_000,
            ttl_secs: 600,
        };
        let tightened = WebCache::new(Path::new("/nonexistent"), 60);
        assert!(!tightened.is_fresh(&entry, 1_100), "config lowered to 60s");
        let loosened = WebCache::new(Path::new("/nonexistent"), 86_400);
        assert!(
            !loosened.is_fresh(&entry, 1_700),
            "raising the config must not extend an entry past its recorded window"
        );
        assert!(loosened.is_fresh(&entry, 1_500), "still inside 600s");
    }

    /// A timestamp from the future is stale, not eternally fresh.
    #[test]
    fn an_entry_timestamped_in_the_future_is_stale() {
        let cache = WebCache::new(Path::new("/nonexistent"), 900);
        let entry = CacheEntry {
            content: "text".to_owned(),
            fetched_at_secs: 9_000,
            ttl_secs: 900,
        };
        assert!(!cache.is_fresh(&entry, 1_000));
    }

    #[test]
    fn evict_removes_the_entry_and_is_idempotent() {
        let base = scratch_base("evict");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "text").expect("put");
        assert!(cache.get(URL).is_some());
        assert!(
            cache.evict(URL).expect("evict"),
            "a present entry reports that it was the one removed"
        );
        assert!(cache.get(URL).is_none());
        assert!(
            !cache.evict(URL).expect("evicting an absent entry succeeds"),
            "the second eviction found nothing, and says so rather than \
             claiming a removal"
        );
    }

    /// The stale case is the one a `get`-then-remove implementation would get
    /// wrong: [`WebCache::get`] answers `None` for a stale entry, so a probe
    /// would report "absent" while the file was there and about to be unlinked.
    #[test]
    fn evicting_a_stale_but_present_entry_reports_a_removal() {
        let base = scratch_base("evict-stale");
        let cache = WebCache::new(&base, 1);
        cache.put(URL, "text").expect("put");
        // Age it past its own window without waiting for a real second.
        let aged = WebCache::new(&base, 900);
        let path = aged.path_for(URL);
        let entry = CacheEntry {
            content: "text".to_owned(),
            fetched_at_secs: 1,
            ttl_secs: 1,
        };
        std::fs::write(&path, serde_json::to_vec(&entry).expect("encode")).expect("write");

        assert!(aged.get(URL).is_none(), "the entry really is stale");
        assert!(
            aged.evict(URL).expect("evict"),
            "a stale entry is present on disk, and eviction removed it"
        );
        assert!(!path.exists());
    }

    /// A disabled cache still evicts: turning the TTL to zero does not delete
    /// what was already written, and a user refreshing one of those files means
    /// the file.
    #[test]
    fn a_disabled_cache_still_evicts_what_an_enabled_one_wrote() {
        let base = scratch_base("evict-disabled");
        WebCache::new(&base, 900).put(URL, "text").expect("put");
        let disabled = WebCache::new(&base, 0);
        assert!(!disabled.is_enabled());
        assert!(
            disabled.evict(URL).expect("evict"),
            "the stored file is still there to remove"
        );
        assert!(!disabled.path_for(URL).exists());
    }

    /// A ttl of zero disables the store outright: nothing is written, no
    /// directory appears, and a read cannot hit.
    #[test]
    fn a_zero_ttl_disables_persistence_entirely() {
        let base = scratch_base("zero-ttl");
        let cache = WebCache::new(&base, 0);
        assert!(!cache.is_enabled());
        cache
            .put(URL, "text")
            .expect("put on a disabled cache succeeds");
        assert!(
            !cache.dir().exists(),
            "a disabled cache creates no directory"
        );
        assert!(cache.get(URL).is_none());
    }

    /// Even entries written while caching was on must stop hitting the moment
    /// the window is zeroed.
    #[test]
    fn zeroing_the_ttl_retires_entries_already_on_disk() {
        let base = scratch_base("zero-retires");
        WebCache::new(&base, 900).put(URL, "text").expect("put");
        assert!(WebCache::new(&base, 900).get(URL).is_some());
        assert!(WebCache::new(&base, 0).get(URL).is_none());
    }

    #[test]
    fn entry_files_are_owner_only_and_so_is_their_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = scratch_base("perms");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "text").expect("put");
        let dir_mode = std::fs::metadata(cache.dir())
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "cache dir must be owner-only");
        let file_mode = std::fs::metadata(cache.path_for(URL))
            .expect("entry metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "cache entries must be owner-only");
    }

    /// A pre-existing loose directory is tightened rather than trusted.
    #[test]
    fn a_loose_cache_directory_is_tightened_on_write() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = scratch_base("loose");
        let cache = WebCache::new(&base, 900);
        std::fs::create_dir_all(cache.dir()).expect("pre-create");
        std::fs::set_permissions(cache.dir(), std::fs::Permissions::from_mode(0o755))
            .expect("loosen");
        cache.put(URL, "text").expect("put");
        let mode = std::fs::metadata(cache.dir())
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// Overwriting must not leave a wider mode behind — the second write goes
    /// through a fresh temporary file, so the entry's mode is set every time.
    #[test]
    fn overwriting_an_entry_keeps_it_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = scratch_base("overwrite");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "first").expect("put");
        std::fs::set_permissions(cache.path_for(URL), std::fs::Permissions::from_mode(0o644))
            .expect("loosen entry");
        cache.put(URL, "second").expect("re-put");
        assert_eq!(cache.get(URL).expect("hit").content, "second");
        let mode = std::fs::metadata(cache.path_for(URL))
            .expect("entry metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// A corrupt or half-written entry is a miss, not an error the caller has to
    /// invent a response to.
    #[test]
    fn an_unreadable_entry_is_a_miss() {
        let base = scratch_base("corrupt");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("dir");
        std::fs::write(cache.path_for(URL), b"{not json").expect("write garbage");
        assert!(cache.get(URL).is_none());
    }

    /// The key is the *normalized* URL, so the spellings normalization declares
    /// equal share one entry — and the ones it deliberately keeps distinct do
    /// not (BR-3: the equivalence must not widen).
    #[test]
    fn the_key_is_the_normalized_url() {
        let base = scratch_base("key");
        let cache = WebCache::new(&base, 900);
        cache
            .put("HTTPS://Example.com/a#frag", "text")
            .expect("put");
        assert!(
            cache.get("https://example.com/a").is_some(),
            "case and fragment must not split the key"
        );
        assert!(
            cache.get("https://example.com/A").is_none(),
            "a different path is a different document"
        );
        assert!(
            cache.get("https://other.com/a").is_none(),
            "a different host is a different document"
        );
    }

    #[test]
    fn the_config_constructor_takes_its_window_from_the_web_table() {
        let base = scratch_base("from-config");
        let config = WebConfig::default();
        let cache = WebCache::from_config(&base, &config);
        assert_eq!(cache.ttl_secs, config.cache_ttl_secs);
        assert!(cache.is_enabled(), "the default window caches");
    }
}
