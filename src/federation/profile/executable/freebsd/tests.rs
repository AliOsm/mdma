use super::*;

#[test]
fn vmmap_entries_match_the_kernel_abi() {
    // KINFO_VMENTRY_SIZE in sys/user.h.
    assert_eq!(size_of::<libc::kinfo_vmentry>(), 1160);
}

#[test]
fn the_mapped_vnode_matches_the_opened_executable_only() {
    let file = open().unwrap();
    validate(&file).unwrap();
    let copy = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), copy.path()).unwrap();
    assert!(validate(copy.as_file()).is_err(), "same bytes, another file");
}
