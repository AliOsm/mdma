use super::*;
use std::assert_matches;
use base64::Engine;

fn pem(label: &str, bytes: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[test]
fn native_pem_reader_keeps_chain_order_and_ignores_unrelated_sections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.pem");
    let data = pem("CERTIFICATE", &[1, 2, 3])
        + &pem("PRIVATE KEY", &[9, 8, 7])
        + &pem("CERTIFICATE", &[4, 5, 6]);
    std::fs::write(&path, data).unwrap();
    let chain = load_certs(&path).unwrap();
    assert_eq!(
        chain.iter().map(std::convert::AsRef::as_ref).collect::<Vec<_>>(),
        [&[1, 2, 3], &[4, 5, 6]]
    );
}

#[test]
fn native_pem_reader_accepts_all_supported_key_containers_and_uses_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.pem");
    for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"] {
        std::fs::write(
            &path,
            pem("CERTIFICATE", &[0]) + &pem(label, &[1, 2, 3]) + &pem("PRIVATE KEY", &[4, 5, 6]),
        )
        .unwrap();
        let key = load_private_key(&path).unwrap();
        assert_eq!(key.secret_der(), &[1, 2, 3]);
        assert_matches!(
            (&key, label),
            (PrivateKeyDer::Pkcs8(_), "PRIVATE KEY")
                | (PrivateKeyDer::Pkcs1(_), "RSA PRIVATE KEY")
                | (PrivateKeyDer::Sec1(_), "EC PRIVATE KEY")
        );
    }
}

#[test]
fn missing_empty_and_malformed_pem_credentials_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.pem");
    assert!(load_certs(&path).is_err());
    assert!(load_private_key(&path).is_err());
    std::fs::write(&path, "").unwrap();
    assert!(
        load_certs(&path)
            .unwrap_err()
            .to_string()
            .contains("no certificates found")
    );
    assert!(
        load_private_key(&path)
            .unwrap_err()
            .to_string()
            .contains("no private key found")
    );
    for label in ["CERTIFICATE", "PRIVATE KEY"] {
        std::fs::write(
            &path,
            format!("-----BEGIN {label}-----\n!not-base64!\n-----END {label}-----\n"),
        )
        .unwrap();
        assert!(load_certs(&path).is_err());
        assert!(load_private_key(&path).is_err());
    }
}

#[test]
fn spki_pin_reads_the_leaf_of_a_pem_file() {
    let dir = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap().self_signed(&key).unwrap();
    let path = dir.path().join("cert.pem");
    std::fs::write(&path, cert.pem()).unwrap();
    let pin = spki_pin(&path).unwrap();
    assert_eq!(pin, crate::federation::tls::certificate_spki_sha256(cert.der()).unwrap());
    assert!(pin.starts_with("sha256:") && pin.len() == 71, "{pin}");
    std::fs::write(&path, "not a certificate").unwrap();
    assert!(spki_pin(&path).unwrap_err().to_string().contains("no certificates found"));
}
