use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;

/// Builds a rustls server config from PEM cert and key files.
pub fn load_server_config(cert_path: &Path, key_path: &Path) -> anyhow::Result<ServerConfig> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to configure the server TLS certificate")
}

/// Builds a rustls client config that trusts the PEM CA file, or `None` when
/// no CA is configured and the caller should keep its default trust.
pub fn load_client_config(ca_cert: Option<&Path>) -> anyhow::Result<Option<rustls::ClientConfig>> {
    let Some(ca_cert) = ca_cert else {
        return Ok(None);
    };
    let certs = load_certs(ca_cert)?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .context("the CA certificate is not a valid trust anchor")?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(config))
}

/// A reqwest client that trusts the PEM CA file when one is configured and
/// the default trust otherwise.
pub fn reqwest_client(ca_cert: Option<&Path>) -> anyhow::Result<reqwest::Client> {
    reqwest_client_with_tls(load_client_config(ca_cert)?.map(Arc::new))
}

/// A reqwest client using a prebuilt TLS config, or the default trust when
/// none is given.
pub fn reqwest_client_with_tls(
    tls: Option<Arc<rustls::ClientConfig>>,
) -> anyhow::Result<reqwest::Client> {
    match tls {
        Some(config) => Ok(reqwest::Client::builder()
            .use_preconfigured_tls((*config).clone())
            .build()
            .context("failed to build the HTTP client")?),
        None => Ok(reqwest::Client::new()),
    }
}

fn load_certs(path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut reader = std::io::BufReader::new(&bytes[..]);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .with_context(|| format!("failed to parse certificates from {}", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

fn load_key(path: &Path) -> anyhow::Result<PrivateKeyDer<'static>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut reader = std::io::BufReader::new(&bytes[..]);
    let key = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("failed to parse private key from {}", path.display()))?
        .with_context(|| format!("no private key found in {}", path.display()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use rcgen::BasicConstraints;
    use rcgen::CertificateParams;
    use rcgen::IsCa;
    use rcgen::KeyPair;

    use super::*;

    fn write_ca_and_leaf(
        dir: &std::path::Path,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        leaf_params.is_ca = IsCa::NoCa;
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let ca_path = dir.join("ca.pem");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();
        std::fs::write(&cert_path, leaf_cert.pem()).unwrap();
        std::fs::write(&key_path, leaf_key.serialize_pem()).unwrap();
        (ca_path, cert_path, key_path)
    }

    #[test]
    fn server_and_client_configs_round_trip_pem_files() {
        let dir = tempfile::tempdir().unwrap();
        let (ca_path, cert_path, key_path) = write_ca_and_leaf(dir.path());

        load_server_config(&cert_path, &key_path).expect("server config should load");
        load_client_config(Some(&ca_path))
            .expect("client config should load")
            .expect("a CA was configured");
    }

    #[test]
    fn no_ca_configures_default_trust() {
        assert!(load_client_config(None).unwrap().is_none());
    }

    #[test]
    fn missing_ca_file_is_an_error() {
        assert!(load_client_config(Some(std::path::Path::new("/nonexistent/ca.pem"))).is_err());
    }
}
