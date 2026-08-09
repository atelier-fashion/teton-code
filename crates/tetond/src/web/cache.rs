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
//! ## The store is bounded, and bounding it is a security property
//!
//! A cache that only ever grows is a way for whatever the model reads to fill a
//! user's disk — the page count is chosen by the pages, not by anyone here — and
//! it is also a way for the record of what was fetched to outlive by months the
//! window in which it was useful. So every [`WebCache::put`] ends by enforcing
//! two bounds over the directory: entries past the configured freshness window
//! are unlinked (they could never be served again — see [`WebCache::is_fresh`]),
//! and if the total still exceeds [`MAX_TOTAL_BYTES`] the oldest are dropped
//! until it does not.
//!
//! Enforcement rides on `put` rather than on a timer because `put` is the only
//! event that can make the directory bigger, and a bound checked exactly when it
//! can be breached needs no thread, no schedule, and no second component that
//! could be missing. The pass reads directory metadata only, never entry bodies:
//! a file's mtime is set by the same write that stamps its `fetched_at_secs`, so
//! ordering by one is ordering by the other at no I/O cost.
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

/// The ceiling on everything the cache holds, in bytes.
///
/// Not a tuning knob and deliberately not a config key: the number a user would
/// want to set is the freshness window (`cache_ttl_secs`), which is the one that
/// changes what the cache *does*. This is the backstop for the case the window
/// cannot bound — a great many documents fetched inside one window — and a
/// second knob whose only effect is "how much disk may this quietly take" is a
/// knob nobody can set correctly without knowing the page sizes in advance.
///
/// 64 MiB is roughly four thousand reduced pages at the reduction cap, which is
/// far more than a freshness window's worth of lookups and small enough that
/// nobody notices it in a state directory.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// How long an abandoned temporary file may sit before the next write removes
/// it.
///
/// The only way one survives is a process that died between creating its temp
/// and renaming it over the entry, so this is not a race window to be won — it
/// is the age past which "still being written" stops being a possible
/// explanation. An hour is well past that for a write of at most the reduction
/// cap, and being generous costs one stale file.
const ORPHAN_TEMP_MAX_AGE_SECS: u64 = 3_600;

/// The extension [`write_entry`]'s temporary files end in.
const TEMP_SUFFIX: &str = "part";

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
    /// Whether the fetch this entry came from hit [the body
    /// cap](crate::egress::lookup::LOOKUP_MAX_BODY_BYTES) and stopped short.
    ///
    /// Stored because it is a property **of the document**, not of the request
    /// that fetched it: the reduction on disk is the same partial text on every
    /// later hit, so the caveat that goes with it has to survive the write.
    /// Without this field a truncated page was announced as truncated exactly
    /// once — on the fetch — and then served silently complete for the rest of
    /// the TTL, which is a model reasoning about the end of a document it was
    /// never given.
    ///
    /// `#[serde(default)]` so an entry written by an earlier build still
    /// deserializes. The default is `false`, which is the wrong answer for a
    /// truncated legacy entry and the right one for every other — and the
    /// alternative, treating an old entry as unreadable, would flush a whole
    /// cache to add a caveat to a handful of pages.
    #[serde(default)]
    pub truncated: bool,
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
    /// `truncated` rides along rather than defaulting, so a caller that has a
    /// partial document cannot store it as a whole one by omission — see
    /// [`CacheEntry::truncated`].
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
    ///
    /// Enforcing the store's bounds afterwards is deliberately **not** part of
    /// that result: the entry is on disk either way, and a failure to unlink
    /// someone else's stale file is not a failed write to report to a caller
    /// whose document was stored.
    pub fn put(&self, url: &str, content: &str, truncated: bool) -> Result<(), CacheError> {
        if !self.is_enabled() {
            return Ok(());
        }
        let now = now_secs();
        let entry = CacheEntry {
            content: content.to_owned(),
            fetched_at_secs: now,
            ttl_secs: self.ttl_secs,
            truncated,
        };
        let encoded = serde_json::to_vec(&entry).map_err(|_| CacheError::Encode)?;
        prepare_dir(&self.dir)?;
        write_entry(&self.path_for(url), &encoded)?;
        enforce_bounds(&self.dir, self.ttl_secs, MAX_TOTAL_BYTES, now);
        Ok(())
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

    // Every temporary file is named for exactly one write: the pid separates two
    // processes sharing a data dir, and the counter separates two writes inside
    // one process.
    //
    // The counter is not decoration. Two concurrent `put`s of the same URL used
    // to name the same `<digest>.<pid>.part`, and `File::create` truncates — so
    // one writer's `write_all` landed in the other's file, and after the first
    // rename consumed it the second renamed a path that no longer existed and
    // failed `NotFound`. Neither writer "wins the file back": the entry on disk
    // is whichever interleaving happened to finish, and the loser reports an I/O
    // error for a write that was never racing over anything but a name. A name
    // per write removes the sharing rather than the symptom, and leaves the
    // rename as the only place two writers meet — which is where atomicity
    // already lives.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = path.with_extension(format!("{}-{seq}.{TEMP_SUFFIX}", std::process::id()));
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

/// Bring the cache directory back under both of its bounds, best-effort.
///
/// Runs after every successful write (see the module doc for why there and not
/// on a timer). Three passes' worth of work in one walk of the directory:
///
/// 1. **Abandoned temporaries** — anything ending in `.part` older than
///    [`ORPHAN_TEMP_MAX_AGE_SECS`]. Not counted toward the size bound and never
///    evicted for it: a temp file young enough to be someone's live write must
///    not be removed by age *or* by pressure, and one old enough is not a live
///    write at all.
/// 2. **Entries past the window** — `mtime + ttl_secs` in the past. [`WebCache::get`]
///    would refuse these, so keeping them spends the size bound on bytes nobody
///    can be served.
/// 3. **Oldest-first eviction**, only if what survives still exceeds
///    `max_total_bytes`.
///
/// Every failure is ignored on purpose. A directory that cannot be read, a file
/// that vanished under a concurrent writer, a permission the daemon does not
/// have — each means "this pass tidied less than it hoped", and none of them
/// makes the entry that was just written any less stored. Reporting them upward
/// would hand the caller an error whose only honest handling is to ignore it.
fn enforce_bounds(dir: &Path, ttl_secs: u64, max_total_bytes: u64, now: u64) {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };
    // (age-ordering key, size, path) for every entry still eligible to be served.
    let mut live: Vec<(u64, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for item in listing.flatten() {
        let path = item.path();
        let Ok(meta) = item.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let written_at = written_at_secs(&meta, now);
        let age = now.saturating_sub(written_at);
        if path.extension().is_some_and(|ext| ext == TEMP_SUFFIX) {
            if age >= ORPHAN_TEMP_MAX_AGE_SECS {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if age >= ttl_secs {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        total = total.saturating_add(meta.len());
        live.push((written_at, meta.len(), path));
    }
    if total <= max_total_bytes {
        return;
    }
    live.sort_unstable_by_key(|(written_at, _, _)| *written_at);
    for (_, size, path) in live {
        if total <= max_total_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// When `meta`'s file was written, in Unix seconds, falling back to `now`.
///
/// The fallback direction is "assume it was just written", which is the one that
/// cannot cascade: a filesystem that reports no mtime would otherwise make every
/// file look infinitely old and turn each write into a wipe of the store. Treated
/// as new, such a file is still bounded — by the size cap, which needs no clock.
fn written_at_secs(meta: &std::fs::Metadata, now: u64) -> u64 {
    meta.modified()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(now, |since_epoch| since_epoch.as_secs())
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

    /// Backdate a file's mtime, which is what [`enforce_bounds`] reads as "when
    /// this was fetched". Cheaper and far more precise than sleeping out a real
    /// window, and it lets the ordering tests state exact ages.
    fn set_written_at(path: &Path, secs: u64) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_times");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(secs)),
        )
        .expect("set mtime");
    }

    /// The names currently in the cache directory, sorted.
    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read cache dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_fresh_entry_round_trips_without_touching_the_network_layer() {
        let base = scratch_base("fresh");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "reduced page text", false).expect("put");
        let hit = cache.get(URL).expect("fresh hit");
        assert_eq!(hit.content, "reduced page text");
        assert_eq!(hit.ttl_secs, 900);
    }

    #[test]
    fn an_absent_entry_is_a_miss() {
        let cache = WebCache::new(&scratch_base("absent"), 900);
        assert!(cache.get(URL).is_none());
    }

    /// **The truncation flag survives the write**, because it describes the
    /// document rather than the fetch. Stored only in the fetching turn's
    /// sentence, the caveat would last one turn while the partial text on disk
    /// lasted the whole TTL — a page announced as cut short once, then served as
    /// complete every time after.
    #[test]
    fn the_truncation_flag_rides_with_the_document_it_describes() {
        let base = scratch_base("truncated");
        let cache = WebCache::new(&base, 900);

        cache
            .put(URL, "the first two megabytes", true)
            .expect("put");
        assert!(cache.get(URL).expect("hit").truncated);

        cache
            .put("https://example.com/whole", "all of it", false)
            .expect("put");
        assert!(
            !cache
                .get("https://example.com/whole")
                .expect("hit")
                .truncated
        );
    }

    /// **An entry written before the field existed still reads.**
    ///
    /// `#[serde(default)]` rather than a version bump: the alternative — a
    /// missing field making the entry undeserializable — silently flushes every
    /// cache in existence to add a caveat that applies to a handful of pages.
    /// `false` is the wrong answer for a truncated legacy entry and the right
    /// one for every other, and each of them expires inside the freshness
    /// window anyway.
    #[test]
    fn an_entry_from_before_the_flag_existed_still_deserializes() {
        let base = scratch_base("legacy-entry");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("mkdir");
        let legacy = format!(
            r#"{{"content":"old text","fetched_at_secs":{},"ttl_secs":900}}"#,
            now_secs()
        );
        write_entry(&cache.path_for(URL), legacy.as_bytes()).expect("write");

        let hit = cache.get(URL).expect("a legacy entry must still be a hit");
        assert_eq!(hit.content, "old text");
        assert!(!hit.truncated);
    }

    /// The cache lives beside the ledger, not inside it and not somewhere else.
    #[test]
    fn entries_live_in_web_cache_beside_the_ledger_db() {
        let base = scratch_base("layout");
        let cache = WebCache::new(&base, 900);
        assert_eq!(cache.dir(), base.join("web-cache"));
        cache.put(URL, "text", false).expect("put");
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
        cache.put(URL, "text", false).expect("put");
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
            truncated: false,
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
            truncated: false,
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
            truncated: false,
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
            truncated: false,
        };
        assert!(!cache.is_fresh(&entry, 1_000));
    }

    #[test]
    fn evict_removes_the_entry_and_is_idempotent() {
        let base = scratch_base("evict");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "text", false).expect("put");
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
        cache.put(URL, "text", false).expect("put");
        // Age it past its own window without waiting for a real second.
        let aged = WebCache::new(&base, 900);
        let path = aged.path_for(URL);
        let entry = CacheEntry {
            content: "text".to_owned(),
            fetched_at_secs: 1,
            ttl_secs: 1,
            truncated: false,
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
        WebCache::new(&base, 900)
            .put(URL, "text", false)
            .expect("put");
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
            .put(URL, "text", false)
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
        WebCache::new(&base, 900)
            .put(URL, "text", false)
            .expect("put");
        assert!(WebCache::new(&base, 900).get(URL).is_some());
        assert!(WebCache::new(&base, 0).get(URL).is_none());
    }

    #[test]
    fn entry_files_are_owner_only_and_so_is_their_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = scratch_base("perms");
        let cache = WebCache::new(&base, 900);
        cache.put(URL, "text", false).expect("put");
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
        cache.put(URL, "text", false).expect("put");
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
        cache.put(URL, "first", false).expect("put");
        std::fs::set_permissions(cache.path_for(URL), std::fs::Permissions::from_mode(0o644))
            .expect("loosen entry");
        cache.put(URL, "second", false).expect("re-put");
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
            .put("HTTPS://Example.com/a#frag", "text", false)
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

    /// Two writers in one process must not share a temporary file. They used to:
    /// the temp name was `<digest>.<pid>.part` for every write, `File::create`
    /// truncates, and so one writer's bytes landed in the other's file and the
    /// writer that renamed second got `NotFound` for a write it had completed.
    ///
    /// The same URL from several threads is the shape that produced it — one
    /// digest, one temp name — and 256 writes is well past the point the old
    /// code survived.
    #[test]
    fn concurrent_writes_of_one_url_do_not_share_a_temporary_file() {
        let base = scratch_base("concurrent");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("dir");

        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    for round in 0..32 {
                        cache
                            .put(URL, &format!("worker {worker} round {round}"), false)
                            .expect("a concurrent put must not fail on a shared temp name");
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker thread");
        }

        // Whichever write landed last is the entry, and it is a *whole* one —
        // not two writers' bytes interleaved into one file.
        let hit = cache.get(URL).expect("an entry survives the race");
        assert!(hit.content.starts_with("worker "), "{:?}", hit.content);
        // And nothing was left behind: every temporary was consumed by its own
        // rename.
        assert_eq!(
            dir_names(cache.dir()).len(),
            1,
            "{:?}",
            dir_names(cache.dir())
        );
    }

    /// The freshness window bounds what can be *served*; on its own it bounds
    /// nothing on disk, because a stale entry is left where it lies (the read
    /// path deliberately does not write). The next write is what collects them.
    #[test]
    fn a_write_unlinks_entries_that_are_past_the_window() {
        let base = scratch_base("sweep-stale");
        let cache = WebCache::new(&base, 900);
        cache
            .put("https://example.com/old", "old", false)
            .expect("put");
        cache
            .put("https://example.com/new", "new", false)
            .expect("put");
        let old = cache.path_for("https://example.com/old");
        set_written_at(&old, now_secs() - 7_200);

        cache
            .put("https://example.com/third", "third", false)
            .expect("put");

        assert!(!old.exists(), "an unservable entry must not hold disk");
        assert_eq!(
            cache.get("https://example.com/new").expect("hit").content,
            "new"
        );
        assert_eq!(
            cache.get("https://example.com/third").expect("hit").content,
            "third"
        );
    }

    /// The size bound is the one the freshness window cannot supply: a great
    /// many documents fetched inside one window. Oldest first, until under the
    /// cap and no further — evicting the whole store to satisfy it would turn a
    /// bound into a wipe.
    #[test]
    fn the_oldest_entries_are_evicted_until_the_store_is_under_its_total_cap() {
        let base = scratch_base("size-cap");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("dir");
        let now = now_secs();
        let payload = vec![b'x'; 1_000];
        let paths: Vec<PathBuf> = (0..10u64)
            .map(|n| {
                let path = cache.dir().join(format!("{n:064x}"));
                std::fs::write(&path, &payload).expect("write");
                // Ascending age keys: index 0 is the oldest.
                set_written_at(&path, now - 100 + n);
                path
            })
            .collect();

        enforce_bounds(cache.dir(), 900, 4_000, now);

        for (index, path) in paths.iter().enumerate() {
            assert_eq!(
                path.exists(),
                index >= 6,
                "entry {index} (1 KiB each, 10 KiB stored, 4 KiB cap)"
            );
        }
    }

    /// Under the cap, nothing is evicted — the bound is a ceiling, not a target
    /// to shrink toward.
    #[test]
    fn a_store_inside_its_cap_loses_nothing() {
        let base = scratch_base("under-cap");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("dir");
        let now = now_secs();
        for n in 0..4u64 {
            let path = cache.dir().join(format!("{n:064x}"));
            std::fs::write(&path, vec![b'x'; 1_000]).expect("write");
            set_written_at(&path, now - 100 + n);
        }
        enforce_bounds(cache.dir(), 900, 64 * 1024, now);
        assert_eq!(dir_names(cache.dir()).len(), 4);
    }

    /// A temporary file from a process that died between `create` and `rename`
    /// would otherwise sit there forever. It is collected by age, never by size
    /// pressure: a young `.part` may be a live write, and removing one would
    /// recreate the collision the unique naming just closed.
    #[test]
    fn an_abandoned_temporary_is_collected_but_a_live_one_is_left_alone() {
        let base = scratch_base("orphan-temp");
        let cache = WebCache::new(&base, 900);
        prepare_dir(cache.dir()).expect("dir");
        let now = now_secs();
        let abandoned = cache.dir().join(format!("{:064x}.999-0.part", 1));
        let live = cache.dir().join(format!("{:064x}.999-1.part", 2));
        std::fs::write(&abandoned, b"half-written").expect("write");
        std::fs::write(&live, b"in flight").expect("write");
        set_written_at(&abandoned, now - ORPHAN_TEMP_MAX_AGE_SECS - 60);

        cache.put(URL, "text", false).expect("put");

        assert!(
            !abandoned.exists(),
            "an hour-old temporary is not a live write"
        );
        assert!(
            live.exists(),
            "a temporary young enough to be in flight is untouched"
        );
        assert!(cache.get(URL).is_some());
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
