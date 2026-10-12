use super::*;

#[test]
fn beagle_partition_matches_libedgetpu() {
    assert_eq!(simple_entries(PAGE_TABLE_ENTRIES), 6144);
    assert_eq!(extended_entries(PAGE_TABLE_ENTRIES), 2048);
    assert_eq!(simple_entries(100), MIN_SIMPLE_ENTRIES);
    assert_eq!(AddressSpace::extended(2048).size, 4096 * 1024 * 1024);
    assert!(AddressSpace::extended(2048).contains(EXTENDED_BIT));
    assert!(!AddressSpace::simple(16).contains(EXTENDED_BIT));
}

#[test]
fn allocations_are_pages_first_fit_and_coalesce() {
    let mut space = AddressSpace::new(0x1000, 8 * PAGE);
    let a = space.allocate(1).unwrap();
    let b = space.allocate(PAGE + 1).unwrap();
    let c = space.allocate(PAGE).unwrap();
    assert_eq!((a, b, c), (0x1000, 0x2000, 0x4000));
    assert_eq!(space.allocated_bytes(), 4 * PAGE);
    space.free(b, PAGE + 1).unwrap();
    assert_eq!(space.allocate(2 * PAGE).unwrap(), b, "the freed run is reused first");
    space.free(a, 1).unwrap();
    space.free(b, 2 * PAGE).unwrap();
    space.free(c, PAGE).unwrap();
    assert_eq!(space.free_bytes(), 8 * PAGE);
    assert_eq!(space.allocate(8 * PAGE).unwrap(), 0x1000, "freed neighbours coalesce into the whole range");
}

#[test]
fn errors_name_the_problem() {
    let mut space = AddressSpace::new(0, 2 * PAGE);
    assert!(matches!(space.allocate(0), Err(Error::Protocol(_))));
    assert!(matches!(space.allocate(3 * PAGE), Err(Error::AddressSpace { requested, free }) if requested == 3 * PAGE && free == 2 * PAGE));
    let a = space.allocate(PAGE).unwrap();
    assert!(matches!(space.free(a, 2 * PAGE), Err(Error::Protocol(_))));
    assert!(matches!(space.free(a + PAGE, PAGE), Err(Error::Protocol(_))));
    space.free(a, PAGE).unwrap();
    assert!(matches!(space.free(a, PAGE), Err(Error::Protocol(_))), "double free");
}
