//! Single-hop origin controller. No implicit discovery, defaults or redispatch.
pub(crate) mod selection;
mod validate;
pub use selection::Route;
pub(crate) use selection::Selection;
mod controller;
mod import;
mod leases;
mod table;
pub(crate) use controller::Router;
pub(crate) use import::Importer;
pub(crate) use table::RouteTable;

#[cfg(test)]
pub(crate) mod tests;
