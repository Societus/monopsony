use std::path::Path;
use std::sync::Arc;

use pnet::datalink;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

/// Manages the Monopsony private Certificate Authority and all derived certificates.
///
/// On first run, generates a self-signed CA key pair and persists it to disk.
/// On subsequent runs, loads the existing CA from the provided paths. The CA is
/// used to sign per-client and per-server leaf certificates, enabling mTLS
/// authentication between the server and all clients without relying on an
/// external CA.
pub struct TlsManager {
    /// DER-encoded CA certificate, used as the trust anchor for both server and
    /// client TLS verification.
    ca_cert_der: CertificateDer<'static>,
    /// Parsed CA certificate (rcgen), used as the signing authority for leaf
    /// certificates produced by [`generate_client_cert`](Self::generate_client_cert)
    /// and [`generate_server_cert`](Self::generate_server_cert).
    ca_cert: rcgen::Certificate,
    /// CA private key, kept in memory for signing operations.
    ca_key: KeyPair,
    /// PEM-encoded CA certificate string. Distributed to clients so they can
    /// verify the server's leaf certificate during the TLS handshake.
    ca_cert_pem: String,
}

impl TlsManager {
    /// Return the PEM-encoded CA certificate bytes.
    pub fn ca_cert_pem_bytes(&self) -> Vec<u8> {
        self.ca_cert_pem.as_bytes().to_vec()
    }

    /// Return the DER-encoded CA certificate bytes (trust anchor).
    pub fn ca_cert_der_bytes(&self) -> CertificateDer<'static> {
        self.ca_cert_der.clone()
    }
    /// Load an existing CA from disk, or generate a new one if neither file exists.
    ///
    /// - `ca_cert_path` — PEM file for the CA certificate.
    /// - `ca_key_path`  — PEM file for the CA private key.
    ///
    /// Both files must be present for loading; if either is missing, a fresh CA
    /// is generated and written to both paths (creating parent directories as
    /// needed).
    pub fn load_or_create(ca_cert_path: &Path, ca_key_path: &Path) -> anyhow::Result<Self> {
        if ca_cert_path.exists() && ca_key_path.exists() {
            info!("Loading existing CA from {:?}", ca_cert_path);
            Self::load_ca(ca_cert_path, ca_key_path)
        } else {
            info!("Generating new CA at {:?}", ca_cert_path);
            Self::generate_ca(ca_cert_path, ca_key_path)
        }
    }

    /// Deserialize a previously persisted CA from PEM files on disk.
    ///
    /// **Implementation note:** We parse the original DER directly from the PEM
    /// and use *those* bytes as the trust anchor (`ca_cert_der`), rather than
    /// re-deriving them from a reconstructed rcgen `Certificate`. The rcgen
    /// round-trip (`from_ca_cert_pem` → `self_signed`) generates a new serial
    /// and may encode the subject DN differently, causing leaf certs to carry
    /// an issuer DN that doesn't byte-match the original CA's subject DN —
    /// which webpki rejects as `UnknownIssuer`.
    ///
    /// We reconstruct the rcgen `Certificate` only for signing operations, and
    /// explicitly restore the CA attributes (`is_ca`, `key_usages`, DN) to
    /// match [`generate_ca`](Self::generate_ca) exactly.
    fn load_ca(ca_cert_path: &Path, ca_key_path: &Path) -> anyhow::Result<Self> {
        let ca_cert_pem = std::fs::read_to_string(ca_cert_path)?;
        let ca_key_pem = std::fs::read_to_string(ca_key_path)?;

        let ca_key = KeyPair::from_pem(&ca_key_pem)?;

        // Parse the original DER directly from the PEM to preserve exact bytes.
        let mut pem_reader = ca_cert_pem.as_bytes();
        let original_der: CertificateDer<'static> = rustls_pemfile::certs(&mut pem_reader)
            .next()
            .ok_or_else(|| anyhow::anyhow!("No certificate found in CA PEM file"))??
            .into_owned();

        // Reconstruct rcgen params from the original DER so we can sign leaf
        // certs. Explicitly restore CA attributes that may be lost in round-trip.
        let mut params = CertificateParams::from_ca_cert_der(&original_der)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        // Restore the exact DN used by generate_ca to guarantee issuer DN match.
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Monopsony CA");
        params
            .distinguished_name
            .push(rcgen::DnType::OrganizationName, "Monopsony");
        let ca_cert = params.self_signed(&ca_key)?;

        // Use the ORIGINAL DER as the trust anchor — not the reconstructed cert.
        Ok(Self {
            ca_cert_der: original_der,
            ca_cert,
            ca_key,
            ca_cert_pem,
        })
    }

    /// Generate a new self-signed CA certificate and private key.
    ///
    /// The CA is marked with `is_ca = true` and `BasicConstraints::Unconstrained`,
    /// so it can sign an arbitrary number of leaf certificates. Both the
    /// certificate and key are written to `ca_cert_path` and `ca_key_path`
    /// respectively.
    ///
    /// Subject DN: `CN=Monopsony CA, O=Monopsony`
    /// Valid until: 2036-01-01
    fn generate_ca(ca_cert_path: &Path, ca_key_path: &Path) -> anyhow::Result<Self> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Monopsony CA");
        params
            .distinguished_name
            .push(rcgen::DnType::OrganizationName, "Monopsony");
        params.not_after = rcgen::date_time_ymd(2036, 1, 1);

        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;

        if let Some(parent) = ca_cert_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ca_cert_pem = cert.pem();
        std::fs::write(ca_cert_path, ca_cert_pem.as_bytes())?;
        std::fs::write(ca_key_path, key.serialize_pem().as_bytes())?;

        info!("CA certificate generated and saved");

        let ca_cert_der = CertificateDer::from(cert.der().as_ref().to_vec());

        Ok(Self {
            ca_cert_der,
            ca_cert: cert,
            ca_key: key,
            ca_cert_pem,
        })
    }

    /// Return the PEM-encoded CA certificate, suitable for distribution to
    /// clients or writing to a trust-store file.
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Build a [`tonic::transport::ServerTlsConfig`] ready to be passed to
    /// `tonic::transport::Server::tls_config()`.
    ///
    /// Generates an ephemeral server leaf certificate (SAN: `monopsony-server`)
    /// signed by the CA and configures the server to require and verify client
    /// certificates against the CA trust anchor, i.e. mTLS.
    ///
    /// **Note:** This returns a tonic-level config. For a raw `rustls::ServerConfig`
    /// (e.g. when using axum/hyper directly), use [`server_tls_config`](Self::server_tls_config).
    pub fn tonic_server_tls_config(
        &self,
        bind_addr: Option<std::net::IpAddr>,
    ) -> anyhow::Result<tonic::transport::ServerTlsConfig> {
        self.tonic_server_tls_config_with_sans(bind_addr, &[])
    }

    /// Build a ServerTlsConfig with additional SAN entries from config.
    pub fn tonic_server_tls_config_with_sans(
        &self,
        bind_addr: Option<std::net::IpAddr>,
        additional_sans: &[String],
    ) -> anyhow::Result<tonic::transport::ServerTlsConfig> {
        let (cert_pem, key_pem) = self.generate_server_cert_with_sans(bind_addr, additional_sans)?;
        let cert_str = String::from_utf8(cert_pem)?;
        let key_str = String::from_utf8(key_pem)?;
        let chain = format!("{}\n{}", cert_str, self.ca_cert_pem);
        let identity = tonic::transport::Identity::from_pem(&chain, &key_str);
        let ca_cert = tonic::transport::Certificate::from_pem(&self.ca_cert_pem);
        Ok(tonic::transport::ServerTlsConfig::new()
            .identity(identity)
            .client_ca_root(ca_cert))
    }

    /// Generate an in-memory server leaf certificate signed by the CA.
    pub fn generate_server_cert(
        &self,
        bind_addr: Option<std::net::IpAddr>,
    ) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        self.generate_server_cert_with_sans(bind_addr, &[])
    }

    /// Generate a server leaf certificate with additional SAN entries.
    pub fn generate_server_cert_with_sans(
        &self,
        bind_addr: Option<std::net::IpAddr>,
        additional_sans: &[String],
    ) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let mut san_types: Vec<rcgen::SanType> =
            vec![rcgen::SanType::DnsName("monopsony-server".try_into()?)];

        if let Some(ip) = bind_addr {
            if ip.is_unspecified() {
                for iface in pnet::datalink::interfaces() {
                    for iface_ip in iface.ips {
                        let addr = iface_ip.ip();
                        let is_link_local = match addr {
                            std::net::IpAddr::V4(v4) => v4.is_link_local(),
                            std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
                        };
                        if !addr.is_loopback() && !is_link_local {
                            san_types.push(rcgen::SanType::IpAddress(addr));
                        }
                    }
                }
            } else {
                san_types.push(rcgen::SanType::IpAddress(ip));
            }
        }

        for san in additional_sans {
            if let Ok(ip) = san.parse::<std::net::IpAddr>() {
                san_types.push(rcgen::SanType::IpAddress(ip));
            } else {
                san_types.push(rcgen::SanType::DnsName(san.as_str().try_into()?));
            }
        }

        // Deduplicate
        let mut seen = std::collections::HashSet::new();
        san_types.retain(|san| match san {
            rcgen::SanType::DnsName(name) => seen.insert(format!("dns:{}", name)),
            rcgen::SanType::IpAddress(ip) => seen.insert(format!("ip:{}", ip)),
            _ => true,
        });

        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.subject_alt_names = san_types;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Monopsony Server");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_after = rcgen::date_time_ymd(2036, 1, 1);
        params.is_ca = IsCa::ExplicitNoCa;
        params.use_authority_key_identifier_extension = true;

        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.ca_cert, &self.ca_key)?;

        Ok((cert.pem().into_bytes(), key.serialize_pem().into_bytes()))
    }

    /// Build a [`rustls::ServerConfig`] from on-disk certificate and key files,
    /// enforcing mTLS via [`WebPkiClientVerifier`].
    ///
    /// The server presents the leaf certificate at `cert_path` / `key_path` and
    /// requires connecting clients to present a certificate signed by the CA.
    #[allow(dead_code)] // Alternative TLS config path for custom cert setups
    pub fn server_tls_config(
        &self,
        cert_path: &Path,
        key_path: &Path,
    ) -> anyhow::Result<ServerConfig> {
        let cert_pem = std::fs::read(cert_path)?;
        let key_pem = std::fs::read(key_path)?;

        let certs: Vec<CertificateDer<'_>> =
            rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<Vec<_>, _>>()?;

        let key_item = rustls_pemfile::read_one(&mut &key_pem[..])?
            .ok_or_else(|| anyhow::anyhow!("No private key in server key file"))?;

        let key_der = match key_item {
            rustls_pemfile::Item::Pkcs8Key(k) => k.into(),
            rustls_pemfile::Item::Pkcs1Key(k) => k.into(),
            rustls_pemfile::Item::Sec1Key(k) => k.into(),
            _ => return Err(anyhow::anyhow!("Unexpected key type in server key file")),
        };

        let mut root_store = RootCertStore::empty();
        root_store.add(self.ca_cert_der.clone())?;

        let client_verifier = WebPkiClientVerifier::builder(Arc::new(root_store)).build()?;

        let config = ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs.into_iter().map(|c| c.into_owned()).collect(), key_der)?;

        Ok(config)
    }

    /// Build a [`rustls::ClientConfig`] that presents a client certificate and
    /// trusts the CA as the root for server verification (mTLS client side).
    ///
    /// - `client_cert` — DER-encoded client leaf certificate.
    /// - `client_key`  — DER-encoded client private key.
    #[allow(dead_code)] // Alternative TLS config path for custom client certs
    pub fn client_tls_config(
        &self,
        client_cert: CertificateDer<'static>,
        client_key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> anyhow::Result<ClientConfig> {
        let mut root_store = RootCertStore::empty();
        root_store.add(self.ca_cert_der.clone())?;

        let config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(vec![client_cert], client_key)?;

        Ok(config)
    }

    /// Return the SHA-256 fingerprint of the CA certificate as colon-separated
    /// hex octets (e.g. `a1:b2:c3:...`). Useful for manual verification or
    /// out-of-band comparison when onboarding a new client.
    pub fn ca_fingerprint(&self) -> String {
        let hash = Sha256::digest(self.ca_cert_der.as_ref());
        hash.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Detect the LAN IP address for certificate SAN inclusion.
    /// Returns the first non-loopback, non-link-local IPv4 address.
    fn detect_lan_ip() -> anyhow::Result<std::net::IpAddr> {
        for iface in pnet::datalink::interfaces() {
            for ip in iface.ips {
                let addr = ip.ip();
                if let std::net::IpAddr::V4(ipv4) = addr {
                    if !ipv4.is_loopback() && !ipv4.is_link_local() {
                        return Ok(std::net::IpAddr::V4(ipv4));
                    }
                }
            }
        }
        Err(anyhow::anyhow!("No suitable LAN IP found"))
    }

    /// Generate a client leaf certificate signed by the CA.
    ///
    /// - `client_id` — Used as both the SAN and the Common Name in the
    ///   certificate subject, typically a unique identifier for the client
    ///   machine (hostname or UUID).
    ///
    /// EKU is set to `clientAuth`. Valid until 2036-01-01.
    ///
    /// Includes SubjectKeyIdentifier and AuthorityKeyIdentifier extensions
    /// required for proper chain validation by webpki/rustls.
    ///
    /// Returns the signed [`rcgen::Certificate`] and its [`KeyPair`]; use
    /// [`cert_and_key_to_pem`](Self::cert_and_key_to_pem) to serialize them
    /// for transport or storage.
    pub fn generate_client_cert(
        &self,
        client_id: &str,
    ) -> anyhow::Result<(rcgen::Certificate, KeyPair)> {
        let mut params = CertificateParams::new(vec![client_id.to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, client_id);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_after = rcgen::date_time_ymd(2036, 1, 1);
        // Use ExplicitNoCa to ensure SubjectKeyIdentifier extension is included
        params.is_ca = IsCa::ExplicitNoCa;
        // Enable AuthorityKeyIdentifier extension for proper chain building
        params.use_authority_key_identifier_extension = true;

        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.ca_cert, &self.ca_key)?;

        debug!(client_id = client_id, "Generated client certificate");
        Ok((cert, key))
    }

    /// Convenience helper to serialize an `rcgen` certificate and key pair into
    /// PEM byte vectors suitable for writing to disk or sending over gRPC.
    ///
    /// Returns `(cert_pem_bytes, key_pem_bytes)`.
    pub fn cert_and_key_to_pem(cert: &rcgen::Certificate, key: &KeyPair) -> (Vec<u8>, Vec<u8>) {
        (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
    }
}
