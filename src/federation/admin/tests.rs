use super::*;
use crate::federation::runtime::Prepared;

/// A client CA and its leaf, as `mdma-certs` makes for the client listener.
struct Client {
    root: CertificateDer<'static>,
    leaf: CertificateDer<'static>,
    pem: String,
}

fn client() -> Client {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let leaf = params
        .signed_by(&KeyPair::generate().unwrap(), &issuer)
        .unwrap();
    Client {
        root: issuer.der().clone(),
        leaf: leaf.der().clone(),
        pem: leaf.pem(),
    }
}

/// What `mdma-serve` does with the directory at startup.
fn serve(dir: &Path, client: &Client) -> Result<Prepared, BoxError> {
    let (_, config) = load(dir)?;
    Prepared::load(
        config,
        "127.0.0.1:6362".parse()?,
        std::slice::from_ref(&client.root),
        std::slice::from_ref(&client.leaf),
    )
}

fn node(root: &tempfile::TempDir, name: &str) -> PathBuf {
    let dir = root.path().join(name);
    init(
        &dir,
        &format!("{name}.fleet.test"),
        &format!("https://{name}.lan:6365"),
        30,
    )
    .unwrap();
    dir
}

fn bundle(dir: &Path) -> Bundle {
    let bytes = serde_json::to_vec(&export(dir).unwrap()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn error<T: std::fmt::Debug>(result: Result<T, BoxError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn init_makes_a_private_node_that_serves_and_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let dir = node(&root, "a");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in [
            "root.pem",
            "tls.pem",
            "tls-key.pem",
            "catalog.pk8",
            "runtime.toml",
        ] {
            assert_eq!(
                std::fs::metadata(dir.join(file))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{file}"
            );
        }
    }
    let client = client();
    drop(serve(&dir, &client).unwrap());
    assert!(error(init(&dir, "a.fleet.test", "https://a.lan:6365", 30)).contains("exists"));
    for (id, endpoint, days, why) in [
        ("A.fleet", "https://a.lan:6365", 30, "uppercase id"),
        ("b.fleet.test", "https://b.lan", 30, "no port"),
        ("b.fleet.test", "https://b.lan:6365", 0, "no days"),
        (
            "b.fleet.test",
            "https://b.lan:6365",
            MAX_DAYS + 1,
            "too many days",
        ),
    ] {
        assert!(
            init(&root.path().join(why), id, endpoint, days).is_err(),
            "{why}"
        );
    }
}

#[test]
fn enrolled_peers_get_their_grants_and_keep_their_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let from_a = bundle(&a);
    let grants = Grants {
        catalog: vec!["native".into()],
        execute: vec![ExecuteExport {
            model: "native".into(),
            operation: Operation::Synthesize,
        }],
    };
    let peer = enroll(&b, &from_a, Some("https://127.0.0.1:7000"), grants).unwrap();
    assert_eq!(
        (peer.endpoint.as_str(), peer.catalog_exports.as_slice()),
        ("https://127.0.0.1:7000", &["native".to_owned()][..])
    );
    let (_, config) = load(&b).unwrap();
    assert!(config.enable_execution, "execute grants turn execution on");
    let shown = status(&b).unwrap();
    assert!(
        shown.contains("peer     a.fleet.test Active at https://127.0.0.1:7000 (29d left)"),
        "{shown}"
    );
    assert!(shown.contains("runs     native (synthesize)"), "{shown}");
    drop(serve(&b, &client()).unwrap());

    // The same bundle again replaces the grants.
    enroll(&b, &from_a, None, Grants::default()).unwrap();
    assert!(!status(&b).unwrap().contains("runs"));

    let mut tampered = from_a.clone();
    tampered.root_pem = bundle(&b).root_pem;
    assert!(
        error(enroll(&b, &tampered, None, Grants::default())).contains("does not match its pin")
    );
    let mut granted = from_a;
    granted.peer.catalog_exports = vec!["native".into()];
    assert!(error(enroll(&b, &granted, None, Grants::default())).contains("no grants"));
    assert!(error(enroll(&b, &bundle(&b), None, Grants::default())).contains("itself"));
    std::fs::remove_dir_all(&a).unwrap();
    let a = node(&root, "a");
    assert!(error(enroll(&b, &bundle(&a), None, Grants::default())).contains("another root"));
    std::fs::remove_file(b.join("peers/a.fleet.test.pem")).unwrap();
    assert!(error(enroll(&b, &bundle(&a), None, Grants::default())).contains("other credentials"));
}

#[test]
fn routes_import_ready_models_for_one_client_and_replace_the_peers_earlier_routes() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, c) = (node(&root, "a"), node(&root, "b"), node(&root, "c"));
    enroll(&a, &bundle(&b), None, Grants::default()).unwrap();
    enroll(&a, &bundle(&c), None, Grants::default()).unwrap();
    let client = client();
    let caller = tls::certificate_spki_sha256(&client.leaf).unwrap();
    let mut models = crate::federation::inventory::tests::native_service()
        .federation_inventory()
        .snapshot(&["native".into(), "alias".into()])
        .models;
    let routed = |peer: &str, models: Vec<ModelDescriptor>, prefix: &str| {
        let (dir, config) = load(&a).unwrap();
        let store = open_store(&config).unwrap();
        apply_routes(&dir, config, &store, peer, models, &caller, prefix)
    };

    let routes = routed("b.fleet.test", models.clone(), "b").unwrap();
    assert_eq!(
        routes.iter().map(|r| r.alias.as_str()).collect::<Vec<_>>(),
        ["b-native", "b-alias"]
    );
    drop(serve(&a, &client).unwrap());
    let shown = status(&a).unwrap();
    assert!(
        shown.contains("route    b-native -> b.fleet.test native (synthesize, 1 client(s))"),
        "{shown}"
    );

    models[1].ready = false;
    routed("b.fleet.test", models.clone(), "b").unwrap();
    let (_, config) = load(&a).unwrap();
    assert_eq!(
        config
            .routes
            .iter()
            .map(|r| r.alias.as_str())
            .collect::<Vec<_>>(),
        ["b-native"],
        "unready models are not routed"
    );
    assert!(!status(&a).unwrap().contains("b-alias"));

    assert!(error(routed("c.fleet.test", models.clone(), "b")).contains("another peer"));
    routed("c.fleet.test", models.clone(), "c").unwrap();
    models[0].ready = false;
    assert!(error(routed("c.fleet.test", models, "c")).contains("no ready models"));
    drop(serve(&a, &client).unwrap());
}

#[test]
fn status_reads_a_running_node_and_changes_wait_for_it_to_stop() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let running = serve(&a, &client()).unwrap();
    assert!(
        status(&a)
            .unwrap()
            .starts_with("node     a.fleet.test at https://a.lan:6365")
    );
    assert!(error(export(&a)).contains("stop mdma-serve first"));
    assert!(
        error(enroll(&a, &bundle(&b), None, Grants::default())).contains("stop mdma-serve first")
    );
    drop(running);
    export(&a).unwrap();
}

#[tokio::test]
async fn routes_name_a_peer_that_cannot_be_dialed() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    enroll(
        &a,
        &bundle(&b),
        Some(&format!("https://{closed}")),
        Grants::default(),
    )
    .unwrap();
    let cert = root.path().join("client.pem");
    std::fs::write(&cert, client().pem).unwrap();

    let refused = routes(&a, "b.fleet.test", &cert, "b").await.unwrap_err();
    let unreachable = refused
        .downcast_ref::<Unreachable>()
        .unwrap_or_else(|| panic!("{refused}"));
    assert_eq!(unreachable.peer, "b.fleet.test");

    let running = serve(&a, &client()).unwrap();
    let busy = routes(&a, "b.fleet.test", &cert, "b").await.unwrap_err();
    assert!(
        busy.downcast_ref::<Unreachable>().is_none()
            && busy.to_string().contains("stop mdma-serve first"),
        "{busy}"
    );
    drop(running);
}
