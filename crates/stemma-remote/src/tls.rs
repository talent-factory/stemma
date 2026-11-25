//! The roots of trust of a connection with TLS (ADR-028): the Mozilla roots that are compiled in,
//! or the certificates of the CA file of a remote.

use std::fs;
use std::path::Path;

use rustls::pki_types::CertificateDer;
use rustls::RootCertStore;
use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig, TlsProvider};

use crate::client::ClientError;

/// The configuration of TLS: rustls with the compiled-in roots, or with the certificates of
/// `ca_file` alone.
pub(crate) fn config(ca_file: Option<&Path>) -> Result<TlsConfig, ClientError> {
    let roots = match ca_file {
        None => RootCerts::WebPki,
        Some(path) => RootCerts::new_with_certs(&certificates(path)?),
    };
    Ok(TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(roots)
        .build())
}

/// The certificates of a CA file in PEM: at least one, and each one a certificate that rustls
/// takes as a root. A private key and a section of another kind in the file are skipped;
/// `ureq` would otherwise drop a certificate that rustls does not take without a word, and the
/// handshake would fail as if the hub were at fault.
fn certificates(path: &Path) -> Result<Vec<Certificate<'static>>, ClientError> {
    let invalid = |detail: String| ClientError::CaFileInvalid {
        path: path.to_owned(),
        detail,
    };
    let bytes = fs::read(path).map_err(|error| ClientError::CaFileRead {
        path: path.to_owned(),
        error,
    })?;
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(&bytes) {
        match item.map_err(|error| invalid(error.to_string()))? {
            PemItem::Certificate(certificate) => certificates.push(certificate),
            _ => continue,
        }
        let number = certificates.len();
        let der = CertificateDer::from(certificates[number - 1].der().to_vec());
        RootCertStore::empty().add(der).map_err(|error| {
            invalid(format!(
                "certificate {number} of the file cannot be used: {error}"
            ))
        })?;
    }
    if certificates.is_empty() {
        return Err(invalid("the file holds no certificate in PEM".to_owned()));
    }
    Ok(certificates)
}
