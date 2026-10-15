//! Models a peer pushes here. Bytes land in a hidden file in the models
//! folder, are checked against the announced digest, then are listed as if
//! the folder had held them at startup. Bytes already here under another
//! name are never sent again.
use super::config::{self, Operation};
use super::control::{InstallPlan, InstallRequest, UploadPart, upload_part::Part};
use super::tls::{Binding, utc_ms};
use super::trust::{Action, Installed, Store};
use crate::artifact::{ArtifactId, Hasher};
use crate::backend::Backend;
use crate::registry::Registry;
use rama::http::grpc::{Status, Streaming};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest upload message payload; fits the 96 KiB control decoder.
pub const CHUNK_BYTES: usize = 64 * 1024;
/// An upload that sends nothing for this long is dropped.
const IDLE: Duration = Duration::from_secs(30);

/// The models folder as a running node lists it.
pub(crate) trait Folder: Send + Sync {
    fn dir(&self) -> Option<PathBuf>;
    /// Every model listed at startup has a known artifact, or never will.
    fn hashed(&self) -> bool;
    fn find(&self, artifact_id: &str) -> Option<String>;
    fn listed(&self, name: &str) -> bool;
    /// What a listed model serves.
    fn operation(&self, name: &str) -> Option<Operation>;
    /// Lists `<name>.gguf` from the folder; a link shares its target's artifact.
    fn add(&self, name: &str, artifact: Option<ArtifactId>) -> Result<(), String>;
}

impl<B> Folder for Arc<Registry<B>>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    fn dir(&self) -> Option<PathBuf> {
        Registry::dir(self).map(Path::to_path_buf)
    }

    fn hashed(&self) -> bool {
        Registry::hashed(self)
    }

    fn find(&self, artifact_id: &str) -> Option<String> {
        self.find_artifact(artifact_id).map(str::to_owned)
    }

    fn listed(&self, name: &str) -> bool {
        self.names().contains(&name)
    }

    fn operation(&self, name: &str) -> Option<Operation> {
        Some(if self.info(name)?.decision.is_some() {
            Operation::Decide
        } else {
            Operation::Synthesize
        })
    }

    fn add(&self, name: &str, artifact: Option<ArtifactId>) -> Result<(), String> {
        Registry::add(self, name, artifact).map_err(|e| e.to_string())
    }
}

enum Plan {
    Installed(String),
    Send { dir: PathBuf },
}

impl From<Plan> for InstallPlan {
    fn from(plan: Plan) -> Self {
        match plan {
            Plan::Installed(installed) => Self {
                installed,
                offset: 0,
            },
            Plan::Send { .. } => Self::default(),
        }
    }
}

/// Removes the hidden upload unless it was renamed into place.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                eprintln!("mdma: cannot remove {}: {error}", self.0.display());
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
pub(crate) struct Service {
    store: Arc<Store>,
    folder: Arc<dyn Folder>,
    /// One upload at a time, node-wide.
    upload: Arc<tokio::sync::Semaphore>,
    /// Changes to the folder and the install ledger happen one at a time.
    commit: Arc<Mutex<()>>,
}

impl Service {
    /// Uploads cut short before this start are never resumed; their hidden
    /// files are removed.
    pub fn new(store: Arc<Store>, folder: Arc<dyn Folder>) -> Self {
        if let Some(dir) = folder.dir() {
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with(".install-") && name.ends_with(".part") {
                            remove(&entry.path());
                        }
                    }
                }
                Err(error) => eprintln!("mdma: cannot list {}: {error}", dir.display()),
            }
        }
        Self {
            store,
            folder,
            upload: Arc::new(tokio::sync::Semaphore::new(1)),
            commit: Arc::new(Mutex::new(())),
        }
    }

    /// What `origin`, already authorized to install, should send.
    pub async fn install(
        &self,
        origin: String,
        request: InstallRequest,
    ) -> Result<InstallPlan, Status> {
        let service = self.clone();
        let plan = tokio::task::spawn_blocking(move || service.plan(&origin, &request))
            .await
            .map_err(|_| Status::internal("install planning failed"))??;
        Ok(plan.into())
    }

    /// Receives the file Install asked for, then lists it. `binding` is the
    /// upload's connection, authorized for `request` already.
    pub async fn upload(
        &self,
        binding: Binding,
        request: InstallRequest,
        mut parts: Streaming<UploadPart>,
    ) -> Result<InstallPlan, Status> {
        let _one = self
            .upload
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("another install is running here"))?;
        let origin = binding.server_id().to_owned();
        let service = self.clone();
        let (planned_origin, planned) = (origin.clone(), request.clone());
        let plan = tokio::task::spawn_blocking(move || service.plan(&planned_origin, &planned))
            .await
            .map_err(|_| Status::internal("install planning failed"))??;
        let dir = match plan {
            Plan::Installed(_) => return Ok(plan.into()),
            Plan::Send { dir } => dir,
        };
        let digest = request
            .artifact_id
            .strip_prefix("sha256:")
            .expect("plan validated the artifact id");
        let staged = Staged(dir.join(format!(".install-{digest}.part")));
        let (chunks, mut received) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let path = staged.0.clone();
        let writer = tokio::task::spawn_blocking(move || -> std::io::Result<(ArtifactId, u64)> {
            let mut file = std::fs::File::create(&path)?;
            let mut hash = Hasher::default();
            let mut written = 0u64;
            while let Some(chunk) = received.blocking_recv() {
                hash.update(&chunk);
                file.write_all(&chunk)?;
                written += chunk.len() as u64;
            }
            file.sync_all()?;
            Ok((hash.finish(), written))
        });
        let receive = async {
            let mut sent = 0u64;
            loop {
                let part = tokio::time::timeout(IDLE, parts.message())
                    .await
                    .map_err(|_| Status::deadline_exceeded("upload stalled"))??;
                let Some(part) = part else {
                    return Ok(sent);
                };
                let Some(Part::Data(data)) = part.part else {
                    return Err(Status::invalid_argument(
                        "an upload carries one request, then data",
                    ));
                };
                if data.is_empty() || data.len() > CHUNK_BYTES {
                    return Err(Status::invalid_argument("upload chunks hold 1 to 64 KiB"));
                }
                sent += data.len() as u64;
                if sent > request.size {
                    return Err(Status::invalid_argument("more bytes than announced"));
                }
                if chunks.send(data).await.is_err() {
                    // The writer stopped; its error says why.
                    return Ok(sent);
                }
            }
        };
        let sent = receive.await;
        // The writer finishes before the staged file can be removed.
        drop(chunks);
        let written = writer
            .await
            .map_err(|_| Status::internal("upload writer failed"))?;
        let sent = sent?;
        let (artifact, written) = written.map_err(|error| match error.kind() {
            std::io::ErrorKind::StorageFull => Status::resource_exhausted("models disk full"),
            _ => Status::internal(format!("writing the upload: {error}")),
        })?;
        if sent != request.size || written != request.size {
            return Err(Status::invalid_argument("upload ended before its size"));
        }
        if artifact.as_str() != request.artifact_id {
            return Err(Status::invalid_argument("bytes do not match artifact_id"));
        }
        // A peer that lost the grant mid-upload installs nothing.
        binding
            .authorize(&self.store, Action::Install, clock()?, Instant::now())
            .map_err(|_| Status::permission_denied("install grant changed"))?;
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            service.commit(&origin, &request.model, artifact, staged)
        })
        .await
        .map_err(|_| Status::internal("install commit failed"))?
    }

    /// Checks a request against the folder and ledger. Bytes already here
    /// are installed under the asked name without sending them.
    fn plan(&self, origin: &str, request: &InstallRequest) -> Result<Plan, Status> {
        config::validate_model(&request.model)
            .map_err(|_| Status::invalid_argument("invalid model name"))?;
        config::validate_digest(&request.artifact_id)
            .map_err(|_| Status::invalid_argument("invalid artifact_id"))?;
        if request.size == 0 {
            return Err(Status::invalid_argument("empty model"));
        }
        let dir = self
            .folder
            .dir()
            .ok_or_else(|| Status::unimplemented("install_disabled"))?;
        if !self.folder.hashed() {
            return Err(Status::unavailable(
                "still hashing its models; retry shortly",
            ));
        }
        let _commit = self
            .commit
            .lock()
            .map_err(|_| Status::internal("install lock poisoned"))?;
        let snapshot = self
            .store
            .snapshot()
            .map_err(|_| Status::unavailable("trust state unavailable"))?;
        // A ledger entry counts while its file is listed; one deleted by hand
        // before a restart can be installed again.
        if let Some((name, _)) = snapshot.installed().iter().find(|(name, model)| {
            model.artifact_id == request.artifact_id && self.folder.listed(name)
        }) {
            return Ok(Plan::Installed(name.clone()));
        }
        let floor = snapshot
            .peer(origin)
            .and_then(|record| record.policy.install)
            .ok_or_else(|| Status::permission_denied("no install grant"))?
            .floor_bytes;
        if let Some(local) = self.folder.find(&request.artifact_id) {
            // One of this node's own models already has these bytes.
            if local != request.model {
                if self.folder.listed(&request.model) {
                    return Err(Status::already_exists("model name taken"));
                }
                let link = dir.join(format!("{}.gguf", request.model));
                std::os::unix::fs::symlink(format!("{local}.gguf"), &link)
                    .map_err(|e| Status::internal(format!("linking {local}: {e}")))?;
                if let Err(error) = self.folder.add(&request.model, None) {
                    remove(&link);
                    return Err(Status::internal(format!("listing the link: {error}")));
                }
            }
            self.record(origin, &request.model, &request.artifact_id)?;
            return Ok(Plan::Installed(request.model.clone()));
        }
        if self.folder.listed(&request.model) {
            return Err(Status::already_exists("model name taken"));
        }
        let (_, free) =
            crate::host::disk_of(&dir).ok_or_else(|| Status::unavailable("models disk unknown"))?;
        if free < request.size.saturating_add(floor) {
            return Err(Status::resource_exhausted(format!(
                "{} bytes would leave less than the {floor} bytes this node keeps free",
                request.size
            )));
        }
        Ok(Plan::Send { dir })
    }

    /// Moves the verified upload into place, lists it and records it.
    fn commit(
        &self,
        origin: &str,
        model: &str,
        artifact: ArtifactId,
        staged: Staged,
    ) -> Result<InstallPlan, Status> {
        let _commit = self
            .commit
            .lock()
            .map_err(|_| Status::internal("install lock poisoned"))?;
        if self.folder.listed(model) {
            return Err(Status::already_exists("model name taken"));
        }
        let dir = staged
            .0
            .parent()
            .expect("staged in the models folder")
            .to_path_buf();
        let target = dir.join(format!("{model}.gguf"));
        // A link, unlike a rename, never replaces a file someone put there.
        std::fs::hard_link(&staged.0, &target).map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => Status::already_exists("model name taken"),
            _ => Status::internal(format!("placing the model: {error}")),
        })?;
        drop(staged);
        std::fs::File::open(&dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| Status::internal(format!("syncing the models folder: {e}")))?;
        let artifact_id = artifact.as_str().to_owned();
        if let Err(error) = self.folder.add(model, Some(artifact)) {
            remove(&target);
            return Err(Status::failed_precondition(format!(
                "not a model this node serves: {error}"
            )));
        }
        self.record(origin, model, &artifact_id)?;
        eprintln!("mdma: {origin} installed model {model} ({artifact_id})");
        Ok(Plan::Installed(model.to_owned()).into())
    }

    fn record(&self, origin: &str, model: &str, artifact_id: &str) -> Result<(), Status> {
        let operation = self
            .folder
            .operation(model)
            .ok_or_else(|| Status::internal("installed model is not listed"))?;
        self.store
            .record_install(
                origin,
                model,
                Installed {
                    artifact_id: artifact_id.into(),
                    operation,
                    installer: origin.into(),
                    installed_at_ms: clock()?,
                },
            )
            .map_err(|error| {
                Status::unavailable(format!("listed {model} but cannot record it: {error}"))
            })
    }
}

fn clock() -> Result<u64, Status> {
    utc_ms().map_err(|_| Status::unavailable("invalid UTC clock"))
}

fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        eprintln!("mdma: cannot remove {}: {error}", path.display());
    }
}
