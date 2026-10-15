//! Importing a peer's routes while serving: what `mdma-fed routes` does with
//! the server stopped. Requests already routed keep running unless their own
//! route changed.
use super::{Route, Router};
use crate::federation::{admin, install::Folder, runtime::Config};
use rama::{error::BoxError, rt::Executor};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Importer {
    router: Router,
    /// runtime.toml, rewritten after each import.
    path: PathBuf,
    /// The config last saved there. Held through an import, so one runs at a time.
    config: Arc<tokio::sync::Mutex<Config>>,
    /// Local models, which no alias may shadow.
    folder: Arc<dyn Folder>,
    executor: Executor,
}

impl Importer {
    pub fn new(
        router: Router,
        path: PathBuf,
        config: Config,
        folder: Arc<dyn Folder>,
        executor: Executor,
    ) -> Self {
        Self {
            router,
            path,
            config: Arc::new(tokio::sync::Mutex::new(config)),
            folder,
            executor,
        }
    }

    /// Routes each ready model in `peer`'s fresh signed catalog as
    /// `prefix-model` for the client pinned `caller`, replacing the peer's
    /// earlier routes in trust, the live table and runtime.toml.
    pub async fn import(
        &self,
        peer: &str,
        prefix: &str,
        caller: &str,
    ) -> Result<Vec<Route>, BoxError> {
        let mut config = self.config.lock().await;
        let catalog = self.fresh_catalog(peer).await?;
        if catalog.server_state.forces_unready() {
            return Err(format!("{peer} is not accepting work").into());
        }
        let shadows = |alias: &str| self.folder.listed(alias);
        let routes = admin::plan_routes(&config, peer, catalog.models, prefix)?;
        self.router.table().check(peer, &routes, shadows)?;
        let mut next = config.clone();
        admin::publish_routes(
            &mut next,
            &self.router.credentials().store,
            peer,
            &routes,
            caller,
        )?;
        self.router.table().replace(peer, routes.clone(), shadows)?;
        admin::save_config(&self.path, &next).map_err(|error| {
            format!(
                "routes are live, but {} was not saved ({error}); run this again before restarting",
                self.path.display()
            )
        })?;
        *config = next;
        Ok(routes)
    }

    /// A catalog polled now, into the cache routing reads, so the new routes
    /// select at once. Waits out the poller's own poll of the same peer.
    async fn fresh_catalog(
        &self,
        peer: &str,
    ) -> Result<crate::federation::catalog::CatalogBody, BoxError> {
        let deadline =
            std::time::Instant::now() + 2 * crate::federation::catalog_cache::POLL_TIMEOUT;
        loop {
            let polled = self
                .router
                .credentials()
                .poll(self.router.cache(), peer, self.executor.clone())
                .await;
            match polled {
                Err(error)
                    if matches!(
                        error.downcast_ref(),
                        Some(crate::federation::catalog_cache::Error::Busy)
                    ) && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                polled => return polled,
            }
        }
    }
}
