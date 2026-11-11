// SPDX-License-Identifier: GPL-2.0-or-later
//! Rate and cost limits of the built-in AI (design 05 §3.13.4, §3.13.6,
//! appendix D V15): the hourly run count, the atomic reservation against
//! the user's day and the organisation's month, settlement, and the 80 % /
//! 100 % notifications to administrators.
//!
//! Amounts are SQL `numeric` throughout (no decimal crate); Rust only
//! carries them as text.

use super::admin::Provider;
use super::chat::AiLimits;
use crate::error::{ApiError, ErrorCode};
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

/// The organisation's today and this month's first day.
const ORG_DAY: &str = "(SELECT (now() AT TIME ZONE timezone)::date FROM org_settings)";

fn exceeded(scope: &str, retry_at: DateTime<Utc>, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::QuotaExceeded, msg.to_string())
        .with_details(json!({"scope": scope, "retry_at": retry_at}))
        .into()
}

/// `GET /ai/status.limits`.
pub(crate) async fn limits_of(conn: &mut PgConnection, user: Uuid) -> anyhow::Result<AiLimits> {
    let (left, oldest, budget_left): (i64, Option<DateTime<Utc>>, String) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT greatest(0, s.user_runs_per_hour - (SELECT count(*) FROM ai_runs r
                                                    WHERE r.user_id = $1 AND r.started_at > now() - interval '1 hour')),
                (SELECT min(r.started_at) + interval '1 hour' FROM ai_runs r
                  WHERE r.user_id = $1 AND r.started_at > now() - interval '1 hour'),
                greatest(0, s.user_daily_budget_usd
                            - coalesce((SELECT d.spent_usd + d.reserved_usd FROM ai_usage_daily d
                                         WHERE d.user_id = $1 AND d.day = {ORG_DAY}), 0))::numeric(10,6)::text
         FROM ai_settings s"
    )))
    .bind(user)
    .fetch_one(&mut *conn)
    .await?;
    Ok(AiLimits {
        runs_left_this_hour: left,
        budget_left_today_usd: budget_left,
        hour_resets_at: oldest,
    })
}

/// Checks the hourly rate, then reserves `ceil_6(50 000 × in + max_out ×
/// out) / 1e6` USD against the month and the user's day, both rows locked
/// (month first, then day). Returns the reserved amount as text.
pub(crate) async fn reserve(
    conn: &mut PgConnection,
    user: Uuid,
    p: &Provider,
) -> anyhow::Result<String> {
    // 5. The hourly rate (the per-user lock is the one-active unique index
    // plus this user's daily row below; two sends of one user serialize on it).
    let (per_hour, used, oldest): (i32, i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT s.user_runs_per_hour, count(r.id), min(r.started_at)
         FROM ai_settings s LEFT JOIN ai_runs r ON r.user_id = $1 AND r.started_at > now() - interval '1 hour'
         GROUP BY s.user_runs_per_hour",
    )
    .bind(user)
    .fetch_one(&mut *conn)
    .await?;
    if used >= per_hour as i64 {
        let at = oldest.unwrap_or_else(Utc::now) + chrono::Duration::hours(1);
        return Err(exceeded(
            "ai_user_hour",
            at,
            "too many AI replies this hour",
        ));
    }
    // 6. Month, then day: insert-if-missing, then lock.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ai_budget_months (month) SELECT date_trunc('month', {ORG_DAY})::date ON CONFLICT DO NOTHING"
    )))
    .execute(&mut *conn)
    .await?;
    let reserved: String = sqlx::query_scalar(
        "SELECT (ceil(50000 * input_usd_per_mtok + max_output_tokens * output_usd_per_mtok) / 1000000)::numeric(10,6)::text
         FROM ai_providers WHERE id = $1",
    )
    .bind(p.id)
    .fetch_one(&mut *conn)
    .await?;
    let (month_ok, month_retry): (bool, DateTime<Utc>) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT s.monthly_budget_usd IS NOT NULL
                AND m.spent_usd + m.reserved_usd + $1::numeric <= s.monthly_budget_usd,
                ((m.month + interval '1 month')::timestamp AT TIME ZONE o.timezone)
         FROM ai_budget_months m, ai_settings s, org_settings o
         WHERE m.month = date_trunc('month', {ORG_DAY})::date
         FOR UPDATE OF m"
        )))
        .bind(&reserved)
        .fetch_one(&mut *conn)
        .await?;
    if !month_ok {
        return Err(exceeded(
            "org_month",
            month_retry,
            "the organisation's AI budget for this month is used up",
        ));
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ai_usage_daily (user_id, day) VALUES ($1, {ORG_DAY}) ON CONFLICT DO NOTHING"
    )))
    .bind(user)
    .execute(&mut *conn)
    .await?;
    let (day_ok, day_retry): (bool, DateTime<Utc>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT d.spent_usd + d.reserved_usd + $2::numeric <= s.user_daily_budget_usd,
                ((d.day + 1)::timestamp AT TIME ZONE o.timezone)
         FROM ai_usage_daily d, ai_settings s, org_settings o
         WHERE d.user_id = $1 AND d.day = {ORG_DAY}
         FOR UPDATE OF d"
    )))
    .bind(user)
    .bind(&reserved)
    .fetch_one(&mut *conn)
    .await?;
    if !day_ok {
        return Err(exceeded(
            "ai_user_day",
            day_retry,
            "your AI budget for today is used up",
        ));
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE ai_budget_months SET reserved_usd = reserved_usd + $1::numeric
         WHERE month = date_trunc('month', {ORG_DAY})::date"
    )))
    .bind(&reserved)
    .execute(&mut *conn)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE ai_usage_daily SET reserved_usd = reserved_usd + $2::numeric, runs = runs + 1
         WHERE user_id = $1 AND day = {ORG_DAY}"
    )))
    .bind(user)
    .bind(&reserved)
    .execute(&mut *conn)
    .await?;
    Ok(reserved)
}

/// The run's day and month in the organisation's time zone (its
/// reservation went there).
const RUN_DAY: &str = "(SELECT (r.started_at AT TIME ZONE o.timezone)::date FROM org_settings o)";

/// Has the run used up what it reserved (or the month its budget)? The
/// run then ends with `budget_exhausted` (§3.13.4).
pub(crate) async fn run_cost_so_far(
    conn: &mut PgConnection,
    run: Uuid,
    input_tokens: u64,
    output_tokens: u64,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT ceil($2::numeric * p.input_usd_per_mtok + $3::numeric * p.output_usd_per_mtok) / 1000000 >= r.reserved_usd
         FROM ai_runs r JOIN ai_providers p ON p.id = r.provider_id WHERE r.id = $1",
    )
    .bind(run)
    .bind(input_tokens as i64)
    .bind(output_tokens as i64)
    .fetch_one(&mut *conn)
    .await?)
}

/// The month's budget is spent (100 %): a run in progress stops.
pub(crate) async fn month_exhausted(conn: &mut PgConnection) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT coalesce((SELECT m.spent_usd >= s.monthly_budget_usd FROM ai_budget_months m, ai_settings s
                          WHERE m.month = date_trunc('month', {ORG_DAY})::date), false)"
    )))
    .fetch_one(&mut *conn)
    .await?)
}

/// Settles a finished run: `cost = ceil_6(usage × prices)`, the
/// reservation released, the spending added to the user's day and the
/// month; then the threshold notifications (100 % included when the cost
/// went past the reservation). Reported usage is recorded as it is, even
/// above the reservation; only an estimate (no usage reported, V15) is
/// capped at the reservation. Returns the cost as text.
pub(crate) async fn settle(
    conn: &mut PgConnection,
    run: Uuid,
    input_tokens: u64,
    output_tokens: u64,
    estimated: bool,
) -> anyhow::Result<String> {
    let row: Option<(Uuid, String, String, chrono::NaiveDate)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE ai_runs r SET
            input_tokens = least($2, 2147483647)::integer, output_tokens = least($3, 2147483647)::integer,
            usage_estimated = $4,
            cost_usd = CASE WHEN $4 THEN least(r.reserved_usd, x.cost) ELSE x.cost END
         FROM ai_providers p,
              LATERAL (SELECT ceil($2::numeric * p.input_usd_per_mtok + $3::numeric * p.output_usd_per_mtok)
                              / 1000000 AS cost) x
         WHERE r.id = $1 AND p.id = r.provider_id
         RETURNING r.user_id, r.cost_usd::text, r.reserved_usd::text, {RUN_DAY}"
    )))
    .bind(run)
    .bind(input_tokens as i64)
    .bind(output_tokens as i64)
    .bind(estimated)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((user, cost, reserved, day)) = row else {
        return Ok("0".into());
    };
    sqlx::query(
        "UPDATE ai_budget_months SET reserved_usd = greatest(0, reserved_usd - $2::numeric),
                spent_usd = spent_usd + $3::numeric
         WHERE month = date_trunc('month', $1::date)::date",
    )
    .bind(day)
    .bind(&reserved)
    .bind(&cost)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE ai_usage_daily SET reserved_usd = greatest(0, reserved_usd - $3::numeric),
                spent_usd = spent_usd + $4::numeric,
                input_tokens = input_tokens + $5, output_tokens = output_tokens + $6
         WHERE user_id = $1 AND day = $2",
    )
    .bind(user)
    .bind(day)
    .bind(&reserved)
    .bind(&cost)
    .bind(input_tokens as i64)
    .bind(output_tokens as i64)
    .execute(&mut *conn)
    .await?;
    notify(&mut *conn, day).await?;
    Ok(cost)
}

/// 80 % and 100 % of the month's budget: one notification per threshold to
/// every active administrator (`notified_*` makes it once per month).
pub(crate) async fn notify(conn: &mut PgConnection, day: chrono::NaiveDate) -> anyhow::Result<()> {
    let row: Option<(chrono::NaiveDate, String, bool, bool)> = sqlx::query_as(
        "SELECT m.month, to_char(m.month, 'YYYY-MM'),
                NOT m.notified_80 AND m.spent_usd >= 0.8 * s.monthly_budget_usd,
                NOT m.notified_100 AND m.spent_usd >= s.monthly_budget_usd
         FROM ai_budget_months m, ai_settings s
         WHERE m.month = date_trunc('month', $1::date)::date AND s.monthly_budget_usd IS NOT NULL
         FOR UPDATE OF m",
    )
    .bind(day)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((month, label, hit80, hit100)) = row else {
        return Ok(());
    };
    for (hit, threshold, col) in [(hit80, 80, "notified_80"), (hit100, 100, "notified_100")] {
        if !hit {
            continue;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE ai_budget_months SET {col} = true WHERE month = $1"
        )))
        .bind(month)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO notifications (recipient_id, kind, reasons, payload, first_at, last_at)
             SELECT id, 'ai_budget', ARRAY['ai_budget'], jsonb_build_object('month', $1::text, 'threshold', $2::int),
                    now(), now()
             FROM principals WHERE kind = 'user' AND status = 'active' AND is_admin",
        )
        .bind(&label)
        .bind(threshold)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
