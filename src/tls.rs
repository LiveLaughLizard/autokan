//! Custom certificate authorities for outbound connections to identity providers. Each
//! provider's settings can carry a pasted PEM bundle, trusted in addition to the system roots.
use std::sync::Arc;

/// Parse a pasted PEM bundle. An empty string is fine and means "no extra CA".
pub fn parse_ca(pem: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    let pem = pem.trim();
    if pem.is_empty() {
        return Ok(Vec::new());
    }
    let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("The CA certificate is not valid PEM: {e}"))?;
    if certs.is_empty() {
        return Err("The CA certificate must contain at least one -----BEGIN CERTIFICATE----- block".into());
    }
    // Reject blocks that are PEM-shaped but not certificates before they are stored.
    let mut store = rustls::RootCertStore::empty();
    for cert in &certs {
        store.add(cert.clone()).map_err(|e| format!("The CA certificate could not be read: {e}"))?;
    }
    Ok(certs)
}

/// An HTTP client that also trusts `pem`. With no custom CA this is just the shared client.
pub fn http_client(shared: &reqwest::Client, pem: &str) -> Result<reqwest::Client, String> {
    if parse_ca(pem)?.is_empty() {
        return Ok(shared.clone());
    }
    let mut builder = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15));
    for cert in reqwest::Certificate::from_pem_bundle(pem.trim().as_bytes()).map_err(|e| format!("The CA certificate could not be read: {e}"))? {
        builder = builder.add_root_certificate(cert);
    }
    builder.build().map_err(|e| format!("Could not set up TLS with the custom CA: {e}"))
}

/// A rustls client configuration trusting the system roots plus `pem` (used for LDAP).
/// `None` when there is no custom CA, so the caller keeps its library's default.
pub fn rustls_config(pem: &str) -> Result<Option<Arc<rustls::ClientConfig>>, String> {
    let extra = parse_ca(pem)?;
    if extra.is_empty() {
        return Ok(None);
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    for cert in extra {
        roots.add(cert).map_err(|e| format!("The CA certificate could not be read: {e}"))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("Could not set up TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(Arc::new(config)))
}
