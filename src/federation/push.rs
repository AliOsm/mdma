//! Sending a GGUF to a peer that grants installs, for `mdma-fed push` and a
//! running server's Push RPC. The peer decides with its install grant and
//! disk floor; bytes it already holds are never sent.
use super::control::{InstallRequest, UploadPart, upload_part};
use super::runtime::outbound::Credentials;
use super::trust::Action;
use rama::error::BoxError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bytes between progress reports.
const PROGRESS_BYTES: u64 = 16 << 20;

/// What a push left on the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// The name holding the bytes there.
    pub model: String,
    pub size: u64,
    /// The peer already held them, so nothing was sent.
    pub present: bool,
}

/// Sends `file` to `peer` as `model`. `progress` hears bytes sent and the
/// file's size: once after hashing it, then every 16 MiB and at the end.
pub(crate) async fn push(
    credentials: &Credentials,
    peer: &str,
    file: &Path,
    model: &str,
    progress: impl Fn(u64, u64) + Send + 'static,
) -> Result<Pushed, BoxError> {
    super::config::validate_model(model).map_err(|e| format!("{model:?}: {e}"))?;
    let path = file.to_path_buf();
    let (artifact_id, size) = tokio::task::spawn_blocking(move || -> Result<_, BoxError> {
        let mut file =
            std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (sha, stamp) = crate::cache::fingerprint(&mut file)?;
        Ok((format!("sha256:{sha}"), stamp.len()))
    })
    .await??;
    progress(0, size);
    let connected = credentials
        .connect(peer, Action::Push, rama::rt::Executor::default())
        .await?;
    let request = InstallRequest {
        origin: credentials.store.snapshot()?.server_id,
        destination: peer.into(),
        model: model.into(),
        artifact_id,
        size,
    };
    let client = connected.client;
    let file = file.to_path_buf();
    let exchange = async move {
        let plan = client.install(request.clone()).await?.into_inner();
        if !plan.installed.is_empty() {
            return Ok(Pushed {
                model: plan.installed,
                size,
                present: true,
            });
        }
        let (parts, stream) = tokio::sync::mpsc::channel(16);
        let offset = plan.offset;
        let reader = tokio::task::spawn_blocking(move || {
            send_file(&file, request, offset, &parts, &progress)
        });
        let response = client
            .upload(tokio_stream::wrappers::ReceiverStream::new(stream))
            .await;
        reader.await??;
        Ok::<_, BoxError>(Pushed {
            model: response?.into_inner().installed,
            size,
            present: false,
        })
    };
    tokio::select! {
        result = exchange => result,
        () = connected.drive => Err(format!("{peer} closed the connection during the push").into()),
        () = tokio::time::sleep_until(connected.binding.deadline().into()) =>
            Err(format!("{peer}'s credentials expired during the push").into()),
    }
}

/// Streams `request` and then the file from `offset`; fails if the file
/// changes while it is read.
fn send_file(
    path: &Path,
    request: InstallRequest,
    offset: u64,
    parts: &tokio::sync::mpsc::Sender<UploadPart>,
    progress: &impl Fn(u64, u64),
) -> Result<(), BoxError> {
    use std::io::{Read, Seek};
    let send = |part| {
        parts
            .blocking_send(UploadPart { part: Some(part) })
            .map_err(|_| BoxError::from("the peer stopped the upload"))
    };
    let size = request.size;
    let mut file = std::fs::File::open(path)?;
    let stamp = crate::cache::Stamp::read(&file.metadata()?)?;
    file.seek(std::io::SeekFrom::Start(offset))?;
    send(upload_part::Part::Request(request))?;
    let mut chunk = vec![0; super::install::CHUNK_BYTES];
    let (mut sent, mut reported) = (offset, offset);
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        send(upload_part::Part::Data(chunk[..n].to_vec()))?;
        sent += n as u64;
        if sent - reported >= PROGRESS_BYTES {
            progress(sent, size);
            reported = sent;
        }
    }
    stamp.check(file.metadata()?)?;
    progress(sent, size);
    Ok(())
}

/// A running server sending files from its own models folder, one at a time.
#[derive(Clone)]
pub(crate) struct Pusher {
    credentials: Credentials,
    models: PathBuf,
    busy: Arc<Semaphore>,
}

impl Pusher {
    pub fn new(credentials: Credentials, models: PathBuf) -> Self {
        Self {
            credentials,
            models,
            busy: Arc::new(Semaphore::new(1)),
        }
    }

    /// The turn to push; `None` while another push runs.
    pub fn begin(&self) -> Option<OwnedSemaphorePermit> {
        self.busy.clone().try_acquire_owned().ok()
    }

    /// `<file>.gguf` in the models folder; `file` is a validated name.
    pub fn file(&self, file: &str) -> PathBuf {
        self.models.join(format!("{file}.gguf"))
    }

    pub async fn push(
        &self,
        peer: &str,
        file: &Path,
        model: &str,
        progress: impl Fn(u64, u64) + Send + 'static,
    ) -> Result<Pushed, BoxError> {
        push(&self.credentials, peer, file, model, progress).await
    }
}
