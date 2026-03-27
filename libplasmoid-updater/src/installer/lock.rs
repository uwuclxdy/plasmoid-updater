// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs::{File, OpenOptions};

use nix::fcntl::{Flock, FlockArg};

use crate::{Error, Result, paths};

/// RAII guard that holds an exclusive flock on the update lock file.
///
/// While this guard is alive, no other `plasmoid-updater` instance can
/// acquire the lock. The lock is released automatically when dropped
/// (or if the process crashes, the kernel releases it).
pub(crate) struct UpdateLock {
    _flock: Flock<File>,
}

impl UpdateLock {
    /// Attempts to acquire an exclusive, non-blocking lock.
    ///
    /// Returns `Err(Error::AlreadyRunning)` if another instance holds the lock.
    pub(crate) fn acquire() -> Result<Self> {
        let lock_path = paths::runtime_dir().join("plasmoid-updater.lock");

        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::other(format!("failed to create lock directory: {e}"))
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::other(format!("failed to open lock file: {e}")))?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => {
                log::debug!(target: "lock", "acquired update lock at {}", lock_path.display());
                Ok(Self { _flock: flock })
            }
            Err((_, errno)) if errno == nix::errno::Errno::EWOULDBLOCK => {
                log::debug!(target: "lock", "another instance is running");
                Err(Error::AlreadyRunning)
            }
            Err((_, errno)) => {
                Err(Error::other(format!("failed to acquire update lock: {errno}")))
            }
        }
    }
}
