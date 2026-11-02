use std::time::Duration;

use ureq::tls::{RootCerts, TlsConfig};

/// HTTP client used for every Spotify host. It verifies certificates against the Windows
/// trust store: ureq's bundled roots reject the CA behind Spotify's image CDN.
/// Error statuses are returned as normal responses so callers can read the body.
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .build()
        .into()
}
