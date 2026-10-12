//! Private native synthesis admission/streaming and execution foundations.
//! Runtime activation requires a separate explicit native execution switch.

mod admission;
pub(crate) mod deadline;
mod decision;
mod ledger;
mod native;
mod synthesis;
pub(crate) use decision::ResponseLease;
pub(crate) mod ingress;

pub(crate) use admission::{Executor, Native, Service};
pub(crate) use synthesis::EventStream;
