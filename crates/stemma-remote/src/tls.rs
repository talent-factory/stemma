//! The roots of trust of a connection with TLS (ADR-028): the Mozilla roots that are compiled in,
//! or the certificates of the CA file of a remote.

use std::fs;
use std::path::Path;

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

/// The certificates of a CA file in PEM, at least one. A private key in the file is skipped.
fn certificates(path: &Path) -> Result<Vec<Certificate<'static>>, ClientError> {
    let bytes = fs::read(path).map_err(|error| ClientError::CaFileRead {
        path: path.to_owned(),
        error,
    })?;
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(&bytes) {
        match item {
            Ok(PemItem::Certificate(certificate)) => certificates.push(certificate),
            Ok(_) => {}
            Err(error) => {
                return Err(ClientError::CaFileInvalid {
                    path: path.to_owned(),
                    detail: error.to_string(),
                })
            }
        }
    }
    if certificates.is_empty() {
        return Err(ClientError::CaFileInvalid {
            path: path.to_owned(),
            detail: "the file holds no certificate in PEM".to_owned(),
        });
    }
    Ok(certificates)
}
