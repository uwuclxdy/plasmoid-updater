// SPDX-License-Identifier: GPL-3.0-or-later

//! The install record: a hidden `.<name>.plasmoid-updater-backup` file beside a
//! component, holding the absolute path of the backup an install took and one
//! closing `\n`. An install writes it before it changes the component and
//! removes it once the install settles, so a record a later run finds belongs
//! to a run that died mid-install, and its backup is what the component held
//! before that install began.
//!
//! This module alone decides where records may sit ([`RecordSites`]), which
//! component a record belongs to (the record's own path) and what makes one
//! valid ([`judge`]); every reader and the writer go through it. A walk that
//! cannot read a registry or a site whole is partial: it reports what it could
//! not read, leaves out every record of a site it read only in part, and
//! counts the records it could not see as possibly there, so retention keeps
//! every backup.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{ErrorKind, Read as _, Write as _},
    os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        fs::OpenOptionsExt as _,
    },
    path::{Component, Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{
    ComponentFailure, InstallSlotGuard,
    backup::{backup_time, restore_component, system_backup_base_dir, user_backup_base_dir},
    install::recover_dirs,
    privilege,
};
use crate::{
    FailureKind,
    paths::{Absent, absence},
    registry::{UnreadRegistry, normalize_path},
    types::{ComponentType, Diagnostic, INTERRUPTED_INSTALL_REASON, InstalledComponent},
};

/// The suffix of a record's file name, after `.` and the component's name.
const RECORD_SUFFIX: &str = ".plasmoid-updater-backup";

/// The largest record a reader accepts, its closing `\n` included.
const MAX_RECORD_BYTES: usize = 4096;

/// The mode a record is created with: the invoking user reads a `--system`
/// record root wrote.
const RECORD_MODE: u32 = 0o644;

/// The record path beside `component`.
pub(super) fn record_path(component: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(component.file_name().unwrap_or_default());
    name.push(RECORD_SUFFIX);
    component.with_file_name(name)
}

/// The component name a record's file name carries, or `None` when the name is
/// not a record's.
fn component_name(file_name: &OsStr) -> Option<&OsStr> {
    let name = file_name
        .as_bytes()
        .strip_prefix(b".")?
        .strip_suffix(RECORD_SUFFIX.as_bytes())?;
    (!name.is_empty()).then(|| OsStr::from_bytes(name))
}

/// Whether `name` is a record's file name, which a scan must never read as a
/// component.
pub(crate) fn is_record_name(name: &str) -> bool {
    component_name(OsStr::new(name)).is_some()
}

/// `path` made absolute against the working directory and folded lexically,
/// the one spelling a record holds and a reader compares.
fn absolute_normalized(path: &Path) -> PathBuf {
    std::path::absolute(path).map_or_else(|_| normalize_path(path), |path| normalize_path(&path))
}

/// Where this run's records may sit, and the backup base they must name.
///
/// The places are this run's type directories plus the directory of every
/// component the KNewStuff registries this run reads name, compared after
/// [`normalize_path`], so a record a run under another `XDG_DATA_HOME` left
/// beside a registered component is still found. A component nothing registers
/// outside the type directories has no record: no reader looks there, and the
/// writer installs it unprotected.
pub(crate) struct RecordSites {
    dirs: Vec<PathBuf>,
    /// The registry sources this run could not read or look at: a record may
    /// sit beside a component one of them names, at a site missing from
    /// `dirs`.
    unread_registries: Vec<UnreadRegistry>,
    /// The backup base a record of this run's mode names, absolute and
    /// folded.
    base: PathBuf,
    /// Records sit beside `--system` components, written and removed as root.
    system: bool,
}

impl RecordSites {
    /// The sites a run in this mode reads and writes, from the environment
    /// and the registries; it reads the disk only.
    pub(crate) fn scan(system: bool) -> Self {
        let base = if system {
            system_backup_base_dir()
        } else {
            absolute_normalized(&user_backup_base_dir())
        };
        let registered = crate::registry::registered_paths(system);
        let mut sites = Self {
            dirs: Vec::new(),
            unread_registries: registered.unread,
            base,
            system,
        };
        for dir in recover_dirs(system) {
            sites.add(dir);
        }
        for component in registered.paths {
            if let Some(dir) = component.parent() {
                sites.add(dir.to_path_buf());
            }
        }
        sites
    }

    fn add(&mut self, dir: PathBuf) {
        if !dir.as_os_str().is_empty() && !self.holds(&dir) {
            self.dirs.push(dir);
        }
    }

    /// Whether a record may sit in `dir`.
    fn holds(&self, dir: &Path) -> bool {
        let wanted = normalize_path(dir);
        self.dirs.iter().any(|site| normalize_path(site) == wanted)
    }

    /// Whether a record may sit beside `component`.
    fn covers(&self, component: &Path) -> bool {
        component.parent().is_some_and(|dir| self.holds(dir))
    }

    /// Every record in every site, each read and judged once, and whether the
    /// walk is partial: a registry it could not read, or a site it could not
    /// read whole, may hide a record, so no caller may treat a record's
    /// absence as proof. A site read in part gives none of its records.
    fn records(&self) -> Found {
        let mut found = Found::default();
        for unread in &self.unread_registries {
            found.partial = true;
            match unread {
                UnreadRegistry::Directory { dir, why } => warn_once(
                    "record",
                    dir,
                    format_args!(
                        "cannot read the KNewStuff registries in {} ({why}); install records beside the components they list may be skipped, so this run deletes no old backup",
                        dir.display()
                    ),
                ),
                UnreadRegistry::File { path, error } => {
                    crate::registry::note_record_scan_warning(path);
                    warn_once(
                        "record",
                        path,
                        format_args!(
                            "cannot read a KNewStuff registry ({error}); install records beside the components it lists may be skipped, so this run deletes no old backup"
                        ),
                    );
                }
            }
        }
        for dir in &self.dirs {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if absence(&e).is_some() => continue,
                Err(e) => {
                    found.unreadable(dir, &e);
                    continue;
                }
            };
            let mut paths = found.paths(dir, entries);
            paths.sort();
            let read: Result<Vec<Record>, (PathBuf, std::io::Error)> = paths
                .iter()
                .filter_map(|path| self.read_listed(path).transpose())
                .collect();
            match read {
                Ok(mut records) => found.records.append(&mut records),
                Err((path, e)) => found.unreadable_record(dir, &path, &e),
            }
        }
        if found.partial {
            PARTIAL_WALK_SEEN.store(true, Ordering::Relaxed);
        }
        found
    }

    /// The record at `path`, a name its site lists, or `None` when it is gone.
    /// The listing proves it is there, so one that cannot be opened is kept,
    /// with the path the warning names; the error is a path too long to open,
    /// which hides what the record names from every reader.
    fn read_listed(&self, path: &Path) -> Result<Option<Record>, (PathBuf, std::io::Error)> {
        let state = match read_state(path, &self.base) {
            Ok(state) => state,
            Err(e) => match absence(&e) {
                Some(Absent::Gone) => return Ok(None),
                Some(Absent::Unreachable) => return Err((path.to_path_buf(), e)),
                None => unreadable(&e),
            },
        };
        Ok(Record::at(path, state))
    }

    /// Whether a repair would write anything: a record to restore from or a
    /// record to remove. A lost or kept record needs no write, and a record
    /// the walk could not see is one the repair cannot see either.
    pub(crate) fn has_repair_work(&self) -> bool {
        self.records()
            .records
            .iter()
            .any(|record| matches!(record.state, State::Pending(_) | State::Corrupt(_)))
    }

    /// Removes `record`, warning when it cannot go.
    fn remove(&self, record: &Path) {
        drop(self.remove_or_err(record));
    }

    /// Removes `record`, warning when it cannot go; the error is that
    /// warning's detail, for a caller that must not go on without the removal.
    fn remove_or_err(&self, record: &Path) -> Result<(), String> {
        let removed = if self.system {
            privilege::remove_file_as_root(record).map_err(|failure| failure.message)
        } else {
            match fs::remove_file(record) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        };
        if let Err(e) = &removed {
            log::warn!(target: "record", "failed to remove {}: {}", record.display(), e);
        }
        removed
    }
}

/// What one walk of this run's sites found.
#[derive(Default)]
struct Found {
    records: Vec<Record>,
    /// A site, or one of its entries, could not be read, so a record may sit
    /// there unseen.
    partial: bool,
}

impl Found {
    /// The paths of the entries `dir` holds that name records, or none when
    /// the listing fails partway: the site then counts as one that could not
    /// be read whole, and what was listed before the failure is dropped with
    /// what came after it.
    fn paths<I>(&mut self, dir: &Path, entries: I) -> Vec<PathBuf>
    where
        I: Iterator<Item = std::io::Result<fs::DirEntry>>,
    {
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path.file_name().and_then(component_name).is_some() {
                        paths.push(path);
                    }
                }
                Err(e) => {
                    self.unreadable(dir, &e);
                    return Vec::new();
                }
            }
        }
        paths
    }

    /// Records a site that could not be read whole, and reports it once per
    /// process: retention keeps every backup, since one may be the backup a
    /// record there names, and the walk's other readers leave that site's
    /// records out.
    fn unreadable(&mut self, dir: &Path, error: &std::io::Error) {
        self.partial = true;
        warn_once(
            "record",
            dir,
            format_args!(
                "cannot read the install records in {} ({}), so this run deletes no old backup",
                dir.display(),
                error
            ),
        );
    }

    /// Records a listed record that could not be opened, and reports it once
    /// per process, naming the record beside the site whose listing proved it
    /// there: the walk is partial for the same reason as a whole unread site.
    fn unreadable_record(&mut self, dir: &Path, record: &Path, error: &std::io::Error) {
        self.partial = true;
        let name = record.file_name().unwrap_or(record.as_os_str());
        warn_once(
            "record",
            record,
            format_args!(
                "cannot read the install record {} in {} ({}), so this run deletes no old backup",
                name.to_string_lossy(),
                dir.display(),
                error
            ),
        );
    }
}

/// Whether a walk in this process has met a site or registry it could not read
/// whole. Its warning says the run deletes no old backup, and a later walk in
/// the same process (a host's `check`, then its `install`) may find the site
/// readable, so retention keeps every backup once this is set.
///
/// A relaxed order is enough: the flag only ever goes false to true, and a
/// retention read follows every walk it must see, on the caller's thread or
/// after the host's own hand-off between threads (no walk runs on an install
/// worker), so a stronger order would publish nothing more.
static PARTIAL_WALK_SEEN: AtomicBool = AtomicBool::new(false);

/// One record a site holds.
struct Record {
    path: PathBuf,
    /// The component the record belongs to: its own directory and the name
    /// its file name carries.
    component: PathBuf,
    state: State,
}

impl Record {
    /// The record at `path` in `state`, or `None` when `path` does not name
    /// a record.
    fn at(path: &Path, state: State) -> Option<Self> {
        let name = path.file_name().and_then(component_name)?;
        Some(Self {
            path: path.to_path_buf(),
            component: path.with_file_name(name),
            state,
        })
    }
}

/// Whether a record sits at `record`, as its site shows: `Ok(false)` when none
/// can (a file name past the filesystem's limit, a site [`absence`] proves
/// empty, a listing that completes without the name), `Ok(true)` when the
/// listing names it, and the site's listing error when it cannot be listed
/// whole. This is the proof a reader takes when the record itself cannot be
/// opened and the walk's own listing did not name it.
fn site_lists(record: &Path) -> std::io::Result<bool> {
    let Some(name) = record
        .file_name()
        .filter(|name| usize::try_from(nix::libc::NAME_MAX).is_ok_and(|max| name.len() <= max))
    else {
        return Ok(false);
    };
    let Some(parent) = record.parent() else {
        return Ok(false);
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if absence(&e).is_some() => return Ok(false),
        Err(e) => return Err(e),
    };
    listing_names(entries, name)
}

/// Whether `entries` lists `name`: `Ok(true)` when it does, `Ok(false)` when
/// the listing completes without it, and the listing's error when it loses an
/// entry first, since the lost one may be `name`.
fn listing_names<I>(entries: I, name: &OsStr) -> std::io::Result<bool>
where
    I: Iterator<Item = std::io::Result<fs::DirEntry>>,
{
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_name().as_os_str() == name => return Ok(true),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// What a record says, as [`judge`] and the recorded backup decide.
#[derive(Debug, PartialEq, Eq)]
enum State {
    /// A valid record whose backup is there: the component is restored from
    /// it.
    Pending(PathBuf),
    /// A valid record whose backup is gone.
    Lost(PathBuf),
    /// A record that may be valid but cannot be used: it stays, and the
    /// component is not updated while it does. `backup` is the path it names,
    /// when that could be read.
    Held {
        why: String,
        backup: Option<PathBuf>,
    },
    /// A record no reader accepts: it is removed, and nothing is restored.
    Corrupt(String),
}

/// Why [`judge`] refuses a record.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    Corrupt(String),
    /// A well-shaped record whose backup lies outside this run's backup base.
    Outside {
        why: String,
        backup: PathBuf,
    },
}

/// Reads and judges the record at `path` without following a link in its
/// place or blocking on a FIFO. The error is the one opening it failed with:
/// it leaves nothing to judge, and whether a record is there at all is for
/// the caller to prove, by [`absence`] or by the site's listing.
fn read_state(path: &Path, base: &Path) -> std::io::Result<State> {
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(nix::libc::ELOOP) => {
            return Ok(State::Corrupt("it is a symlink".to_string()));
        }
        Err(e) => return Err(e),
    };
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Ok(State::Held {
                why: "it is not a regular file".to_string(),
                backup: None,
            });
        }
        Err(e) => return Ok(unreadable(&e)),
    }
    let mut bytes = Vec::new();
    if let Err(e) = file
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
    {
        return Ok(unreadable(&e));
    }
    let backup = match judge(path, &bytes, base) {
        Ok(backup) => backup,
        Err(Refusal::Corrupt(why)) => return Ok(State::Corrupt(why)),
        Err(Refusal::Outside { why, backup }) => {
            return Ok(State::Held {
                why,
                backup: Some(backup),
            });
        }
    };
    Ok(match backup.symlink_metadata() {
        Ok(_) => State::Pending(backup),
        Err(e) if absence(&e).is_some() => State::Lost(backup),
        Err(e) => State::Held {
            why: format!("its backup {} cannot be looked up ({e})", backup.display()),
            backup: Some(backup),
        },
    })
}

fn unreadable(error: &std::io::Error) -> State {
    State::Held {
        why: format!("it cannot be read ({error})"),
        backup: None,
    }
}

/// The one rule every reader and the writer judge a record's bytes by: an
/// absolute, `..`-free backup path and one closing `\n`, at most
/// [`MAX_RECORD_BYTES`] in all, naming `<base>/<timestamp>/<type>/<name>`, where
/// `<timestamp>` is a name the backups are written under, `<type>` is any
/// component type's backup directory (one directory can hold components of
/// several types, and a record does not say which type took it), and `<name>`
/// is the component's own name from the record's file name. A path of that
/// shape under another base is refused apart, since another cache home may
/// still hold the backup.
fn judge(record: &Path, bytes: &[u8], base: &Path) -> Result<PathBuf, Refusal> {
    let corrupt = |why: &str| Err(Refusal::Corrupt(why.to_owned()));
    if bytes.len() > MAX_RECORD_BYTES {
        return corrupt(&format!("it is longer than {MAX_RECORD_BYTES} bytes"));
    }
    let Some(body) = bytes.strip_suffix(b"\n") else {
        return corrupt("it does not end in a newline");
    };
    let shapeless = "it does not name a backup of its component";
    let Some(name) = record.file_name().and_then(component_name) else {
        return corrupt(shapeless);
    };
    if body.contains(&b'\n') || body.contains(&0) {
        return corrupt(shapeless);
    }
    let backup = Path::new(OsStr::from_bytes(body));
    if !backup.is_absolute()
        || backup
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return corrupt(shapeless);
    }
    let parts: Vec<Component<'_>> = backup.components().collect();
    let [
        ..,
        Component::Normal(timestamp),
        Component::Normal(subdir),
        Component::Normal(leaf),
    ] = parts.as_slice()
    else {
        return corrupt(shapeless);
    };
    let known_subdir = ComponentType::all()
        .iter()
        .any(|component_type| OsStr::new(component_type.backup_subdir()) == *subdir);
    let named_backup = timestamp.to_str().and_then(backup_time).is_some();
    if *leaf != name || !known_subdir || !named_backup {
        return corrupt(shapeless);
    }
    let backup = normalize_path(backup);
    let under = backup.ancestors().nth(3).map(normalize_path);
    if under.as_deref() != Some(base) {
        return Err(Refusal::Outside {
            why: format!(
                "its backup {} lies outside {}",
                backup.display(),
                base.display()
            ),
            backup,
        });
    }
    Ok(backup)
}

/// Puts back every component a killed run left with a record whose backup is
/// there, removes the records no reader accepts and warns about the ones it
/// keeps. A lost record is left to the check, which knows whether an update is
/// offered for its component.
///
/// The caller holds the run lock, so every record seen here is a dead run's.
pub(crate) fn repair(sites: &RecordSites) {
    let found = sites.records();
    for record in found.records {
        match record.state {
            State::Pending(backup) => match restore_component(&backup, &record.component) {
                Ok(()) => {
                    log::info!(
                        target: "restore",
                        "restored {} from {} after an interrupted install",
                        record.component.display(),
                        backup.display()
                    );
                    sites.remove(&record.path);
                }
                Err(e) => warn_once(
                    "restore",
                    &record.path,
                    format_args!(
                        "failed to restore {} from its backup {} ({}); its install record stays, so the next run tries again",
                        record.component.display(),
                        backup.display(),
                        e.message
                    ),
                ),
            },
            State::Held { why, .. } => warn_once(
                "record",
                &record.path,
                format_args!(
                    "keeping the install record {} and restoring nothing: {why}; check the component, then delete that record to update it",
                    record.path.display()
                ),
            ),
            State::Corrupt(why) => {
                warn_corrupt(&record.path, &why);
                sites.remove(&record.path);
            }
            State::Lost(_) => {}
        }
    }
}

static WARNED: LazyLock<Mutex<HashSet<(PathBuf, String)>>> = LazyLock::new(Mutex::default);

/// Logs `line` once per process for `key`: a process can walk the same record
/// or site more than once (the bin repairs before its check, and its install
/// repairs again), and a judgment that stays — a kept record, a failed
/// restore, a site that cannot be read — comes out the same each time, so the
/// second line would only bury the first. A new reason for the same key is
/// still news.
fn warn_once(target: &str, key: &Path, line: fmt::Arguments<'_>) {
    let message = line.to_string();
    let first = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((key.to_path_buf(), message.clone()));
    if first {
        log::log!(target: target, log::Level::Warn, "{message}");
    }
}

fn warn_corrupt(record: &Path, why: &str) {
    warn_once(
        "record",
        record,
        format_args!(
            "removing the unusable install record {}: {why}; nothing is restored from it",
            record.display()
        ),
    );
}

/// The check's rows for the components records hold, and those components'
/// paths, which the check then leaves out of every other row.
#[derive(Default)]
pub(crate) struct RecordRows {
    pub(crate) rows: Vec<Diagnostic>,
    held: Vec<PathBuf>,
}

impl RecordRows {
    /// Whether a record row answers for the component at `path`.
    pub(crate) fn holds(&self, path: &Path) -> bool {
        self.held.contains(&normalize_path(path))
    }
}

/// One row per discovered component a pending or kept record belongs to, each
/// named and placed as that component, or one row named by the record's own
/// file name when no discovered component sits at its path.
pub(crate) fn record_rows(sites: &RecordSites, components: &[InstalledComponent]) -> RecordRows {
    let mut rows = RecordRows::default();
    let found = sites.records();
    for record in found.records {
        let reason = match &record.state {
            State::Pending(_) => INTERRUPTED_INSTALL_REASON.to_string(),
            State::Held { why, .. } => format!(
                "interrupted install, its install record {} is kept because {why}; check the component, then delete that record to update it",
                record.path.display()
            ),
            State::Lost(_) | State::Corrupt(_) => continue,
        };
        let wanted = normalize_path(&record.component);
        let owners: Vec<&InstalledComponent> = components
            .iter()
            .filter(|component| normalize_path(&component.path) == wanted)
            .collect();
        if owners.is_empty() {
            let name = record
                .component
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            rows.rows
                .push(Diagnostic::new(name, record.component, reason));
        } else {
            for owner in owners {
                rows.rows.push(Diagnostic::new(
                    owner.name.clone(),
                    owner.path.clone(),
                    reason.clone(),
                ));
            }
        }
        rows.held.push(wanted);
    }
    rows
}

/// Warns once about every lost record, by what this check offers for its
/// component. A lost record beside a component the check read as current draws
/// no warning: nothing is left to restore. With `settle` such a record is
/// removed, a `--system` one only by a run that is root; any other run leaves
/// it for a run that may remove it.
///
/// `offered` is the check's updates after exclusions, and `current` the
/// folded paths of the components the check read as up to date.
pub(crate) fn report_lost(
    sites: &RecordSites,
    offered: &[crate::AvailableUpdate],
    current: &[PathBuf],
    settle: bool,
) {
    let found = sites.records();
    for record in found.records {
        let State::Lost(backup) = &record.state else {
            continue;
        };
        let wanted = normalize_path(&record.component);
        if let Some(update) = offered
            .iter()
            .find(|update| normalize_path(&update.installed.path) == wanted)
        {
            log::warn!(
                target: "record",
                "{}: the backup {} an interrupted install recorded is gone; `update` reinstalls it",
                update.installed.path.display(),
                backup.display()
            );
        } else if current.contains(&wanted) {
            if !settle {
                continue;
            }
            settle_current_lost(sites, &record.path, privilege::is_root());
        } else {
            log::warn!(
                target: "record",
                "{}: the backup {} an interrupted install recorded is gone, and this run offers no update for it; reinstall it, then delete {}",
                record.component.display(),
                backup.display(),
                record.path.display()
            );
        }
    }
}

/// Removes a current component's lost record, or keeps it for a run that may
/// not: a `--system` record goes only in a root run.
fn settle_current_lost(sites: &RecordSites, record: &Path, root: bool) {
    if sites.system && !root {
        log::debug!(target: "record", "keeping {}: removing it needs root", record.display());
    } else {
        sites.remove(record);
    }
}

/// The backups retention must keep: the timestamp directory of every backup a
/// record names, and, for a record whose content cannot be read, every backup
/// of its component's name, since any of them may be the one it names. When a
/// site could not be read whole, every backup is kept: one of them may be the
/// backup a record there names.
#[derive(Default)]
pub(crate) struct Spared {
    timestamp_dirs: Vec<PathBuf>,
    names: Vec<OsString>,
    all: bool,
}

impl Spared {
    /// Whether retention must keep `timestamp_dir`'s `type_subdir`. A backup
    /// whose lookup fails is kept: its absence is not proven.
    pub(super) fn spares(&self, timestamp_dir: &Path, type_subdir: &str) -> bool {
        if self.all
            || self
                .timestamp_dirs
                .contains(&absolute_normalized(timestamp_dir))
        {
            return true;
        }
        self.names.iter().any(|name| {
            match timestamp_dir
                .join(type_subdir)
                .join(name)
                .symlink_metadata()
            {
                Ok(_) => true,
                Err(e) => absence(&e).is_none(),
            }
        })
    }
}

/// What retention keeps for the records the sites hold now, and for every
/// backup when any walk in this process met a site or registry it could not
/// read whole.
pub(crate) fn spared_backups(sites: &RecordSites) -> Spared {
    let found = sites.records();
    let mut spared = Spared {
        all: PARTIAL_WALK_SEEN.load(Ordering::Relaxed),
        ..Spared::default()
    };
    for record in found.records {
        match record.state {
            State::Pending(backup)
            | State::Held {
                backup: Some(backup),
                ..
            } => {
                if let Some(timestamp_dir) = backup.ancestors().nth(2) {
                    spared.timestamp_dirs.push(timestamp_dir.to_path_buf());
                }
            }
            State::Held { backup: None, .. } => {
                if let Some(name) = record.component.file_name() {
                    spared.names.push(name.to_os_string());
                }
            }
            State::Lost(_) | State::Corrupt(_) => {}
        }
    }
    spared
}

/// What an install found in its component's record before it took its
/// backup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Prior {
    None,
    /// A lost record: it stays as it is while the install runs, and goes only
    /// with an install that lands.
    Lost,
}

/// Settles the record beside the component an install is about to replace,
/// under the install slot and before the backup: a pending record restores the
/// component first, or refuses the install when that restore fails or when the
/// record cannot then be removed; a kept record refuses it; a corrupt one is
/// removed.
///
/// The component's own record is read wherever the walk says a record may sit,
/// and after a partial walk even where the walk could not look: an unread
/// registry may name the component, so that silence is no proof no record sits
/// beside it. A record path [`absence`] proves empty reads as no record (a
/// path through a regular file included), and so does one that cannot be
/// opened where [`site_lists`] proves none sits; any other record that cannot
/// be opened refuses the install, since it may name the backup the component
/// needs put back.
pub(super) fn settle_prior(
    sites: &RecordSites,
    component: &InstalledComponent,
    _slot: &InstallSlotGuard<'_>,
) -> Result<Prior, ComponentFailure> {
    let record = record_path(&component.path);
    if !sites.covers(&component.path) && sites.unread_registries.is_empty() {
        return Ok(Prior::None);
    }
    let record = match settle_record(sites, &record) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(Prior::None),
        Err(error) => {
            return Err(ComponentFailure::new(
                FailureKind::Install,
                format!(
                    "not updating it: an interrupted install may have left the install record {}, but its directory cannot be read ({error}); make that directory readable, then update again",
                    record.display()
                ),
            ));
        }
    };
    match record.state {
        State::Pending(backup) => match restore_component(&backup, &component.path) {
            Ok(()) => {
                log::info!(
                    target: "restore",
                    "restored {} from {} before updating it",
                    component.path.display(),
                    backup.display()
                );
                sites.remove_or_err(&record.path).map_err(|e| {
                    ComponentFailure::new(
                        FailureKind::Install,
                        format!(
                            "not updating it: the install record {} an interrupted install left is kept because removing it after restoring its backup failed ({e}); check the component, then delete that record to update it",
                            record.path.display()
                        ),
                    )
                })?;
                Ok(Prior::None)
            }
            Err(e) => Err(ComponentFailure::new(
                FailureKind::RestoreFailed,
                format!(
                    "an interrupted install may have left it half-written, and restoring its backup {} failed ({}); its install record {} stays so the next run tries again",
                    backup.display(),
                    e.message,
                    record.path.display()
                ),
            )),
        },
        State::Lost(_) => Ok(Prior::Lost),
        State::Held { why, .. } => Err(ComponentFailure::new(
            FailureKind::Install,
            format!(
                "not updating it: the install record {} an interrupted install left is kept because {why}; check the component, then delete that record to update it",
                record.path.display()
            ),
        )),
        State::Corrupt(why) => {
            warn_corrupt(&record.path, &why);
            sites.remove(&record.path);
            Ok(Prior::None)
        }
    }
}

/// The record beside a component an install is about to replace, or `None`
/// when nothing sits there. A record that cannot be opened is there unless
/// [`absence`] or the site's listing proves it is not; the error is the
/// listing's own, for a site that cannot be listed whole and may hold it.
fn settle_record(sites: &RecordSites, record: &Path) -> std::io::Result<Option<Record>> {
    let error = match read_state(record, &sites.base) {
        Ok(state) => return Ok(Record::at(record, state)),
        Err(error) => error,
    };
    if absence(&error) == Some(Absent::Gone) || !site_lists(record)? {
        return Ok(None);
    }
    Ok(Record::at(record, unreadable(&error)))
}

/// The record an install holds open while it changes the component.
#[must_use = "an open record is settled by `landed` or `restored`; dropping it keeps it open"]
pub(super) enum Open {
    /// No record: nothing to back up, or none could be written.
    None,
    /// The record this install wrote, or one a write that gave up while its
    /// process may still run can land after the give-up.
    Written(PathBuf),
    /// A lost record the install runs over, left as it is.
    Lost(PathBuf),
}

impl Open {
    /// Opens the component's record for the install about to run, after
    /// `backup` was taken and before any install step; a record that cannot
    /// be written leaves the install unprotected, with one warning. A write
    /// that gave up while its process may still run keeps the record tracked,
    /// so the landed install removes whatever that process writes.
    pub(super) fn write(
        sites: &RecordSites,
        component: &InstalledComponent,
        backup: Option<&Path>,
        prior: Prior,
        _slot: &InstallSlotGuard<'_>,
    ) -> Self {
        let record = record_path(&component.path);
        if prior == Prior::Lost {
            return Self::Lost(record);
        }
        let Some(backup) = backup else {
            return Self::None;
        };
        match write_record(sites, &record, &component.path, backup) {
            Ok(()) => Self::Written(record),
            Err(failure) => {
                let err = &failure.message;
                log::warn!(
                    target: "record",
                    "{:?}: installing it unprotected: no install record could be written ({err}), so if this run is killed mid-install, no later run can restore it",
                    component.name
                );
                if failure.process_may_still_run {
                    Self::Written(record)
                } else {
                    Self::None
                }
            }
        }
    }

    /// The install's files are in place: the record goes, a lost one too. A
    /// record that cannot be removed refuses the update, because a later run's
    /// repair would restore the backup it names over the files this run landed;
    /// a lost record names no backup that could be restored, so its removal
    /// only warns.
    pub(super) fn landed(
        self,
        sites: &RecordSites,
        _slot: &InstallSlotGuard<'_>,
    ) -> Result<(), ComponentFailure> {
        let record = match self {
            Self::Written(record) => record,
            Self::Lost(record) => {
                sites.remove(&record);
                return Ok(());
            }
            Self::None => return Ok(()),
        };
        sites.remove_or_err(&record).map_err(|e| {
            ComponentFailure::new(
                FailureKind::Registry,
                format!(
                    "updated, but its install record {} could not be removed ({e}), so the KNewStuff registry was not updated; delete that record by hand, or the next update that can read and write its directory first puts the old version back from its backup",
                    record.display()
                ),
            )
        })
    }

    /// The install failed and its backup is back in place: the record this
    /// install wrote goes, and a lost record it ran over stays.
    pub(super) fn restored(self, sites: &RecordSites, _slot: &InstallSlotGuard<'_>) {
        match self {
            Self::Written(record) => {
                sites.remove(&record);
            }
            Self::Lost(_) | Self::None => {}
        }
    }
}

/// Writes `record` naming `backup`, only where a reader looks and only bytes
/// [`judge`] accepts; the error's message is the one the unprotected warning
/// names, and its `process_may_still_run` says the record may still appear.
fn write_record(
    sites: &RecordSites,
    record: &Path,
    component: &Path,
    backup: &Path,
) -> Result<(), ComponentFailure> {
    let mut bytes = absolute_normalized(backup).into_os_string().into_vec();
    bytes.push(b'\n');
    if !sites.covers(component) || judge(record, &bytes, &sites.base).is_err() {
        return Err(ComponentFailure::new(
            FailureKind::Install,
            format!("{} cannot be recorded", backup.display()),
        ));
    }
    if sites.system {
        let written = privilege::write_file_as_root(record, &bytes)
            .and_then(|()| privilege::set_mode_as_root(record, RECORD_MODE));
        if let Err(failure) = written {
            sites.remove(record);
            return Err(failure);
        }
        return Ok(());
    }
    let io_failure = |e: std::io::Error| ComponentFailure::io(FailureKind::Install, record, e);
    match fs::remove_file(record) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io_failure(e)),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .mode(RECORD_MODE)
        .open(record)
        .map_err(io_failure)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    if let Err(e) = written {
        drop(file);
        sites.remove(record);
        return Err(io_failure(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sites(root: &Path, system: bool) -> RecordSites {
        RecordSites {
            dirs: vec![root.join("schemes")],
            unread_registries: Vec::new(),
            base: root.join("base"),
            system,
        }
    }

    fn record(root: &Path, name: &str) -> PathBuf {
        record_path(&root.join("schemes").join(name))
    }

    fn line(path: &Path) -> Vec<u8> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    fn is_corrupt(verdict: &Result<PathBuf, Refusal>) -> bool {
        matches!(verdict, Err(Refusal::Corrupt(_)))
    }

    /// Each guard of the record rule refuses the one shape it names, while
    /// the record that differs only there is accepted. The guards have no
    /// input through the public entry points that a stricter neighbour does
    /// not refuse first, so they are pinned here.
    #[test]
    fn the_record_rule_refuses_each_shape_it_names() {
        let root = Path::new("/r");
        let base = root.join("base");
        let record = record(root, "S.colors");
        let good = base.join("2020-01-01T00-00-00/color-schemes/S.colors");
        assert_eq!(judge(&record, &line(&good), &base), Ok(good.clone()));

        let other_type = base.join("2020-01-01T00-00-00/icons/S.colors");
        assert_eq!(
            judge(&record, &line(&other_type), &base),
            Ok(other_type),
            "a backup under another type's directory was refused"
        );

        let mut torn = line(&good);
        torn.pop();
        assert!(is_corrupt(&judge(&record, &torn, &base)), "torn");
        let mut long = vec![b'/'; MAX_RECORD_BYTES];
        long.push(b'\n');
        assert!(is_corrupt(&judge(&record, &long, &base)), "long");
        let mut two_lines = line(&good);
        two_lines.extend_from_slice(&line(&good));
        assert!(is_corrupt(&judge(&record, &two_lines, &base)), "two lines");
        let mut nul = good.as_os_str().as_bytes().to_vec();
        nul.extend_from_slice(b"\0\n");
        assert!(is_corrupt(&judge(&record, &nul, &base)), "nul");
        let relative = Path::new("base/2020-01-01T00-00-00/color-schemes/S.colors");
        assert!(
            is_corrupt(&judge(&record, &line(relative), &base)),
            "relative"
        );
        let dotdot = base.join("x/../2020-01-01T00-00-00/color-schemes/S.colors");
        assert!(is_corrupt(&judge(&record, &line(&dotdot), &base)), "dotdot");
        let named_other = base.join("2020-01-01T00-00-00/color-schemes/T.colors");
        assert!(
            is_corrupt(&judge(&record, &line(&named_other), &base)),
            "name"
        );
        let bad_time = base.join("2020-1-01T00-00-00/color-schemes/S.colors");
        assert!(is_corrupt(&judge(&record, &line(&bad_time), &base)), "time");
        let bad_type = base.join("2020-01-01T00-00-00/colors/S.colors");
        assert!(is_corrupt(&judge(&record, &line(&bad_type), &base)), "type");
        let not_a_record = root.join("schemes/S.colors");
        assert!(
            is_corrupt(&judge(&not_a_record, &line(&good), &base)),
            "file"
        );

        let outside = root.join("other/2020-01-01T00-00-00/color-schemes/S.colors");
        assert!(
            matches!(
                judge(&record, &line(&outside), &base),
                Err(Refusal::Outside { backup, .. }) if backup == outside
            ),
            "another base"
        );
    }

    /// A registry source the run could not look at draws one line naming the
    /// directory and why the trust check could not decide it.
    #[test]
    fn an_undecided_directory_is_one_record_line() {
        let sites = RecordSites {
            unread_registries: vec![UnreadRegistry::Directory {
                dir: PathBuf::from("/r/secret/share/knewstuff3"),
                why: "cannot check \"/r/secret/share\": Permission denied (os error 13)"
                    .to_string(),
            }],
            ..sites(Path::new("/r"), false)
        };
        let lines = crate::testutil::logs::captured_logs_while(|| {
            assert!(sites.records().partial);
        });
        let expected = "record: cannot read the KNewStuff registries in /r/secret/share/knewstuff3 (cannot check \"/r/secret/share\": Permission denied (os error 13)); install records beside the components they list may be skipped, so this run deletes no old backup";
        assert!(lines.iter().any(|line| line == expected), "{lines:?}");
    }

    /// A site the run cannot list may hold records, but the repair cannot list
    /// it either, so nothing there is a write the repair would make: the walk
    /// is partial, and the probe still finds no repair work.
    #[test]
    fn an_unlistable_site_is_no_repair_work() {
        use std::os::unix::fs::PermissionsExt as _;

        if privilege::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o311)).unwrap();
        let partial = sites.records().partial;
        let work = sites.has_repair_work();
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(partial, "the site stayed listable");
        assert!(!work, "an unlistable site counted as repair work");
    }

    /// A site listing that fails after some entries were read drops what it
    /// read: its warning says the site's records are left out of the run, so
    /// no reader may act on the part of it that was seen.
    #[test]
    fn a_site_listing_that_fails_midway_drops_what_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let schemes = dir.path().join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        std::fs::write(record_path(&schemes.join("S.colors")), b"x\n").unwrap();
        assert_eq!(std::fs::read_dir(&schemes).unwrap().count(), 1);
        let torn = std::fs::read_dir(&schemes)
            .unwrap()
            .chain(std::iter::once(Err(std::io::Error::other(
                "torn directory block",
            ))));
        let mut found = Found::default();
        let paths = found.paths(&schemes, torn);
        assert!(found.partial, "a lost entry left the walk complete");
        assert_eq!(
            paths,
            Vec::<PathBuf>::new(),
            "a listing that failed midway kept the records it read"
        );
    }

    /// A listing that loses an entry before it shows the record's name proves
    /// nothing: the lost entry may be the record.
    #[test]
    fn a_listing_that_loses_an_entry_proves_no_absence() {
        let dir = tempfile::tempdir().unwrap();
        let schemes = dir.path().join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let record = record_path(&schemes.join("S.colors"));
        let name = record.file_name().unwrap();
        std::fs::write(schemes.join("S.colors"), b"x").unwrap();
        assert_eq!(
            listing_names(std::fs::read_dir(&schemes).unwrap(), name).ok(),
            Some(false),
            "a complete listing without the name did not prove it absent"
        );
        std::fs::write(&record, b"x\n").unwrap();
        assert_eq!(
            listing_names(std::fs::read_dir(&schemes).unwrap(), name).ok(),
            Some(true),
            "a listing that shows the name did not prove it there"
        );
        std::fs::remove_file(&record).unwrap();
        let torn = std::fs::read_dir(&schemes)
            .unwrap()
            .chain(std::iter::once(Err(std::io::Error::other(
                "torn directory block",
            ))));
        let error = listing_names(torn, name).expect_err("a torn listing proved the record absent");
        assert_eq!(error.to_string(), "torn directory block");
    }

    /// A record the site lists but whose path is too long to open hides what
    /// it names as surely as a site that cannot be listed: the walk is
    /// partial, so retention keeps every backup.
    #[test]
    fn a_listed_record_past_path_max_marks_the_walk_partial() {
        // Linux's PATH_MAX counts the closing NUL: a 4080-byte directory
        // still opens, and the record's path inside it does not.
        const SITE_BYTES: usize = 4080;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut free = SITE_BYTES - root.as_os_str().len() - 1;
        let mut parent = root.to_path_buf();
        while free > 255 {
            parent.push("d".repeat(200));
            free -= 201;
        }
        // The site is made under a short last name, so its record can be
        // written, then renamed to the length that puts the record past
        // PATH_MAX.
        let short = parent.join("s");
        std::fs::create_dir_all(&short).unwrap();
        std::fs::write(record_path(&short.join("S.colors")), b"x\n").unwrap();
        let site = parent.join("s".repeat(free));
        std::fs::rename(&short, &site).unwrap();
        assert_eq!(site.as_os_str().len(), SITE_BYTES);
        assert_eq!(std::fs::read_dir(&site).unwrap().count(), 1);

        let sites = RecordSites {
            dirs: vec![site.clone()],
            ..sites(root, false)
        };
        let lines = crate::testutil::logs::captured_logs_while(|| {
            assert!(
                sites.records().partial,
                "a record too long to open read as no record"
            );
        });
        let expected = format!(
            "record: cannot read the install record .S.colors.plasmoid-updater-backup in {} (File name too long (os error 36)), so this run deletes no old backup",
            site.display()
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(&site.display().to_string()))
                .collect::<Vec<_>>(),
            [&expected],
            "{lines:?}"
        );
        assert!(
            spared_backups(&sites).spares(&root.join("base/2020-01-01T00-00-00"), "color-schemes"),
            "retention may prune the backup a record too long to open names"
        );
    }

    /// A record the site lists whose path is too long to open cannot be read
    /// before an install either: the settle refuses the component rather than
    /// run over the tree the record may name.
    #[test]
    fn a_record_past_path_max_before_an_install_refuses_it() {
        // Linux's PATH_MAX counts the closing NUL: a 4080-byte directory
        // still opens, and the record's path inside it does not.
        const SITE_BYTES: usize = 4080;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut free = SITE_BYTES - root.as_os_str().len() - 1;
        let mut parent = root.to_path_buf();
        while free > 255 {
            parent.push("d".repeat(200));
            free -= 201;
        }
        // The site is made under a short last name, so its record can be
        // written, then renamed to the length that puts the record past
        // PATH_MAX.
        let short = parent.join("s");
        std::fs::create_dir_all(&short).unwrap();
        std::fs::write(record_path(&short.join("S.colors")), b"x\n").unwrap();
        let site = parent.join("s".repeat(free));
        std::fs::rename(&short, &site).unwrap();
        assert_eq!(site.as_os_str().len(), SITE_BYTES);

        let sites = RecordSites {
            dirs: vec![site.clone()],
            ..sites(root, false)
        };
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed = InstalledComponent::new(
            "S",
            "S.colors",
            ComponentType::ColorScheme,
            site.join("S.colors"),
        );
        let settled = settle_prior(&sites, &installed, &guard);
        let Err(failure) = settled else {
            panic!("the install went on with a record it could not read");
        };
        assert!(matches!(failure.kind, FailureKind::Install), "{failure:?}");
        assert!(
            failure
                .message
                .contains("an interrupted install left is kept because it cannot be read"),
            "{}",
            failure.message
        );
    }

    /// A component whose record path is too long to open installs unprotected
    /// when its site holds no record: nothing sits there to settle.
    #[test]
    fn a_component_too_deep_for_its_record_installs_unprotected() {
        // Linux's PATH_MAX counts the closing NUL: a 4080-byte directory
        // still opens, and the record's path inside it does not.
        const SITE_BYTES: usize = 4080;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut free = SITE_BYTES - root.as_os_str().len() - 1;
        let mut parent = root.to_path_buf();
        while free > 255 {
            parent.push("d".repeat(200));
            free -= 201;
        }
        let short = parent.join("s");
        std::fs::create_dir_all(&short).unwrap();
        let site = parent.join("s".repeat(free));
        std::fs::rename(&short, &site).unwrap();
        assert_eq!(site.as_os_str().len(), SITE_BYTES);

        let sites = RecordSites {
            dirs: vec![site.clone()],
            ..sites(root, false)
        };
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed = InstalledComponent::new(
            "S",
            "S.colors",
            ComponentType::ColorScheme,
            site.join("S.colors"),
        );
        let settled = settle_prior(&sites, &installed, &guard);
        assert!(
            matches!(settled, Ok(Prior::None)),
            "the settle refused a site that holds no record"
        );
    }

    /// A record path that runs through a regular file cannot hold a record:
    /// the settle reads none there, never a kept record the user is told to
    /// delete.
    #[test]
    fn a_record_under_a_regular_file_is_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let site = root.join("schemes");
        std::fs::write(&site, b"a file, not a directory").unwrap();
        let sites = sites(root, false);
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed = InstalledComponent::new(
            "S",
            "S.colors",
            ComponentType::ColorScheme,
            site.join("S.colors"),
        );

        let settled = settle_prior(&sites, &installed, &guard);

        assert!(
            matches!(settled, Ok(Prior::None)),
            "a record path through a regular file read as a record: {:?}",
            settled.err()
        );
    }

    /// A record that cannot be opened in a site that cannot be listed may be
    /// there: nothing proves it absent, so the settle refuses the install
    /// rather than run over the tree the record may name.
    #[test]
    fn an_unopened_record_in_a_site_that_cannot_be_listed_refuses_the_install() {
        use std::os::unix::fs::PermissionsExt as _;

        if privilege::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let record = record(root, "S.colors");
        std::fs::write(&record, b"x\n").unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o311)).unwrap();
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed = InstalledComponent::new(
            "S",
            "S.colors",
            ComponentType::ColorScheme,
            schemes.join("S.colors"),
        );

        let settled = settle_prior(&sites, &installed, &guard);
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o755)).unwrap();

        let Err(failure) = settled else {
            panic!("the install went on beside a record its site could not show");
        };
        assert!(matches!(failure.kind, FailureKind::Install), "{failure:?}");
        assert_eq!(
            failure.message,
            format!(
                "not updating it: an interrupted install may have left the install record {}, but its directory cannot be read (Permission denied (os error 13)); make that directory readable, then update again",
                record.display()
            )
        );
    }

    /// A record name past the filesystem's name limit cannot exist, so the
    /// settle reads none there even in a site it cannot list.
    #[test]
    fn a_record_name_past_the_name_limit_is_none_in_a_site_that_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt as _;

        if privilege::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let name = format!("{}.colors", "s".repeat(240));
        assert_eq!(
            record(root, &name).file_name().map(OsStr::len),
            Some(272),
            "the fixture's record name is not past the name limit"
        );
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o311)).unwrap();
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed = InstalledComponent::new(
            "S",
            name.as_str(),
            ComponentType::ColorScheme,
            schemes.join(&name),
        );

        let settled = settle_prior(&sites, &installed, &guard);
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(settled, Ok(Prior::None)),
            "a record name no filesystem holds refused the install: {:?}",
            settled.err()
        );
    }

    /// A site past PATH_MAX, which no path-based call reaches.
    fn site_past_path_max(root: &Path) -> PathBuf {
        let mut site = root.to_path_buf();
        while site.as_os_str().len() <= 4096 {
            site.push("d".repeat(200));
        }
        site
    }

    /// A site too long to name holds no record any reader or the writer can
    /// reach: the walk passes it as complete.
    #[test]
    fn a_site_too_long_to_name_leaves_the_walk_complete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = RecordSites {
            dirs: vec![site_past_path_max(root)],
            ..sites(root, false)
        };

        assert!(
            !sites.records().partial,
            "a site too long to name made the walk partial"
        );
    }

    /// The settle reads no record beside a component whose site is too long
    /// to name, as the walk finds none there.
    #[test]
    fn a_component_in_a_site_too_long_to_name_settles_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let site = site_past_path_max(root);
        let sites = RecordSites {
            dirs: vec![site.clone()],
            ..sites(root, false)
        };
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed = InstalledComponent::new(
            "S",
            "S.colors",
            ComponentType::ColorScheme,
            site.join("S.colors"),
        );
        let settled = settle_prior(&sites, &installed, &guard);
        assert!(
            matches!(settled, Ok(Prior::None)),
            "the settle refused a component in a site too long to name: {:?}",
            settled.err()
        );
    }

    /// A recorded backup whose path runs through a regular file cannot exist:
    /// the record is lost, as for a backup that is gone.
    #[test]
    fn a_backup_under_a_regular_file_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = root.join("base");
        let timestamp_dir = base.join("2020-01-01T00-00-00");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&timestamp_dir, b"a file, not a directory").unwrap();
        let backup = timestamp_dir.join("color-schemes/S.colors");
        let record = record(root, "S.colors");
        std::fs::create_dir_all(root.join("schemes")).unwrap();
        std::fs::write(&record, line(&backup)).unwrap();

        assert_eq!(read_state(&record, &base).ok(), Some(State::Lost(backup)));
    }

    /// Retention keeps no backup by a name whose path runs through a regular
    /// file: nothing can be there.
    #[test]
    fn retention_keeps_nothing_under_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let timestamp_dir = dir.path().join("2020-01-01T00-00-00");
        std::fs::create_dir_all(&timestamp_dir).unwrap();
        std::fs::write(timestamp_dir.join("color-schemes"), b"a file").unwrap();
        let spared = Spared {
            names: vec![OsString::from("S.colors")],
            ..Spared::default()
        };

        assert!(
            !spared.spares(&timestamp_dir, "color-schemes"),
            "retention kept a backup path through a regular file"
        );
    }

    /// A per-entry error from a site's iterator hides the record that entry
    /// names exactly as a site that cannot be listed hides all of them: the
    /// walk is partial.
    #[test]
    fn a_site_entry_error_marks_the_walk_partial() {
        let mut found = Found::default();
        let dir = Path::new("/r/schemes");
        let paths = found.paths(
            dir,
            std::iter::once(Err::<fs::DirEntry, std::io::Error>(std::io::Error::other(
                "torn directory entry",
            ))),
        );
        assert_eq!(paths, Vec::<PathBuf>::new());
        assert!(found.partial, "a lost entry left the walk complete");
    }

    /// A record path holding a directory is kept, not read as a torn record:
    /// it may name the backup a repair must restore from.
    #[test]
    fn a_record_path_that_is_not_a_regular_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let record = record(root, "S.colors");
        std::fs::create_dir_all(&record).unwrap();
        assert_eq!(
            read_state(&record, &root.join("base")).ok(),
            Some(State::Held {
                why: "it is not a regular file".to_string(),
                backup: None,
            })
        );
    }

    /// A corrupt record an install finds beside its component is removed with
    /// its warning before the install runs, and nothing is restored.
    #[test]
    fn a_corrupt_record_before_an_install_is_removed_and_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let component = schemes.join("S.colors");
        let record = record_path(&component);
        std::fs::write(&record, b"not a backup\n").unwrap();
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let installed =
            InstalledComponent::new("S", "S.colors", ComponentType::ColorScheme, &component);
        crate::testutil::logs::capture_logs();
        let prior = settle_prior(&sites, &installed, &guard)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        assert!(matches!(prior, Prior::None));
        let expected = format!(
            "removing the unusable install record {}: it does not name a backup of its component; nothing is restored from it",
            record.display()
        );
        let warnings = crate::testutil::logs::captured_warnings_for("record");
        assert!(
            warnings.iter().any(|line| line == &expected),
            "{warnings:?}"
        );
        assert!(!record.exists(), "the corrupt record survived");
    }

    /// A record the install cannot remove once the files are in place refuses
    /// the component: a later repair would restore the backup it names over
    /// the landed files.
    #[test]
    fn a_record_that_cannot_be_removed_after_the_files_land_refuses_the_component() {
        use std::os::unix::fs::PermissionsExt as _;

        if privilege::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let record = record(root, "S.colors");
        std::fs::write(&record, b"x\n").unwrap();
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o500)).unwrap();
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let refused = Open::Written(record.clone()).landed(&sites, &guard);
        std::fs::set_permissions(&schemes, std::fs::Permissions::from_mode(0o755)).unwrap();
        let Err(failure) = refused else {
            panic!("the landed install reported success with its record still open");
        };
        assert!(matches!(failure.kind, FailureKind::Registry), "{failure:?}");
        assert!(
            failure.message.contains("updated, but its install record "),
            "{}",
            failure.message
        );
        assert!(record.exists(), "the refused update removed its record");
    }

    /// A record already gone when the files land is no failure: nothing is
    /// left that a later repair could restore over them.
    #[test]
    fn a_record_already_gone_when_the_files_land_is_no_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        std::fs::create_dir_all(root.join("schemes")).unwrap();
        let slot = super::super::InstallSlot::new();
        let guard = slot.acquire();
        let landed = Open::Written(record(root, "S.colors")).landed(&sites, &guard);
        assert!(matches!(landed, Ok(())), "{landed:?}");
    }

    /// A record's file name is one of the installer's own, which no scan
    /// reads as a component; the bare suffix names no component.
    #[test]
    fn a_record_name_is_an_installer_sibling() {
        assert!(super::super::is_installer_sibling(
            ".S.colors.plasmoid-updater-backup"
        ));
        assert!(!super::super::is_installer_sibling(
            ".plasmoid-updater-backup"
        ));
        assert!(!super::super::is_installer_sibling(
            "S.colors.plasmoid-updater-backup"
        ));
    }

    /// Retention keeps a timestamp directory a record names in any spelling,
    /// and, for a record it could not read, every one holding the component's
    /// name, a lookup that fails included; a walk that could not read a site
    /// keeps everything.
    #[test]
    fn retention_spares_what_a_record_may_name() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let named = base.join("2020-01-01T00-00-00");
        let other = base.join("2020-01-02T00-00-00");
        std::fs::create_dir_all(named.join("color-schemes")).unwrap();
        std::fs::create_dir_all(other.join("color-schemes")).unwrap();
        let spared = Spared {
            timestamp_dirs: vec![named],
            names: vec![OsString::from("S.colors")],
            ..Spared::default()
        };
        assert!(spared.spares(&base.join("x/../2020-01-01T00-00-00"), "color-schemes"));
        assert!(!spared.spares(&other, "color-schemes"));
        std::fs::write(other.join("color-schemes/S.colors"), b"").unwrap();
        assert!(spared.spares(&other, "color-schemes"));
        let unseen = base.join("2020-01-04T00-00-00");
        assert!(!spared.spares(&unseen, "color-schemes"));
        assert!(
            Spared {
                all: true,
                ..Spared::default()
            }
            .spares(&unseen, "color-schemes"),
            "a site that could not be read kept nothing"
        );

        if privilege::is_root() {
            return;
        }
        let closed = base.join("2020-01-03T00-00-00");
        std::fs::create_dir_all(closed.join("color-schemes")).unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let kept = spared.spares(&closed, "color-schemes");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(kept, "a backup whose lookup failed was not kept");
    }

    /// The writer writes only where a reader looks, and only bytes the
    /// reader's own rule accepts, its backup path made absolute and folded.
    #[test]
    fn the_writer_writes_only_what_and_where_the_reader_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let backup = root.join("base/2020-01-01T00-00-00/color-schemes/S.colors");
        let spelled = root.join("base/x/../2020-01-01T00-00-00/color-schemes/./S.colors");

        let component = schemes.join("S.colors");
        let written = write_record(&sites, &record_path(&component), &component, &spelled);
        assert!(matches!(written, Ok(())), "{written:?}");
        assert_eq!(
            std::fs::read(record_path(&component)).unwrap(),
            line(&backup)
        );
        assert_eq!(
            sites
                .read_listed(&record_path(&component))
                .unwrap()
                .map(|record| record.state),
            Some(State::Lost(backup.clone()))
        );

        let elsewhere = root.join("elsewhere/S.colors");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        assert!(write_record(&sites, &record_path(&elsewhere), &elsewhere, &backup).is_err());
        assert!(!record_path(&elsewhere).exists());

        let other = schemes.join("T.colors");
        assert!(write_record(&sites, &record_path(&other), &other, &backup).is_err());
        assert!(!record_path(&other).exists());
    }

    /// `--system` records are written and removed as root, the way the system
    /// backups are: through `sudo tee`, `sudo chmod` and `sudo rm`.
    #[test]
    fn a_system_record_is_written_and_removed_through_sudo() {
        const TEST: &str = "a_system_record_is_written_and_removed_through_sudo";
        if privilege::is_root() {
            return;
        }
        if let Some(root) = super::super::backup::tests::backup_child_root(TEST) {
            super::super::backup::tests::assert_fake_sudo(&root);
            let sites = sites(&root, true);
            let component = root.join("schemes/S.colors");
            std::fs::create_dir_all(component.parent().unwrap()).unwrap();
            let backup = root.join("base/2020-01-01T00-00-00/color-schemes/S.colors");
            let slot = super::super::InstallSlot::new();
            let guard = slot.acquire();
            let installed =
                InstalledComponent::new("S", "S.colors", ComponentType::ColorScheme, &component)
                    .with_system(true);
            let open = Open::write(&sites, &installed, Some(&backup), Prior::None, &guard);
            assert!(matches!(&open, Open::Written(path) if *path == record_path(&component)));
            assert_eq!(
                std::fs::read(record_path(&component)).unwrap(),
                line(&backup)
            );
            open.landed(&sites, &guard)
                .unwrap_or_else(|failure| panic!("{failure:?}"));
            assert!(!record_path(&component).exists());
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }
        let child = super::super::backup::tests::run_sudo_child(
            "installer::record::tests",
            TEST,
            "exec \"$@\"\n",
        );
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        let record = record_path(&child.root.join("schemes/S.colors"));
        assert_eq!(
            child.log.lines().collect::<Vec<_>>(),
            [
                format!("tee {}", record.display()),
                format!("chmod 644 {}", record.display()),
                format!("rm -f {}", record.display()),
            ]
        );
    }

    /// The `--system` repair restores from a pending record and removes it
    /// through `sudo`; a lost record there needs no privileged write, so it
    /// never asks for one.
    #[test]
    fn a_system_repair_restores_and_removes_through_sudo_and_a_lost_record_needs_none() {
        const TEST: &str =
            "a_system_repair_restores_and_removes_through_sudo_and_a_lost_record_needs_none";
        if privilege::is_root() {
            return;
        }
        if let Some(root) = super::super::backup::tests::backup_child_root(TEST) {
            super::super::backup::tests::assert_fake_sudo(&root);
            let sites = sites(&root, true);
            let schemes = root.join("schemes");
            std::fs::create_dir_all(&schemes).unwrap();
            let lost = root.join("base/2020-01-01T00-00-00/color-schemes/L.colors");
            std::fs::write(record_path(&schemes.join("L.colors")), line(&lost)).unwrap();
            assert!(!sites.has_repair_work(), "a lost record asked for a repair");

            let backup = root.join("base/2020-01-01T00-00-00/color-schemes/S.colors");
            std::fs::create_dir_all(backup.parent().unwrap()).unwrap();
            std::fs::write(&backup, "old\n").unwrap();
            std::fs::write(schemes.join("S.colors"), "half\n").unwrap();
            std::fs::write(record_path(&schemes.join("S.colors")), line(&backup)).unwrap();
            assert!(
                sites.has_repair_work(),
                "a pending record asked for no repair"
            );

            repair(&sites);
            assert_eq!(
                std::fs::read_to_string(schemes.join("S.colors")).unwrap(),
                "old\n"
            );
            // A current component's lost record needs root to go.
            let current = [normalize_path(&schemes.join("L.colors"))];
            report_lost(&sites, &[], &current, true);
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }
        let child = super::super::backup::tests::run_sudo_child(
            "installer::record::tests",
            TEST,
            "exec \"$@\"\n",
        );
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        let schemes = child.root.join("schemes");
        assert_eq!(
            child.log.lines().collect::<Vec<_>>(),
            [format!(
                "rm -f {}",
                record_path(&schemes.join("S.colors")).display()
            )]
        );
        assert!(record_path(&schemes.join("L.colors")).exists());
    }

    /// A prior record that restored but cannot be removed refuses the install:
    /// going on would leave that record to restore the pre-install backup over
    /// the landed update on the next run.
    #[test]
    fn a_settled_record_that_cannot_be_removed_refuses_the_install() {
        const TEST: &str = "a_settled_record_that_cannot_be_removed_refuses_the_install";
        if privilege::is_root() {
            return;
        }
        if let Some(root) = super::super::backup::tests::backup_child_root(TEST) {
            super::super::backup::tests::assert_fake_sudo(&root);
            let sites = sites(&root, true);
            let schemes = root.join("schemes");
            std::fs::create_dir_all(&schemes).unwrap();
            let backup = root.join("base/2020-01-01T00-00-00/color-schemes/S.colors");
            std::fs::create_dir_all(backup.parent().unwrap()).unwrap();
            std::fs::write(&backup, "old\n").unwrap();
            let component = schemes.join("S.colors");
            std::fs::write(&component, "half\n").unwrap();
            std::fs::write(record_path(&component), line(&backup)).unwrap();
            let slot = super::super::InstallSlot::new();
            let guard = slot.acquire();
            let installed =
                InstalledComponent::new("S", "S.colors", ComponentType::ColorScheme, &component)
                    .with_system(true);

            let settled = settle_prior(&sites, &installed, &guard);
            let Err(failure) = settled else {
                panic!("the install went on with the record it could not remove");
            };
            assert!(matches!(failure.kind, FailureKind::Install), "{failure:?}");
            assert!(
                failure.message.contains(
                    "an interrupted install left is kept because removing it after restoring its backup failed"
                ),
                "{}",
                failure.message
            );
            assert_eq!(std::fs::read_to_string(&component).unwrap(), "old\n");
            assert!(record_path(&component).exists());
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }
        let child = super::super::backup::tests::run_sudo_child(
            "installer::record::tests",
            TEST,
            "case \"$1\" in rm) exit 1 ;; *) exec \"$@\" ;; esac\n",
        );
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        let record = record_path(&child.root.join("schemes/S.colors"));
        assert_eq!(
            child.log.lines().collect::<Vec<_>>(),
            [format!("rm -f {}", record.display())]
        );
        assert!(
            record.exists(),
            "a refused removal still removed the record"
        );
    }

    /// One process repairs twice — the bin repairs before its check and its
    /// install repairs again — and a record that stays (kept, or a failed
    /// restore) draws its line once, not once per repair.
    #[test]
    fn a_repeated_repair_warns_once_about_a_record_it_cannot_settle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sites = sites(root, false);
        let schemes = root.join("schemes");
        std::fs::create_dir_all(&schemes).unwrap();
        let elsewhere = root.join("other/2020-01-01T00-00-00/color-schemes/S.colors");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, "old\n").unwrap();
        let record = record_path(&schemes.join("S.colors"));
        std::fs::write(&record, line(&elsewhere)).unwrap();
        let needle = record.display().to_string();

        let lines = crate::testutil::logs::captured_logs_while(|| {
            repair(&sites);
            repair(&sites);
        });
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(&needle) && line.contains("keeping the install record"))
                .count(),
            1,
            "{lines:?}"
        );
        assert!(record.exists());
    }

    /// A record write that fails after the elevation wrote the file removes
    /// what it wrote, so no record is left naming a backup the install will
    /// never be repaired from.
    #[test]
    fn a_record_write_that_fails_after_writing_removes_its_file() {
        const TEST: &str = "a_record_write_that_fails_after_writing_removes_its_file";
        if privilege::is_root() {
            return;
        }
        if let Some(root) = super::super::backup::tests::backup_child_root(TEST) {
            super::super::backup::tests::assert_fake_sudo(&root);
            let sites = sites(&root, true);
            let schemes = root.join("schemes");
            std::fs::create_dir_all(&schemes).unwrap();
            let backup = root.join("base/2020-01-01T00-00-00/color-schemes/S.colors");
            let component = schemes.join("S.colors");
            let slot = super::super::InstallSlot::new();
            let guard = slot.acquire();
            let installed =
                InstalledComponent::new("S", "S.colors", ComponentType::ColorScheme, &component)
                    .with_system(true);

            let warnings = crate::testutil::logs::captured_logs_while(|| {
                let open = Open::write(&sites, &installed, Some(&backup), Prior::None, &guard);
                assert!(
                    matches!(open, Open::None),
                    "a failed record write left a record open"
                );
            });
            assert!(
                warnings
                    .iter()
                    .any(|line| line.contains("installing it unprotected")),
                "{warnings:?}"
            );
            assert!(
                !record_path(&component).exists(),
                "the failed write left its file"
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }
        let child = super::super::backup::tests::run_sudo_child(
            "installer::record::tests",
            TEST,
            "case \"$1\" in chmod) exit 1 ;; *) exec \"$@\" ;; esac\n",
        );
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        let record = record_path(&child.root.join("schemes/S.colors"));
        assert_eq!(
            child.log.lines().collect::<Vec<_>>(),
            [
                format!("tee {}", record.display()),
                format!("chmod 644 {}", record.display()),
                format!("rm -f {}", record.display()),
            ]
        );
        assert!(!record.exists());
    }

    /// A record write given up while its `tee` may still run keeps the record
    /// tracked: a `tee` that lands the record after the give-up has it removed
    /// once the install's files land, so no later repair restores the backup
    /// it names over them. The install still runs with the one unprotected
    /// warning.
    #[test]
    fn a_record_a_given_up_tee_lands_late_goes_when_the_files_land() {
        const TEST: &str = "a_record_a_given_up_tee_lands_late_goes_when_the_files_land";
        if privilege::is_root() {
            return;
        }
        if let Some(root) = super::super::backup::tests::backup_child_root(TEST) {
            super::super::backup::tests::assert_fake_sudo(&root);
            let sites = sites(&root, true);
            let schemes = root.join("schemes");
            std::fs::create_dir_all(&schemes).unwrap();
            let backup = root.join("base/2020-01-01T00-00-00/color-schemes/S.colors");
            let component = schemes.join("S.colors");
            let slot = super::super::InstallSlot::new();
            let guard = slot.acquire();
            let installed =
                InstalledComponent::new("S", "S.colors", ComponentType::ColorScheme, &component)
                    .with_system(true);

            let lines = crate::testutil::logs::captured_logs_while(|| {
                let open = Open::write(&sites, &installed, Some(&backup), Prior::None, &guard);
                // The late `tee` lands the record only now, after the give-up.
                std::fs::write(root.join("go"), b"").unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !root.join("landed").exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                assert_eq!(
                    std::fs::read(record_path(&component)).ok(),
                    Some(line(&backup)),
                    "the late tee never landed the record"
                );
                open.landed(&sites, &guard)
                    .unwrap_or_else(|failure| panic!("{failure:?}"));
            });
            assert!(
                !record_path(&component).exists(),
                "the landed install left the record a late tee wrote"
            );
            assert_eq!(
                lines,
                [
                    "record: \"S\": installing it unprotected: no install record could be written (sudo tee timed out after 1 s), so if this run is killed mid-install, no later run can restore it"
                ]
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }
        // The `tee` leaves sudo's group with the content and lands the record
        // when the test says so; sudo itself outlives the run bound.
        let child = super::super::backup::tests::run_sudo_child_with_env(
            "installer::record::tests",
            TEST,
            "case \"$1\" in\n\
             tee)\n\
             exec 3<&0\n\
             setsid sh -c 'cat > \"$PU_BACKUP_ROOT/tee.part\"; \
             while [ ! -e \"$PU_BACKUP_ROOT/go\" ]; do sleep 0.05; done; \
             mv \"$PU_BACKUP_ROOT/tee.part\" \"$0\"; : > \"$PU_BACKUP_ROOT/landed\"' \"$2\" \
             <&3 3<&- >/dev/null 2>&1 &\n\
             echo $! > \"$PU_BACKUP_ROOT/tee.pid\"\n\
             exec sleep 30 3<&- ;;\n\
             *) exec \"$@\" ;;\n\
             esac\n",
            &[("PU_TEST_PRIVILEGED_TIMEOUT_MS", "1000")],
        );
        let tee_pid = child.root.join("tee.pid");
        let late_tee = std::fs::read_to_string(&tee_pid)
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let late_tee_gone = late_tee.is_none_or(|pid| {
            loop {
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                    == Err(nix::errno::Errno::ESRCH)
                {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        });
        if let Some(pid) = late_tee {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        assert!(late_tee.is_some(), "the fake never started its late tee");
        assert!(late_tee_gone, "the late tee {late_tee:?} is still running");
        let record = record_path(&child.root.join("schemes/S.colors"));
        assert_eq!(
            child.log.lines().collect::<Vec<_>>(),
            [
                format!("tee {}", record.display()),
                format!("rm -f {}", record.display()),
                format!("rm -f {}", record.display()),
            ]
        );
    }

    /// A current component's lost record goes only in a root run under
    /// `--system`: a non-root run keeps it and asks for no privilege, and a
    /// root run removes it through the elevation path.
    #[test]
    fn a_system_lost_record_settles_only_in_a_root_run() {
        const TEST: &str = "a_system_lost_record_settles_only_in_a_root_run";
        if privilege::is_root() {
            return;
        }
        if let Some(root) = super::super::backup::tests::backup_child_root(TEST) {
            super::super::backup::tests::assert_fake_sudo(&root);
            let sites = sites(&root, true);
            let schemes = root.join("schemes");
            std::fs::create_dir_all(&schemes).unwrap();
            let lost = root.join("base/2020-01-01T00-00-00/color-schemes/L.colors");
            let record = record_path(&schemes.join("L.colors"));
            std::fs::write(&record, line(&lost)).unwrap();
            let current = [normalize_path(&schemes.join("L.colors"))];

            report_lost(&sites, &[], &current, true);
            assert!(record.exists(), "a non-root run removed a --system record");
            settle_current_lost(&sites, &record, true);
            assert!(!record.exists(), "a root run kept the record");
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }
        let child = super::super::backup::tests::run_sudo_child(
            "installer::record::tests",
            TEST,
            "exec \"$@\"\n",
        );
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        let record = record_path(&child.root.join("schemes/L.colors"));
        assert_eq!(
            child.log.lines().collect::<Vec<_>>(),
            [format!("rm -f {}", record.display())]
        );
        assert!(!record.exists());
    }
}
