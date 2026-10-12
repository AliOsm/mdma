//! Coral Edge TPU ("beagle") backend.
//!
//! A program is a stream of 128-bit words run by the chip's scalar core. On the
//! PCIe and M.2 cards the host maps its buffers into the device page table through
//! the `gasket` driver, links their device addresses into the program, queues the
//! instruction chunks and waits for the completion interrupt; the program's own DMA
//! descriptors then move inputs, parameters and outputs. `executable` is the program
//! model, `link` patches the addresses in, `address_space` hands out device
//! addresses, `regs` names the registers, `pcie` is the FreeBSD transport and `tpu`
//! runs a program end to end. `isa` encodes the instructions and `codegen` builds
//! programs from them, checked word for word against the compiler-exact corpus.

pub mod address_space;
pub mod codegen;
pub mod executable;
pub mod isa;
pub mod link;
pub mod regs;
#[cfg(target_os = "freebsd")]
pub mod pcie;
#[cfg(target_os = "freebsd")]
pub mod tpu;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("coral executable: {0}")]
    Executable(String),
    #[error("coral link: {0}")]
    Link(String),
    #[error("coral codegen: {0}")]
    Codegen(String),
    #[error("coral device address space exhausted: {requested} bytes requested, {free} free")]
    AddressSpace { requested: u64, free: u64 },
    #[error("coral device {path}: {op}: {source}")]
    Device { path: String, op: &'static str, #[source] source: std::io::Error },
    #[error("coral register {name} (0x{offset:x}) did not reach 0x{expected:x} within {timeout:?}: last 0x{last:x}")]
    Poll { name: &'static str, offset: u64, expected: u64, last: u64, timeout: std::time::Duration },
    #[error("coral HIB error: status 0x{status:x} ({decoded}), first 0x{first:x}")]
    Hib { status: u64, first: u64, decoded: String },
    #[error("coral host queue: {0}")]
    HostQueue(String),
    #[error("coral program {name:?} did not complete within {timeout:?}: {state}")]
    Timeout { name: String, timeout: std::time::Duration, state: String },
    #[error("coral: {0}")]
    Protocol(String),
}

impl From<Error> for super::Error {
    fn from(error: Error) -> Self { Self::Backend(error.to_string()) }
}
