use super::*;
use std::assert_matches;

fn client(pin: &str) -> Holder {
    Holder::Client(pin.into())
}

#[test]
fn one_lease_per_holder_and_model_renewed_in_place() {
    let table = Table::default();
    let (first, new) = table.take(client("a"), "m".into(), 0, None).unwrap();
    assert!(new);
    assert_eq!((first.model.as_str(), first.ttl), ("m", DEFAULT_TTL));
    assert_eq!(first.id.len(), 32);
    let (again, new) = table
        .take(client("a"), "m".into(), 0, Some(Duration::from_secs(1)))
        .unwrap();
    assert!(!new);
    assert_eq!(
        (again.id, again.ttl),
        (first.id.clone(), MIN_TTL),
        "ttl is clamped"
    );
    let (other, new) = table
        .take(client("b"), "m".into(), 0, Some(Duration::from_hours(1)))
        .unwrap();
    assert!(new && other.id != first.id);
    assert_eq!(other.ttl, MAX_TTL);
}

#[test]
fn only_the_holder_renews_or_releases() {
    let table = Table::default();
    let (lease, _) = table.take(client("a"), "m".into(), 3, None).unwrap();
    assert_eq!(
        table.renew(&client("b"), &lease.id, None),
        Err(LeaseError::Unknown)
    );
    assert_eq!(
        table.release(&client("b"), &lease.id),
        Err(LeaseError::Unknown)
    );
    assert!(table.renew(&client("a"), &lease.id, None).is_ok());
    assert_eq!(table.release(&client("a"), &lease.id), Ok(3));
    assert_eq!(
        table.release(&client("a"), &lease.id),
        Err(LeaseError::Unknown)
    );
}

#[test]
fn holders_are_capped_and_leases_expire() {
    let table = Table::default();
    for slot in 0..PER_PEER {
        table
            .take(Holder::Peer("p".into()), format!("m{slot}"), slot, None)
            .unwrap();
    }
    assert_matches!(
        table.take(Holder::Peer("p".into()), "x".into(), 99, None),
        Err(LeaseError::Limit(_, 4))
    );
    table
        .take(client("a"), "m".into(), 0, Some(MIN_TTL))
        .unwrap();
    assert!(table.expire(Instant::now()).is_empty());
    let mut expired = table.expire(Instant::now() + MIN_TTL);
    expired.sort_unstable();
    assert_eq!(expired, [0]);
    assert_eq!(table.expire(Instant::now() + MAX_TTL).len(), PER_PEER);
}
