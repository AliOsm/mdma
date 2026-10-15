//! What a client pinned in MDMA_ADMIN_SPKI may change on a running federation
//! server: push a file from its models folder to a peer, and import a peer's
//! routes.
use super::MdmaService;
use super::proto::{
    FederationRoute, ImportRoutesRequest, ImportRoutesResponse, PushProgress, PushRequest,
};
use crate::backend::Backend;
use crate::federation::tls::ClientSpki;
use crate::session::CancelOnDrop;
use rama::error::BoxError;
use rama::extensions::{Extensions, ExtensionsRef, Ingress};
use rama::http::grpc::{Request, Status};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// Progress, then the outcome; dropping it cancels the push.
pub type ProgressStream = CancelOnDrop<ReceiverStream<Result<PushProgress, Status>>>;

/// The caller's pin, when it is in MDMA_ADMIN_SPKI.
fn admin<B: Backend, T>(service: &MdmaService<B>, req: &Request<T>) -> Result<String, Status> {
    req.extensions()
        .get_ref::<Ingress<Extensions>>()
        .and_then(|ingress| ingress.get_ref::<ClientSpki>())
        .filter(|spki| service.admins.contains(&spki.0))
        .map(|spki| spki.0.clone())
        .ok_or_else(|| Status::permission_denied("this needs the client's pin in MDMA_ADMIN_SPKI"))
}

/// Checks the caller and request now; the returned future imports.
pub(super) fn import_routes<B: Backend>(
    service: &MdmaService<B>,
    req: Request<ImportRoutesRequest>,
) -> Result<impl Future<Output = Result<ImportRoutesResponse, Status>> + Send + use<B>, Status> {
    let caller = admin(service, &req)?;
    let importer = service.importer.clone().ok_or_else(|| {
        Status::failed_precondition(
            "this server does not route to peers; run mdma-fed routes once with it stopped",
        )
    })?;
    let request = req.into_inner();
    crate::federation::server_id::validate(&request.peer)
        .map_err(|_| Status::invalid_argument("peer is a server id"))?;
    let prefix = if request.prefix.is_empty() {
        request
            .peer
            .split('.')
            .next()
            .unwrap_or(&request.peer)
            .to_owned()
    } else {
        request.prefix
    };
    let client = if request.client_spki.is_empty() {
        caller
    } else {
        crate::federation::config::validate_digest(&request.client_spki)
            .map_err(|_| Status::invalid_argument("client_spki is a sha256: pin"))?;
        request.client_spki
    };
    let peer = request.peer;
    Ok(async move {
        let routes = importer
            .import(&peer, &prefix, &client)
            .await
            .map_err(|cause| status(&peer, &cause))?;
        Ok(ImportRoutesResponse {
            routes: routes
                .into_iter()
                .map(|route| FederationRoute {
                    alias: route.alias,
                    peer_id: route.peer_id,
                    model: route.model,
                    operation: route.operation.as_str().into(),
                })
                .collect(),
        })
    })
}

pub(super) fn push<B: Backend>(
    service: &MdmaService<B>,
    req: Request<PushRequest>,
) -> Result<ProgressStream, Status> {
    admin(service, &req)?;
    let pusher = service
        .pusher
        .clone()
        .ok_or_else(|| Status::failed_precondition("this server has no federation peers"))?;
    let request = req.into_inner();
    let name = |name: &str, what: &str| {
        crate::federation::config::validate_model(name)
            .map_err(|_| Status::invalid_argument(format!("{what} is 1 to 128 of [A-Za-z0-9._-]")))
    };
    name(&request.file, "file, without .gguf,")?;
    let model = if request.model.is_empty() {
        request.file.clone()
    } else {
        request.model
    };
    name(&model, "model")?;
    let turn = pusher
        .begin()
        .ok_or_else(|| Status::resource_exhausted("another push is running here"))?;
    let file = pusher.file(&request.file);
    let (send, receive) = tokio::sync::mpsc::channel(16);
    let cancel = CancellationToken::new();
    let stopped = cancel.clone();
    tokio::spawn(async move {
        let _turn = turn;
        let progress = {
            let send = send.clone();
            move |sent, size| {
                // Reports are cumulative: one dropped while the client is
                // slow is replaced by the next.
                let _ = send.try_send(Ok(PushProgress {
                    sent,
                    size,
                    ..PushProgress::default()
                }));
            }
        };
        let outcome = tokio::select! {
            () = stopped.cancelled() => return,
            outcome = pusher.push(&request.peer, &file, &model, progress) => outcome,
        };
        let last = outcome
            .map(|pushed| PushProgress {
                sent: if pushed.present { 0 } else { pushed.size },
                size: pushed.size,
                installed: pushed.model,
                present: pushed.present,
            })
            .map_err(|cause| status(&request.peer, &cause));
        // A client that left needs no outcome.
        let _ = send.send(last).await;
    });
    Ok(CancelOnDrop::new(ReceiverStream::new(receive), cancel))
}

/// The peer's own refusal keeps its code; anything else names the peer.
fn status(peer: &str, cause: &BoxError) -> Status {
    if let Some(refusal) = cause.downcast_ref::<Status>() {
        return Status::new(refusal.code(), format!("{peer}: {}", refusal.message()));
    }
    if crate::federation::runtime::outbound::connect_failed(cause) {
        return Status::unavailable(format!("cannot connect to {peer}: {cause}"));
    }
    Status::failed_precondition(format!("{peer}: {cause}"))
}
