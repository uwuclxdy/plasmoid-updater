// SPDX-License-Identifier: GPL-3.0-or-later

/// Progress events the core emits during [`update`](crate::update) and
/// [`install`](crate::install).
///
/// Every per-component event carries two identifiers: `name`, the component's
/// display name for rendering, and `id`, its installed path — unique within a
/// run, except that two component types can share one directory under
/// `--system`; reporters key progress rows on it.
///
/// The enum is `#[non_exhaustive]`: a new stage is not a breaking change, so
/// match with a wildcard arm. The variants that carry fields are
/// `#[non_exhaustive]` as well, so a field added to one later ships in a minor
/// release: an outside match names the fields it uses and ends with `..`.
///
/// ```
/// # use libplasmoid_updater::Event;
/// fn component_of(event: &Event) -> Option<&str> {
///     match event {
///         Event::Downloading { name, .. }
///         | Event::Installed { name, .. }
///         | Event::Failed { name, .. } => Some(name),
///         _ => None,
///     }
/// }
/// ```
///
/// A match naming every field of a variant without `..` does not compile:
///
/// ```compile_fail
/// # use libplasmoid_updater::Event;
/// fn component_of(event: &Event) -> Option<&str> {
///     match event {
///         Event::Failed {
///             name,
///             id,
///             message,
///         } => Some(name),
///         _ => None,
///     }
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Event {
    /// The core finished scanning installed components and is querying the
    /// store for them.
    ///
    /// Emitted once by [`check`](crate::check) and [`update`](crate::update)
    /// after the scan, carrying how many installed components this run will
    /// check.
    #[non_exhaustive]
    Checking {
        /// Number of installed components the run will check.
        components: usize,
    },
    /// The check part of the run is over: the scan and the store lookups are
    /// done, and no further [`Event::Checking`] line is live.
    ///
    /// A reporter that drew the checking line ends it here, so the caller can
    /// print its own output below without a half-drawn line above it.
    Checked,
    /// A chunk of one component's download arrived.
    #[non_exhaustive]
    Downloading {
        /// Display name of the component being downloaded.
        name: String,
        /// The component's installed path, which reporters key progress rows
        /// on: it tells same-named components apart, though two component
        /// types can share one directory under `--system`.
        id: String,
        /// Bytes received so far.
        bytes: u64,
        /// Total size from the download's `Content-Length`, when the server
        /// sent one.
        total: Option<u64>,
    },
    /// One component finished installing.
    #[non_exhaustive]
    Installed {
        /// Display name of the component that installed.
        name: String,
        /// The component's installed path, as in [`Event::Downloading`].
        id: String,
        /// The version the install verified: what
        /// [`UpdateResult::succeeded`](crate::UpdateResult::succeeded) records
        /// for the component.
        version: String,
    },
    /// One component failed; the row lands in
    /// [`UpdateResult::failed`](crate::UpdateResult::failed) and the run
    /// continues with the remaining components.
    #[non_exhaustive]
    Failed {
        /// Display name of the component that failed.
        name: String,
        /// The component's installed path, as in [`Event::Downloading`].
        id: String,
        /// The per-component error message.
        message: String,
    },
}

/// Receives [`Event`]s from the core during
/// [`update`](crate::update) and [`install`](crate::install).
///
/// The default implementation does nothing, so the unit type works as a
/// no-op reporter (`&()`; [`Reporter`] is implemented for `()`), and a named
/// type costs one line:
///
/// ```
/// # use libplasmoid_updater::Reporter;
/// struct Quiet;
///
/// impl Reporter for Quiet {}
/// ```
///
/// `Sync` because installs run on a rayon pool.
pub trait Reporter: Sync {
    /// Called once per progress event, in emission order. Never blocks the
    /// run beyond the implementation itself.
    fn event(&self, _event: &Event) {}
}

impl Reporter for () {}
