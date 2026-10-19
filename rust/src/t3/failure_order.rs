use super::{
    timestamp,
    types::{EvidenceScope, FailureEvidenceOrder},
};
use std::cmp::Ordering;

pub fn failure_order_with_era(
    mut order: FailureEvidenceOrder,
    existing: Option<&FailureEvidenceOrder>,
) -> FailureEvidenceOrder {
    let Some((existing, admission)) = existing.zip(order.read_started_at) else {
        return order;
    };
    let boundary = existing.protocol_started_at.unwrap_or(0);
    if admission < boundary {
        return order;
    }
    let started = if order.protocol_version == existing.protocol_version {
        boundary
    } else if admission > existing.read_started_at.unwrap_or(0) {
        admission
    } else {
        0
    };
    if started != 0 {
        order.protocol_started_at = Some(started);
    }
    order
}
pub fn compare_failure_order(
    candidate: &FailureEvidenceOrder,
    existing: &FailureEvidenceOrder,
) -> Ordering {
    if candidate
        .read_started_at
        .is_some_and(|at| at < existing.protocol_started_at.unwrap_or(0))
    {
        return Ordering::Less;
    }
    if existing
        .read_started_at
        .is_some_and(|at| at < candidate.protocol_started_at.unwrap_or(0))
    {
        return Ordering::Greater;
    }
    if candidate.protocol_version != existing.protocol_version {
        return candidate
            .read_started_at
            .unwrap_or(0)
            .cmp(&existing.read_started_at.unwrap_or(0));
    }
    candidate
        .snapshot_sequence
        .cmp(&existing.snapshot_sequence)
        .then_with(|| {
            compare_time(
                candidate.updated_at.as_deref(),
                existing.updated_at.as_deref(),
            )
        })
        .then_with(|| {
            (candidate.scope == EvidenceScope::Full).cmp(&(existing.scope == EvidenceScope::Full))
        })
        .then_with(|| match (&candidate.item, &existing.item) {
            (Some(a), Some(b)) => compare_time(Some(&a.updated_at), Some(&b.updated_at))
                .then(a.ordinal.cmp(&b.ordinal))
                .then(a.id.cmp(&b.id)),
            _ => Ordering::Equal,
        })
}
fn compare_time(a: Option<&str>, b: Option<&str>) -> Ordering {
    match (timestamp(a), timestamp(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => Ordering::Equal,
    }
}
pub fn same_persisted_failure_order(
    left: Option<&FailureEvidenceOrder>,
    right: Option<&FailureEvidenceOrder>,
) -> bool {
    let strip = |order: &FailureEvidenceOrder| {
        let mut order = order.clone();
        order.read_started_at = None;
        order
    };
    left.map(strip) == right.map(strip)
}
