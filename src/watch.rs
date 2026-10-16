//! Watching the config file and certificates for changes.
//!
//! The files are polled: once a second each one's metadata is compared with
//! what it was. Following symlinks and comparing inodes means saves that
//! rename a new file into place (editors) and symlink swaps (certbot,
//! Kubernetes secrets) are both noticed, and only real changes trigger a
//! reload.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::{Interval, MissedTickBehavior};

use crate::config::Config;

/// How often the files are checked.
const POLL: Duration = Duration::from_secs(1);
/// After noticing a change, wait this long and look again, so a cert and key
/// written one after the other turn into one reload.
const SETTLE: Duration = Duration::from_millis(250);

/// Identifies a version of a file: device, inode, mtime and size, following
/// symlinks. `None` if the file doesn't exist.
type Fingerprint = Option<(u64, u64, i64, i64, u64)>;

fn fingerprint(path: &Path) -> Fingerprint {
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino(), m.mtime(), m.mtime_nsec(), m.size()))
}

/// The files whose changes should reload the config: the config file and
/// every certificate and key it uses.
pub fn files(config: &Config) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    files.extend(config.config_path.clone());
    let cert_keys = config
        .cert_key
        .iter()
        .chain(config.listeners.iter().filter_map(|l| l.cert_key.as_ref()));
    for ck in cert_keys {
        files.insert(ck.cert.clone());
        files.insert(ck.key.clone());
    }
    files
}

pub struct FileWatcher {
    files: HashMap<PathBuf, Fingerprint>,
    tick: Interval,
    /// Changes seen but not yet returned. Kept here rather than in
    /// `changed`, so they survive it being cancelled by `select!`.
    pending: Vec<PathBuf>,
}

impl FileWatcher {
    pub fn new() -> FileWatcher {
        FileWatcher::with_interval(POLL)
    }

    fn with_interval(period: Duration) -> FileWatcher {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        FileWatcher {
            files: HashMap::new(),
            tick,
            pending: Vec::new(),
        }
    }

    /// Replaces the set of watched files, remembering their current state.
    pub fn set_files(&mut self, files: BTreeSet<PathBuf>) {
        self.files = files
            .into_iter()
            .map(|f| (f.clone(), fingerprint(&f)))
            .collect();
        self.pending.clear();
    }

    /// Waits until a watched file has changed, and returns the changed files.
    pub async fn changed(&mut self) -> Vec<PathBuf> {
        while self.pending.is_empty() {
            self.tick.tick().await;
            self.pending = self.refresh();
        }
        tokio::time::sleep(SETTLE).await;
        let mut changed = std::mem::take(&mut self.pending);
        changed.extend(self.refresh());
        changed.sort();
        changed.dedup();
        changed
    }

    /// Updates the stored fingerprints, returning the files that changed.
    fn refresh(&mut self) -> Vec<PathBuf> {
        self.files
            .iter_mut()
            .filter_map(|(path, old)| {
                let new = fingerprint(path);
                (new != *old).then(|| {
                    *old = new;
                    path.clone()
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notices_rename_over_and_ignores_unrelated_files() {
        let dir = std::env::temp_dir().join(format!("ferry-watch-{}", crate::proxy::random_id(12)));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let file = dir.join("ferry.toml");
        std::fs::write(&file, "a").unwrap();

        let mut w = FileWatcher::with_interval(Duration::from_millis(100));
        w.set_files([file.clone()].into());

        std::fs::write(dir.join("unrelated"), "x").unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(800), w.changed()).await;
        assert!(quiet.is_err(), "unrelated file triggered a change");

        // Editor-style save: write a temp file and rename it over.
        std::fs::write(dir.join(".ferry.toml.swp"), "bb").unwrap();
        std::fs::rename(dir.join(".ferry.toml.swp"), &file).unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), w.changed())
            .await
            .expect("change noticed");
        assert_eq!(changed, vec![file]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn change_survives_cancellation() {
        let dir = std::env::temp_dir().join(format!("ferry-watch-{}", crate::proxy::random_id(12)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cert.pem");
        std::fs::write(&file, "a").unwrap();
        let mut w = FileWatcher::with_interval(Duration::from_millis(100));
        w.set_files([file.clone()].into());

        std::fs::write(&file, "bb").unwrap();
        // Cancelled while settling, after the change has been seen, the way
        // a signal arriving in main's select! would cancel it.
        let cancelled = tokio::time::timeout(SETTLE / 2, w.changed()).await;
        assert!(cancelled.is_err());
        assert_eq!(w.changed().await, vec![file]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
