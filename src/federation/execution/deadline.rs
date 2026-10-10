//! Mandatory, strictly parsed gRPC deadline captured before body decoding.
//! Rama's general GrpcTimeout layer accepts absent/malformed timeouts and only
//! wraps response creation. Federation also needs a fixed streaming/worker
//! deadline, not a reset at handler entry or transport-response completion.

use crate::federation::tls::Binding;
use rama::http::HeaderMap;
use std::time::{Duration, Instant};

pub(super) const MAX_TIMEOUT_MS: u32 = 300_000;
const MAX_TIMEOUT: Duration = Duration::from_millis(MAX_TIMEOUT_MS as u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("one positive gRPC deadline is required")]
    InvalidGrpcDeadline,
    #[error("execution timeout must be 1..300000 milliseconds")]
    InvalidBudget,
    #[error("execution deadline exceeded")]
    Expired,
}

/// Only HTTP ingress creates this value. Future workload handlers must retrieve
/// it from transport extensions, never envelope fields or tracing metadata.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ingress {
    received: Instant,
    grpc_deadline: Instant,
}

impl Ingress {
    pub(crate) fn transport_deadline(self) -> Instant {
        self.grpc_deadline
    }
    pub(crate) fn capture(headers: &HeaderMap) -> Result<Self, Error> {
        Self::capture_at(headers, Instant::now())
    }

    fn capture_at(headers: &HeaderMap, received: Instant) -> Result<Self, Error> {
        let mut values = headers.get_all("grpc-timeout").iter();
        let value = values.next().ok_or(Error::InvalidGrpcDeadline)?;
        if values.next().is_some() {
            return Err(Error::InvalidGrpcDeadline);
        }
        let value = value.to_str().map_err(|_| Error::InvalidGrpcDeadline)?;
        if !(2..=9).contains(&value.len()) {
            return Err(Error::InvalidGrpcDeadline);
        }
        let (digits, unit) = value.split_at(value.len() - 1);
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidGrpcDeadline);
        }
        let number: u64 = digits.parse().map_err(|_| Error::InvalidGrpcDeadline)?;
        if number == 0 {
            return Err(Error::InvalidGrpcDeadline);
        }
        let duration = match unit {
            "H" => Duration::from_secs(number * 3600),
            "M" => Duration::from_secs(number * 60),
            "S" => Duration::from_secs(number),
            "m" => Duration::from_millis(number),
            "u" => Duration::from_micros(number),
            "n" => Duration::from_nanos(number),
            _ => return Err(Error::InvalidGrpcDeadline),
        };
        // Bound before adding: no large remote timeout can overflow Instant.
        let grpc_deadline = received
            .checked_add(duration.min(MAX_TIMEOUT))
            .ok_or(Error::InvalidGrpcDeadline)?;
        Ok(Self {
            received,
            grpc_deadline,
        })
    }

    pub(super) fn bind(
        self,
        timeout_ms: u32,
        binding: &Binding,
        now: Instant,
    ) -> Result<Deadline, Error> {
        self.resolve(timeout_ms, binding.deadline(), now)
    }

    pub(crate) fn resolve(
        self,
        timeout_ms: u32,
        credential_deadline: Instant,
        now: Instant,
    ) -> Result<Deadline, Error> {
        if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
            return Err(Error::InvalidBudget);
        }
        let execution = self
            .received
            .checked_add(Duration::from_millis(u64::from(timeout_ms)))
            .ok_or(Error::InvalidBudget)?;
        let deadline = Deadline {
            at: execution.min(self.grpc_deadline).min(credential_deadline),
        };
        deadline.remaining(now)?;
        Ok(deadline)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    at: Instant,
}

impl Deadline {
    pub(crate) fn at(self) -> Instant {
        self.at
    }

    pub(crate) fn remaining(self, now: Instant) -> Result<Duration, Error> {
        self.at
            .checked_duration_since(now)
            .filter(|d| !d.is_zero())
            .ok_or(Error::Expired)
    }

    /// Wire budget rounds down, never up. Less than 1ms is not dispatchable.
    /// Call again after negotiation; reusing an earlier duration resets budgets.
    pub(crate) fn remaining_ms(self, now: Instant) -> Result<u32, Error> {
        let millis = self.remaining(now)?.as_millis();
        if millis == 0 {
            return Err(Error::Expired);
        }
        Ok(millis.min(u128::from(MAX_TIMEOUT_MS)) as u32)
    }
}

#[cfg(test)]
mod tests;
