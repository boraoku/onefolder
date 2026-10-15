//! Folders on a OneDrive account for the browser, through rclone on the
//! tokio runtime the job form runs beside.
//!
//! Opening a folder spawns one task: `rclone lsjson` of the folder (its
//! folders and files, one level), then `rclone size --json` of each
//! subfolder, two at a time on a `JoinSet`. Opening the next folder, or
//! dropping the source when the browser closes, aborts that task; the
//! `JoinSet` it owns aborts the size runs with it, and dropping their
//! futures kills the rclone processes (every capture in `rclone::exec` is
//! `kill_on_drop`). `rclone about` runs once per browser for the free
//! space, and is aborted the same way.

use std::sync::mpsc::Sender;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::{AbortHandle, JoinSet};

use super::{ListFailure, Listing, Size, SizeCache, Source, Update};
use crate::rclone::detect::Rclone;
use crate::rclone::listing::{self, ListError};
use crate::rclone::remotes;

/// `rclone size` runs at once.
const AT_ONCE: usize = 2;

/// How long `rclone about` may take before the free space is left out.
const ABOUT_LIMIT: Duration = Duration::from_secs(30);

/// `remote:folder`, the drive root for an empty folder.
pub fn target(remote: &str, folder: &str) -> String {
    listing::join(&format!("{remote}:"), folder)
}

/// Lists and measures folders on one OneDrive account.
pub struct RemoteSource {
    handle: Handle,
    rclone: Rclone,
    /// The account's rclone remote, without the colon.
    remote: String,
    tx: Sender<Update>,
    cache: SizeCache,
    task: Option<AbortHandle>,
    about: Option<AbortHandle>,
}

impl RemoteSource {
    /// A source for `remote`; asks for the drive's free space at once.
    pub fn new(
        handle: Handle,
        rclone: Rclone,
        remote: String,
        tx: Sender<Update>,
        cache: SizeCache,
    ) -> RemoteSource {
        let about = {
            let (rclone, remote, tx) = (rclone.clone(), remote.clone(), tx.clone());
            handle
                .spawn(async move {
                    if let Ok(quota) = remotes::about(&rclone, &remote, ABOUT_LIMIT).await {
                        if let Some(free) = quota.free {
                            let _ = tx.send(Update::Free(free));
                        }
                    }
                })
                .abort_handle()
        };
        RemoteSource {
            handle,
            rclone,
            remote,
            tx,
            cache,
            task: None,
            about: Some(about),
        }
    }
}

impl Source for RemoteSource {
    fn open(&mut self, folder: &str, generation: u64) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let rclone = self.rclone.clone();
        let remote = self.remote.clone();
        let tx = self.tx.clone();
        let cache = self.cache.clone();
        let folder = folder.to_string();
        let task = self.handle.spawn(async move {
            let here = target(&remote, &folder);
            let entries = match listing::top_level(&rclone, &here, false).await {
                Ok(entries) => entries,
                Err(e) => {
                    let failure = ListFailure {
                        missing: matches!(e, ListError::NotFound { .. }),
                        message: e.to_string(),
                    };
                    let _ = tx.send(Update::Listed {
                        generation,
                        result: Err(failure),
                    });
                    return;
                }
            };
            let mut listing = Listing::default();
            for entry in entries {
                // lsjson gives the path relative to the listed folder.
                let name = entry
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if entry.is_dir {
                    listing.dirs.push(name);
                } else {
                    listing.files += 1;
                    listing.file_bytes += u64::try_from(entry.size).unwrap_or(0);
                }
            }
            let mut order = listing.dirs.clone();
            order.sort_by_key(|d| d.starts_with('.'));
            if tx
                .send(Update::Listed {
                    generation,
                    result: Ok(listing),
                })
                .is_err()
            {
                return;
            }
            let mut waiting = Vec::new();
            for name in order {
                let key = target(&remote, &super::join(&folder, &name));
                let cached = cache.lock().ok().and_then(|c| c.get(&key).copied());
                match cached {
                    Some((bytes, partial)) => {
                        let _ = tx.send(Update::Size {
                            generation,
                            name,
                            size: Size::Done { bytes, partial },
                        });
                    }
                    None => waiting.push((name, key)),
                }
            }
            // Two at a time; dropping the set (an aborted task) aborts the rest.
            let mut running = JoinSet::new();
            let mut waiting = waiting.into_iter();
            loop {
                while running.len() < AT_ONCE {
                    let Some((name, key)) = waiting.next() else {
                        break;
                    };
                    let (rclone, tx, cache) = (rclone.clone(), tx.clone(), cache.clone());
                    running.spawn(async move {
                        let size = match listing::size(&rclone, &key, &[]).await {
                            Ok(s) => {
                                let bytes = u64::try_from(s.bytes).unwrap_or(0);
                                if let Ok(mut c) = cache.lock() {
                                    c.insert(key, (bytes, false));
                                }
                                Size::Done {
                                    bytes,
                                    partial: false,
                                }
                            }
                            Err(_) => Size::Failed,
                        };
                        let _ = tx.send(Update::Size {
                            generation,
                            name,
                            size,
                        });
                    });
                }
                if running.join_next().await.is_none() {
                    break;
                }
            }
        });
        self.task = Some(task.abort_handle());
    }

    fn forget(&mut self, folder: &str) {
        let here = target(&self.remote, folder);
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|k, _| {
                let parent = k
                    .rsplit_once('/')
                    .map_or_else(|| format!("{}:", self.remote), |(p, _)| p.to_string());
                parent != here
            });
        }
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        for task in [self.task.take(), self.about.take()].into_iter().flatten() {
            task.abort();
        }
    }
}
