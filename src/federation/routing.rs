//! Single-hop origin controller. No implicit discovery, defaults or redispatch.
pub(crate) mod selection;
mod validate;
pub use selection::Route;
pub(crate) use selection::Selection;
mod controller;
pub(crate) use controller::Router;

#[cfg(test)]
pub(crate) mod tests;
