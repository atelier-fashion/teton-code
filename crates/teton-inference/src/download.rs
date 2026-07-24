//! Resumable, checksum-verified GGUF download with progress events.
//!
//! The transport is abstracted behind [`RangeFetcher`] (a byte-range GET) so the
//! resume/verify/retry orchestration is testable against in-memory fakes and the
//! crate takes on no HTTP dependency — the daemon supplies the real fetcher.
//!
//! Guarantees exercised by the tests and required by the task ACs:
//! - **Resume after interruption.** A partially-written file is continued from
//!   its current length rather than restarted; progress is reported as
//!   `downloaded_bytes` on a `model_lifecycle` `Download` event.
//! - **Checksum verification.** After the file reaches its expected size its
//!   SHA-256 is compared to the catalog value; a mismatch discards the file and
//!   re-fetches from scratch, up to a bounded number of attempts.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::catalog::ModelEntry;
use crate::hash;
use crate::lifecycle::LifecycleEvent;

/// A failure during download.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// A local filesystem error.
    #[error("download I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A recoverable transport interruption. Any bytes already handed to the
    /// sink are durably written, so the download resumes from there.
    #[error("transport error while fetching model: {0}")]
    Transport(String),
    /// The completed file's checksum did not match, even after re-fetching.
    #[error("checksum mismatch after {attempts} attempt(s): expected {expected}, got {actual}")]
    Checksum {
        /// Expected lowercase-hex SHA-256.
        expected: String,
        /// Actual lowercase-hex SHA-256 of the last attempt.
        actual: String,
        /// How many full download attempts were made.
        attempts: u32,
    },
    /// The transport made no progress across repeated resume attempts.
    #[error("transport stalled after {attempts} resume attempts with no progress")]
    Stalled {
        /// Consecutive no-progress attempts before giving up.
        attempts: u32,
    },
    /// The stream delivered more bytes than the catalog size — a corrupt source.
    #[error("downloaded {actual} bytes exceeds the catalog size of {expected}")]
    Oversized {
        /// Catalog-declared size.
        expected: u64,
        /// Bytes written before the overflow was detected.
        actual: u64,
    },
}

impl DownloadError {
    /// Whether this error is a resumable transport interruption.
    fn is_resumable(&self) -> bool {
        matches!(self, DownloadError::Transport(_))
    }
}

/// A byte-range fetch transport.
pub trait RangeFetcher {
    /// Stream the bytes of `url` starting at byte `offset`, invoking `sink` for
    /// each chunk in order.
    ///
    /// Returns the resource's total length. Returning
    /// [`DownloadError::Transport`] models a mid-transfer interruption: any bytes
    /// already passed to `sink` are considered durably written by the caller, and
    /// a later call with a higher `offset` resumes.
    ///
    /// # Errors
    /// Returns a [`DownloadError`] on transport failure or if `sink` errors.
    fn fetch(
        &self,
        url: &str,
        offset: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
    ) -> Result<u64, DownloadError>;
}

/// Tuning for the download orchestration.
#[derive(Debug, Clone, Copy)]
pub struct DownloadConfig {
    /// How many times to re-fetch from scratch on a checksum mismatch before
    /// giving up.
    pub max_checksum_retries: u32,
    /// How many consecutive under-`min_progress_bytes` transport interruptions to
    /// tolerate before declaring the download stalled.
    pub max_stall_retries: u32,
    /// The least a single fetch attempt must durably add for that attempt to
    /// count as progress and reset the stall counter (M-5).
    ///
    /// The bug this closes: an attempt that added *any* bytes reset the stall
    /// counter, so a host dribbling one byte per connection resets it forever —
    /// an unbounded loop that spins up a fresh OS thread and tokio runtime each
    /// iteration. Charging a below-threshold attempt as a stall makes a trickle
    /// terminate through `max_stall_retries` instead. A real connection delivers
    /// far more than this per fetch before it drops, so a legitimate resume is
    /// never charged; the count still tolerates `max_stall_retries` short reads
    /// in a row before giving up. An attempt that *completes* the file is exempt —
    /// a final sub-threshold tail is not a stall.
    pub min_progress_bytes: u64,
    /// A hard ceiling on fetch attempts within one `download_once`, regardless of
    /// progress (M-5). The principled bound is `min_progress_bytes`; this is a
    /// backstop that guarantees termination even against an adversary that stays
    /// exactly at the threshold. Set well above any healthy transfer's needs.
    pub max_total_attempts: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_checksum_retries: 2,
            max_stall_retries: 5,
            // A single real fetch delivers kilobytes-to-gigabytes before it ends
            // or drops; 4 KiB is comfortably below that and comfortably above a
            // pathological trickle.
            min_progress_bytes: 4096,
            // Even an 18 GiB artifact resumed in 4 KiB increments (the smallest
            // increment that still counts as progress) needs far fewer than this;
            // anything hitting it is not a transfer, it is an attack.
            max_total_attempts: 100_000,
        }
    }
}

/// Orchestrates a resumable, verified download over a [`RangeFetcher`].
pub struct Downloader<'a> {
    fetcher: &'a dyn RangeFetcher,
    config: DownloadConfig,
}

impl<'a> Downloader<'a> {
    /// A downloader over `fetcher` with default tuning.
    pub fn new(fetcher: &'a dyn RangeFetcher) -> Self {
        Self {
            fetcher,
            config: DownloadConfig::default(),
        }
    }

    /// A downloader over `fetcher` with explicit tuning.
    pub fn with_config(fetcher: &'a dyn RangeFetcher, config: DownloadConfig) -> Self {
        Self { fetcher, config }
    }

    /// Fetch `model` to `dest`, resuming any partial file, verifying its SHA-256,
    /// and reporting progress through `on_event`.
    ///
    /// # Errors
    /// Returns a [`DownloadError`] if the transport stalls, the checksum keeps
    /// mismatching, the source oversends, or a filesystem error occurs.
    pub fn fetch(
        &self,
        model: &ModelEntry,
        dest: &Path,
        on_event: &mut dyn FnMut(LifecycleEvent),
    ) -> Result<(), DownloadError> {
        let attempts = self.config.max_checksum_retries + 1;
        for attempt in 1..=attempts {
            self.download_once(model, dest, on_event)?;

            let actual = hash::sha256_file(dest)?;
            if actual == model.sha256 {
                return Ok(());
            }

            // Corrupt download: discard and (if attempts remain) re-fetch clean.
            std::fs::remove_file(dest).ok();
            if attempt == attempts {
                return Err(DownloadError::Checksum {
                    expected: model.sha256.clone(),
                    actual,
                    attempts,
                });
            }
        }
        // Unreachable: the loop returns on the final attempt.
        unreachable!("download retry loop always returns on the final attempt")
    }

    /// Fetch bytes until `dest` reaches `model.size_bytes`, resuming across
    /// transport interruptions. Does not verify the checksum.
    fn download_once(
        &self,
        model: &ModelEntry,
        dest: &Path,
        on_event: &mut dyn FnMut(LifecycleEvent),
    ) -> Result<(), DownloadError> {
        let mut written = current_len(dest)?;
        // A partial file longer than expected is corrupt; start over.
        if written > model.size_bytes {
            std::fs::remove_file(dest).ok();
            written = 0;
        }

        let mut stalls = 0u32;
        let mut total_attempts = 0u32;
        while written < model.size_bytes {
            // M-5: a hard ceiling on attempts, checked before the attempt so no
            // pathological source can spin here unbounded.
            total_attempts += 1;
            if total_attempts > self.config.max_total_attempts {
                return Err(DownloadError::Stalled { attempts: stalls });
            }

            let before = written;
            let mut file = open_partial(dest)?;

            let result = self.fetcher.fetch(&model.url, written, &mut |chunk| {
                file.write_all(chunk)?;
                written += chunk.len() as u64;
                if written > model.size_bytes {
                    return Err(DownloadError::Oversized {
                        expected: model.size_bytes,
                        actual: written,
                    });
                }
                on_event(LifecycleEvent::Download {
                    model_id: model.name.clone(),
                    downloaded_bytes: written,
                    total_bytes: Some(model.size_bytes),
                });
                Ok(())
            });
            file.flush()?;
            drop(file);

            // A `Result` this loop can inspect: only a resumable transport
            // interruption may continue; a permanent error stops immediately.
            match result {
                Ok(_total) => {}
                Err(err) if err.is_resumable() => {
                    // Loop: resume from the durably-written offset — after the
                    // stall accounting below.
                }
                Err(err) => return Err(err),
            }

            // M-5: reset the stall counter only on *meaningful* progress. An
            // attempt that completed the file needs no accounting (the `while`
            // exits); one that ended still short must have added at least
            // `min_progress_bytes`, or it is charged as a stall — a host trickling
            // a byte at a time trips `max_stall_retries` instead of looping
            // forever, even though every one of those bytes was technically
            // "progress".
            if written < model.size_bytes {
                if written.saturating_sub(before) < self.config.min_progress_bytes {
                    stalls += 1;
                    if stalls > self.config.max_stall_retries {
                        return Err(DownloadError::Stalled { attempts: stalls });
                    }
                } else {
                    stalls = 0;
                }
            }
        }
        Ok(())
    }
}

/// Open the partial-download file for append, refusing to follow a symlink.
///
/// The `.part` path is predictable, so a hostile symlink planted there would
/// otherwise turn a resumed download into a write to an arbitrary file. On Unix
/// the open carries `O_NOFOLLOW`, which fails (`ELOOP`) rather than following a
/// symlink at the final path component; a real regular `.part` opens and resumes
/// as before. This is defence in depth behind the daemon-owned `0700` weights
/// directory (REQ-547 M-11), which already denies another user the ability to
/// plant the symlink in the first place.
fn open_partial(dest: &Path) -> Result<std::fs::File, DownloadError> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(opts.open(dest)?)
}

/// Current length of `path`, or `0` if it does not exist yet.
fn current_len(path: &Path) -> Result<u64, DownloadError> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ModelEntry, TierBand};
    use std::cell::Cell;

    /// A 40-hex stand-in commit SHA, so the fixture satisfies the BR-15
    /// revision-pinning invariant that `Catalog::validate` enforces.
    const TEST_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

    fn model_for(data: &[u8]) -> ModelEntry {
        ModelEntry {
            name: "test-model".to_owned(),
            url: format!("https://example.test/acme/models/resolve/{TEST_REVISION}/model.gguf"),
            revision: TEST_REVISION.to_owned(),
            sha256: hash::sha256_hex(data),
            size_bytes: data.len() as u64,
            ram_floor_bytes: 0,
            band: TierBand::Small,
        }
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // Unique per test + process to avoid cross-test collisions.
        p.push(format!("teton-dl-{tag}-{}.part", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Streams the whole resource from `offset` in one shot.
    struct WholeFetcher {
        data: Vec<u8>,
    }
    impl RangeFetcher for WholeFetcher {
        fn fetch(
            &self,
            _url: &str,
            offset: u64,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            let start = offset as usize;
            if start < self.data.len() {
                sink(&self.data[start..])?;
            }
            Ok(self.data.len() as u64)
        }
    }

    /// Streams `cutoff` bytes from `offset` then interrupts, once; thereafter
    /// streams to the end. Models a dropped connection mid-transfer.
    struct FlakyFetcher {
        data: Vec<u8>,
        cutoff: usize,
        interrupted: Cell<bool>,
    }
    impl RangeFetcher for FlakyFetcher {
        fn fetch(
            &self,
            _url: &str,
            offset: u64,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            let start = offset as usize;
            if !self.interrupted.get() {
                self.interrupted.set(true);
                let end = self.cutoff.min(self.data.len());
                if start < end {
                    sink(&self.data[start..end])?;
                }
                return Err(DownloadError::Transport(
                    "simulated connection drop".to_owned(),
                ));
            }
            if start < self.data.len() {
                sink(&self.data[start..])?;
            }
            Ok(self.data.len() as u64)
        }
    }

    /// Serves `corrupt` bytes on the first full download and `good` bytes after,
    /// exercising the checksum-mismatch discard-and-refetch path.
    struct CorruptThenGoodFetcher {
        good: Vec<u8>,
        corrupt: Vec<u8>,
        calls: Cell<u32>,
    }
    impl RangeFetcher for CorruptThenGoodFetcher {
        fn fetch(
            &self,
            _url: &str,
            offset: u64,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
        ) -> Result<u64, DownloadError> {
            let first = self.calls.get() == 0;
            self.calls.set(self.calls.get() + 1);
            let data = if first { &self.corrupt } else { &self.good };
            let start = offset as usize;
            if start < data.len() {
                sink(&data[start..])?;
            }
            Ok(data.len() as u64)
        }
    }

    #[test]
    fn downloads_and_verifies_checksum() {
        let data: Vec<u8> = (0u8..251).cycle().take(4096).collect();
        let model = model_for(&data);
        let fetcher = WholeFetcher { data: data.clone() };
        let dest = temp_path("whole");

        let mut events = Vec::new();
        Downloader::new(&fetcher)
            .fetch(&model, &dest, &mut |e| events.push(e))
            .expect("download succeeds");

        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(events
            .iter()
            .any(|e| matches!(e, LifecycleEvent::Download { .. })));
        // The final progress event reports the full size.
        match events.last().unwrap() {
            LifecycleEvent::Download {
                downloaded_bytes,
                total_bytes,
                ..
            } => {
                assert_eq!(*downloaded_bytes, data.len() as u64);
                assert_eq!(*total_bytes, Some(data.len() as u64));
            }
            other => panic!("expected a Download event last, got {other:?}"),
        }
        std::fs::remove_file(&dest).ok();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_symlinked_partial() {
        // M-11: a symlink planted at the predictable `.part` path must not
        // redirect the write. `O_NOFOLLOW` makes the open fail rather than follow
        // it, and the target file is left untouched.
        let data: Vec<u8> = (0u8..251).cycle().take(4096).collect();
        let model = model_for(&data);
        let dest = temp_path("symlink-part");
        let target = temp_path("symlink-target");
        std::fs::write(&target, b"do-not-clobber").unwrap();
        let _ = std::fs::remove_file(&dest);
        std::os::unix::fs::symlink(&target, &dest).expect("plant a symlink at the .part path");

        let fetcher = WholeFetcher { data };
        let result = Downloader::new(&fetcher).fetch(&model, &dest, &mut |_| {});
        assert!(result.is_err(), "a symlinked .part must be refused");
        // The symlink's target is untouched — the write never followed the link.
        assert_eq!(std::fs::read(&target).unwrap(), b"do-not-clobber");

        std::fs::remove_file(&dest).ok();
        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn resumes_after_an_interruption() {
        let data: Vec<u8> = (0u8..97).cycle().take(3000).collect();
        let model = model_for(&data);
        let fetcher = FlakyFetcher {
            data: data.clone(),
            cutoff: 1200,
            interrupted: Cell::new(false),
        };
        let dest = temp_path("resume");

        let mut max_seen = 0u64;
        Downloader::new(&fetcher)
            .fetch(&model, &dest, &mut |e| {
                if let LifecycleEvent::Download {
                    downloaded_bytes, ..
                } = e
                {
                    max_seen = max_seen.max(downloaded_bytes);
                }
            })
            .expect("download resumes and completes");

        // The fetcher was interrupted once and the second call resumed.
        assert!(fetcher.interrupted.get());
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(max_seen, data.len() as u64);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn resume_continues_a_preexisting_partial_file() {
        let data: Vec<u8> = (0u8..131).cycle().take(2500).collect();
        let model = model_for(&data);
        let dest = temp_path("preexisting");
        // Simulate a prior run that wrote the first 1000 bytes.
        std::fs::write(&dest, &data[..1000]).unwrap();

        let fetcher = WholeFetcher { data: data.clone() };
        let mut first_offset_seen = None;
        Downloader::new(&fetcher)
            .fetch(&model, &dest, &mut |e| {
                if let LifecycleEvent::Download {
                    downloaded_bytes, ..
                } = e
                {
                    first_offset_seen.get_or_insert(downloaded_bytes);
                }
            })
            .expect("download completes from the partial file");

        // The very first progress report is already past the pre-existing bytes,
        // proving the download resumed rather than restarted.
        assert!(first_offset_seen.unwrap() > 1000);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn checksum_mismatch_discards_and_refetches() {
        let good: Vec<u8> = (0u8..211).cycle().take(2048).collect();
        let corrupt: Vec<u8> = std::iter::repeat_n(0xFFu8, good.len()).collect();
        let model = model_for(&good); // sha is of the good data
        let fetcher = CorruptThenGoodFetcher {
            good: good.clone(),
            corrupt,
            calls: Cell::new(0),
        };
        let dest = temp_path("checksum");

        Downloader::new(&fetcher)
            .fetch(&model, &dest, &mut |_| {})
            .expect("second fetch produces a matching checksum");

        assert_eq!(std::fs::read(&dest).unwrap(), good);
        // First (corrupt) + second (good) full downloads.
        assert_eq!(fetcher.calls.get(), 2);
        std::fs::remove_file(&dest).ok();
    }

    /// A host that hands back a single byte per connection makes progress on
    /// every attempt — the exact case that used to reset the stall counter and
    /// loop forever (M-5). It must terminate.
    #[test]
    fn a_trickle_host_terminates_rather_than_looping() {
        /// Delivers exactly one byte per `fetch`, from the requested offset, and
        /// counts how many times it was asked. Never completes a real artifact.
        struct TrickleFetcher {
            data: Vec<u8>,
            calls: Cell<u32>,
        }
        impl RangeFetcher for TrickleFetcher {
            fn fetch(
                &self,
                _url: &str,
                offset: u64,
                sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
            ) -> Result<u64, DownloadError> {
                self.calls.set(self.calls.get() + 1);
                let start = offset as usize;
                if start < self.data.len() {
                    sink(&self.data[start..start + 1])?;
                }
                Ok(self.data.len() as u64)
            }
        }

        // A large-enough artifact that a byte-at-a-time transfer would take
        // effectively forever to finish honestly.
        let data: Vec<u8> = (0u8..=250).cycle().take(1_000_000).collect();
        let model = model_for(&data);
        let fetcher = TrickleFetcher {
            data,
            calls: Cell::new(0),
        };
        let dest = temp_path("trickle");

        let err = Downloader::new(&fetcher)
            .fetch(&model, &dest, &mut |_| {})
            .expect_err("a trickle host must not download a million bytes one at a time");
        assert!(
            matches!(err, DownloadError::Stalled { .. }),
            "expected the trickle to be declared stalled, got {err:?}"
        );
        // It gave up after a handful of sub-threshold attempts, not after a
        // million: `max_stall_retries` (5) plus the first attempt.
        assert!(
            fetcher.calls.get() <= DownloadConfig::default().max_stall_retries + 2,
            "the trickle was allowed {} attempts before terminating",
            fetcher.calls.get()
        );
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn persistent_checksum_mismatch_errors_out() {
        let good: Vec<u8> = (0u8..251).cycle().take(1024).collect();
        // A fetcher that always serves corrupt bytes.
        struct AlwaysCorrupt {
            len: usize,
        }
        impl RangeFetcher for AlwaysCorrupt {
            fn fetch(
                &self,
                _url: &str,
                offset: u64,
                sink: &mut dyn FnMut(&[u8]) -> Result<(), DownloadError>,
            ) -> Result<u64, DownloadError> {
                let bytes = vec![0u8; self.len];
                let start = offset as usize;
                if start < bytes.len() {
                    sink(&bytes[start..])?;
                }
                Ok(self.len as u64)
            }
        }
        let model = model_for(&good);
        let fetcher = AlwaysCorrupt { len: good.len() };
        let dest = temp_path("always-corrupt");

        let err = Downloader::with_config(
            &fetcher,
            DownloadConfig {
                max_checksum_retries: 1,
                ..DownloadConfig::default()
            },
        )
        .fetch(&model, &dest, &mut |_| {})
        .unwrap_err();

        match err {
            DownloadError::Checksum { attempts, .. } => assert_eq!(attempts, 2),
            other => panic!("expected Checksum error, got {other:?}"),
        }
        // The corrupt file is discarded, not left behind.
        assert!(!dest.exists());
    }
}
