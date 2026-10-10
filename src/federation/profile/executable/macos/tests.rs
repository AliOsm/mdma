use super::*;

#[test]
fn darwin_region_abi_matches_public_proc_info_header() {
    assert_eq!(std::mem::size_of::<RegionInfo>(), 96);
    assert_eq!(std::mem::offset_of!(RegionWithPath, vnode), 96);
    assert_eq!(std::mem::size_of::<RegionWithPath>(), 1272);
}

#[test]
fn the_actual_header_vnode_matches_its_open_descriptor() {
    let file = open().unwrap();
    validate(&file).unwrap();
    let other = tempfile::NamedTempFile::new().unwrap();
    assert!(validate(other.as_file()).is_err());
}
