// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs::{DirBuilder, File, OpenOptions},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
};

use nix::{
    fcntl::{Flock, FlockArg},
    unistd::{Gid, Uid},
};

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
    ///
    /// The lock sits directly in the user's cache home, not under
    /// `XDG_RUNTIME_DIR` (a desktop session and an ssh session resolve
    /// different runtime directories, so two runs that share one user would
    /// never meet and would install over the same components) and not inside
    /// `plasmoid-updater/` (an older release could leave that directory owned
    /// by root after a sudo run, which would lock the user out of every run).
    pub(crate) fn acquire() -> Result<Self> {
        let cache_home = paths::cache_home();
        let lock_path = lock_path();
        // glibc applies setegid/seteuid to every thread of the process, and no
        // other thread is touching files yet: the idle rayon workers an
        // interactive check has already started only wait for tasks.
        let _invoking_user = DelegatedIds::enter(delegated_ids(), IdCalls::PROCESS, &lock_path)?;
        create_dir_chain(&cache_home).map_err(|source| Error::Io {
            path: cache_home.clone(),
            source,
        })?;
        if let Some(dir) = prepare_backups_dir(&cache_home) {
            log::warn!(
                target: "lock",
                "cannot prepare the backup directory {}; backups will report their own failure",
                dir.display()
            );
        }

        Self::acquire_at(&lock_path)
    }

    /// The file stays on disk after the run on purpose: unlinking it at the
    /// end would let two runs that start together create and lock two
    /// different inodes for the same path, each believing it holds the lock.
    fn acquire_at(lock_path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            // The cache is the invoking user's to write, so the lock path may
            // be a symlink they placed; following it would create the link's
            // target with this run's credentials.
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(lock_path)
            .map_err(|e| Error::Io {
                path: lock_path.to_path_buf(),
                source: e,
            })?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => {
                log::debug!(target: "lock", "acquired update lock at {}", lock_path.display());
                Ok(Self { _flock: flock })
            }
            Err((_, errno)) if errno == nix::errno::Errno::EWOULDBLOCK => {
                log::debug!(target: "lock", "another instance is running");
                Err(Error::AlreadyRunning)
            }
            Err((_, errno)) => Err(Error::Io {
                path: lock_path.to_path_buf(),
                source: std::io::Error::from_raw_os_error(errno as i32),
            }),
        }
    }
}

/// The lock file every run takes. The name never moves: a release and an
/// older release must meet on the same inode to exclude each other.
fn lock_path() -> PathBuf {
    paths::cache_home().join("plasmoid-updater.lock")
}

/// Whether another process holds the update lock right now.
///
/// Probes without blocking and releases at once, so a caller that never writes
/// (`check`) can tell a live run's swap from a dead run's leftovers without
/// taking the lock itself and without creating it.
pub(crate) fn run_lock_held() -> bool {
    lock_held_at(&lock_path())
}

/// The same probe over a named lock file.
fn lock_held_at(lock_path: &Path) -> bool {
    // `flock` needs no write access, and a probe that asked for it would read
    // an unwritable lock file as free.
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(lock_path)
    else {
        // No file means no run has ever taken the lock here; the probe stays
        // read-only rather than creating one.
        return false;
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            drop(flock);
            false
        }
        Err((_, errno)) => errno == nix::errno::Errno::EWOULDBLOCK,
    }
}

/// The ids root must switch to when it runs on another user's behalf: the
/// invoking user's, as `sudo` records them.
///
/// A sudo run creates the lock in the invoking user's cache, so entries root
/// owns would lock that user out of every later run. A real root login keeps
/// root's own cache, and both ids are needed to name a user.
fn delegated_ids_for(
    euid: Uid,
    sudo_uid: Option<&str>,
    sudo_gid: Option<&str>,
) -> Option<(Uid, Gid)> {
    if !euid.is_root() {
        return None;
    }
    let uid = sudo_uid?.parse().ok()?;
    let gid = sudo_gid?.parse().ok()?;
    Some((Uid::from_raw(uid), Gid::from_raw(gid)))
}

/// The ids this run must switch to, if any.
fn delegated_ids() -> Option<(Uid, Gid)> {
    let sudo_uid = std::env::var("SUDO_UID").ok();
    let sudo_gid = std::env::var("SUDO_GID").ok();
    delegated_ids_for(Uid::effective(), sudo_uid.as_deref(), sudo_gid.as_deref())
}

/// The two effective-id calls the switch makes, behind fields so a test can
/// watch the order and the restore without root.
struct IdCalls {
    set_egid: fn(Gid) -> nix::Result<()>,
    set_euid: fn(Uid) -> nix::Result<()>,
}

impl IdCalls {
    const PROCESS: Self = Self {
        set_egid: nix::unistd::setegid,
        set_euid: nix::unistd::seteuid,
    };
}

/// Runs its scope on the invoking user's effective ids, then puts root's back.
///
/// The user can place a symlink anywhere in the path to their cache, so a
/// chmod, chown or create done as root can be steered outside it. Doing the
/// work as that user instead gives every operation the user's own rights: it
/// reaches only what the user could reach themselves, whoever the link names.
struct DelegatedIds {
    calls: IdCalls,
    switched: bool,
}

impl DelegatedIds {
    fn enter(ids: Option<(Uid, Gid)>, calls: IdCalls, lock_path: &Path) -> Result<Self> {
        let Some((uid, gid)) = ids else {
            return Ok(Self {
                calls,
                switched: false,
            });
        };
        let mut guard = Self {
            calls,
            switched: false,
        };
        // The group goes first: only while the effective uid is still root is
        // the change permitted, and a failure past it is what the drop undoes.
        if let Err(source) = (guard.calls.set_egid)(gid) {
            return Err(switch_error(lock_path, uid, gid, source));
        }
        guard.switched = true;
        if let Err(source) = (guard.calls.set_euid)(uid) {
            return Err(switch_error(lock_path, uid, gid, source));
        }
        Ok(guard)
    }
}

impl Drop for DelegatedIds {
    fn drop(&mut self) {
        if !self.switched {
            return;
        }
        // Root's uid first: that is what permits its group again.
        if let Err(e) = (self.calls.set_euid)(Uid::from_raw(0)) {
            log::error!(target: "lock", "failed to restore the effective uid 0: {e}");
        }
        if let Err(e) = (self.calls.set_egid)(Gid::from_raw(0)) {
            log::error!(target: "lock", "failed to restore the effective gid 0: {e}");
        }
    }
}

fn switch_error(lock_path: &Path, uid: Uid, gid: Gid, source: nix::errno::Errno) -> Error {
    // `Error::Io` names the path only, so the ids the switch wanted go to the
    // log where the failure is still diagnosable.
    log::error!(
        target: "lock",
        "failed to switch to uid {} and gid {}: {source}",
        uid.as_raw(),
        gid.as_raw()
    );
    Error::Io {
        path: lock_path.to_path_buf(),
        source: std::io::Error::from_raw_os_error(source as i32),
    }
}

/// Creates `dir` and every missing parent, each readable by its owner only.
///
/// An existing directory keeps its mode: it belongs to whoever created it, and
/// a delegated run must not rewrite what that user already had.
fn create_dir_chain(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut missing = Some(dir);
    let mut created: Vec<&Path> = Vec::new();
    while let Some(path) = missing {
        if path.exists() {
            break;
        }
        created.push(path);
        missing = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
    }
    for path in created.iter().rev() {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Creates the directory the run's backups land in, as the invoking user.
///
/// Returns that directory when an earlier run left one this user cannot write:
/// the run goes on and the backup step reports its own failure per component,
/// rather than a stale root-owned tree blocking every later run.
fn prepare_backups_dir(cache_home: &Path) -> Option<PathBuf> {
    let backups = cache_home.join("plasmoid-updater/backups");
    match create_dir_chain(&backups) {
        Ok(()) if writable_by_current_ids(&backups) => None,
        _ => Some(backups),
    }
}

/// Whether the current effective ids may create entries in `dir`.
///
/// A missing directory counts as writable: creating it is the caller's next
/// step and reports its own error. The mode bits decide, not `access(2)`:
/// during a delegated switch the real ids are still root's, so `access` answers
/// for root instead of for the user the run acts as.
fn writable_by_current_ids(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(metadata) = std::fs::metadata(dir) else {
        return true;
    };
    writable_by(
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
        &nix::unistd::getgroups().unwrap_or_default(),
        Uid::effective(),
        Gid::effective(),
    )
}

fn writable_by(mode: u32, owner: u32, group: u32, groups: &[Gid], euid: Uid, egid: Gid) -> bool {
    if owner == euid.as_raw() {
        mode & 0o200 != 0
    } else if group == egid.as_raw() || groups.iter().any(|g| g.as_raw() == group) {
        mode & 0o020 != 0
    } else {
        mode & 0o002 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_file_survives_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("plasmoid-updater.lock");

        let held = UpdateLock::acquire_at(&lock_path).unwrap();
        assert!(lock_path.exists());
        assert_eq!(
            mode_of(&lock_path),
            0o600,
            "the lock file is readable beyond its owner"
        );
        drop(held);

        assert!(
            lock_path.exists(),
            "a removed lock file would let two runs lock different inodes"
        );
        UpdateLock::acquire_at(&lock_path).unwrap();
    }

    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"untouched").unwrap();
        let lock_path = dir.path().join("plasmoid-updater.lock");
        std::os::unix::fs::symlink(&target, &lock_path).unwrap();

        let err = match UpdateLock::acquire_at(&lock_path) {
            Ok(_) => panic!("the symlinked lock was followed"),
            Err(e) => e,
        };

        match err {
            Error::Io { path, source } => {
                assert_eq!(path, lock_path);
                assert_eq!(source.raw_os_error(), Some(nix::libc::ELOOP), "{source}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"untouched",
            "the lock open wrote through the symlink"
        );
    }

    #[test]
    fn the_probe_reports_a_held_lock_without_taking_it() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("plasmoid-updater.lock");

        assert!(
            !lock_held_at(&lock_path),
            "a missing lock file read as held"
        );
        assert!(
            !lock_path.exists(),
            "the probe created the lock file it only reads"
        );

        let held = UpdateLock::acquire_at(&lock_path).unwrap();
        assert!(lock_held_at(&lock_path), "a held lock read as free");
        assert!(
            matches!(
                UpdateLock::acquire_at(&lock_path),
                Err(Error::AlreadyRunning)
            ),
            "the probe took the lock"
        );
        drop(held);
        assert!(!lock_held_at(&lock_path), "a released lock read as held");
    }

    #[test]
    fn a_second_holder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("plasmoid-updater.lock");

        let _held = UpdateLock::acquire_at(&lock_path).unwrap();

        assert!(matches!(
            UpdateLock::acquire_at(&lock_path),
            Err(Error::AlreadyRunning)
        ));
    }

    #[test]
    fn the_switch_is_decided_only_for_another_users_run() {
        let root = Uid::from_raw(0);
        let user_uid = Uid::from_raw(1000);
        let user_gid = Gid::from_raw(1000);

        // root on another user's behalf: work is done as that user, so their
        // next run can still take the lock
        assert_eq!(
            delegated_ids_for(root, Some("1000"), Some("1000")),
            Some((user_uid, user_gid))
        );
        // a real root login owns its own cache
        assert_eq!(delegated_ids_for(root, None, None), None);
        // half a pair, or one that is no id, names no owner
        assert_eq!(delegated_ids_for(root, Some("1000"), None), None);
        assert_eq!(delegated_ids_for(root, None, Some("1000")), None);
        assert_eq!(delegated_ids_for(root, Some("nobody"), Some("1000")), None);
        // a normal run already owns what it creates
        assert_eq!(
            delegated_ids_for(Uid::from_raw(1000), Some("1000"), Some("1000")),
            None
        );
    }

    use std::{cell::RefCell, collections::VecDeque};

    use nix::errno::Errno;

    thread_local! {
        static SWITCHES: RefCell<VecDeque<(&'static str, u32)>> = const { RefCell::new(VecDeque::new()) };
    }

    fn record(what: &'static str, id: u32) {
        SWITCHES.with(|switches| switches.borrow_mut().push_back((what, id)));
    }

    fn take_switches() -> Vec<(&'static str, u32)> {
        SWITCHES.with(|switches| switches.borrow_mut().drain(..).collect())
    }

    fn record_egid(gid: Gid) -> nix::Result<()> {
        record("egid", gid.as_raw());
        Ok(())
    }

    fn record_euid(uid: Uid) -> nix::Result<()> {
        record("euid", uid.as_raw());
        Ok(())
    }

    fn refuse_nonroot_egid(gid: Gid) -> nix::Result<()> {
        record("egid", gid.as_raw());
        match gid.as_raw() {
            0 => Ok(()),
            _ => Err(Errno::EPERM),
        }
    }

    fn refuse_nonroot_euid(uid: Uid) -> nix::Result<()> {
        record("euid", uid.as_raw());
        match uid.as_raw() {
            0 => Ok(()),
            _ => Err(Errno::EPERM),
        }
    }

    fn recording_calls() -> IdCalls {
        IdCalls {
            set_egid: record_egid,
            set_euid: record_euid,
        }
    }

    #[test]
    fn a_run_without_a_delegating_user_switches_nothing() {
        drop(DelegatedIds::enter(None, recording_calls(), Path::new("lock")).unwrap());

        assert_eq!(take_switches(), Vec::new());
    }

    #[test]
    fn a_delegated_run_switches_to_the_invoking_user_and_back() {
        {
            let _guard = DelegatedIds::enter(
                Some((Uid::from_raw(1000), Gid::from_raw(1000))),
                recording_calls(),
                Path::new("lock"),
            )
            .unwrap();
            assert_eq!(
                take_switches(),
                [("egid", 1000), ("euid", 1000)],
                "the group must change while the uid is still root"
            );
        }
        assert_eq!(
            take_switches(),
            [("euid", 0), ("egid", 0)],
            "the guard did not put root's ids back"
        );
    }

    #[test]
    fn a_refused_switch_names_the_lock_path_and_restores() {
        let owner = Some((Uid::from_raw(1000), Gid::from_raw(1000)));
        let lock_path = Path::new("/home/user/.cache/plasmoid-updater.lock");

        let calls = IdCalls {
            set_egid: refuse_nonroot_egid,
            set_euid: record_euid,
        };
        let err = match DelegatedIds::enter(owner, calls, lock_path) {
            Ok(_) => panic!("the refused group switch reported success"),
            Err(e) => e,
        };
        match &err {
            Error::Io { path, source } => {
                assert_eq!(path, lock_path);
                assert_eq!(source.raw_os_error(), Some(Errno::EPERM as i32));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            take_switches(),
            [("egid", 1000)],
            "a failed group switch left nothing to restore"
        );

        let calls = IdCalls {
            set_egid: recording_calls().set_egid,
            set_euid: refuse_nonroot_euid,
        };
        let err = match DelegatedIds::enter(owner, calls, lock_path) {
            Ok(_) => panic!("the refused user switch reported success"),
            Err(e) => e,
        };
        match &err {
            Error::Io { path, source } => {
                assert_eq!(path, lock_path);
                assert_eq!(source.raw_os_error(), Some(Errno::EPERM as i32));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            take_switches(),
            [("egid", 1000), ("euid", 1000), ("euid", 0), ("egid", 0)],
            "a failed user switch must put the group back"
        );
    }

    #[test]
    fn writable_by_reads_the_class_bits_that_match_the_ids() {
        let user = Uid::from_raw(1000);
        let group = Gid::from_raw(1000);

        assert!(writable_by(0o700, 1000, 1000, &[], user, group));
        assert!(!writable_by(0o500, 1000, 1000, &[], user, group));
        assert!(
            writable_by(0o070, 0, 1000, &[], user, group),
            "a group member writes through the group bits"
        );
        assert!(
            writable_by(0o020, 0, 2000, &[Gid::from_raw(2000)], user, group),
            "a supplementary group counts as the file's group"
        );
        assert!(writable_by(0o002, 0, 0, &[], user, group));
        assert!(
            !writable_by(0o020, 0, 0, &[], user, group),
            "a non-member must not write through the group bits"
        );
    }

    #[test]
    fn an_unwritable_backup_directory_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let backups = cache.join("plasmoid-updater/backups");
        std::fs::create_dir_all(&backups).unwrap();
        set_mode(&backups, 0o555);

        assert_eq!(
            prepare_backups_dir(&cache).as_deref(),
            Some(backups.as_path()),
            "an unwritable backup directory was not reported"
        );
    }

    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const CROSS_SESSION_CHILD: &str = "installer::lock::tests::two_sessions_contend_on_one_lock";

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn child(
        test: &str,
        role: &str,
        root: &Path,
        cache: &Path,
        runtime: &Path,
    ) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_CACHE_HOME", cache)
            .env("XDG_RUNTIME_DIR", runtime)
            .env("TMPDIR", root.join("tmp"))
            .env("PU_TEST_LOCK_ROLE", role)
            .env("PU_TEST_LOCK_ROOT", root);
        command
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Two logins (a desktop session and an ssh one) have different
    /// `XDG_RUNTIME_DIR` values; both must meet on one lock, or the ssh run
    /// installs over the desktop run's component.
    #[test]
    fn two_sessions_contend_on_one_lock() {
        let Some(role) = std::env::var_os("PU_TEST_LOCK_ROLE") else {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            std::fs::create_dir_all(&cache).unwrap();
            let mut holder = child(
                CROSS_SESSION_CHILD,
                "hold",
                root.path(),
                &cache,
                &root.path().join("run-a"),
            )
            .spawn()
            .unwrap();
            wait_for(&root.path().join("held"));

            let contender = child(
                CROSS_SESSION_CHILD,
                "contend",
                root.path(),
                &cache,
                &root.path().join("run-b"),
            )
            .output()
            .unwrap();
            let outcome = std::fs::read_to_string(root.path().join("contended")).unwrap();
            std::fs::write(root.path().join("release"), b"ok").unwrap();
            let status = holder.wait().unwrap();

            assert!(
                contender.status.success(),
                "the contending child failed: {}",
                String::from_utf8_lossy(&contender.stderr)
            );
            assert!(status.success(), "the holding child failed");
            assert_eq!(outcome, "refused", "two sessions locked different files");

            let lock = cache.join("plasmoid-updater.lock");
            assert!(
                lock.exists(),
                "the lock file is not directly in the cache home"
            );
            assert_eq!(
                mode_of(&cache.join("plasmoid-updater/backups")),
                0o700,
                "the backup directory is not private"
            );
            return;
        };

        let root = PathBuf::from(std::env::var("PU_TEST_LOCK_ROOT").unwrap());
        match role.to_str().unwrap() {
            "hold" => {
                let _lock =
                    UpdateLock::acquire().expect("the holding child could not take the lock");
                std::fs::write(root.join("held"), b"ok").unwrap();
                wait_for(&root.join("release"));
            }
            "contend" => {
                let outcome = match UpdateLock::acquire() {
                    Ok(_) => "acquired",
                    Err(Error::AlreadyRunning) => "refused",
                    Err(e) => panic!("unexpected lock error: {e:?}"),
                };
                std::fs::write(root.join("contended"), outcome).unwrap();
            }
            other => panic!("unknown role {other}"),
        }
    }

    const STALE_SUBDIR_CHILD: &str =
        "installer::lock::tests::a_stale_cache_subdir_does_not_block_the_lock";

    /// An older release could leave `plasmoid-updater/` under the cache home
    /// owned by root after a sudo run; the user's next run must still take the
    /// lock, which lives beside that directory rather than inside it.
    #[test]
    fn a_stale_cache_subdir_does_not_block_the_lock() {
        let Some(role) = std::env::var_os("PU_TEST_LOCK_ROLE") else {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let subdir = cache.join("plasmoid-updater");
            std::fs::create_dir_all(&subdir).unwrap();
            set_mode(&subdir, 0o555);

            let out = child(
                STALE_SUBDIR_CHILD,
                "acquire",
                root.path(),
                &cache,
                &root.path().join("run"),
            )
            .output()
            .unwrap();

            assert!(
                out.status.success(),
                "a stale cache subdirectory blocked the run: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                cache.join("plasmoid-updater.lock").exists(),
                "the lock was not taken directly in the cache home"
            );
            assert_eq!(
                mode_of(&subdir),
                0o555,
                "an existing directory was chmodded"
            );
            return;
        };

        match role.to_str().unwrap() {
            "acquire" => {
                UpdateLock::acquire().expect("the lock was not taken");
            }
            other => panic!("unknown role {other}"),
        }
    }

    const BACKUP_DIR_CHILD: &str =
        "installer::lock::tests::the_backups_directory_is_created_for_the_invoking_user";

    /// Backups a delegated root run takes must land in a directory the
    /// invoking user owns, or that user's later runs cannot back up or prune.
    #[test]
    fn the_backups_directory_is_created_for_the_invoking_user() {
        let Some(role) = std::env::var_os("PU_TEST_LOCK_ROLE") else {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            std::fs::create_dir(&cache).unwrap();
            set_mode(&cache, 0o755);

            let out = child(
                BACKUP_DIR_CHILD,
                "acquire",
                root.path(),
                &cache,
                &root.path().join("run"),
            )
            .output()
            .unwrap();
            assert!(
                out.status.success(),
                "the run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );

            let backups = cache.join("plasmoid-updater/backups");
            assert!(backups.is_dir(), "the backup directory was not created");
            assert_eq!(
                mode_of(&backups),
                0o700,
                "the backup directory is not private"
            );
            assert_eq!(
                mode_of(&cache),
                0o755,
                "an existing cache home was chmodded"
            );
            return;
        };

        match role.to_str().unwrap() {
            "acquire" => {
                UpdateLock::acquire().expect("the lock was not taken");
            }
            other => panic!("unknown role {other}"),
        }
    }
}
