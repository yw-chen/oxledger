// SPDX-License-Identifier: GPL-2.0-or-later
//! Filter AST over issue custom fields (decision D14).
//!
//! A [`Filter`] is fully typed: every condition names a field by id together
//! with its [`FieldType`], and every value already has the field's value kind.
//! Callers turn user input (field names, option labels, logins) into this form
//! first, then the server compiles it to SQL (`oxledger::filter::push_filter`).
//! [`Filter::eval`] is the in-memory form with the same semantics, used as the
//! reference in tests.
//!
//! Every condition also carries `applies`, a visibility-style [`Pred`] saying
//! for which issues the field counts at all: the field applies to the issue's
//! tracker and project, and the caller may read the field in that project.
//! Outside `applies` a stored value is ignored. This follows Redmine
//! (`Query#sql_for_custom_field` + `IssueCustomField#visibility_by_project_condition`,
//! research doc 02 §7).
//!
//! Semantics (identical in SQL and [`Filter::eval`]), with E = `applies`:
//! * a comparison, range, `in` or `not_null` is true only when E holds and the
//!   issue has a value that satisfies it;
//! * `ne x` is the complement of `eq x`: true when E fails or no value equals x
//!   (missing values count, as in Redmine's `!`);
//! * `is_null` is true when E holds and the issue has no value (Redmine's `!*`).
//!   So `is_null` and `not(not_null)` differ when E fails — never rewrite one
//!   into the other;
//! * `And([])` is true, `Or([])` is false.
//!
//! Because each leaf only looks at values inside E, no boolean combination can
//! depend on a value the caller may not read.
//!
//! Storage rules this AST is built for (research doc 02 §5.3): each condition
//! compiles to one `EXISTS` / `NOT EXISTS` over `issue_cf` carrying a type
//! condition (`v_x IS NOT NULL`, or a comparison on `v_x`).

use crate::visibility::{IssueFacts, Pred};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use uuid::Uuid;

/// Upper bound on conditions in one filter, so a request cannot build a huge query.
pub const MAX_CONDITIONS: usize = 32;
/// Upper bound on nesting of and/or.
pub const MAX_DEPTH: usize = 4;
/// Upper bound on values in one `in`.
pub const MAX_IN_VALUES: usize = 100;

/// Custom field type, as stored in `field_defs.ftype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Int,
    Float,
    Date,
    /// Short text (indexed).
    String,
    /// Long text (not indexed; only `is_null` / `not_null`).
    Text,
    /// One option of the field's option list (stored as the option id).
    List,
    /// A user or group (stored as the principal id).
    User,
    Bool,
    Link,
}

/// Which typed value column a field type uses in `issue_cf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    Num,
    Date,
    Text,
    Ref,
    Bool,
}

impl FieldType {
    pub const ALL: [FieldType; 9] = [
        FieldType::Int,
        FieldType::Float,
        FieldType::Date,
        FieldType::String,
        FieldType::Text,
        FieldType::List,
        FieldType::User,
        FieldType::Bool,
        FieldType::Link,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::Int => "int",
            FieldType::Float => "float",
            FieldType::Date => "date",
            FieldType::String => "string",
            FieldType::Text => "text",
            FieldType::List => "list",
            FieldType::User => "user",
            FieldType::Bool => "bool",
            FieldType::Link => "link",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    pub fn kind(self) -> ValueKind {
        match self {
            FieldType::Int | FieldType::Float => ValueKind::Num,
            FieldType::Date => ValueKind::Date,
            FieldType::String | FieldType::Text | FieldType::Link => ValueKind::Text,
            FieldType::List | FieldType::User => ValueKind::Ref,
            FieldType::Bool => ValueKind::Bool,
        }
    }

    /// `lt` / `le` / `gt` / `ge` / range are allowed.
    pub fn is_ordered(self) -> bool {
        matches!(self, FieldType::Int | FieldType::Float | FieldType::Date)
    }

    /// Value tests (eq, in, …) are allowed; `text` only supports is_null / not_null
    /// because it has no index.
    pub fn has_value_tests(self) -> bool {
        self != FieldType::Text
    }
}

/// A resolved custom field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub id: i32,
    pub ty: FieldType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Num(f64),
    Date(NaiveDate),
    Text(String),
    Ref(Uuid),
    Bool(bool),
}

impl Value {
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Num(_) => ValueKind::Num,
            Value::Date(_) => ValueKind::Date,
            Value::Text(_) => ValueKind::Text,
            Value::Ref(_) => ValueKind::Ref,
            Value::Bool(_) => ValueKind::Bool,
        }
    }

    /// Ordering between values of the same kind; `None` across kinds.
    fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a.partial_cmp(b),
            (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Ref(a), Value::Ref(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    pub fn is_ordering(self) -> bool {
        matches!(self, Cmp::Lt | Cmp::Le | Cmp::Gt | Cmp::Ge)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Cmp(Cmp, Value),
    /// Inclusive on both ends; at least one end is set.
    Range {
        min: Option<Value>,
        max: Option<Value>,
    },
    In(Vec<Value>),
    IsNull,
    NotNull,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Cond {
        field: Field,
        test: Test,
        /// Issues for which the field counts (applicable and readable).
        applies: Pred,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    #[error("value for field {field} must be {expected:?}, got {got:?}")]
    WrongKind {
        field: i32,
        expected: ValueKind,
        got: ValueKind,
    },
    #[error("field {field} of type {ty} does not support {what}")]
    Unsupported {
        field: i32,
        ty: &'static str,
        what: &'static str,
    },
    #[error("field {field}: {msg}")]
    BadValue { field: i32, msg: String },
    #[error(
        "filter too large: at most {MAX_CONDITIONS} conditions and {MAX_DEPTH} levels of and/or"
    )]
    TooLarge,
}

impl Filter {
    /// Builds a validated condition that applies to every issue.
    pub fn cond(field: Field, test: Test) -> Result<Filter, FilterError> {
        Self::cond_where(field, test, Pred::True)
    }

    /// Builds a validated condition that only counts where `applies` holds.
    pub fn cond_where(field: Field, test: Test, applies: Pred) -> Result<Filter, FilterError> {
        check_cond(field, &test)?;
        Ok(Filter::Cond {
            field,
            test,
            applies: applies.simplify(),
        })
    }

    /// Checks every condition and the size limits. The SQL compiler assumes a
    /// validated filter.
    pub fn validate(&self) -> Result<(), FilterError> {
        let mut conditions = 0;
        self.walk(1, &mut conditions)
    }

    fn walk(&self, depth: usize, conditions: &mut usize) -> Result<(), FilterError> {
        match self {
            Filter::And(fs) | Filter::Or(fs) => {
                if depth > MAX_DEPTH {
                    return Err(FilterError::TooLarge);
                }
                fs.iter().try_for_each(|f| f.walk(depth + 1, conditions))
            }
            Filter::Cond { field, test, .. } => {
                *conditions += 1;
                if *conditions > MAX_CONDITIONS {
                    return Err(FilterError::TooLarge);
                }
                check_cond(*field, test)
            }
        }
    }

    /// Evaluates against one issue: its visibility facts and custom values (`get(field_id)`).
    pub fn eval<'v>(&self, facts: &IssueFacts, get: &dyn Fn(i32) -> Option<&'v Value>) -> bool {
        match self {
            Filter::And(fs) => fs.iter().all(|f| f.eval(facts, get)),
            Filter::Or(fs) => fs.iter().any(|f| f.eval(facts, get)),
            Filter::Cond {
                field,
                test,
                applies,
            } => {
                let e = applies.eval(facts);
                // Outside E the value does not count, whatever is stored.
                let v = if e { get(field.id) } else { None };
                let ord = |x: &Value| v.and_then(|v| v.compare(x));
                match test {
                    Test::Cmp(Cmp::Ne, x) => ord(x) != Some(Ordering::Equal),
                    Test::Cmp(op, x) => ord(x).is_some_and(|o| match op {
                        Cmp::Eq => o.is_eq(),
                        Cmp::Lt => o.is_lt(),
                        Cmp::Le => o.is_le(),
                        Cmp::Gt => o.is_gt(),
                        Cmp::Ge => o.is_ge(),
                        Cmp::Ne => unreachable!(),
                    }),
                    Test::Range { min, max } => {
                        v.is_some()
                            && min
                                .as_ref()
                                .is_none_or(|m| ord(m).is_some_and(|o| o.is_ge()))
                            && max
                                .as_ref()
                                .is_none_or(|m| ord(m).is_some_and(|o| o.is_le()))
                    }
                    Test::In(xs) => xs.iter().any(|x| ord(x).is_some_and(|o| o.is_eq())),
                    Test::IsNull => e && v.is_none(),
                    Test::NotNull => v.is_some(),
                }
            }
        }
    }

    /// Fields the filter mentions (deduplicated, in first-use order).
    pub fn fields(&self) -> Vec<Field> {
        let mut out = Vec::new();
        self.collect_fields(&mut out);
        out
    }

    fn collect_fields(&self, out: &mut Vec<Field>) {
        match self {
            Filter::And(fs) | Filter::Or(fs) => fs.iter().for_each(|f| f.collect_fields(out)),
            Filter::Cond { field, .. } => {
                if !out.contains(field) {
                    out.push(*field);
                }
            }
        }
    }
}

fn check_value(field: Field, v: &Value) -> Result<(), FilterError> {
    let expected = field.ty.kind();
    if v.kind() != expected {
        return Err(FilterError::WrongKind {
            field: field.id,
            expected,
            got: v.kind(),
        });
    }
    match v {
        Value::Num(n) if !n.is_finite() => Err(FilterError::BadValue {
            field: field.id,
            msg: "number must be finite".into(),
        }),
        Value::Num(n) if field.ty == FieldType::Int && n.fract() != 0.0 => {
            Err(FilterError::BadValue {
                field: field.id,
                msg: format!("{n} is not an integer"),
            })
        }
        _ => Ok(()),
    }
}

fn check_cond(field: Field, test: &Test) -> Result<(), FilterError> {
    let unsupported = |what| FilterError::Unsupported {
        field: field.id,
        ty: field.ty.as_str(),
        what,
    };
    if !matches!(test, Test::IsNull | Test::NotNull) && !field.ty.has_value_tests() {
        return Err(unsupported("value tests (only is_null / not_null)"));
    }
    match test {
        Test::Cmp(op, v) => {
            if op.is_ordering() && !field.ty.is_ordered() {
                return Err(unsupported("ordering comparisons"));
            }
            check_value(field, v)
        }
        Test::Range { min, max } => {
            if !field.ty.is_ordered() {
                return Err(unsupported("ranges"));
            }
            if min.is_none() && max.is_none() {
                return Err(FilterError::BadValue {
                    field: field.id,
                    msg: "range needs a min or a max".into(),
                });
            }
            min.iter()
                .chain(max)
                .try_for_each(|v| check_value(field, v))
        }
        Test::In(vs) => {
            if vs.is_empty() || vs.len() > MAX_IN_VALUES {
                return Err(FilterError::BadValue {
                    field: field.id,
                    msg: format!("`in` needs 1 to {MAX_IN_VALUES} values"),
                });
            }
            vs.iter().try_for_each(|v| check_value(field, v))
        }
        Test::IsNull | Test::NotNull => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const NUM: Field = Field {
        id: 1,
        ty: FieldType::Float,
    };
    const INT: Field = Field {
        id: 2,
        ty: FieldType::Int,
    };
    const DUE: Field = Field {
        id: 3,
        ty: FieldType::Date,
    };
    const SEV: Field = Field {
        id: 4,
        ty: FieldType::List,
    };
    const NOTE: Field = Field {
        id: 5,
        ty: FieldType::Text,
    };
    const FLAG: Field = Field {
        id: 6,
        ty: FieldType::Bool,
    };

    fn d(s: &str) -> Value {
        Value::Date(s.parse().unwrap())
    }

    fn facts() -> IssueFacts {
        IssueFacts {
            project_id: Uuid::from_u128(100),
            tracker_id: Uuid::from_u128(7),
            is_private: false,
            author_id: Uuid::from_u128(1),
            assigned_to_id: None,
        }
    }

    fn eval(f: &Filter, vals: &[(i32, Value)]) -> bool {
        let m: HashMap<i32, Value> = vals.iter().cloned().collect();
        f.eval(&facts(), &|id| m.get(&id))
    }

    fn c(field: Field, test: Test) -> Filter {
        Filter::cond(field, test).unwrap()
    }

    #[test]
    fn comparisons_need_a_value_except_ne() {
        let gt = c(NUM, Test::Cmp(Cmp::Gt, Value::Num(2.0)));
        assert!(eval(&gt, &[(1, Value::Num(2.5))]));
        assert!(!eval(&gt, &[(1, Value::Num(2.0))]));
        assert!(!eval(&gt, &[]), "missing value never compares");

        let ne = c(NUM, Test::Cmp(Cmp::Ne, Value::Num(2.0)));
        assert!(eval(&ne, &[]), "ne includes issues without a value");
        assert!(eval(&ne, &[(1, Value::Num(3.0))]));
        assert!(!eval(&ne, &[(1, Value::Num(2.0))]));
    }

    #[test]
    fn range_is_inclusive_and_may_be_open() {
        let r = c(
            DUE,
            Test::Range {
                min: Some(d("2026-01-01")),
                max: Some(d("2026-01-31")),
            },
        );
        assert!(eval(&r, &[(3, d("2026-01-01"))]));
        assert!(eval(&r, &[(3, d("2026-01-31"))]));
        assert!(!eval(&r, &[(3, d("2026-02-01"))]));
        assert!(!eval(&r, &[]));
        let open = c(
            NUM,
            Test::Range {
                min: None,
                max: Some(Value::Num(5.0)),
            },
        );
        assert!(eval(&open, &[(1, Value::Num(-100.0))]));
        assert!(!eval(&open, &[(1, Value::Num(5.5))]));
    }

    #[test]
    fn in_and_null_tests() {
        let (hi, lo) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let f = c(SEV, Test::In(vec![Value::Ref(hi)]));
        assert!(eval(&f, &[(4, Value::Ref(hi))]));
        assert!(!eval(&f, &[(4, Value::Ref(lo))]));
        assert!(!eval(&f, &[]));
        assert!(eval(&c(NOTE, Test::IsNull), &[]));
        assert!(!eval(
            &c(NOTE, Test::IsNull),
            &[(5, Value::Text("x".into()))]
        ));
        assert!(eval(
            &c(NOTE, Test::NotNull),
            &[(5, Value::Text("x".into()))]
        ));
    }

    #[test]
    fn and_or_compose() {
        let f = Filter::And(vec![
            c(FLAG, Test::Cmp(Cmp::Eq, Value::Bool(true))),
            Filter::Or(vec![
                c(INT, Test::Cmp(Cmp::Ge, Value::Num(10.0))),
                c(INT, Test::IsNull),
            ]),
        ]);
        assert!(eval(&f, &[(6, Value::Bool(true))]));
        assert!(eval(&f, &[(6, Value::Bool(true)), (2, Value::Num(10.0))]));
        assert!(!eval(&f, &[(6, Value::Bool(true)), (2, Value::Num(9.0))]));
        assert!(!eval(&f, &[(6, Value::Bool(false))]));
        assert!(eval(&Filter::And(vec![]), &[]));
        assert!(!eval(&Filter::Or(vec![]), &[]));
        assert_eq!(f.fields(), vec![FLAG, INT]);
        f.validate().unwrap();
    }

    #[test]
    fn rejects_mismatched_or_unsupported_tests() {
        assert!(matches!(
            Filter::cond(NUM, Test::Cmp(Cmp::Eq, Value::Text("2".into()))),
            Err(FilterError::WrongKind { .. })
        ));
        assert!(matches!(
            Filter::cond(SEV, Test::Cmp(Cmp::Lt, Value::Ref(Uuid::nil()))),
            Err(FilterError::Unsupported { .. })
        ));
        assert!(matches!(
            Filter::cond(NOTE, Test::Cmp(Cmp::Eq, Value::Text("x".into()))),
            Err(FilterError::Unsupported { .. })
        ));
        assert!(matches!(
            Filter::cond(INT, Test::Cmp(Cmp::Eq, Value::Num(1.5))),
            Err(FilterError::BadValue { .. })
        ));
        assert!(matches!(
            Filter::cond(NUM, Test::Cmp(Cmp::Eq, Value::Num(f64::NAN))),
            Err(FilterError::BadValue { .. })
        ));
        assert!(Filter::cond(NUM, Test::In(vec![])).is_err());
        assert!(
            Filter::cond(
                NUM,
                Test::Range {
                    min: None,
                    max: None
                }
            )
            .is_err()
        );
        // validate() also checks conditions built without Filter::cond.
        let raw = Filter::And(vec![Filter::Cond {
            applies: Pred::True,
            field: SEV,
            test: Test::Range {
                min: Some(Value::Ref(Uuid::nil())),
                max: None,
            },
        }]);
        assert!(raw.validate().is_err());
    }

    #[test]
    fn values_outside_applies_do_not_count() {
        let elsewhere = Pred::ProjectIs(Uuid::from_u128(999)); // not facts().project_id
        let stored = [(1, Value::Num(5.0))];
        let cond = |test| Filter::cond_where(NUM, test, elsewhere.clone()).unwrap();
        assert!(!eval(&cond(Test::Cmp(Cmp::Eq, Value::Num(5.0))), &stored));
        assert!(!eval(&cond(Test::NotNull), &stored));
        assert!(
            !eval(&cond(Test::IsNull), &stored),
            "is_null needs E (Redmine !*)"
        );
        assert!(!eval(&cond(Test::IsNull), &[]));
        assert!(
            eval(&cond(Test::Cmp(Cmp::Ne, Value::Num(5.0))), &stored),
            "ne is the complement of eq (Redmine !)"
        );
        // The result is the same whatever the hidden value is.
        for hidden in [Value::Num(5.0), Value::Num(6.0)] {
            let f = Filter::Or(vec![
                cond(Test::Cmp(Cmp::Ne, Value::Num(5.0))),
                cond(Test::IsNull),
            ]);
            assert!(eval(&f, &[(1, hidden)]));
        }
        let here = Pred::ProjectIs(facts().project_id);
        let f = Filter::cond_where(NUM, Test::IsNull, here).unwrap();
        assert!(eval(&f, &[]));
    }

    #[test]
    fn size_limits() {
        let one = || c(FLAG, Test::NotNull);
        assert!(
            Filter::And((0..MAX_CONDITIONS).map(|_| one()).collect())
                .validate()
                .is_ok()
        );
        assert_eq!(
            Filter::And((0..=MAX_CONDITIONS).map(|_| one()).collect()).validate(),
            Err(FilterError::TooLarge)
        );
        let mut deep = one();
        for _ in 0..MAX_DEPTH {
            deep = Filter::And(vec![deep]);
        }
        assert!(deep.validate().is_ok());
        assert_eq!(
            Filter::Or(vec![deep]).validate(),
            Err(FilterError::TooLarge)
        );
    }

    #[test]
    fn field_types_round_trip() {
        for t in FieldType::ALL {
            assert_eq!(FieldType::parse(t.as_str()), Some(t));
        }
        assert_eq!(FieldType::parse("version"), None);
    }
}
