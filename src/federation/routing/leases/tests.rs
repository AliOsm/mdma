use super::*;

fn client(pin: &str) -> Holder {
    Holder::Client(pin.into())
}

#[test]
fn the_first_lease_on_an_alias_starts_its_keeper() {
    let leases = RemoteLeases::default();
    let (first, start) = leases.take(client("a"), "remote-native", None).unwrap();
    assert!(start);
    let (again, start) = leases.take(client("a"), "remote-native", Some(Duration::from_secs(1))).unwrap();
    assert!(!start && again.id == first.id && again.ttl == lease::MIN_TTL, "renewed in place");
    let (_, start) = leases.take(client("b"), "remote-native", None).unwrap();
    assert!(!start, "one keeper per alias");
    let (_, start) = leases.take(client("a"), "remote-laya", None).unwrap();
    assert!(start);
}

#[test]
fn a_keeper_stops_once_its_last_lease_ends_and_a_new_lease_starts_another() {
    let leases = RemoteLeases::default();
    let (a, _) = leases.take(client("a"), "remote-native", Some(lease::MIN_TTL)).unwrap();
    let (b, _) = leases.take(client("b"), "remote-native", None).unwrap();
    assert_eq!(leases.count("remote-native", Instant::now()), 2);
    assert!(leases.keep("remote-native", Instant::now()));
    leases.release(&client("b"), &b.id).unwrap();
    assert_eq!(leases.release(&client("b"), &b.id), Err(LeaseError::Unknown));
    assert_eq!(leases.renew(&client("b"), &a.id, None), Err(LeaseError::Unknown), "only the holder renews");
    assert!(leases.keep("remote-native", Instant::now()));
    assert!(!leases.keep("remote-native", Instant::now() + lease::MIN_TTL), "the last lease expired");
    let (_, start) = leases.take(client("a"), "remote-native", None).unwrap();
    assert!(start, "a stopped keeper is replaced");
}
