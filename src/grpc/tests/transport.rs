//! Shared ephemeral mTLS transport for fixture-free and checkpoint tests.
use super::*;
use crate::grpc::client::{Client, mtls_client};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) struct Fixture {
    pub(super) certs: TestCerts,
    pub(super) dir: PathBuf,
}

impl Fixture {
    pub(super) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("mdma-mtls-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&dir).unwrap();
        let certs = generate_test_certs();
        for (file, pem) in [("client.pem", &certs.client_pem), ("client-key.pem", &certs.client_key_pem), ("ca.pem", &certs.ca_pem)] {
            std::fs::write(dir.join(file), pem).unwrap();
        }
        Self { certs, dir }
    }

    pub(super) fn client(&self, url: &str) -> Client {
        mtls_client(url, &self.dir.join("client.pem"), &self.dir.join("client-key.pem"), &self.dir.join("ca.pem")).unwrap()
    }

    pub(super) async fn start<S: proto::mdma_server::Mdma>(&self, make_service: impl FnOnce(&str) -> S) -> Node {
        self.start_on(Executor::default(), make_service).await
    }

    pub(super) async fn start_on<S: proto::mdma_server::Mdma>(&self, executor: Executor, make_service: impl FnOnce(&str) -> S) -> Node {
        let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("https://localhost:{}", addr.port());
        let tls = TlsServerConfig::new()
            .with_single_cert(ServerAuthData {
                private_key: self.certs.server_key.clone_key(),
                cert_chain: self.certs.server_cert.clone(),
                ocsp: None,
            })
            .with_client_verify(ClientVerifyMode::ClientAuth(vec![self.certs.ca_der.clone()]))
            .with_alpn_http_auto();
        let service = TlsAcceptorLayer::new(tls).into_layer(HttpServer::auto(executor).service(MdmaServer::new(make_service(&url))));
        Node { url, addr, task: tokio::spawn(listener.serve(service)) }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); }
}

pub(super) struct Node {
    pub(super) url: String,
    pub(super) addr: SocketAddr,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl Drop for Node {
    fn drop(&mut self) { self.task.abort(); }
}
