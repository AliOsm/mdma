use super::{Error, Result};

/// A failed submit/wait is not idle. Stop reusing its command buffer and
/// keep deferred allocations alive until device teardown can join the work.
#[derive(Default)]
pub(super) struct Submission {
    open: bool,
    failed: bool,
}

impl Submission {
    pub(super) fn is_idle(&self) -> bool {
        !self.open && !self.failed
    }

    fn check(&self) -> Result<()> {
        if self.failed {
            return Err(Error::Backend("command submission failed; backend must be recreated".into()));
        }
        Ok(())
    }

    pub(super) fn begin(&mut self, begin: impl FnOnce() -> Result<()>) -> Result<()> {
        self.check()?;
        if !self.open {
            self.failed = true;
            begin()?;
            self.open = true;
            self.failed = false;
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, submit_and_wait: impl FnOnce() -> Result<()>) -> Result<()> {
        self.check()?;
        if self.open {
            self.failed = true;
            submit_and_wait()?;
            self.open = false;
            self.failed = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
