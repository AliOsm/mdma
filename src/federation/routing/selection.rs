use crate::federation::{
    catalog::ModelDescriptor,
    catalog_cache::Cache,
    config::{self, Operation},
    control::ExecutionEnvelope,
    execution::deadline::{Deadline, Ingress},
    tls::{self, Binding, Caller},
    trust::{Action, Store},
};
use rama::{
    extensions::{Extensions, ExtensionsRef, Ingress as TransportIngress},
    http::grpc::{Request, Status},
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// An operator-configured alias resolves to exactly one target. There is no
/// failover list, URL from a request, wildcard/default or payload hop counter.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub alias: String,
    pub peer_id: String,
    pub model: String,
    pub artifact_id: String,
    pub profile_id: String,
    pub operation: Operation,
}
impl Route {
    pub(crate) fn validate(&self) -> Result<(), config::Error> {
        config::validate_model(&self.alias)?;
        config::validate_model(&self.model)?;
        config::validate_digest(&self.artifact_id)?;
        config::validate_digest(&self.profile_id)?;
        super::super::server_id::validate(&self.peer_id)
            .map_err(|_| config::Error::Policy("invalid route peer".into()))?;
        Ok(())
    }
    pub(crate) fn action(&self) -> Action<'_> {
        Action::Import {
            model: &self.model,
            artifact_id: &self.artifact_id,
            profile_id: &self.profile_id,
            operation: self.operation,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Receipt(
    pub(crate) Result<Ingress, crate::federation::execution::deadline::Error>,
);
impl rama::extensions::Extension for Receipt {}

pub(crate) struct Selection {
    pub(crate) route: Route,
    pub(crate) caller: Caller,
    pub(crate) descriptor: ModelDescriptor,
    pub(crate) deadline: Deadline,
    pub(crate) envelope: ExecutionEnvelope,
}

fn denied() -> Status {
    Status::permission_denied("route_denied")
}
fn expired() -> Status {
    Status::deadline_exceeded("deadline_exceeded")
}

impl Selection {
    /// Readiness is disclosure, not admission or a reservation. It requires the
    /// same actual caller, current consent/import and fresh exact catalog, but
    /// does not consume a request ID, quota, connection or workload deadline.
    pub(crate) fn visible<T>(
        store: &Store,
        cache: &Cache,
        route: &Route,
        request: &Request<T>,
    ) -> Result<(ModelDescriptor, crate::grpc::Host), Status> {
        let caller = request
            .extensions()
            .get_ref::<TransportIngress<Extensions>>()
            .and_then(|ingress| ingress.get_ref::<Caller>())
            .ok_or_else(denied)?;
        let now = Instant::now();
        let now_ms = tls::utc_ms().map_err(|_| denied())?;
        caller
            .authorize(
                store,
                &route.peer_id,
                &route.model,
                route.operation,
                now_ms,
                now,
            )
            .map_err(|_| denied())?;
        store
            .authorize(&route.peer_id, route.action(), now_ms)
            .map_err(|_| denied())?;
        let catalog = cache
            .snapshot(store, &route.peer_id, now_ms, now)
            .map_err(|_| denied())?
            .ok_or_else(|| Status::unavailable("catalog_unavailable"))?;
        if catalog.server_state.forces_unready() {
            return Err(Status::unavailable("not_accepting"));
        }
        let descriptor = catalog
            .models
            .into_iter()
            .find(|d| {
                d.model == route.model
                    && d.artifact_id == route.artifact_id
                    && d.profile_id == route.profile_id
                    && d.ready
                    && d.operation() == route.operation
            })
            .ok_or_else(|| Status::failed_precondition("selection_changed"))?;
        caller
            .authorize(
                store,
                &route.peer_id,
                &route.model,
                route.operation,
                tls::utc_ms().map_err(|_| denied())?,
                Instant::now(),
            )
            .map_err(|_| denied())?;
        Ok((descriptor, catalog.host))
    }

    pub(crate) fn new<T>(
        store: &Store,
        cache: &Cache,
        route: &Route,
        request: &Request<T>,
    ) -> Result<Self, Status> {
        route.validate().map_err(|_| denied())?;
        // Actual stream ingress only, not grpc/extensions supplied by an
        // end-user, request fields, tracing headers or an upstream binding.
        let transport = request
            .extensions()
            .get_ref::<TransportIngress<Extensions>>()
            .ok_or_else(denied)?;
        let caller = transport.get_ref::<Caller>().ok_or_else(denied)?.clone();
        let receipt = request
            .extensions()
            .get_ref::<Receipt>()
            .ok_or_else(denied)?
            .0
            .map_err(|_| Status::invalid_argument("invalid_request"))?;
        Self::select(store, cache, route, caller, receipt)
    }

    fn select(
        store: &Store,
        cache: &Cache,
        route: &Route,
        caller: Caller,
        receipt: Ingress,
    ) -> Result<Self, Status> {
        let now = Instant::now();
        let now_ms = tls::utc_ms().map_err(|_| denied())?;
        let consent = caller
            .authorize(
                store,
                &route.peer_id,
                &route.model,
                route.operation,
                now_ms,
                now,
            )
            .map_err(|_| denied())?;
        let deadline = receipt
            .resolve(300_000, consent, now)
            .map_err(|_| expired())?;
        let snapshot = store
            .authorize(&route.peer_id, route.action(), now_ms)
            .map_err(|_| denied())?;
        let catalog = cache
            .snapshot(store, &route.peer_id, now_ms, now)
            .map_err(|_| denied())?
            .ok_or_else(|| Status::unavailable("catalog_unavailable"))?;
        if catalog.server_state.forces_unready() {
            return Err(Status::unavailable("not_accepting"));
        }
        let descriptor = catalog
            .models
            .into_iter()
            .find(|d| {
                d.model == route.model
                    && d.artifact_id == route.artifact_id
                    && d.profile_id == route.profile_id
                    && d.ready
                    && d.operation() == route.operation
            })
            .ok_or_else(|| Status::failed_precondition("selection_changed"))?;
        let mut request_id = vec![0; 16];
        aws_lc_rs::rand::fill(&mut request_id)
            .map_err(|_| Status::internal("request_id_unavailable"))?;
        let envelope = ExecutionEnvelope {
            origin: snapshot.server_id,
            destination: route.peer_id.clone(),
            request_id,
            model: route.model.clone(),
            artifact_id: route.artifact_id.clone(),
            profile_id: route.profile_id.clone(),
            timeout_ms: deadline
                .remaining_ms(Instant::now())
                .map_err(|_| expired())?,
        };
        let selected = Self {
            route: route.clone(),
            caller,
            descriptor,
            deadline,
            envelope,
        };
        selected.authorize(store, None)?;
        Ok(selected)
    }

    pub(crate) fn action(&self) -> Action<'_> {
        self.route.action()
    }

    /// The caller's consent, the route's exact import and the peer's state
    /// and binding, as they are now. Unrelated trust edits do not end a
    /// routed request; losing any of these does.
    pub(crate) fn authorize(&self, store: &Store, binding: Option<&Binding>) -> Result<(), Status> {
        let now = Instant::now();
        self.deadline.remaining(now).map_err(|_| expired())?;
        let now_ms = tls::utc_ms().map_err(|_| denied())?;
        self.caller
            .authorize(
                store,
                &self.route.peer_id,
                &self.route.model,
                self.route.operation,
                now_ms,
                now,
            )
            .map_err(|_| denied())?;
        match binding {
            Some(binding) => {
                if binding.server_id() != self.route.peer_id
                    || binding.role() != config::TlsRole::ServerAuth
                {
                    return Err(denied());
                }
                binding
                    .authorize(store, self.action(), now_ms, now)
                    .map_err(|_| denied())?;
            }
            None => {
                store
                    .authorize(&self.route.peer_id, self.action(), now_ms)
                    .map_err(|_| denied())?;
            }
        }
        Ok(())
    }

    /// Revalidate fresh catalog and actual outbound binding at the final
    /// dispatch boundary. Any change fails; no replacement peer or new ID.
    pub(crate) fn dispatch(
        &self,
        store: &Store,
        cache: &Cache,
        binding: &Binding,
    ) -> Result<ExecutionEnvelope, Status> {
        self.authorize(store, Some(binding))?;
        let catalog = cache
            .snapshot(
                store,
                &self.route.peer_id,
                tls::utc_ms().map_err(|_| denied())?,
                Instant::now(),
            )
            .map_err(|_| denied())?
            .ok_or_else(|| Status::unavailable("catalog_unavailable"))?;
        if catalog.server_state.forces_unready()
            || !catalog
                .models
                .iter()
                .any(|d| d.same_selection(&self.descriptor) && d.ready)
        {
            return Err(Status::failed_precondition("selection_changed"));
        }
        let mut envelope = self.envelope.clone();
        let remaining = self
            .deadline
            .at()
            .min(binding.deadline())
            .checked_duration_since(Instant::now())
            .map(|d| d.as_millis())
            .filter(|ms| *ms > 0)
            .ok_or_else(expired)?;
        envelope.timeout_ms = remaining.min(300_000) as u32;
        self.authorize(store, Some(binding))?;
        Ok(envelope)
    }
}

#[cfg(test)]
mod tests;
