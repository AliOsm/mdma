//! Darwin PROC_PIDREGIONPATHINFO binds the main dyld image to its mapped vnode.
//! A later path replacement must match that vnode, not just the dyld path/UUID.
use std::{
    fs::File,
    io,
    mem::MaybeUninit,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::PathBuf,
};

// Public Darwin proc_info.h ABI, missing from libc's region bindings.
// The fourteen u32 counters between offset/address are unused, not omitted.
#[repr(C)]
struct RegionInfo {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    flags: u32,
    offset: u64,
    counters: [u32; 14],
    address: u64,
    size: u64,
}

#[repr(C)]
struct RegionWithPath {
    region: RegionInfo,
    vnode: libc::vnode_info_path,
}

fn mapped() -> io::Result<RegionWithPath> {
    // SAFETY: image 0 is the main executable and remains loaded for the
    // process lifetime. We use its address only; no pointer dereference.
    // libc deprecates its Mach bindings in favor of mach2. This stable dyld C
    // function needs no Mach port/type manipulation; keep the narrow binding.
    #[allow(deprecated)]
    let header = unsafe { libc::_dyld_get_image_header(0) };
    if header.is_null() {
        return Err(io::Error::other(
            "dyld did not identify the main executable",
        ));
    }
    let address = header as usize as u64;
    let mut info = MaybeUninit::<RegionWithPath>::zeroed();
    let size = std::mem::size_of::<RegionWithPath>();
    let abi_size = i32::try_from(size).map_err(|_| io::Error::other("executable region ABI size exceeds i32"))?;
    // SAFETY: valid writable buffer of the advertised ABI size, current PID,
    // flavor 8 (PROC_PIDREGIONPATHINFO), and the actual mapped header address.
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id().cast_signed(),
            8,
            address,
            info.as_mut_ptr().cast(),
            abi_size,
        )
    };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(n).ok() != Some(size) {
        return Err(io::Error::other("executable region ABI size mismatch"));
    }
    // SAFETY: the successful query filled the complete integer/array-only
    // repr(C) structure; zero initialization also covered any padding.
    let info = unsafe { info.assume_init() };
    let region = &info.region;
    if region.protection & libc::VM_PROT_EXECUTE as u32 == 0
        || region.offset != 0
        || address < region.address
        || address - region.address >= region.size
        || info.vnode.vip_vi.vi_stat.vst_ino == 0
    {
        return Err(io::Error::other(
            "main executable region is not a file-backed executable header",
        ));
    }
    Ok(info)
}

fn check(file: &File, info: &RegionWithPath) -> io::Result<()> {
    let metadata = file.metadata()?;
    let stat = &info.vnode.vip_vi.vi_stat;
    if !metadata.is_file()
        || metadata.dev() as u32 != stat.vst_dev
        || metadata.ino() != stat.vst_ino
        || i64::try_from(metadata.len()).ok() != Some(stat.vst_size)
        || metadata.mtime() != stat.vst_mtime
        || metadata.mtime_nsec() != stat.vst_mtimensec
        || metadata.ctime() != stat.vst_ctime
        || metadata.ctime_nsec() != stat.vst_ctimensec
    {
        return Err(io::Error::other(
            "executable descriptor does not match the mapped vnode",
        ));
    }
    Ok(())
}

pub(crate) fn open() -> io::Result<File> {
    let info = mapped()?;
    let bytes: Vec<u8> = info
        .vnode
        .vip_path
        .iter()
        .flatten()
        .map(|&byte| byte.cast_unsigned())
        .collect();
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| io::Error::other("executable vnode path is not terminated"))?;
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end]));
    if !path.is_absolute() {
        return Err(io::Error::other("executable vnode path is not absolute"));
    }
    let file = File::open(path)?;
    check(&file, &info)?;
    Ok(file)
}

pub(crate) fn validate(file: &File) -> io::Result<()> {
    check(file, &mapped()?)
}

#[cfg(test)]
mod tests;
