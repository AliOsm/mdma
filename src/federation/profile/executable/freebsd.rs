//! FreeBSD KERN_PROC_PATHNAME names the running image; the vnode the kernel
//! maps at this code's address (KERN_PROC_VMMAP) proves an opened file is it.
use std::{
    ffi::{CStr, OsStr},
    fs::File,
    io,
    mem::{MaybeUninit, size_of},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::PathBuf,
};

/// A sysctl's value, retried while it grows between the size query and the read.
fn sysctl(mib: &[libc::c_int]) -> io::Result<Vec<u8>> {
    let len = u32::try_from(mib.len()).map_err(|_| io::Error::other("sysctl name too long"))?;
    for _ in 0..4 {
        let mut size = 0;
        // SAFETY: a size query; the null buffer is never written.
        if unsafe {
            libc::sysctl(
                mib.as_ptr(),
                len,
                std::ptr::null_mut(),
                &raw mut size,
                std::ptr::null(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        size += size / 4;
        let mut buffer = vec![0u8; size];
        // SAFETY: `buffer` is writable for `size` bytes, which the kernel
        // lowers to what it wrote.
        if unsafe {
            libc::sysctl(
                mib.as_ptr(),
                len,
                buffer.as_mut_ptr().cast(),
                &raw mut size,
                std::ptr::null(),
                0,
            )
        } == 0
        {
            buffer.truncate(size);
            return Ok(buffer);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error);
        }
    }
    Err(io::Error::other("sysctl value kept growing"))
}

/// What the kernel reports about the vnode mapped at this function.
#[derive(Debug)]
struct Mapping {
    fsid: u64,
    fileid: u64,
    size: u64,
}

fn mapped() -> io::Result<Mapping> {
    let address = (mapped as fn() -> io::Result<Mapping>) as usize as u64;
    let pid = libc::c_int::try_from(std::process::id())
        .map_err(|_| io::Error::other("pid out of range"))?;
    let entries = sysctl(&[libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_VMMAP, pid])?;
    let mut rest = entries.as_slice();
    while rest.len() >= size_of::<libc::c_int>() {
        let size = usize::try_from(libc::c_int::from_ne_bytes(
            rest[..4].try_into().expect("four bytes"),
        ))
        .ok()
        .filter(|&size| size >= 4 && size <= rest.len())
        .ok_or_else(|| io::Error::other("malformed kern.proc.vmmap entry"))?;
        let mut entry = MaybeUninit::<libc::kinfo_vmentry>::zeroed();
        // SAFETY: packed entries end with a shortened path; at most the
        // struct's size is copied into zeroed, integer-only storage.
        let entry = unsafe {
            std::ptr::copy_nonoverlapping(
                rest.as_ptr(),
                entry.as_mut_ptr().cast::<u8>(),
                size.min(size_of::<libc::kinfo_vmentry>()),
            );
            entry.assume_init()
        };
        if entry.kve_start <= address && address < entry.kve_end {
            if entry.kve_type != libc::KVME_TYPE_VNODE
                || entry.kve_protection & libc::KVME_PROT_EXEC == 0
                || entry.kve_vn_fileid == 0
            {
                return Err(io::Error::other(
                    "running code is not a file-backed executable mapping",
                ));
            }
            return Ok(Mapping {
                fsid: entry.kve_vn_fsid,
                fileid: entry.kve_vn_fileid,
                size: entry.kve_vn_size,
            });
        }
        rest = &rest[size..];
    }
    Err(io::Error::other("running code is not mapped"))
}

fn check(file: &File, mapping: &Mapping) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.dev() != mapping.fsid
        || metadata.ino() != mapping.fileid
        || metadata.len() != mapping.size
    {
        return Err(io::Error::other(
            "executable descriptor does not match the mapped vnode",
        ));
    }
    Ok(())
}

pub(crate) fn open() -> io::Result<File> {
    let name = sysctl(&[
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PATHNAME,
        -1,
    ])?;
    let path = CStr::from_bytes_until_nul(&name)
        .map_err(|_| io::Error::other("executable path is not terminated"))?;
    let path = PathBuf::from(OsStr::from_bytes(path.to_bytes()));
    if !path.is_absolute() {
        return Err(io::Error::other("executable path is not absolute"));
    }
    let file = File::open(path)?;
    check(&file, &mapped()?)?;
    Ok(file)
}

pub(crate) fn validate(file: &File) -> io::Result<()> {
    check(file, &mapped()?)
}

#[cfg(test)]
mod tests;
