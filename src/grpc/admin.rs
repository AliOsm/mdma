//! Push: a client pinned in MDMA_PUSH_SPKI has this server send a file from
//! its models folder to a federation peer, while it keeps serving.
use super::MdmaService;
use super::proto::{PushProgress, PushRequest};
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

pub(super) fn push<B: Backend>(
    service: &MdmaService<B>,
    req: Request<PushRequest>,
) -> Result<ProgressStream, Status> {
    let caller = req
        .extensions()
        .get_ref::<Ingress<Extensions>>()
        .and_then(|ingress| ingress.get_ref::<ClientSpki>());
    if !caller.is_some_and(|spki| service.pushers.contains(&spki.0)) {
        return Err(Status::permission_denied(
            "pushing needs this client's pin in MDMA_PUSH_SPKI",
        ));
    }
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
    Status::unavailable(format!("pushing to {peer}: {cause}"))
}
