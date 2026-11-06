// SPDX-License-Identifier: GPL-2.0-or-later
//! Domain core for oxledger: permission/visibility rules, custom field
//! filters and workflow.
//!
//! Rules are implemented once here and compiled by callers into SQL and search
//! filters (research doc §5.1). Nothing in this crate touches I/O.

pub mod filter;
pub mod visibility;
pub mod workflow;

pub use visibility::{IssueFacts, Pred, ProjectGrant, RoleGrant, Scope, Visibility};
