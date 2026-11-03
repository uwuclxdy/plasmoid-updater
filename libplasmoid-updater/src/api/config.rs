// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

pub(crate) const DEFAULT_PAGE_SIZE: u8 = 100;
pub(crate) const DEFAULT_MAX_RETRIES: u8 = 3;
pub(crate) const DEFAULT_INITIAL_BACKOFF_MS: u32 = 100;
/// Hard cap on catalog pages fetched in one run.
///
/// Registry-only types (color schemes, icon themes, wallpapers, cursor themes)
/// never reach the catalog. The other supported categories joined held 4189
/// entries on 2026-11-01, plus 18 in widget category 713 and 192 in comic
/// category 155 on 2026-11-02: 44 pages at 100 per page; 64 pages is about 1.5
/// times that.
pub(crate) const DEFAULT_MAX_PAGES: u32 = 64;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_DOWNLOAD_LINKS: usize = 64;

pub(crate) const USER_AGENT: &str = concat!("plasmoid-updater/", env!("CARGO_PKG_VERSION"));

/// Tuning knobs for KDE Store API interactions, minus the base URL: that one
/// comes from [`crate::Config::api_url`] per run.
pub(super) struct ApiConfig {
    pub(super) page_size: u8,
    pub(super) max_retries: u8,
    pub(super) initial_backoff_ms: u32,
    pub(super) max_pages: u32,
}

pub(crate) static API_CONFIG: ApiConfig = ApiConfig {
    page_size: DEFAULT_PAGE_SIZE,
    max_retries: DEFAULT_MAX_RETRIES,
    initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
    max_pages: DEFAULT_MAX_PAGES,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::api::ApiClient;
    use crate::config::Config;
    use crate::testutil::{ok, serve};
    use crate::types::ComponentType;

    /// Answers every request with a 200 body no OCS parser accepts, so the
    /// request fails at the payload and is retried, and counts the requests.
    fn garbage_store() -> (String, Arc<AtomicUsize>) {
        serve(|_| ok("<html>maintenance</html>"))
    }

    /// The retry count drives how many times one failing request is attempted,
    /// observed at the server rather than read off the constant.
    #[test]
    fn a_failing_request_is_attempted_the_configured_number_of_times() {
        let (base, requests) = garbage_store();
        let catalog = ApiClient::new(&Config::new().with_api_url(base))
            .unwrap()
            .fetch_all(&[ComponentType::PlasmaWidget]);

        assert!(catalog.error.is_some(), "a garbage page must stay an error");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "a failing request must be attempted three times in total"
        );
    }

    /// The connect timeout caps the TCP handshake, observed against a connect
    /// that never completes rather than read off the constant.
    ///
    /// A socket whose accept queue is never drained drops further SYNs once
    /// the queue is full: measured on Linux 2026-11-03, a connect against a
    /// full queue under a 3 s socket timeout was cut off at 3.003 s, so the
    /// kernel neither answers nor refuses it and the client's own cap is what
    /// ends the connect.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_connect_that_never_completes_is_cut_off_after_ten_seconds() {
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut queued = Vec::new();
        while let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            queued.push(stream);
        }
        assert!(!queued.is_empty(), "no connection was queued");

        let client =
            ApiClient::new(&Config::new().with_api_url(format!("http://{address}/ocs/v1")))
                .unwrap();
        let started = Instant::now();
        let error = client
            .http_client()
            .get(format!("http://{address}/ocs/v1/content/data"))
            .send()
            .unwrap_err();
        let elapsed = started.elapsed();

        assert!(error.is_timeout(), "{error}");
        assert!(
            elapsed >= Duration::from_millis(9_500) && elapsed < Duration::from_millis(11_000),
            "a hung connect took {elapsed:?}, not the ten-second cap"
        );
    }
}
