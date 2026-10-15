//! Folders on this Mac for the browser: a listing thread per folder
//! opened, and a pool of at most [`WORKERS`] threads that measure the
//! subfolders with `std::fs`.
//!
//! Each opened folder gets a fresh cancel flag (an `AtomicBool`); opening
//! the next folder, or dropping the source when the browser closes, sets
//! the old one, and the listing thread and the walkers check it between
//! directories (and every few hundred entries), so a walk of a large tree
//! stops within moments. The queue is emptied at the same time. A walker
//! reports its running total at most ten times a second and its final
//! size once; finished sizes go into the [`SizeCache`], so going back up
//! is instant. A subfolder that is a mount point (another drive, such as
//! each one under `/Volumes`) is never walked: its used space comes from
//! `statvfs` instead.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{ListFailure, Listing, Size, SizeCache, Source, Update};
use crate::volume;

/// At most this many folders are measured at once.
pub const WORKERS: usize = 4;

/// How often a walker reports its running total.
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// One folder to measure.
struct Job {
    generation: u64,
    name: String,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
    closed: AtomicBool,
}

/// Lists and measures local folders.
pub struct LocalSource {
    tx: Sender<Update>,
    cache: SizeCache,
    cancel: Arc<AtomicBool>,
    queue: Arc<Queue>,
    workers: usize,
}

impl LocalSource {
    pub fn new(tx: Sender<Update>, cache: SizeCache) -> LocalSource {
        LocalSource {
            tx,
            cache,
            cancel: Arc::new(AtomicBool::new(false)),
            queue: Arc::new(Queue::default()),
            workers: 0,
        }
    }

    /// The cancel flag of the folder opened last (for tests).
    #[cfg(test)]
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    fn start_workers(&mut self) {
        while self.workers < WORKERS {
            let queue = self.queue.clone();
            let tx = self.tx.clone();
            let cache = self.cache.clone();
            let spawned = std::thread::Builder::new()
                .name("onefolder-size".into())
                .spawn(move || worker(&queue, &tx, &cache));
            if spawned.is_err() {
                break;
            }
            self.workers += 1;
        }
    }
}

impl Source for LocalSource {
    fn open(&mut self, folder: &str, generation: u64) {
        self.cancel.store(true, Ordering::SeqCst);
        self.cancel = Arc::new(AtomicBool::new(false));
        if let Ok(mut jobs) = self.queue.jobs.lock() {
            jobs.clear();
        }
        self.start_workers();
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        let cache = self.cache.clone();
        let queue = self.queue.clone();
        let folder = PathBuf::from(folder);
        // Listing a folder of many files, or one on a slow drive, takes a
        // while: never on the key loop.
        let _ = std::thread::Builder::new()
            .name("onefolder-list".into())
            .spawn(move || {
                let result = list(&folder);
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                let (dirs, mounts) = match &result {
                    Ok((listing, mounts)) => (listing.dirs.clone(), mounts.clone()),
                    Err(_) => (Vec::new(), Vec::new()),
                };
                let result = result.map(|(listing, _)| listing);
                if tx.send(Update::Listed { generation, result }).is_err() {
                    return;
                }
                // A drive mounted here: its used space, never a walk of it.
                for name in &mounts {
                    let size = match volume::space(&folder.join(name)) {
                        Ok(space) => Size::Volume { used: space.used },
                        Err(_) => Size::Failed,
                    };
                    let _ = tx.send(Update::Size {
                        generation,
                        name: name.clone(),
                        size,
                    });
                }
                // Visible folders first, hidden ones after.
                let mut order: Vec<String> =
                    dirs.into_iter().filter(|d| !mounts.contains(d)).collect();
                order.sort_by_key(|d| d.starts_with('.'));
                let mut fresh = Vec::new();
                for name in order {
                    let path = folder.join(&name);
                    let key = path.display().to_string();
                    let cached = cache.lock().ok().and_then(|c| c.get(&key).copied());
                    match cached {
                        Some((bytes, partial)) => {
                            let _ = tx.send(Update::Size {
                                generation,
                                name,
                                size: Size::Done { bytes, partial },
                            });
                        }
                        None => fresh.push(Job {
                            generation,
                            name,
                            path,
                            cancel: cancel.clone(),
                        }),
                    }
                }
                if let Ok(mut jobs) = queue.jobs.lock() {
                    if !cancel.load(Ordering::SeqCst) {
                        jobs.extend(fresh);
                    }
                }
                queue.ready.notify_all();
            });
    }

    fn forget(&mut self, folder: &str) {
        let prefix = Path::new(folder);
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|k, _| Path::new(k).parent() != Some(prefix));
        }
    }
}

impl Drop for LocalSource {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.queue.closed.store(true, Ordering::SeqCst);
        if let Ok(mut jobs) = self.queue.jobs.lock() {
            jobs.clear();
        }
        self.queue.ready.notify_all();
    }
}

/// A walker: takes folders off the queue until the source is dropped.
fn worker(queue: &Queue, tx: &Sender<Update>, cache: &SizeCache) {
    loop {
        let job = {
            let Ok(mut jobs) = queue.jobs.lock() else {
                return;
            };
            loop {
                if queue.closed.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = match queue.ready.wait(jobs) {
                    Ok(jobs) => jobs,
                    Err(_) => return,
                };
            }
        };
        if job.cancel.load(Ordering::SeqCst) {
            continue;
        }
        let send = |size| {
            let _ = tx.send(Update::Size {
                generation: job.generation,
                name: job.name.clone(),
                size,
            });
        };
        let mut report = |bytes, partial| send(Size::Measuring { bytes, partial });
        if let Some((bytes, partial)) = walk(&job.path, &job.cancel, &mut report) {
            if let Ok(mut c) = cache.lock() {
                c.insert(job.path.display().to_string(), (bytes, partial));
            }
            send(Size::Done { bytes, partial });
        }
    }
}

/// One entry of a folder as [`sort_out`] needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub dir: bool,
    pub file: bool,
    /// The file's size.
    pub len: u64,
    /// The device the entry is on (`st_dev`).
    pub device: u64,
}

/// Splits a folder's entries into the listing (subfolders, and the count
/// and bytes of the files) and the subfolders that are mount points: on a
/// device other than the folder's own (`parent_device`), such as each
/// drive under `/Volumes`. Those are not walked; their used space is shown
/// instead.
pub fn sort_out(found: Vec<Found>, parent_device: u64) -> (Listing, Vec<String>) {
    let mut listing = Listing::default();
    let mut mounts = Vec::new();
    for f in found {
        if f.dir {
            if f.device != parent_device {
                mounts.push(f.name.clone());
            }
            listing.dirs.push(f.name);
        } else if f.file {
            listing.files += 1;
            listing.file_bytes += f.len;
        }
    }
    (listing, mounts)
}

/// The subfolders of `folder` (real folders, not symbolic links to them),
/// the files directly in it and the free space on its drive, and which
/// subfolders are mount points.
pub fn list(folder: &Path) -> Result<(Listing, Vec<String>), ListFailure> {
    let failure = |e: std::io::Error| ListFailure {
        missing: e.kind() == std::io::ErrorKind::NotFound,
        message: e.to_string(),
    };
    let parent_device = fs::metadata(folder).map(|m| device(&m)).map_err(failure)?;
    let entries = fs::read_dir(folder).map_err(failure)?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        let meta = entry.metadata().ok();
        found.push(Found {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: kind.is_dir(),
            file: kind.is_file(),
            len: meta.as_ref().map_or(0, |m| m.len()),
            device: meta.as_ref().map_or(parent_device, device),
        });
    }
    let (mut listing, mounts) = sort_out(found, parent_device);
    listing.free = volume::space(folder).ok().map(|s| s.free);
    Ok((listing, mounts))
}

/// The device a file or folder is on, to keep a walk on one file system.
#[cfg(unix)]
fn device(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}

#[cfg(not(unix))]
fn device(_m: &fs::Metadata) -> u64 {
    0
}

/// The apparent size of the files under `root`: symbolic links are not
/// followed and other file systems mounted inside it are not entered (as
/// `du -x`), so `/` does not count every drive under `/Volumes`. What
/// cannot be read is left out and makes the result partial. Reports the
/// running total through `report` at most every [`REPORT_EVERY`]. `None`
/// when `cancel` was set before the walk finished.
pub fn walk(
    root: &Path,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(u64, bool),
) -> Option<(u64, bool)> {
    let root_device = match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => device(&m),
        Ok(_) => return Some((0, false)),
        Err(_) => return Some((0, true)),
    };
    let mut total = 0u64;
    let mut partial = false;
    let mut stack = vec![root.to_path_buf()];
    let mut last = Instant::now();
    let mut seen = 0u32;
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            partial = true;
            continue;
        };
        for entry in entries {
            seen = seen.wrapping_add(1);
            if seen.is_multiple_of(512) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Ok(entry) = entry else {
                partial = true;
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                partial = true;
                continue;
            };
            if kind.is_dir() {
                match entry.metadata() {
                    Ok(m) if device(&m) == root_device => stack.push(entry.path()),
                    Ok(_) => {}
                    Err(_) => partial = true,
                }
            } else if kind.is_file() {
                match entry.metadata() {
                    Ok(m) => total += m.len(),
                    Err(_) => partial = true,
                }
            }
        }
        if last.elapsed() >= REPORT_EVERY {
            report(total, partial);
            last = Instant::now();
        }
    }
    Some((total, partial))
}
