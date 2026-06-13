//! rustls client setup using the pure-Rust RustCrypto provider, so the
//! static musl / Android build needs no C toolchain (no `ring`, no
//! `aws-lc-rs`).
//!
//! Server certificates are checked against the Mozilla roots embedded via
//! `webpki-roots` (we don't touch the device's own trust store, which
//! varies by vendor). A verification failure only **warns** and proceeds —
//! the transport stays encrypted but unauthenticated. That's a deliberate
//! choice for read-only image analysis; integrity is expected to come from
//! a separate checksum on the download, not the TLS chain.

use crate::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme,
    StreamOwned,
};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};

/// One process-wide client config (building the verifier parses every root,
/// so we do it once).
fn config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let provider = Arc::new(rustls_rustcrypto::provider());
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .expect("build webpki verifier from embedded roots");
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("default TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(WarnVerifier(webpki)))
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

/// Begin a TLS session over an established TCP connection to `host`.
pub fn connect(tcp: TcpStream, host: &str) -> Result<StreamOwned<ClientConnection, TcpStream>> {
    let server = ServerName::try_from(host.to_string())
        .map_err(|_| Error::Http(format!("invalid TLS server name: {host}")))?;
    let conn = ClientConnection::new(config(), server)
        .map_err(|e| Error::Http(format!("TLS init: {e}")))?;
    Ok(StreamOwned::new(conn, tcp))
}

/// Wraps the real webpki verifier: a chain that fails to validate against
/// the embedded roots is accepted with a warning rather than aborting the
/// connection. The handshake signature checks are delegated unchanged, so
/// the peer still has to prove it holds the presented certificate's key.
#[derive(Debug)]
struct WarnVerifier(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for WarnVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Ok(verified) => Ok(verified),
            Err(e) => {
                eprintln!(
                    "lbx: warning: TLS certificate not trusted for {server_name:?}: {e} \
                     (proceeding without verification)"
                );
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
