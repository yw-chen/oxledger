// SPDX-License-Identifier: GPL-2.0-or-later
//! Delegation write policies (contract 02 §16.3, D20).
//!
//! Inside a delegation's ceiling (`proposal_ops`, `writable_fields`) each
//! command and each field has a mode: `direct` (the agent writes), `review`
//! (a ChangeSet a person approves) or `off`. A missing row means "not in the
//! delegation" (rule 1). A whole document gets one decision (§16.4):
//! `off` anywhere → refused; else `review` anywhere → review; else direct.

use crate::commands::Parsed;
use crate::error::{ApiError, ErrorCode};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    sqlx::Type,
    schemars::JsonSchema,
)]
#[sqlx(type_name = "agent_policy_mode", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum PolicyMode {
    Direct,
    Review,
    Off,
}

/// The decision for a whole document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Direct,
    Review,
    /// Refused: the first op (and field) that is off or outside the policy.
    Off {
        op_index: usize,
        command: String,
        field: Option<String>,
    },
}

impl Decision {
    /// `forbidden_op` for [`Decision::Off`] (§16.8: fix_input is not offered;
    /// the delegator decides).
    pub fn refusal(&self) -> Option<anyhow::Error> {
        let Decision::Off {
            op_index,
            command,
            field,
        } = self
        else {
            return None;
        };
        let what = match field {
            Some(f) => format!("{command} of {f}"),
            None => command.clone(),
        };
        Some(
            ApiError::new(
                ErrorCode::ForbiddenOp,
                format!("the delegation does not allow {what}"),
            )
            .with_details(json!({"reason": "policy", "op_index": op_index, "command": command, "field": field}))
            .into(),
        )
    }
}

/// The policy rows of a delegation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policies {
    pub commands: BTreeMap<String, PolicyMode>,
    pub fields: BTreeMap<String, PolicyMode>,
}

impl Policies {
    /// Rule 2 over a parsed document: `off` > `review` > `direct`.
    pub(crate) fn decide(&self, doc: &Parsed) -> Decision {
        self.decide_items(&doc.policy_items())
    }

    pub fn decide_items(&self, items: &[(usize, &str, Vec<String>)]) -> Decision {
        let mut review = false;
        for (i, command, fields) in items {
            let off = |field: Option<&String>| Decision::Off {
                op_index: *i,
                command: command.to_string(),
                field: field.cloned(),
            };
            match self.commands.get(*command) {
                None | Some(PolicyMode::Off) => return off(None),
                Some(PolicyMode::Review) => review = true,
                Some(PolicyMode::Direct) => {}
            }
            for f in fields {
                match self.fields.get(f) {
                    None | Some(PolicyMode::Off) => return off(Some(f)),
                    Some(PolicyMode::Review) => review = true,
                    Some(PolicyMode::Direct) => {}
                }
            }
        }
        if review {
            Decision::Review
        } else {
            Decision::Direct
        }
    }
}

/// Reads the rows. The caller holds the delegation row (FOR SHARE at
/// execution, rule 4).
pub async fn load(conn: &mut PgConnection, delegation: Uuid) -> Result<Policies> {
    let commands: Vec<(String, PolicyMode)> = sqlx::query_as(
        "SELECT command_kind, mode FROM delegation_command_policies WHERE delegation_id = $1",
    )
    .bind(delegation)
    .fetch_all(&mut *conn)
    .await?;
    let fields: Vec<(String, PolicyMode)> = sqlx::query_as(
        "SELECT field_key, mode FROM delegation_field_policies WHERE delegation_id = $1",
    )
    .bind(delegation)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Policies {
        commands: commands.into_iter().collect(),
        fields: fields.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use PolicyMode::*;

    fn p(c: &[(&str, PolicyMode)], f: &[(&str, PolicyMode)]) -> Policies {
        Policies {
            commands: c.iter().map(|(k, m)| (k.to_string(), *m)).collect(),
            fields: f.iter().map(|(k, m)| (k.to_string(), *m)).collect(),
        }
    }

    #[test]
    fn composition() {
        let all = p(
            &[
                ("create_issue", Direct),
                ("update_fields", Direct),
                ("add_note", Review),
            ],
            &[("subject", Direct), ("description", Review)],
        );
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            all.decide_items(&[(0, "update_fields", s(&["subject"]))]),
            Decision::Direct
        );
        // Field review → review (T-D2: also on create).
        assert_eq!(
            all.decide_items(&[(0, "create_issue", s(&["subject", "description"]))]),
            Decision::Review
        );
        // Command review, field direct → review.
        assert_eq!(
            all.decide_items(&[(0, "add_note", vec![])]),
            Decision::Review
        );
        // Missing rows are off (rule 1), and off wins over review.
        assert_eq!(
            all.decide_items(&[(0, "add_note", vec![]), (1, "transition", vec![])]),
            Decision::Off {
                op_index: 1,
                command: "transition".into(),
                field: None
            }
        );
        assert_eq!(
            all.decide_items(&[(0, "update_fields", s(&["due_date"]))]),
            Decision::Off {
                op_index: 0,
                command: "update_fields".into(),
                field: Some("due_date".into())
            }
        );
        let off = p(&[("update_fields", Direct)], &[("subject", Off)]);
        assert!(matches!(
            off.decide_items(&[(0, "update_fields", s(&["subject"]))]),
            Decision::Off { .. }
        ));
    }
}
