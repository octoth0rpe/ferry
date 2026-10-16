//! Watching the config file and certificates for changes.
//!
//! Directories are watched rather than files, because editors save by
//! writing a new file and renaming it over the old one, and certificate
//! tools (certbot, Kubernetes secrets) swap symlinks. After any event the
//! watched files' metadata is compared with what it was, so only real
//! changes trigger a reload.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

use crate::config::Config;

/// Wait this long after an event before checking, so a burst of events
/// from one save turns into one reload.
const DEBOUNCE: Duration = Duration::from_millis(250);

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
    watcher: RecommendedWatcher,
    dirs: BTreeSet<PathBuf>,
    files: HashMap<PathBuf, Fingerprint>,
    events: mpsc::Receiver<()>,
}

impl FileWatcher {
    pub fn new() -> notify::Result<FileWatcher> {
        let (tx, events) = mpsc::channel(1);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                // Full means a check is already pending; that's enough.
                let _ = tx.try_send(());
            }
        })?;
        Ok(FileWatcher {
            watcher,
            dirs: BTreeSet::new(),
            files: HashMap::new(),
            events,
        })
    }

    /// Replaces the set of watched files, remembering their current state.
    pub fn set_files(&mut self, files: BTreeSet<PathBuf>) {
        let dirs: BTreeSet<PathBuf> = files
            .iter()
            .filter_map(|f| f.parent().map(Path::to_path_buf))
            .collect();
        for dir in self.dirs.difference(&dirs) {
            let _ = self.watcher.unwatch(dir);
        }
        for dir in dirs.difference(&self.dirs) {
            if let Err(e) = self.watcher.watch(dir, RecursiveMode::NonRecursive) {
                tracing::warn!("can't watch {} for changes: {e}", dir.display());
            }
        }
        self.dirs = dirs;
        self.files = files
            .into_iter()
            .map(|f| (f.clone(), fingerprint(&f)))
            .collect();
    }

    /// Waits until a watched file has changed, and returns the changed files.
    pub async fn changed(&mut self) -> Vec<PathBuf> {
        loop {
            if self.events.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(DEBOUNCE).await;
            while self.events.try_recv().is_ok() {}

            let changed: Vec<PathBuf> = self
                .files
                .iter_mut()
                .filter_map(|(path, old)| {
                    let new = fingerprint(path);
                    (new != *old).then(|| {
                        *old = new;
                        path.clone()
                    })
                })
                .collect();
            if !changed.is_empty() {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notices_rename_over_and_ignores_unrelated_files() {
        let dir = std::env::temp_dir().join(format!("ferry-watch-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let file = dir.join("ferry.toml");
        std::fs::write(&file, "a").unwrap();

        let mut w = FileWatcher::new().unwrap();
        w.set_files([file.clone()].into());
        tokio::time::sleep(Duration::from_millis(100)).await;

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
}
