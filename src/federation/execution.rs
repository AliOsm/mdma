//! Private native synthesis admission/streaming and execution foundations.
//! Runtime activation requires a separate explicit native execution switch.

pub(crate) mod deadline;
mod ledger;
mod native;
mod admission;
mod synthesis;
mod decision;
pub(crate) use decision::ResponseLease;
pub(crate) mod ingress;

pub(crate) use admission::{Executor, Native, Service};
pub(crate) use synthesis::EventStream;
