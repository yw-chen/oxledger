// SPDX-License-Identifier: GPL-2.0-or-later
//! R-G6 (review) and R-G7 (CI) roll-ups: pure functions over what is
//! stored, so webhooks, backfill and reconciliation agree.

/// One review: reviewer id, GitHub state (`APPROVED`, `CHANGES_REQUESTED`,
/// `COMMENTED`, `DISMISSED`, `PENDING`), submitted order (larger = later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub reviewer: i64,
    pub state: String,
    pub order: i64,
}

/// R-G6: the latest review per reviewer that is a decision; any
/// CHANGES_REQUESTED → `changes_requested`; else APPROVED → `approved`;
/// else a pending request → `pending`; else `none`. A COMMENTED review does
/// not replace an earlier decision; a DISMISSED one clears it.
pub fn review_state(reviews: &[Review], pending_requests: usize) -> &'static str {
    let mut latest: Vec<(i64, i64, &str)> = Vec::new();
    for r in reviews {
        let s = r.state.as_str();
        if !matches!(s, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue;
        }
        match latest.iter_mut().find(|(who, _, _)| *who == r.reviewer) {
            Some(x) if x.1 < r.order => *x = (r.reviewer, r.order, s),
            Some(_) => {}
            None => latest.push((r.reviewer, r.order, s)),
        }
    }
    if latest.iter().any(|x| x.2 == "CHANGES_REQUESTED") {
        "changes_requested"
    } else if latest.iter().any(|x| x.2 == "APPROVED") {
        "approved"
    } else if pending_requests > 0 {
        "pending"
    } else {
        "none"
    }
}

/// One CI unit of R-G7: a check suite, a run without a suite, or a commit
/// status context. `pending` true = not completed; `conclusion` for
/// completed ones (status API states map to conclusions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub pending: bool,
    pub conclusion: Option<String>,
}

const FAILING: &[&str] = &[
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
    "error",
];

/// R-G7: failure > pending > success; nothing → `None`; neutral and
/// skipped do not count.
pub fn ci_state(units: &[Unit]) -> Option<&'static str> {
    if units.iter().any(|u| {
        !u.pending
            && u.conclusion
                .as_deref()
                .is_some_and(|c| FAILING.contains(&c))
    }) {
        return Some("failure");
    }
    if units.iter().any(|u| u.pending) {
        return Some("pending");
    }
    if units
        .iter()
        .any(|u| u.conclusion.as_deref() == Some("success"))
    {
        return Some("success");
    }
    None
}

/// A commit status state as a unit.
pub fn status_unit(state: &str) -> Unit {
    match state {
        "pending" => Unit {
            pending: true,
            conclusion: None,
        },
        s => Unit {
            pending: false,
            conclusion: Some(s.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(who: i64, s: &str, o: i64) -> Review {
        Review {
            reviewer: who,
            state: s.into(),
            order: o,
        }
    }

    fn done(c: &str) -> Unit {
        Unit {
            pending: false,
            conclusion: Some(c.into()),
        }
    }

    #[test]
    fn reviews() {
        // T-G8: A approves, B requests changes, B approves.
        let rs = [
            r(1, "APPROVED", 1),
            r(2, "CHANGES_REQUESTED", 2),
            r(2, "APPROVED", 3),
        ];
        assert_eq!(review_state(&rs, 0), "approved");
        assert_eq!(review_state(&rs[..2], 0), "changes_requested");
        assert_eq!(review_state(&[r(1, "COMMENTED", 1)], 1), "pending");
        assert_eq!(review_state(&[], 0), "none");
        assert_eq!(
            review_state(&[r(1, "APPROVED", 1), r(1, "COMMENTED", 2)], 0),
            "approved"
        );
        assert_eq!(
            review_state(&[r(1, "CHANGES_REQUESTED", 1), r(1, "DISMISSED", 2)], 0),
            "none"
        );
        // Out of order input: order decides.
        assert_eq!(
            review_state(&[r(2, "APPROVED", 3), r(2, "CHANGES_REQUESTED", 2)], 0),
            "approved"
        );
    }

    #[test]
    fn ci() {
        let pend = Unit {
            pending: true,
            conclusion: None,
        };
        assert_eq!(ci_state(&[]), None);
        assert_eq!(
            ci_state(&[done("success"), done("failure")]),
            Some("failure")
        );
        assert_eq!(ci_state(&[done("success"), pend.clone()]), Some("pending"));
        assert_eq!(ci_state(&[done("failure"), pend.clone()]), Some("failure"));
        assert_eq!(
            ci_state(&[done("success"), done("neutral"), done("skipped")]),
            Some("success")
        );
        assert_eq!(ci_state(&[done("neutral"), done("skipped")]), None);
        for f in FAILING {
            assert_eq!(
                ci_state(&[done("success"), done(f)]),
                Some("failure"),
                "{f}"
            );
        }
        assert_eq!(ci_state(&[status_unit("error")]), Some("failure"));
        assert_eq!(ci_state(&[status_unit("pending")]), Some("pending"));
    }
}
