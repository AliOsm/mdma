//! Join the actual native job with its claimed ID. No registry lookup, grants,
//! RPC activation or forwarding happens here. Those gates must precede launch.

use super::ledger::{Claim, Outcome};
use crate::backend::{Backend, Error, Result};
use crate::session::{
    tracked::Tokens,
    worker::{Prepared, WorkerEnd},
};
use rama::graceful::ShutdownGuard;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(super) struct CompletionTask {
    worker: Option<JoinHandle<Outcome>>,
    report: oneshot::Receiver<Result<WorkerEnd>>,
}

fn outcome(result: &Result<WorkerEnd>) -> Outcome {
    match result {
        Ok(WorkerEnd::Completed(_)) => Outcome::Completed,
        Ok(WorkerEnd::Cancelled) => Outcome::Cancelled,
        Err(_) => Outcome::Failed,
    }
}

impl CompletionTask {
    /// EOF or even receipt of the internal report is NOT completion. Join the
    /// accounting worker first: native resources/guard and charges are freed.
    pub(super) async fn finish(self) -> Result<WorkerEnd> {
        let terminal = self
            .worker
            .ok_or_else(|| Error::Backend("native worker launch failed".into()))?
            .await
            .map_err(|_| Error::Backend("native worker failed".into()))?;
        let report = self
            .report
            .await
            .map_err(|_| Error::Backend("native worker report absent".into()))?;
        if outcome(&report) != terminal {
            return Err(Error::Backend("native worker outcome mismatch".into()));
        }
        report
    }
}

pub(super) fn launch_with_charge<B>(
    prepared: Prepared<B>,
    claim: Claim,
    cancel: CancellationToken,
    guard: Option<ShutdownGuard>,
    charge: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
    boundary: crate::session::tracked::Boundary,
) -> (Tokens, CompletionTask)
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    launch_inner(prepared, claim, cancel, guard, charge, boundary)
}

fn launch_inner<B, C: Send + 'static>(
    prepared: Prepared<B>,
    claim: Claim,
    cancel: CancellationToken,
    guard: Option<ShutdownGuard>,
    charge: C,
    boundary: crate::session::tracked::Boundary,
) -> (Tokens, CompletionTask)
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    let (tokens, job) = prepared.into_job_checked(cancel, guard, boundary);
    let (report, receive) = oneshot::channel();
    let worker = claim
        .spawn_dedicated(move || {
            // The native job consumes/drops KV, workspaces and its shutdown guard
            // before reporting. The outer claimed job then releases accounting.
            let result = job();
            drop(charge);
            let terminal = outcome(&result);
            let _ = report.send(result);
            terminal
        })
        .ok();
    (
        tokens,
        CompletionTask {
            worker,
            report: receive,
        },
    )
}

#[cfg(test)]
mod tests;
