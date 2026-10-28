use std::sync::Arc;

use lru::LruCache;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Manages a root CA and generates per-domain TLS certificates for MITM.
pub struct CertificateAuthority {
    // Signs the leaves. Issuer fields come from its name and key only, so a
    // CA loaded from a saved session re-signs this with the saved key.
    ca_cert: Certificate,
    ca_key: KeyPair,
    // The certificate the guest trusts.
    ca_pem: String,
    ca_der: CertificateDer<'static>,
    cache: LruCache<String, Arc<ServerConfig>>,
}

impl CertificateAuthority {
    /// Generate a new self-signed root CA.
    pub fn new() -> anyhow::Result<Self> {
        let key = KeyPair::generate()?;
        let cert = Self::params().self_signed(&key)?;
        Ok(CertificateAuthority {
            ca_pem: cert.pem(),
            ca_der: cert.der().clone(),
            ca_cert: cert,
            ca_key: key,
            cache: Self::cache(),
        })
    }

    /// The CA of an earlier session, which a restored guest still trusts.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> anyhow::Result<Self> {
        let key = KeyPair::from_pem(key_pem)?;
        let saved = boring::x509::X509::from_pem(cert_pem.as_bytes())?;
        if saved.public_key()?.public_key_to_der()? != key.public_key_der() {
            anyhow::bail!("the saved CA certificate is not for the saved key");
        }
        Ok(CertificateAuthority {
            ca_cert: Self::params().self_signed(&key)?,
            ca_key: key,
            ca_pem: cert_pem.to_string(),
            ca_der: CertificateDer::from(saved.to_der()?),
            cache: Self::cache(),
        })
    }

    fn cache() -> LruCache<String, Arc<ServerConfig>> {
        LruCache::new(std::num::NonZeroUsize::new(256).unwrap())
    }

    fn params() -> CertificateParams {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "Shuru Proxy CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Shuru");
        params
    }

    /// Get the CA certificate in PEM format (for injecting into the guest trust store).
    pub fn ca_cert_pem(&self) -> Vec<u8> {
        self.ca_pem.clone().into_bytes()
    }

    pub fn ca_key_pem(&self) -> String {
        self.ca_key.serialize_pem()
    }

    /// Get or create a TLS ServerConfig for the given domain.
    pub fn server_config_for_domain(&mut self, domain: &str) -> anyhow::Result<Arc<ServerConfig>> {
        if let Some(config) = self.cache.get(domain) {
            return Ok(config.clone());
        }

        let config = self.generate_server_config(domain)?;
        let config = Arc::new(config);
        self.cache.put(domain.to_string(), config.clone());
        Ok(config)
    }

    /// Get a TlsAcceptor for the given domain.
    pub fn acceptor_for_domain(&mut self, domain: &str) -> anyhow::Result<TlsAcceptor> {
        let config = self.server_config_for_domain(domain)?;
        Ok(TlsAcceptor::from(config))
    }

    fn leaf(&self, domain: &str) -> anyhow::Result<(Certificate, KeyPair)> {
        let mut params = CertificateParams::new(vec![domain.to_string()])?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, domain);

        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.ca_cert, &self.ca_key)?;
        Ok((cert, key))
    }

    fn generate_server_config(&self, domain: &str) -> anyhow::Result<ServerConfig> {
        let (cert, key) = self.leaf(domain)?;
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let ca_cert_der = self.ca_der.clone();
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der, ca_cert_der], key_der)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[test]
    fn test_server_config_alpn_h1_only() {
        init_crypto();
        let mut ca = CertificateAuthority::new().unwrap();
        let config = ca.server_config_for_domain("example.com").unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn test_server_config_cached() {
        init_crypto();
        let mut ca = CertificateAuthority::new().unwrap();
        let c1 = ca.server_config_for_domain("example.com").unwrap();
        let c2 = ca.server_config_for_domain("example.com").unwrap();
        assert!(Arc::ptr_eq(&c1, &c2));
    }

    #[test]
    fn a_loaded_ca_issues_leaves_the_saved_certificate_verifies() {
        use boring::x509::store::X509StoreBuilder;
        use boring::x509::{X509StoreContext, X509};

        init_crypto();
        let original = CertificateAuthority::new().unwrap();
        let cert_pem = String::from_utf8(original.ca_cert_pem()).unwrap();
        let loaded = CertificateAuthority::from_pem(&cert_pem, &original.ca_key_pem()).unwrap();
        assert_eq!(loaded.ca_cert_pem(), original.ca_cert_pem());

        let mut store = X509StoreBuilder::new().unwrap();
        store
            .add_cert(X509::from_pem(cert_pem.as_bytes()).unwrap())
            .unwrap();
        let store = store.build();
        let leaf = X509::from_der(loaded.leaf("example.com").unwrap().0.der()).unwrap();
        let chain = boring::stack::Stack::new().unwrap();
        let mut context = X509StoreContext::new().unwrap();
        assert!(context
            .init(&store, &leaf, &chain, |c| c.verify_cert())
            .unwrap());
    }

    #[test]
    fn a_saved_certificate_for_another_key_is_refused() {
        init_crypto();
        let one = CertificateAuthority::new().unwrap();
        let two = CertificateAuthority::new().unwrap();
        let cert_pem = String::from_utf8(one.ca_cert_pem()).unwrap();
        assert!(CertificateAuthority::from_pem(&cert_pem, &two.ca_key_pem()).is_err());
    }
}
