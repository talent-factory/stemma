//! The roots of trust of a connection with TLS (ADR-028): the Mozilla roots that are compiled in,
//! or the certificates of the CA file of a remote.

use std::fs;
use std::path::Path;

use rustls::pki_types::{pem, CertificateDer};
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
/// takes as a root. A private key and a section of another kind in the file are skipped.
///
/// Each certificate is checked here, because `ureq` would drop one that rustls does not take
/// without an error, and every handshake would then fail as if the hub were at fault.
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
        match item.map_err(|error| invalid(pem_problem(&error)))? {
            PemItem::Certificate(certificate) => certificates.push(certificate),
            _ => continue,
        }
        let number = certificates.len();
        let der = CertificateDer::from(certificates[number - 1].der().to_vec());
        RootCertStore::empty().add(der).map_err(|error| {
            invalid(format!(
                "certificate {number} of the file is no root for rustls: {error}"
            ))
        })?;
    }
    if certificates.is_empty() {
        return Err(invalid("the file holds no certificate in PEM".to_owned()));
    }
    Ok(certificates)
}

/// What is wrong with a section of the file, in words: the errors of PEM print their bytes as
/// a list of numbers.
fn pem_problem(error: &ureq::Error) -> String {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).escape_debug().to_string();
    match error {
        ureq::Error::Pem(pem::Error::MissingSectionEnd { end_marker }) => {
            format!("a section `{}` has no line that ends it", text(end_marker))
        }
        ureq::Error::Pem(pem::Error::IllegalSectionStart { line }) => {
            format!("the line `{}` begins no section of PEM", text(line))
        }
        ureq::Error::Pem(pem::Error::Base64Decode(detail)) => {
            format!("a section is no base64: {detail}")
        }
        ureq::Error::Pem(pem::Error::SectionTooLarge) => "a section is too large".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn test_ca() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/test-ca/ca.pem")
    }

    #[test]
    fn a_ca_file_replaces_the_compiled_in_roots() {
        assert!(matches!(
            config(None).unwrap().root_certs(),
            RootCerts::WebPki
        ));
        let with_file = config(Some(&test_ca())).unwrap();
        let RootCerts::Specific(roots) = with_file.root_certs() else {
            panic!("expected the certificates of the file alone");
        };
        assert_eq!(roots.len(), 1);
    }
}
