// SPDX-License-Identifier: GPL-2.0-or-later
//! Workflow (contract 02 §6.3), mirroring Redmine:
//!
//! - status transitions: tracker × role × old status → new status, with an
//!   `always`, `author` or `assignee` condition (`new_statuses_allowed_to`);
//! - field rules: tracker × role × current status → field readonly/required
//!   (`workflow_rule_by_attribute`).
//!
//! The roles passed in are the caller's roles that *consider workflow*
//! (`add_issues` or `edit_issues`); admins pass every role.

use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    Always,
    /// Applies only when the caller is the issue's author.
    Author,
    /// Applies only when the caller (or one of their groups) is the assignee.
    Assignee,
}

impl Condition {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "author" => Some(Self::Author),
            "assignee" => Some(Self::Assignee),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub tracker_id: Uuid,
    pub role_id: Uuid,
    pub old_status_id: Uuid,
    pub new_status_id: Uuid,
    pub condition: Condition,
}

/// Who the caller is relative to the issue, taken from the issue as it was
/// when the transaction started (the *baseline*): an earlier op in the same
/// document that assigns the issue to the caller must not unlock an
/// assignee-only transition (§6.3, T-P3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Baseline {
    pub is_author: bool,
    pub is_assignee: bool,
}

/// New statuses reachable from `current`, in the order of `status_order`.
/// Staying in the current status is always allowed.
pub fn allowed_statuses(
    transitions: &[Transition],
    role_ids: &[Uuid],
    tracker_id: Uuid,
    current: Uuid,
    baseline: Baseline,
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
                        && match t.condition {
                            Condition::Always => true,
                            Condition::Author => baseline.is_author,
                            Condition::Assignee => baseline.is_assignee,
                        }
                })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    Readonly,
    Required,
}

impl Rule {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "readonly" => Some(Self::Readonly),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Required => "required",
        }
    }
}

/// One `workflow_field_rules` row, already narrowed to the issue's tracker
/// and current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRule {
    pub role_id: Uuid,
    pub field_key: String,
    pub rule: Rule,
}

/// A custom field with `visible = false` and the roles that may read it
/// (`field_roles`), keyed like rules (`cf:<id>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenField {
    pub field_key: String,
    pub roles: Vec<Uuid>,
}

/// Redmine `Issue#workflow_rule_by_attribute` (§6.3):
///
/// ```text
/// roles empty → no rules
/// rules = rows for (tracker, current status, role ∈ roles)
/// if rules not empty:
///     for each visible=false field f, each role r not in field_roles(f):
///         rules[f][r] = readonly
/// per field: some role has no rule → no rule;
///            all roles agree → that rule; readonly and required mixed → required
/// ```
///
/// `rules` must already be narrowed to the tracker and status; rows for roles
/// outside `roles` are ignored.
pub fn field_rules(
    rules: &[FieldRule],
    roles: &[Uuid],
    hidden: &[HiddenField],
) -> BTreeMap<String, Rule> {
    let roles: BTreeSet<Uuid> = roles.iter().copied().collect();
    let mut out = BTreeMap::new();
    if roles.is_empty() {
        return out;
    }
    let mut by_field: BTreeMap<String, BTreeMap<Uuid, Rule>> = BTreeMap::new();
    for r in rules.iter().filter(|r| roles.contains(&r.role_id)) {
        by_field
            .entry(r.field_key.clone())
            .or_default()
            .insert(r.role_id, r.rule);
    }
    if by_field.is_empty() {
        return out;
    }
    for f in hidden {
        for role in &roles {
            if !f.roles.contains(role) {
                by_field
                    .entry(f.field_key.clone())
                    .or_default()
                    .insert(*role, Rule::Readonly);
            }
        }
    }
    for (field, per_role) in by_field {
        if per_role.len() < roles.len() {
            continue;
        }
        let distinct: BTreeSet<Rule> = per_role.values().copied().collect();
        let rule = if distinct.len() == 1 {
            *distinct.iter().next().expect("one rule")
        } else {
            Rule::Required
        };
        out.insert(field, rule);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn t(role: Uuid, old: Uuid, new: Uuid, condition: Condition) -> Transition {
        Transition {
            tracker_id: u(1),
            role_id: role,
            old_status_id: old,
            new_status_id: new,
            condition,
        }
    }

    #[test]
    fn union_over_roles_and_current_kept() {
        let (bug, dev, rep) = (u(1), u(2), u(3));
        let (new, doing, done) = (u(10), u(11), u(12));
        let ts = vec![
            t(dev, new, doing, Condition::Always),
            t(rep, new, done, Condition::Always),
        ];
        let order = [new, doing, done];
        let b = Baseline::default();
        assert_eq!(
            allowed_statuses(&ts, &[dev], bug, new, b, &order),
            vec![new, doing]
        );
        assert_eq!(
            allowed_statuses(&ts, &[dev, rep], bug, new, b, &order),
            vec![new, doing, done]
        );
        assert_eq!(allowed_statuses(&ts, &[], bug, new, b, &order), vec![new]);
        assert_eq!(
            allowed_statuses(&ts, &[dev], u(9), new, b, &order),
            vec![new]
        );
    }

    /// T-P3 at the unit level: the condition is judged on the baseline, and
    /// author/assignee rows apply only with that relation.
    #[test]
    fn author_and_assignee_conditions_use_baseline() {
        let (dev, new, doing, done) = (u(2), u(10), u(11), u(12));
        let ts = vec![
            t(dev, new, doing, Condition::Author),
            t(dev, new, done, Condition::Assignee),
        ];
        let order = [new, doing, done];
        let none = Baseline::default();
        let author = Baseline {
            is_author: true,
            is_assignee: false,
        };
        let assignee = Baseline {
            is_author: false,
            is_assignee: true,
        };
        assert_eq!(
            allowed_statuses(&ts, &[dev], u(1), new, none, &order),
            vec![new]
        );
        assert_eq!(
            allowed_statuses(&ts, &[dev], u(1), new, author, &order),
            vec![new, doing]
        );
        assert_eq!(
            allowed_statuses(&ts, &[dev], u(1), new, assignee, &order),
            vec![new, done]
        );
        // The same transition with an always row is open to everyone.
        let mut both = ts.clone();
        both.push(t(dev, new, done, Condition::Always));
        assert_eq!(
            allowed_statuses(&both, &[dev], u(1), new, none, &order),
            vec![new, done]
        );
    }

    fn r(role: Uuid, key: &str, rule: Rule) -> FieldRule {
        FieldRule {
            role_id: role,
            field_key: key.into(),
            rule,
        }
    }

    /// T-P1: each case of the §6.3 algorithm.
    #[test]
    fn field_rule_reducer() {
        let (a, b) = (u(2), u(3));
        use Rule::*;
        // One role without a rule + one role required → no rule.
        let got = field_rules(&[r(b, "subject", Required)], &[a, b], &[]);
        assert!(got.is_empty(), "{got:?}");
        // One role without a rule + one role readonly → no rule.
        let got = field_rules(&[r(b, "subject", Readonly)], &[a, b], &[]);
        assert!(got.is_empty(), "{got:?}");
        // Readonly + required → required.
        let got = field_rules(
            &[r(a, "subject", Readonly), r(b, "subject", Required)],
            &[a, b],
            &[],
        );
        assert_eq!(got.get("subject"), Some(&Required));
        // Both readonly → readonly; both required → required.
        let got = field_rules(
            &[
                r(a, "subject", Readonly),
                r(b, "subject", Readonly),
                r(a, "due_date", Required),
                r(b, "due_date", Required),
            ],
            &[a, b],
            &[],
        );
        assert_eq!(got.get("subject"), Some(&Readonly));
        assert_eq!(got.get("due_date"), Some(&Required));
        // Empty role set → nothing, even with rules present.
        let got = field_rules(&[r(a, "subject", Readonly)], &[], &[]);
        assert!(got.is_empty());
        // Rows for roles the caller does not hold are ignored.
        let got = field_rules(&[r(b, "subject", Readonly)], &[a], &[]);
        assert!(got.is_empty());
        // Single role with a rule → that rule.
        let got = field_rules(&[r(a, "subject", Readonly)], &[a], &[]);
        assert_eq!(got.get("subject"), Some(&Readonly));
    }

    /// Admin = every role: one role without a rule cancels the rule (Redmine
    /// does the same; admins are not exempt from rules all roles share).
    #[test]
    fn admin_uses_all_roles() {
        let (a, b, c) = (u(2), u(3), u(4));
        let rules = [
            r(a, "subject", Rule::Readonly),
            r(b, "subject", Rule::Readonly),
        ];
        assert!(field_rules(&rules, &[a, b, c], &[]).is_empty());
        let mut all = rules.to_vec();
        all.push(r(c, "subject", Rule::Readonly));
        assert_eq!(
            field_rules(&all, &[a, b, c], &[]).get("subject"),
            Some(&Rule::Readonly)
        );
    }

    /// Hidden custom fields become readonly for roles that cannot read them,
    /// but only when some rule exists for the tracker/status at all.
    #[test]
    fn hidden_fields_readonly_only_when_rules_exist() {
        let (a, b) = (u(2), u(3));
        let hidden = [HiddenField {
            field_key: "cf:7".into(),
            roles: vec![a],
        }];
        // No rules → no synthesized readonly (oxledger blocks unreadable
        // fields by its own rule instead, §6.3).
        assert!(field_rules(&[], &[b], &hidden).is_empty());
        let rules = [r(b, "subject", Rule::Required)];
        let got = field_rules(&rules, &[b], &hidden);
        assert_eq!(got.get("cf:7"), Some(&Rule::Readonly));
        // Role a can read it and has no rule for cf:7 → no rule overall.
        let got = field_rules(
            &[
                r(a, "subject", Rule::Required),
                r(b, "subject", Rule::Required),
            ],
            &[a, b],
            &hidden,
        );
        assert_eq!(got.get("cf:7"), None);
        assert_eq!(got.get("subject"), Some(&Rule::Required));
        // A hidden field with no readers at all → readonly.
        let nobody = [HiddenField {
            field_key: "cf:8".into(),
            roles: vec![],
        }];
        let got = field_rules(&rules, &[b], &nobody);
        assert_eq!(got.get("cf:8"), Some(&Rule::Readonly));
    }
}
