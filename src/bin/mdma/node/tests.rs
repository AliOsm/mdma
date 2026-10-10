use super::*;

#[tokio::test]
async fn resource_rpc_rejects_missing_identity_before_collecting_resources() {
    use proto::node_server::Node;
    let service = NodeService {
        storage: PathBuf::from("/nonexistent-mdma-storage"),
        address: "127.0.0.1".parse().unwrap(),
        policy: version::Policy::new(None, None).unwrap(),
    };
    let error = service
        .scan_capabilities(Request::new(proto::ScanCapabilitiesRequest {
            requester: None,
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), rama::http::grpc::Code::FailedPrecondition);
    let identity = service
        .identify(Request::new(proto::IdentifyRequest {
            requester: Some(version::local()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(identity, version::local());
}

#[test]
fn resources_are_measured_on_storage_filesystem() {
    let storage = std::env::current_dir().unwrap().canonicalize().unwrap();
    let s = snapshot(&storage, None).unwrap();
    assert!(s.logical_cpu_count > 0);
    assert!(s.memory_total_bytes > 0);
    assert!(s.memory_available_bytes <= s.memory_total_bytes);
    assert!(s.disk_total_bytes > 0);
    assert!(s.disk_available_bytes <= s.disk_total_bytes);
    assert!(s.collected_at_unix_ms > 0);
}

#[test]
fn rejects_non_origin_peers() {
    for peer in [
        "",
        "https://host:50052",
        "http://host/path",
        "http://user@host",
        "http://host/?x=1",
    ] {
        assert!(peer_origin(peer).is_err(), "{peer}");
    }
    assert!(peer_origin("http://192.0.2.10:50052").is_ok());
}
