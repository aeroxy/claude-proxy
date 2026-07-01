use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs;
use std::path::PathBuf;
use tracing::info;

use crate::config::ProxyConfig;

pub struct CaCert {
    pub cert: Certificate,
}

fn build_ca_params(key_pair: Option<KeyPair>) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new());
    params
        .distinguished_name
        .push(DnType::CommonName, "Claude Proxy CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Claude Proxy");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    if let Some(kp) = key_pair {
        params.key_pair = Some(kp);
    }
    params
}

fn load_ca_from_pem(cert_pem: &str, key_pem: &str) -> anyhow::Result<CaCert> {
    let key_pair = KeyPair::from_pem(key_pem)?;
    let params = CertificateParams::from_ca_cert_pem(cert_pem, key_pair)?;
    let cert = Certificate::from_params(params)?;
    Ok(CaCert { cert })
}

pub fn get_or_create_ca(cfg: &ProxyConfig) -> anyhow::Result<CaCert> {
    // User-supplied CA takes precedence.
    if let (Some(cert_path), Some(key_path)) = (&cfg.ca_cert_path, &cfg.ca_key_path) {
        let cert_pem = fs::read_to_string(cert_path)?;
        let key_pem = fs::read_to_string(key_path)?;
        info!("Loaded user-supplied CA from {:?}", cert_path);
        return load_ca_from_pem(&cert_pem, &key_pem);
    }

    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("claude-proxy");
    fs::create_dir_all(&config_dir)?;

    let cert_path = config_dir.join("ca.crt");
    let key_path = config_dir.join("ca.key");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = fs::read_to_string(&cert_path)?;
        let key_pem = fs::read_to_string(&key_path)?;
        info!("Loaded existing CA key from {:?}", key_path);
        return load_ca_from_pem(&cert_pem, &key_pem);
    }

    info!("Generating new CA cert at {:?}", cert_path);
    let params = build_ca_params(None);
    let cert = Certificate::from_params(params)?;

    fs::write(&cert_path, cert.serialize_pem()?)?;
    fs::write(&key_path, cert.serialize_private_key_pem())?;

    Ok(CaCert { cert })
}

pub fn generate_leaf_cert(
    ca: &CaCert,
    domain: &str,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let mut params = CertificateParams::new(vec![domain.to_string()]);
    params.distinguished_name.push(DnType::CommonName, domain);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let cert = Certificate::from_params(params)?;
    let cert_der = cert.serialize_der_with_signer(&ca.cert)?;

    let rustls_cert = CertificateDer::from(cert_der);
    let rustls_key = PrivateKeyDer::try_from(cert.serialize_private_key_der()).unwrap();

    Ok((vec![rustls_cert], rustls_key))
}
