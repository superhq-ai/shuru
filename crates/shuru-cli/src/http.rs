//! The HTTP client behind every download the CLI makes.
//!
//! Trust comes from the platform store rather than a bundled root list, so a
//! TLS interceptor whose root is installed on the machine works the same way
//! it does for curl and git: the keychain on macOS, the system store plus
//! SSL_CERT_FILE and SSL_CERT_DIR on Linux.

use std::sync::OnceLock;

use ureq::tls::{RootCerts, TlsConfig};
use ureq::Agent;

pub fn agent() -> Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            Agent::config_builder()
                .tls_config(
                    TlsConfig::builder()
                        .root_certs(RootCerts::PlatformVerifier)
                        .build(),
                )
                .build()
                .new_agent()
        })
        .clone()
}
