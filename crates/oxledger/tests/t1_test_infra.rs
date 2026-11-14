// SPDX-License-Identifier: GPL-2.0-or-later
//! Checks on the test infrastructure itself (T1, docs/impl/testing-playbook.md).
//!
//! - G4: every test that drops a database is in nextest's `drop-database`
//!   group, and every test the overrides name with `test(=…)` exists, so a
//!   rename cannot quietly take a test out of its group (no database).
//! - G2: the horizon timeout message of `sync_all` / `dispatch_all` names
//!   the transactions holding the horizon back (needs PostgreSQL 18).
//! - G11 (T1b): bounded waits (fault gates, `common::poll_until`) panic
//!   with what they waited for and the state instead of hanging.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

mod common;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn nextest_toml() -> String {
    let p = manifest_dir().join("../../.config/nextest.toml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// One `[[profile.default.overrides]]` entry: its filter and test group.
struct Override {
    filter: String,
    group: Option<String>,
}

fn overrides(toml: &str) -> Vec<Override> {
    toml.split("[[profile.default.overrides]]")
        .skip(1)
        .map(|block| {
            let filter = if let Some(i) = block.find("filter = '''") {
                let rest = &block[i + "filter = '''".len()..];
                rest[..rest.find("'''").expect("closing '''")].to_string()
            } else {
                let i = block.find("filter = '").expect("override without a filter");
                let rest = &block[i + "filter = '".len()..];
                rest[..rest.find('\'').expect("closing '")].to_string()
            };
            let group = block.find("test-group = '").map(|i| {
                let rest = &block[i + "test-group = '".len()..];
                rest[..rest.find('\'').unwrap()].to_string()
            });
            Override { filter, group }
        })
        .collect()
}

/// What a filter names: binaries by regex suffix (`binary(/x$/)`), whole
/// binaries (`binary(=b)` with no test after it) and (binary, test) pairs
/// (`binary(=b) & … test(=t)`: each `test(=…)` belongs to the nearest
/// `binary(=…)` before it).
#[derive(Default)]
struct Named {
    binary_suffixes: Vec<String>,
    binaries: BTreeSet<String>,
    tests: BTreeSet<(String, String)>,
}

fn parse_filter(filter: &str) -> Named {
    let mut named = Named::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut with_tests: BTreeSet<String> = BTreeSet::new();
    let mut current: Option<String> = None;
    let mut rest = filter;
    loop {
        let b = rest.find("binary(");
        let t = rest.find("test(=");
        match (b, t) {
            (None, None) => break,
            (Some(bi), t) if t.is_none_or(|ti| bi < ti) => {
                let arg = &rest[bi + "binary(".len()..];
                let arg = &arg[..arg.find(')').unwrap()];
                if let Some(name) = arg.strip_prefix('=') {
                    seen.insert(name.to_string());
                    current = Some(name.to_string());
                } else if let Some(suffix) =
                    arg.strip_prefix('/').and_then(|a| a.strip_suffix("$/"))
                {
                    assert!(
                        suffix
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                        "binary regex {arg} is not a plain suffix; extend this checker"
                    );
                    named.binary_suffixes.push(suffix.to_string());
                } else {
                    panic!("unsupported binary filter {arg}; extend this checker");
                }
                rest = &rest[bi + "binary(".len()..];
            }
            (_, Some(ti)) => {
                let arg = &rest[ti + "test(=".len()..];
                let name = &arg[..arg.find(')').unwrap()];
                let bin = current
                    .clone()
                    .unwrap_or_else(|| panic!("test(={name}) without a binary(=…) before it"));
                with_tests.insert(bin.clone());
                named.tests.insert((bin, name.to_string()));
                rest = &rest[ti + "test(=".len()..];
            }
            _ => unreachable!(),
        }
    }
    // A binary with no test(=…) after it is named whole.
    named.binaries = seen.difference(&with_tests).cloned().collect();
    named
}

/// A top-level item of a test file: a chunk of lines ending with a `}` line.
struct Item {
    name: Option<String>,
    is_test: bool,
    code: String,
}

fn items(src: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let mut chunk = Vec::new();
    for line in src.lines() {
        // Comments do not drop databases (common/mod.rs talks about it).
        if !line.trim_start().starts_with("//") {
            chunk.push(line);
        }
        if line == "}" {
            out.push(item(&chunk));
            chunk.clear();
        }
    }
    out.push(item(&chunk));
    out
}

fn item(lines: &[&str]) -> Item {
    let code = lines.join("\n");
    let is_test = lines
        .iter()
        .any(|l| l.starts_with("#[tokio::test") || l.starts_with("#[test]"));
    let name = lines.iter().find_map(|l| {
        let l = l.strip_prefix("pub ").unwrap_or(l);
        let l = l.strip_prefix("async ").unwrap_or(l);
        let l = l.strip_prefix("fn ")?;
        Some(
            l.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect(),
        )
    });
    Item {
        name,
        is_test,
        code,
    }
}

// Split so that this file does not look like a dropper itself.
const DROP_DATABASE: &str = concat!("DROP", " DATABASE");
const WITH_FORCE: &str = concat!("WITH", " (FORCE)");

fn drops(code: &str) -> bool {
    code.contains(DROP_DATABASE) || code.contains(WITH_FORCE)
}

/// The tests of one file that drop a database, directly or through helper
/// functions of the same file.
fn dropping_tests(src: &str) -> BTreeSet<String> {
    let items = items(src);
    let mut droppers: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = droppers.len();
        for it in &items {
            let Some(name) = &it.name else { continue };
            if droppers.contains(name) {
                continue;
            }
            let calls_dropper = droppers.iter().any(|d| it.code.contains(&format!("{d}(")));
            if drops(&it.code) || calls_dropper {
                droppers.insert(name.clone());
            }
        }
        if droppers.len() == before {
            break;
        }
    }
    items
        .iter()
        .filter(|it| it.is_test)
        .filter_map(|it| it.name.clone())
        .filter(|n| droppers.contains(n))
        .collect()
}

fn test_files(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "rs").then(|| {
                let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
                (stem, std::fs::read_to_string(&p).unwrap())
            })
        })
        .collect()
}

/// The (binary, test) pairs that DROP DATABASE but are not in the
/// `drop-database` group, and the names the overrides use that do not exist.
fn check(toml: &str, files: &BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    let ovs = overrides(toml);
    let drop_group: Vec<Named> = ovs
        .iter()
        .filter(|o| o.group.as_deref() == Some("drop-database"))
        .map(|o| parse_filter(&o.filter))
        .collect();
    assert!(
        !drop_group.is_empty(),
        "no drop-database override in nextest.toml"
    );
    for (bin, src) in files {
        for test in dropping_tests(src) {
            let covered = drop_group.iter().any(|n| {
                n.binary_suffixes.iter().any(|s| bin.ends_with(s.as_str()))
                    || n.binaries.contains(bin)
                    || n.tests.contains(&(bin.clone(), test.clone()))
            });
            if !covered {
                problems.push(format!(
                    "{bin}::{test} drops a database but is not in the drop-database group"
                ));
            }
        }
    }
    for o in &ovs {
        let n = parse_filter(&o.filter);
        for b in &n.binaries {
            if !files.contains_key(b) {
                problems.push(format!(
                    "nextest.toml names binary(={b}), which does not exist"
                ));
            }
        }
        for (b, t) in &n.tests {
            let exists = files.get(b).is_some_and(|src| {
                items(src)
                    .iter()
                    .any(|it| it.is_test && it.name.as_deref() == Some(t.as_str()))
            });
            if !exists {
                problems.push(format!("nextest.toml names {b}::{t}, which is not a test"));
            }
        }
    }
    problems
}

/// G4.
#[test]
fn nextest_groups_cover_the_tests_they_must() {
    let files = test_files(&manifest_dir().join("tests"));
    let problems = check(&nextest_toml(), &files);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    // The checker sees the known droppers (a broken parser finds none).
    let s3 = dropping_tests(&files["s3_permissions"]);
    assert_eq!(
        s3,
        BTreeSet::from(["migration_0007_repairs_dates".to_string()])
    );
    let s5 = dropping_tests(&files["s5_changesets"]);
    assert_eq!(s5, BTreeSet::from(["s5_migrations_from_empty".to_string()]));
    assert!(!dropping_tests(&files["m1_0_upgrade"]).is_empty());
}

/// G4: the checker itself catches a new dropping test and a renamed one.
#[test]
fn nextest_group_checker_catches_omissions() {
    let toml = nextest_toml();
    let mut files = test_files(&manifest_dir().join("tests"));
    let s3 = files.get_mut("s3_permissions").unwrap();
    s3.push_str(&format!(
        "\n#[tokio::test]\nasync fn new_dropper() {{\n    let q = \"{DROP_DATABASE} x\";\n}}\n"
    ));
    let p = check(&toml, &files);
    assert!(
        p.iter().any(|m| m.contains("s3_permissions::new_dropper")),
        "{p:?}"
    );

    let mut files = test_files(&manifest_dir().join("tests"));
    let s5 = files.get_mut("s5_changesets").unwrap();
    *s5 = s5.replace("fn s5_migrations_from_empty(", "fn s5_migrations_renamed(");
    let p = check(&toml, &files);
    assert!(
        p.iter()
            .any(|m| m.contains("s5_changesets::s5_migrations_renamed")),
        "{p:?}"
    );
    assert!(
        p.iter()
            .any(|m| m.contains("s5_changesets::s5_migrations_from_empty")),
        "{p:?}"
    );
}

/// G2: a transaction holding an xid shows up in the timeout message with its
/// pid and database, its query cut to 120 characters.
#[tokio::test]
async fn horizon_timeout_message_names_the_holder() {
    let pool = common::test_pool().await;
    let db = common::test_db();
    let mut holder = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    // A write takes an xid; the long statement text is what the session
    // last ran when the message is built.
    let long_tail = "x".repeat(300);
    let sql = format!(
        "SELECT txid_current(), '{long_tail}'::text AS padding_to_make_the_statement_longer_than_120"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.clone()))
        .execute(&mut *holder)
        .await
        .unwrap();

    let msg = oxledger::metrics::horizon_timeout_message(&pool, "search index").await;
    holder.rollback().await.unwrap();

    assert!(
        msg.starts_with("search index did not catch up with the outbox within 20 s; xid holders"),
        "{msg}"
    );
    let line = msg
        .lines()
        .find(|l| l.contains(&format!(" pid={pid} ")))
        .unwrap_or_else(|| panic!("no line for pid {pid}: {msg}"));
    assert!(line.contains(&format!("datname={} ", db.name)), "{line}");
    let query = &line[line.find("query=").unwrap() + "query=".len()..];
    let query: String = serde_json::from_str(query).unwrap_or_else(|_| query.to_string());
    assert_eq!(query.chars().count(), 120, "{query}");
    assert!(sql.starts_with(&query), "{query}");
}

/// The panic message of a task that panicked.
async fn panic_of(h: tokio::task::JoinHandle<()>) -> String {
    let e = common::wait_for("the waiting task to panic", common::WAIT_TIMEOUT, h)
        .await
        .expect_err("the wait panics");
    let p = e.into_panic();
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// G11 (T1b): a fault gate nobody reaches, and a poll whose condition never
/// holds, panic after their timeout instead of hanging, naming what they
/// waited for and the state: the outbox backlog and the xid holders.
#[tokio::test]
async fn bounded_waits_panic_with_the_state() {
    use oxledger::changesets::faults::{self, Action, Gate};
    use std::sync::Arc;
    use std::time::Duration;

    let _pool = common::test_pool().await;
    let g = Arc::new(Gate::with_timeout(Duration::from_millis(200)));
    faults::arm("t1b.never_hit", Action::Block(g.clone()));
    let started = std::time::Instant::now();
    let msg = panic_of(tokio::spawn(async move { g.reached.notified().await })).await;
    faults::clear();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the gate gave up late: {:?}",
        started.elapsed()
    );
    assert!(
        msg.starts_with("timed out after 200ms waiting for fault gate t1b.never_hit to be reached"),
        "{msg}"
    );
    assert!(msg.contains("\noutbox: H="), "{msg}");
    assert!(msg.contains("xid holders"), "{msg}");

    let msg = panic_of(tokio::spawn(async {
        common::poll_until(
            "a condition that never holds",
            Duration::from_millis(100),
            async || common::Continue("n=0".to_string()),
        )
        .await
    }))
    .await;
    assert!(
        msg.starts_with(
            "timed out after 100ms waiting for a condition that never holds (last seen: n=0)"
        ),
        "{msg}"
    );
    assert!(msg.contains("xid holders"), "{msg}");
    // `#[track_caller]`: the message names this file, not test_support.rs.
    assert!(
        msg.contains("(wait at ") && msg.contains("t1_test_infra.rs:"),
        "{msg}"
    );
}

/// G11 (T1b): a gate waits `WAIT_TIMEOUT` (60 s) unless made with
/// `Gate::with_timeout`, and `arm` names the point it waits for.
#[test]
fn gates_wait_sixty_seconds_by_default() {
    use oxledger::changesets::faults::{self, Action, Gate};
    use std::sync::Arc;
    use std::time::Duration;

    assert_eq!(common::WAIT_TIMEOUT, Duration::from_secs(60));
    let g = Gate::default();
    assert_eq!(g.reached.timeout(), common::WAIT_TIMEOUT);
    assert_eq!(g.reached.point(), None);
    let g = Arc::new(Gate::with_timeout(Duration::from_millis(250)));
    assert_eq!(g.reached.timeout(), Duration::from_millis(250));
    faults::arm("t1b.named", Action::Block(g.clone()));
    faults::clear();
    assert_eq!(g.reached.point().as_deref(), Some("t1b.named"));
}

/// G11 (T1b): a check that panics (a failed query, an assertion inside the
/// poll) is reported like a timeout: what was waited for, where, and the
/// state.
#[tokio::test]
async fn a_failing_check_reports_the_state() {
    let _pool = common::test_pool().await;
    let msg = panic_of(tokio::spawn(async {
        common::poll_until(
            "a check that panics",
            common::WAIT_TIMEOUT,
            async || -> std::ops::ControlFlow<(), String> { panic!("boom in the check") },
        )
        .await
    }))
    .await;
    assert!(
        msg.starts_with("the check failed while waiting for a check that panics (wait at "),
        "{msg}"
    );
    assert!(msg.contains("t1_test_infra.rs:"), "{msg}");
    assert!(msg.contains("boom in the check"), "{msg}");
    assert!(msg.contains("\noutbox: H="), "{msg}");
    assert!(msg.contains("xid holders"), "{msg}");
}

/// G11 (T1b): `wait_horizon_past` does not return while an older xid holds
/// H at or below x (it panics at its bound, with the last H it saw), and
/// returns once the holder ends.
#[tokio::test]
async fn wait_horizon_past_waits_for_older_holders() {
    use std::time::Duration;

    let pool = common::test_pool().await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_current_xact_id()")
        .execute(&mut *holder)
        .await
        .unwrap();
    // A newer xid, committed: H stays at or below the holder's xid, so
    // below x.
    let x: String = sqlx::query_scalar("SELECT pg_current_xact_id()::text")
        .fetch_one(&pool)
        .await
        .unwrap();
    let x: u64 = x.parse().unwrap();

    // A local task: the helper's future is not `Send` for every lifetime
    // (an `async` closure over a borrowed pool), which `tokio::spawn` needs.
    let p = pool.clone();
    let msg = tokio::task::LocalSet::new()
        .run_until(async move {
            panic_of(tokio::task::spawn_local(async move {
                common::wait_horizon_past_within(&p, x, Duration::from_millis(300)).await
            }))
            .await
        })
        .await;
    assert!(
        msg.starts_with(&format!(
            "timed out after 300ms waiting for the snapshot horizon to pass xid {x} (last seen: H="
        )),
        "{msg}"
    );
    assert!(msg.contains("xid holders"), "{msg}");

    holder.rollback().await.unwrap();
    common::wait_horizon_past(&pool, x).await;
}

/// G11 (T1b): the report bounds each part on its own. With `outbox` locked,
/// its part fails on the report connection's `lock_timeout` (2 s) and the
/// xid holders still print, well inside the report's own bounds.
#[tokio::test]
async fn state_report_survives_a_locked_outbox() {
    use std::time::{Duration, Instant};

    let pool = common::test_pool().await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE outbox IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .unwrap();
    sqlx::query("SELECT pg_current_xact_id()")
        .execute(&mut *holder)
        .await
        .unwrap();
    let started = Instant::now();
    let msg = common::state_report_for(&pool).await;
    let took = started.elapsed();
    holder.rollback().await.unwrap();

    assert!(msg.starts_with("outbox: lookup failed: "), "{msg}");
    assert!(msg.contains("lock timeout"), "{msg}");
    // The holder above has an xid, so the list is not empty.
    assert!(msg.contains("\nxid holders (oldest first):\n"), "{msg}");
    assert!(
        took < Duration::from_secs(10),
        "the report took {took:?}: {msg}"
    );
}

/// G11 (T1b): a refused connection keeps its error; a server that accepts
/// and never answers is named as such after `REPORT_CONNECT_TIMEOUT` (3 s),
/// not after the report's 15 s backstop.
#[tokio::test]
async fn state_report_says_why_it_cannot_connect() {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::time::{Duration, Instant};

    let opts = |port| {
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username("nobody")
            .database("nothing")
    };

    // Refused: a port that was just free.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let pool = PgPoolOptions::new().connect_lazy_with(opts(port));
    let msg = common::state_report_for(&pool).await;
    assert!(msg.starts_with("state: cannot connect: "), "{msg}");
    assert!(!msg.contains("no answer within"), "{msg}");

    // Silent: accepts and keeps the socket open without a word.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    let pool = PgPoolOptions::new().connect_lazy_with(opts(port));
    let started = Instant::now();
    let msg = common::state_report_for(&pool).await;
    let took = started.elapsed();
    silent.abort();
    assert_eq!(msg, "state: cannot connect: no answer within 3s");
    assert!(took < Duration::from_secs(10), "the report took {took:?}");
}
