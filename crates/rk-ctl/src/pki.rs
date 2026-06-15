//! Offline PKI for `rk-ctl pki init`: a self-signed CA, an API-server
//! certificate signed by it, and a random bearer token. This is everything the
//! hardening phase needs to turn on TLS and auth, produced with rcgen and the
//! kernel CSPRNG — no openssl, no network, nothing to copy off a real cluster.

use std::path::Path;

use anyhow::{Context as _, bail};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

/// Generate `ca.crt`, `ca.key`, `server.crt`, `server.key` and `token` into
/// `dir`. The server cert is valid for `127.0.0.1`, `localhost`, and every
/// `--api-name` the caller adds (the address clients will dial).
pub fn init(dir: &Path, api_names: &[String], force: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut sans = vec!["127.0.0.1".to_owned(), "localhost".to_owned()];
    for name in api_names {
        if !sans.contains(name) {
            sans.push(name.clone());
        }
    }

    // CA: self-signed, allowed to sign certificates.
    let ca_key = KeyPair::generate().context("generating CA key")?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "RustKube CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_key).context("self-signing CA")?;

    // API-server leaf, signed by the CA, for serverAuth on the SANs.
    let server_key = KeyPair::generate().context("generating server key")?;
    let mut server_params = CertificateParams::new(sans.clone()).context("server params")?;
    server_params
        .distinguished_name
        .push(DnType::CommonName, "rk-api");
    server_params.use_authority_key_identifier_extension = true;
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .context("signing server cert")?;

    let token = random_token().context("generating token")?;

    write_file(dir, "ca.crt", ca_cert.pem().as_bytes(), false, force)?;
    write_file(
        dir,
        "ca.key",
        ca_key.serialize_pem().as_bytes(),
        true,
        force,
    )?;
    write_file(
        dir,
        "server.crt",
        server_cert.pem().as_bytes(),
        false,
        force,
    )?;
    write_file(
        dir,
        "server.key",
        server_key.serialize_pem().as_bytes(),
        true,
        force,
    )?;
    write_file(dir, "token", format!("{token}\n").as_bytes(), true, force)?;

    let dir = dir.display();
    println!("wrote PKI to {dir} (SANs: {})", sans.join(", "));
    println!(
        "  rk-api:  authTokenFile: {dir}/token, tls.certFile: {dir}/server.crt, tls.keyFile: {dir}/server.key"
    );
    println!("  clients: tokenFile: {dir}/token, caFile: {dir}/ca.crt");
    Ok(())
}

/// 32 random bytes (256 bits) from the kernel CSPRNG, hex-encoded — the same
/// source the API server uses for object uids.
fn random_token() -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write a PKI file, refusing to clobber unless `force`. Secrets (keys, token)
/// are written `0600` so they aren't world-readable.
fn write_file(
    dir: &Path,
    name: &str,
    contents: &[u8],
    secret: bool,
    force: bool,
) -> anyhow::Result<()> {
    let path = dir.join(name);
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    if secret {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("setting 0600 on {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_256_bits_of_hex() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 64, "32 bytes -> 64 hex chars");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(random_token().unwrap(), token, "tokens differ");
    }

    #[test]
    fn init_writes_a_usable_bundle() {
        let dir = std::env::temp_dir().join(format!("rk-pki-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        init(
            &dir,
            &["10.0.0.5".to_owned(), "rk-api.local".to_owned()],
            false,
        )
        .unwrap();

        for name in ["ca.crt", "ca.key", "server.crt", "server.key", "token"] {
            assert!(dir.join(name).exists(), "{name} written");
        }
        let ca = std::fs::read_to_string(dir.join("ca.crt")).unwrap();
        assert!(ca.starts_with("-----BEGIN CERTIFICATE-----"));
        let token = std::fs::read_to_string(dir.join("token")).unwrap();
        assert_eq!(token.trim().len(), 64);

        // A second run without --force must refuse rather than clobber.
        let err = init(&dir, &[], false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // With --force it succeeds.
        init(&dir, &[], true).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
