//! Where interrupt events come from. The FreeBSD driver queues 16-byte
//! `gasket_interrupt_event` records that `read(2)` on the device returns. Linux
//! gasket signals one eventfd per interrupt, registered with
//! `GASKET_IOCTL_SET_EVENTFD`; each read says how many times it fired since the
//! last one, and the cumulative count is kept here so both report the same `Event`.

use super::{Event, os};
use std::io;
use std::os::fd::BorrowedFd;

cfg_select! {
    target_os = "freebsd" => {
        pub(super) struct Source;

        // The Linux source owns eventfds; both share one signature.
        #[expect(
            clippy::unnecessary_wraps,
            clippy::unused_self,
            clippy::needless_pass_by_ref_mut,
            reason = "the per-OS event sources share one interface"
        )]
        impl Source {
            pub fn attach(_device: BorrowedFd<'_>) -> io::Result<Self> {
                Ok(Self)
            }

            /// Every event the kernel has queued, without blocking.
            pub fn drain(&mut self, device: BorrowedFd<'_>, events: &mut Vec<Event>) -> io::Result<()> {
                let mut buf = [0u8; Event::BYTES * 32];
                loop {
                    let n = os::read_nonblocking(device, &mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    if n % Event::BYTES != 0 {
                        return Err(io::Error::other(format!(
                            "event read returned {n} bytes, not a multiple of {}",
                            Event::BYTES
                        )));
                    }
                    for raw in buf[..n].as_chunks::<{ Event::BYTES }>().0 {
                        events.push(Event {
                            interrupt: u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
                            dropped: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                            count: u64::from_le_bytes([
                                raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
                            ]),
                        });
                    }
                }
            }

            pub fn detach(&mut self, _device: BorrowedFd<'_>) {}
        }
    }
    _ => {
        use super::interrupt;
        use std::os::fd::{AsFd, AsRawFd, OwnedFd};

        pub(super) struct Source {
            fds: Vec<OwnedFd>,
            /// Cumulative count per interrupt since `attach`.
            counts: [u64; interrupt::COUNT as usize],
        }

        impl Source {
            /// One eventfd per interrupt, registered with the driver.
            pub fn attach(device: BorrowedFd<'_>) -> io::Result<Self> {
                let mut source = Self {
                    fds: Vec::with_capacity(interrupt::COUNT as usize),
                    counts: [0; interrupt::COUNT as usize],
                };
                for id in 0..interrupt::COUNT {
                    let registered = os::eventfd().and_then(|fd| {
                        let mut request = os::InterruptEventfdIoctl {
                            interrupt: u64::from(id),
                            event_fd: fd.as_raw_fd().cast_unsigned().into(),
                        };
                        os::ioctl(device, os::GASKET_IOCTL_SET_EVENTFD, &mut request).map(|()| fd)
                    });
                    match registered {
                        Ok(fd) => source.fds.push(fd),
                        Err(error) => {
                            source.detach(device);
                            return Err(error);
                        }
                    }
                }
                Ok(source)
            }

            /// Every interrupt that fired since the last call, without blocking.
            pub fn drain(&mut self, _device: BorrowedFd<'_>, events: &mut Vec<Event>) -> io::Result<()> {
                for (id, fd) in self.fds.iter().enumerate() {
                    let fired = os::read_eventfd(fd.as_fd())?;
                    if fired == 0 {
                        continue;
                    }
                    self.counts[id] += fired;
                    events.push(Event {
                        interrupt: id as u32,
                        dropped: 0,
                        count: self.counts[id],
                    });
                }
                Ok(())
            }

            /// Unregisters the eventfds, then closes them.
            pub fn detach(&mut self, device: BorrowedFd<'_>) {
                for id in 0..self.fds.len() {
                    let mut request = id as libc::c_ulong;
                    // Best effort: a failure leaves the kernel holding a reference to an
                    // eventfd nobody reads, until the device is closed.
                    let _ = os::ioctl(device, os::GASKET_IOCTL_CLEAR_EVENTFD, &mut request);
                }
                self.fds.clear();
            }
        }
    }
}
