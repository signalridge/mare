//! Stage B TLS acceptor helper using `rustls`.

use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// Load PEM certificate chain and private key into a `tokio_rustls::TlsAcceptor`.
pub(super) fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| format!("cannot read tls_cert {}: {e}", cert_path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid tls_cert PEM {}: {e}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!("tls_cert {} contains no certificates", cert_path.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| format!("cannot read tls_key {}: {e}", key_path.display()))?;
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("invalid TLS cert/key pair: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(cfg)))
}
