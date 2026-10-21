//! Persistent IRC client certificate and private key.

use std::{
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer,
    pem::{self, PemObject as _},
};
use sha2::{Digest as _, Sha256};

use crate::{
    chat::{ClientCertificateFingerprint, transport::ClientIdentity},
    identity::permissions::{self, Permissions},
};

/// A persistent client identity and its manifest fingerprint.
#[derive(Debug, Clone)]
pub(crate) struct Credentials {
    identity: ClientIdentity,
    fingerprint: ClientCertificateFingerprint,
}

impl Credentials {
    /// Loads an existing identity or creates it with private permissions.
    pub(crate) fn load_or_generate(path: &Path, common_name: &str) -> Result<Self, Error> {
        match Self::load(path) {
            Ok(credentials) => Ok(credentials),
            Err(error) if error.is_not_found() => Self::generate(path, common_name),
            Err(error) => Err(error),
        }
    }

    /// Loads and validates an existing identity.
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        let mut file = Permissions::open_private(path)?;
        let mut pem = Vec::new();
        file.read_to_end(&mut pem).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
        let certificates = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_source| Error::Pem { path: path.into() })?;
        let private_key = PrivateKeyDer::from_pem_slice(&pem).map_err(|source| match source {
            pem::Error::NoItemsFound => Error::MissingPrivateKey(path.into()),
            _source => Error::Pem { path: path.into() },
        })?;
        let certificate = certificates
            .first()
            .ok_or_else(|| Error::MissingCertificate(path.into()))?;
        let fingerprint =
            ClientCertificateFingerprint::from_digest(Sha256::digest(certificate.as_ref()).into());
        let identity = ClientIdentity::new(certificates, private_key)?;

        Ok(Self {
            identity,
            fingerprint,
        })
    }

    /// Returns the TLS client identity.
    pub(crate) fn identity(&self) -> ClientIdentity {
        self.identity.clone()
    }

    /// Returns the certificate fingerprint used by manifest attestations.
    pub(crate) const fn fingerprint(&self) -> ClientCertificateFingerprint {
        self.fingerprint
    }

    fn generate(path: &Path, common_name: &str) -> Result<Self, Error> {
        let generated = generate_simple_self_signed(vec![common_name.to_owned()])?;
        let pem = format!(
            "{}{}",
            generated.cert.pem(),
            generated.signing_key.serialize_pem()
        );
        let parent = path.parent().ok_or_else(|| Error::Io {
            path: path.into(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "identity path has no parent"),
        })?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".irc-identity-")
            .tempfile_in(parent)
            .map_err(|source| Error::Io {
                path: path.into(),
                source,
            })?;
        temporary
            .write_all(pem.as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| Error::Io {
                path: path.into(),
                source,
            })?;
        match temporary.persist_noclobber(path) {
            Ok(_file) => Self::load(path),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(error) => Err(Error::Io {
                path: path.into(),
                source: error.error,
            }),
        }
    }
}

impl Error {
    fn is_not_found(&self) -> bool {
        match self {
            Self::Io { source, .. } | Self::Permissions(permissions::Error::Io { source, .. }) => {
                source.kind() == io::ErrorKind::NotFound
            }
            _ => false,
        }
    }
}

/// Failure creating or loading an IRC client identity.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The identity file could not be accessed.
    #[error("failed to access IRC identity '{path}': {source}")]
    Io {
        /// Identity path.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// The identity file has unsafe ownership, permissions, or type.
    #[error(transparent)]
    Permissions(#[from] permissions::Error),
    /// PEM decoding failed. Parser diagnostics can contain private input bytes.
    #[error("failed to decode IRC identity '{path}'")]
    Pem {
        /// Identity path.
        path: PathBuf,
    },
    /// The identity contains no certificate.
    #[error("IRC identity '{0}' contains no certificate")]
    MissingCertificate(PathBuf),
    /// The identity contains no private key.
    #[error("IRC identity '{0}' contains no private key")]
    MissingPrivateKey(PathBuf),
    /// Certificate generation failed.
    #[error("failed to generate IRC client certificate: {0}")]
    Generate(#[from] rcgen::Error),
    /// The decoded TLS identity is invalid.
    #[error(transparent)]
    Transport(#[from] crate::chat::transport::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Barrier},
    };

    use crate::{
        chat::credentials::{Credentials, Error},
        identity::permissions::Permissions,
    };

    #[test]
    fn malformed_pem_diagnostics_do_not_disclose_private_input() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("irc-identity.pem");
        let canary = "private-material-must-not-appear";
        Permissions::write_private(&path, &format!("-----BEGIN {canary}\n"))
            .expect("malformed identity fixture");

        let error = Credentials::load(&path).expect_err("invalid PEM must be refused");
        assert!(matches!(error, Error::Pem { .. }));
        let diagnostic = format!("{error} {error:?}");
        let encoded = canary
            .as_bytes()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        assert!(!diagnostic.contains(canary));
        assert!(
            !diagnostic.contains(&encoded),
            "debug byte arrays also disclose the input"
        );
    }

    #[test]
    fn generates_and_reloads_the_same_private_identity() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let path = root.path().join("irc-identity.pem");
        let first =
            Credentials::load_or_generate(&path, "nico").expect("identity should be generated");
        let second =
            Credentials::load_or_generate(&path, "nico").expect("identity should be reloaded");

        assert_eq!(first.fingerprint(), second.fingerprint());
        assert!(
            !fs::read_to_string(path)
                .expect("identity should be readable")
                .is_empty()
        );
    }

    #[test]
    fn concurrent_generation_publishes_one_complete_identity() {
        let root = tempfile::tempdir().expect("temporary directory should be created");
        let path = Arc::new(root.path().join("irc-identity.pem"));
        let barrier = Arc::new(Barrier::new(4));
        let mut workers = Vec::new();
        for _worker in 0..4 {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                Credentials::load_or_generate(&path, "nico")
                    .expect("identity generation should converge")
                    .fingerprint()
            }));
        }
        let fingerprints = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker should complete"))
            .collect::<Vec<_>>();

        assert!(fingerprints.windows(2).all(|pair| pair[0] == pair[1]));
        Credentials::load(&path).expect("published identity should be complete");
    }
}
