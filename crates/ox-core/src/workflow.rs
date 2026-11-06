// SPDX-License-Identifier: GPL-2.0-or-later
//! Status transitions: tracker × role × old status → new status
//! (Redmine `WorkflowTransition`). A caller may move an issue to any status
//! allowed by at least one of their roles; staying in the current status is
//! always allowed.

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub tracker_id: Uuid,
    pub role_id: Uuid,
    pub old_status_id: Uuid,
    pub new_status_id: Uuid,
}

/// New statuses reachable from `current`, in the order of `status_order`.
pub fn allowed_statuses(
    transitions: &[Transition],
    role_ids: &[Uuid],
    tracker_id: Uuid,
    current: Uuid,
    status_order: &[Uuid],
) -> Vec<Uuid> {
    status_order
        .iter()
        .copied()
        .filter(|s| {
            *s == current
                || transitions.iter().any(|t| {
                    t.tracker_id == tracker_id
                        && t.old_status_id == current
                        && t.new_status_id == *s
                        && role_ids.contains(&t.role_id)
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_over_roles_and_current_kept() {
        let (bug, dev, rep) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let (new, doing, done) = (
            Uuid::from_u128(10),
            Uuid::from_u128(11),
            Uuid::from_u128(12),
        );
        let t = vec![
            Transition {
                tracker_id: bug,
                role_id: dev,
                old_status_id: new,
                new_status_id: doing,
            },
            Transition {
                tracker_id: bug,
                role_id: rep,
                old_status_id: new,
                new_status_id: done,
            },
        ];
        let order = [new, doing, done];
        assert_eq!(
            allowed_statuses(&t, &[dev], bug, new, &order),
            vec![new, doing]
        );
        assert_eq!(
            allowed_statuses(&t, &[dev, rep], bug, new, &order),
            vec![new, doing, done]
        );
        assert_eq!(allowed_statuses(&t, &[], bug, new, &order), vec![new]);
        assert_eq!(
            allowed_statuses(&t, &[dev], Uuid::from_u128(9), new, &order),
            vec![new]
        );
    }
}
