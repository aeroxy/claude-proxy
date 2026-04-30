use rcgen::{Certificate, CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs;
use std::path::PathBuf;
use tracing::info;

pub struct CaCert {
    pub cert: Certificate,
}

pub fn get_or_create_ca() -> anyhow::Result<CaCert> {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("claude-proxy");
    fs::create_dir_all(&config_dir)?;

    let cert_path = config_dir.join("ca.crt");
    let key_path = config_dir.join("ca.key");

    if cert_path.exists() && key_path.exists() {
        let key_pem = fs::read_to_string(&key_path)?;
        let _cert_pem = fs::read_to_string(&cert_path)?;
        
        let key_pair = KeyPair::from_pem(&key_pem)?;
        
        // Use from_ca_cert_pem to parse an existing certificate in older rcgen
        // if not available, we can just load the key pair and generate a new CA cert each time for the same key pair
        
        let mut params = CertificateParams::new(vec!["Claude Proxy CA".to_string()]);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_pair = Some(key_pair);
        
        let cert = Certificate::from_params(params)?;
        
        info!("Loaded existing CA key from {:?}", key_path);
        return Ok(CaCert { cert });
    }

    info!("Generating new CA cert at {:?}", cert_path);
    let mut params = CertificateParams::new(vec!["Claude Proxy CA".to_string()]);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    
    let cert = Certificate::from_params(params)?;
    
    let cert_pem = cert.serialize_pem()?;
    let key_pem = cert.serialize_private_key_pem();
    fs::write(&cert_path, cert_pem)?;
    fs::write(&key_path, key_pem)?;

    Ok(CaCert { cert })
}

pub fn generate_leaf_cert(ca: &CaCert, domain: &str) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let params = CertificateParams::new(vec![domain.to_string()]);
    let cert = Certificate::from_params(params)?;
    
    let cert_der = cert.serialize_der_with_signer(&ca.cert)?;
    
    let rustls_cert = CertificateDer::from(cert_der);
    let rustls_key = PrivateKeyDer::try_from(cert.serialize_private_key_der()).unwrap();
    
    Ok((vec![rustls_cert], rustls_key))
}
