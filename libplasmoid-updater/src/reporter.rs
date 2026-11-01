// SPDX-License-Identifier: GPL-3.0-or-later

/// Progress events the core emits during [`update`](crate::update) and
/// [`install`](crate::install).
///
/// Every per-component event carries two identifiers: `name`, the component's
/// display name for rendering, and `id`, its installed path — unique within a
/// run, which reporters key progress rows on.
///
/// The enum is `#[non_exhaustive]`: a new stage is not a breaking change, so
/// match with a wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Event {
    /// The core is scanning installed components and querying the store.
    ///
    /// Emitted by [`update`](crate::update) only; a caller of
    /// [`install`](crate::install) drives the check itself and can emit it.
    Checking,
    /// A chunk of one component's download arrived.
    Downloading {
        /// Display name of the component being downloaded.
        name: String,
        /// The component's installed path: unique within a run, so reporters
        /// can key progress rows on it even when two components share a
        /// display name.
        id: String,
        /// Bytes received so far.
        bytes: u64,
        /// Total size from the download's `Content-Length`, when the server
        /// sent one.
        total: Option<u64>,
    },
    /// One component finished installing.
    Installed {
        /// Display name of the component that installed.
        name: String,
        /// The component's installed path, as in [`Event::Downloading`].
        id: String,
    },
    /// One component failed; the row lands in
    /// [`UpdateResult::failed`](crate::UpdateResult::failed) and the run
    /// continues with the remaining components.
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
