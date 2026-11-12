//! TLS for PostgreSQL with rustls (ring provider), in libpq's modes:
//!
//! - `disable`: no TLS.
//! - `prefer`: TLS when the server offers it, else plain; the certificate is not checked.
//! - `require`: TLS or fail; the certificate is not checked (libpq's `require` without a root
//!   certificate).
//! - `verify-full`: TLS, the certificate chain checked against the Mozilla roots plus the
//!   optional CA file, and the host name checked.

use std::path::Path;
use std::sync::Arc;

use quarry_core::TlsMode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::EngineError;

/// Accepts any certificate, but still checks the handshake signatures (libpq `require`).
#[derive(Debug)]
struct Unverified(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

fn tls_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Connect(format!("TLS: {e}"))
}

fn roots(ca_file: Option<&Path>) -> Result<RootCertStore, EngineError> {
    let mut roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    if let Some(path) = ca_file {
        let mut added = 0usize;
        for cert in CertificateDer::pem_file_iter(path).map_err(|e| tls_err(format!("cannot read the CA file: {e}")))? {
            let cert = cert.map_err(|e| tls_err(format!("bad certificate in the CA file: {e}")))?;
            roots.add(cert).map_err(tls_err)?;
            added += 1;
        }
        if added == 0 {
            return Err(tls_err("the CA file has no certificate"));
        }
    }
    Ok(roots)
}

/// The SSL mode and connector for a TLS mode (`None`: plain connection).
pub(crate) fn connector(mode: &TlsMode) -> Result<(SslMode, Option<MakeRustlsConnect>), EngineError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = || ClientConfig::builder_with_provider(Arc::clone(&provider)).with_safe_default_protocol_versions().map_err(tls_err);
    let unverified = |p: &CryptoProvider| -> Result<ClientConfig, EngineError> {
        Ok(builder()?.dangerous().with_custom_certificate_verifier(Arc::new(Unverified(p.signature_verification_algorithms))).with_no_client_auth())
    };
    Ok(match mode {
        TlsMode::Disable => (SslMode::Disable, None),
        TlsMode::Prefer => (SslMode::Prefer, Some(MakeRustlsConnect::new(unverified(&provider)?))),
        TlsMode::Require => (SslMode::Require, Some(MakeRustlsConnect::new(unverified(&provider)?))),
        TlsMode::VerifyFull { ca_file } => {
            let config = builder()?.with_root_certificates(roots(ca_file.as_deref())?).with_no_client_auth();
            (SslMode::Require, Some(MakeRustlsConnect::new(config)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_builds_and_bad_ca_files_are_refused() {
        for m in [TlsMode::Disable, TlsMode::Prefer, TlsMode::Require, TlsMode::VerifyFull { ca_file: None }] {
            assert!(connector(&m).is_ok(), "{m:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("ca.pem");
        std::fs::write(&empty, "not a cert").unwrap();
        assert!(connector(&TlsMode::VerifyFull { ca_file: Some(empty) }).is_err());
        assert!(connector(&TlsMode::VerifyFull { ca_file: Some(dir.path().join("missing.pem")) }).is_err());
    }
}
