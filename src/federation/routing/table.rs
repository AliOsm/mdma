//! The aliases a router serves. Startup lists them; an import replaces one
//! peer's while requests run, which keep the route they selected.
use super::Route;
use rama::error::BoxError;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub(crate) struct RouteTable(Arc<RwLock<Arc<BTreeMap<String, Route>>>>);

impl RouteTable {
    /// Aliases are unique and never shadow a local model.
    pub fn new(routes: Vec<Route>, shadows: impl Fn(&str) -> bool) -> Result<Self, BoxError> {
        Ok(Self(Arc::new(RwLock::new(Arc::new(index(
            routes, &shadows,
        )?)))))
    }

    /// The table as it is now; later imports do not change it.
    pub fn snapshot(&self) -> Arc<BTreeMap<String, Route>> {
        self.0.read().expect("route table lock poisoned").clone()
    }

    pub fn get(&self, alias: &str) -> Option<Route> {
        self.snapshot().get(alias).cloned()
    }

    /// Whether [`Self::replace`] would take these routes, without taking them.
    pub fn check(
        &self,
        peer: &str,
        routes: &[Route],
        shadows: impl Fn(&str) -> bool,
    ) -> Result<(), BoxError> {
        next(&self.snapshot(), peer, routes.to_vec(), &shadows).map(drop)
    }

    /// `peer`'s routes become `routes`; other peers' stay. Refused whole when
    /// an alias would repeat or shadow a local model.
    pub fn replace(
        &self,
        peer: &str,
        routes: Vec<Route>,
        shadows: impl Fn(&str) -> bool,
    ) -> Result<(), BoxError> {
        let mut table = self.0.write().expect("route table lock poisoned");
        *table = Arc::new(next(&table, peer, routes, &shadows)?);
        Ok(())
    }
}

fn next(
    table: &BTreeMap<String, Route>,
    peer: &str,
    routes: Vec<Route>,
    shadows: &impl Fn(&str) -> bool,
) -> Result<BTreeMap<String, Route>, BoxError> {
    if routes.iter().any(|route| route.peer_id != peer) {
        return Err("an import replaces one peer's routes".into());
    }
    let kept = table
        .values()
        .filter(|route| route.peer_id != peer)
        .cloned();
    index(kept.chain(routes).collect(), shadows)
}

fn index(
    routes: Vec<Route>,
    shadows: &impl Fn(&str) -> bool,
) -> Result<BTreeMap<String, Route>, BoxError> {
    if routes.len() > 256 {
        return Err("at most 256 route aliases".into());
    }
    let mut indexed = BTreeMap::new();
    for route in routes {
        route.validate()?;
        if shadows(&route.alias) || indexed.insert(route.alias.clone(), route).is_some() {
            return Err(
                "route aliases must be unique and cannot shadow a local/default model".into(),
            );
        }
    }
    Ok(indexed)
}
