//! CSV import (design 06 §3.10, R-I1–R-I15).
//!
//! A person uploads a file into one project, maps its columns, has it
//! checked (a dry run that writes nothing) and then runs it. Every row is
//! an ordinary command document executed as that person, in its own
//! transaction: the row's permissions, workflow and form rules are the
//! command engine's, not this module's. Only results are kept per row (a
//! code and a field key), never a cell of the file.

use crate::auth::AuthContext;
use crate::commands;
use crate::describe;
use crate::error::{ApiError, ErrorCode, api, classify};
use crate::perm::{self, IMPORT_ISSUES, IMPORT_TIME_ENTRIES};
use crate::service::App;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

/// R-I2: the uploaded file.
pub const MAX_FILE: usize = 10 * 1024 * 1024;
/// R-I2: data rows (the header is not one).
pub const MAX_ROWS: usize = 5000;
/// R-I2: columns.
pub const MAX_COLUMNS: usize = 64;
/// R-I2: one cell.
pub const MAX_CELL: usize = 64 * 1024;
/// §6.12: rows of the preview.
const PREVIEW_ROWS: usize = 10;
/// §6.12: different values listed per field in the value mapping.
const MAX_VALUES: usize = 200;
/// R-I12, R-I13: how long an import's results and its undo stay.
const KEEP_DAYS: i32 = 30;

const ACTION_RUN: &str = "import_run";

/// Tests: the worker stops (as if the process died) after this many rows
/// of one call; 0 = never.
#[cfg(feature = "test-support")]
pub static STOP_AFTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// ---------------------------------------------------------------- errors

fn invalid_file(kind: &str, extra: Json) -> anyhow::Error {
    let mut d = json!({"kind": kind});
    if let (Some(d), Some(x)) = (d.as_object_mut(), extra.as_object()) {
        d.extend(x.clone());
    }
    ApiError::new(ErrorCode::ImportInvalidFile, "the file cannot be imported")
        .with_details(d)
        .into()
}

fn state_invalid(state: &str) -> anyhow::Error {
    ApiError::new(
        ErrorCode::ImportStateInvalid,
        "the import is not in a state for this",
    )
    .with_details(json!({"state": state}))
    .into()
}

fn mapping_invalid(field: &str, reason: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::ImportMappingInvalid, "the mapping is not valid")
        .with_details(json!({"field": field, "reason": reason}))
        .into()
}

fn not_found() -> anyhow::Error {
    api(ErrorCode::NotFound, "no such import")
}

fn bad(path: &str, msg: &str) -> anyhow::Error {
    ApiError::new(ErrorCode::InvalidArgument, msg)
        .with_details(json!({"path": path}))
        .into()
}

// ---------------------------------------------------------------- the file (R-I2–R-I4)

/// The file as text rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub header: Vec<String>,
    /// `(row number, cells)`: rows are numbered from 1 after the header;
    /// a row with nothing in it is skipped but still counted.
    pub rows: Vec<(i32, Vec<String>)>,
}

/// What the encoding detection found (R-I3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoding {
    pub name: &'static str,
    pub guessed: bool,
}

const ENCODINGS: &[&str] = &["utf-8", "utf-16le", "utf-16be", "big5"];

fn encoding_error(row: usize) -> anyhow::Error {
    invalid_file("encoding", json!({"row": row}))
}

/// The line (1 = the header) the byte at `at` is on.
fn line_of(bytes: &[u8], at: usize) -> usize {
    bytes[..at.min(bytes.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn decode_as(bytes: &[u8], name: &'static str) -> Result<String> {
    let text = match name {
        "utf-8" => {
            let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
            match std::str::from_utf8(b) {
                Ok(s) => Some(s.to_string()),
                Err(e) => return Err(encoding_error(line_of(b, e.valid_up_to()))),
            }
        }
        "utf-16le" => encoding_rs::UTF_16LE
            .decode_without_bom_handling_and_without_replacement(
                bytes.strip_prefix(b"\xFF\xFE").unwrap_or(bytes),
            )
            .map(|s| s.into_owned()),
        "utf-16be" => encoding_rs::UTF_16BE
            .decode_without_bom_handling_and_without_replacement(
                bytes.strip_prefix(b"\xFE\xFF").unwrap_or(bytes),
            )
            .map(|s| s.into_owned()),
        _ => encoding_rs::BIG5
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(|s| s.into_owned()),
    };
    text.ok_or_else(|| encoding_error(1))
}

/// R-I3: by BOM; else UTF-8 when the whole file is valid UTF-8; else Big5
/// when it decodes without a replacement character (a guess the person is
/// asked to confirm); else the file is refused. `forced`: the person chose.
pub fn decode(bytes: &[u8], forced: Option<&str>) -> Result<(String, Encoding)> {
    let (text, enc) = decode_any(bytes, forced)?;
    // UTF-16 without a BOM reads as "valid" UTF-8 full of NULs: not text.
    if let Some(at) = text.find('\0') {
        return Err(encoding_error(line_of(text.as_bytes(), at)));
    }
    Ok((text, enc))
}

fn decode_any(bytes: &[u8], forced: Option<&str>) -> Result<(String, Encoding)> {
    if let Some(f) = forced {
        let name = ENCODINGS
            .iter()
            .find(|e| **e == f)
            .copied()
            .ok_or_else(|| bad("encoding", "utf-8, utf-16le, utf-16be or big5"))?;
        return Ok((
            decode_as(bytes, name)?,
            Encoding {
                name,
                guessed: false,
            },
        ));
    }
    let by_bom = if bytes.starts_with(b"\xEF\xBB\xBF") {
        Some("utf-8")
    } else if bytes.starts_with(b"\xFF\xFE") {
        Some("utf-16le")
    } else if bytes.starts_with(b"\xFE\xFF") {
        Some("utf-16be")
    } else {
        None
    };
    if let Some(name) = by_bom {
        return Ok((
            decode_as(bytes, name)?,
            Encoding {
                name,
                guessed: false,
            },
        ));
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok((
            s.to_string(),
            Encoding {
                name: "utf-8",
                guessed: false,
            },
        )),
        Err(e) => match decode_as(bytes, "big5") {
            Ok(s) => Ok((
                s,
                Encoding {
                    name: "big5",
                    guessed: true,
                },
            )),
            Err(_) => Err(encoding_error(line_of(bytes, e.valid_up_to()))),
        },
    }
}

/// R-I4: of `,` `;` and tab, the one the header line holds most often
/// outside quotes.
pub fn detect_delimiter(text: &str) -> char {
    let mut counts = [0usize; 3];
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '\n' if !quoted => break,
            ',' if !quoted => counts[0] += 1,
            ';' if !quoted => counts[1] += 1,
            '\t' if !quoted => counts[2] += 1,
            _ => {}
        }
    }
    let best = counts.iter().copied().max().unwrap_or(0);
    if best == 0 || counts[0] == best {
        ','
    } else if counts[1] == best {
        ';'
    } else {
        '\t'
    }
}

/// Per record of `text` that has anything on it: its place among all
/// records, the ones with nothing at all on them included (the CSV reader
/// drops those; R-I2 numbers rows as the file has them).
fn ordinals(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let (mut quoted, mut any, mut ordinal) = (false, false, 0usize);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\n' if !quoted => {
                if any {
                    out.push(ordinal);
                }
                ordinal += 1;
                any = false;
            }
            '\r' if !quoted => {}
            _ => any = true,
        }
    }
    if any {
        out.push(ordinal);
    }
    out
}

/// R-I2: the rows of `text`, within the limits.
pub fn parse(text: &str, delimiter: char) -> Result<Table> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter as u8)
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut header: Option<Vec<String>> = None;
    let mut rows: Vec<(i32, Vec<String>)> = Vec::new();
    let mut records = Vec::new();
    for rec in reader.records() {
        let at = records.len();
        records.push(rec.map_err(|_| invalid_file("parse", json!({"row": at})))?);
    }
    // Where the two ways of reading disagree (a quote inside an unquoted
    // cell), rows are numbered as the reader gives them.
    let places = ordinals(text);
    let places = (places.len() == records.len()).then_some(places);
    let mut n = 0usize;
    for (k, rec) in records.iter().enumerate() {
        let cells: Vec<String> = rec.iter().map(String::from).collect();
        let skipped = match &places {
            Some(p) if k >= 1 => p[k] - p[k - 1] - 1,
            _ => 0,
        };
        if cells.len() > MAX_COLUMNS {
            return Err(invalid_file("too_many_columns", json!({})));
        }
        if let Some(c) = cells.iter().position(|c| c.len() > MAX_CELL) {
            return Err(invalid_file(
                "cell_too_large",
                json!({"row": n + skipped + usize::from(header.is_some()), "column": c + 1}),
            ));
        }
        let Some(h) = &header else {
            if cells.iter().all(|c| c.trim().is_empty()) {
                return Err(invalid_file("no_header", json!({})));
            }
            header = Some(cells);
            continue;
        };
        n += skipped + 1;
        if cells.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        if rows.len() >= MAX_ROWS {
            return Err(invalid_file("too_many_rows", json!({})));
        }
        let mut cells = cells;
        // A short row has empty cells where the header has columns.
        if cells.len() < h.len() {
            cells.resize(h.len(), String::new());
        }
        rows.push((n as i32, cells));
    }
    let Some(header) = header else {
        return Err(invalid_file("empty", json!({})));
    };
    if rows.is_empty() {
        return Err(invalid_file("empty", json!({})));
    }
    Ok(Table { header, rows })
}

fn delimiter_of(s: &str) -> Result<char> {
    match s {
        "," => Ok(','),
        ";" => Ok(';'),
        "\t" | "tab" => Ok('\t'),
        _ => Err(bad("delimiter", "\",\", \";\" or a tab")),
    }
}

// ---------------------------------------------------------------- settings (R-I5–R-I7)

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImportOptions {
    /// R-I7: a cell that is `'` and then `= + - @`, a tab or CR (or their
    /// full-width forms) loses the apostrophe (what the export added).
    #[serde(default = "yes")]
    pub strip_formula_quote: bool,
    /// R-I9: accept the new issues at once instead of leaving them in the
    /// Triage inbox (only with `triage_issues`).
    #[serde(default)]
    pub skip_triage: bool,
    /// R-I11: notify as for an ordinary write. Off: no notification, no
    /// mention, no automatic watching.
    #[serde(default)]
    pub notify: bool,
    /// Issues: the tracker of rows without a tracker column or value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tracker_id: Option<Uuid>,
}

fn yes() -> bool {
    true
}

/// The options as sent with `PUT /imports/{id}/settings` (each may be left out).
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImportOptionsInput {
    /// Default true: a cell that is `'` and then `= + - @`, a tab or CR
    /// loses the apostrophe (what the export added).
    #[serde(default = "yes")]
    pub strip_formula_quote: bool,
    /// Accept the new issues at once instead of leaving them in the Triage
    /// inbox (ignored without `triage_issues`).
    #[serde(default)]
    pub skip_triage: bool,
    /// Notify as for an ordinary write (default: nobody is notified).
    #[serde(default)]
    pub notify: bool,
    /// Issues: the tracker of rows without a tracker column or value.
    #[serde(default)]
    pub default_tracker_id: Option<Uuid>,
}

impl Default for ImportOptionsInput {
    fn default() -> Self {
        ImportOptions::default().into()
    }
}

impl From<ImportOptions> for ImportOptionsInput {
    fn from(o: ImportOptions) -> Self {
        Self {
            strip_formula_quote: o.strip_formula_quote,
            skip_triage: o.skip_triage,
            notify: o.notify,
            default_tracker_id: o.default_tracker_id,
        }
    }
}

impl From<ImportOptionsInput> for ImportOptions {
    fn from(o: ImportOptionsInput) -> Self {
        Self {
            strip_formula_quote: o.strip_formula_quote,
            skip_triage: o.skip_triage,
            notify: o.notify,
            default_tracker_id: o.default_tracker_id,
        }
    }
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            strip_formula_quote: true,
            skip_triage: false,
            notify: false,
            default_tracker_id: None,
        }
    }
}

/// What `imports.settings` holds: never a cell of the file, except the
/// values the person mapped by hand (`value_map` keys).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImportSettings {
    /// `ymd` (`YYYY-MM-DD`, `YYYY/MM/DD`), `mdy` or `dmy`.
    #[serde(default = "ymd")]
    pub date_format: String,
    /// Per column of the file: the field it fills, or null (not imported).
    #[serde(default)]
    pub mapping: Vec<Option<String>>,
    /// Per field: a value of the file → the id or name to use, or "" to
    /// leave the field empty.
    #[serde(default)]
    pub value_map: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub options: ImportOptions,
}

fn ymd() -> String {
    "ymd".into()
}

/// `PUT /imports/{id}/settings`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportSettingsInput {
    /// Decode the file again as this (`utf-8`, `utf-16le`, `utf-16be`, `big5`).
    #[serde(default)]
    pub encoding: Option<String>,
    /// `,`, `;` or a tab.
    #[serde(default)]
    pub delimiter: Option<String>,
    #[serde(default = "ymd")]
    pub date_format: String,
    #[serde(default)]
    pub mapping: Vec<Option<String>>,
    #[serde(default)]
    pub value_map: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub options: ImportOptionsInput,
    pub lock_version: i64,
}

const ISSUE_FIELDS: &[(&str, &str, &str, &[&str])] = &[
    ("subject", "標題", "text", &["subject", "主旨", "title"]),
    (
        "tracker",
        "追蹤類型",
        "list",
        &["tracker", "追蹤標籤", "類型"],
    ),
    ("status", "狀態", "list", &["status"]),
    ("priority", "優先度", "list", &["priority", "優先權"]),
    (
        "assigned_to",
        "負責人",
        "user",
        &["assignee", "assigned to", "被指派者", "指派給"],
    ),
    (
        "fixed_version",
        "里程碑",
        "list",
        &["milestone", "target version", "版本", "目標版本"],
    ),
    ("start_date", "開始日", "date", &["start date", "開始日期"]),
    (
        "due_date",
        "到期日",
        "date",
        &["due date", "完成日期", "到期日期"],
    ),
    (
        "estimated_hours",
        "預估工時",
        "number",
        &["estimated hours", "estimated time", "預估時間"],
    ),
    ("description", "說明", "text", &["description", "描述"]),
    ("is_private", "私有", "bool", &["private", "私人"]),
];

const TIME_FIELDS: &[(&str, &str, &str, &[&str])] = &[
    ("spent_on", "日期", "date", &["date", "spent on"]),
    ("hours", "時數", "number", &["hours", "小時"]),
    ("activity", "活動", "list", &["activity"]),
    ("issue", "單號", "text", &["issue", "議題", "key"]),
    ("user", "使用者", "user", &["user", "成員"]),
    (
        "comments",
        "說明",
        "text",
        &["comment", "comments", "註解", "備註"],
    ),
];

/// A field a column can fill (§6.12 step 3).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ImportField {
    /// `subject`, `tracker`, … or `cf:<id>`.
    pub key: String,
    pub label: String,
    /// `text`, `list`, `user`, `date`, `number` or `bool`.
    pub kind: String,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ImportChoice {
    pub id: Uuid,
    pub label: String,
}

// ---------------------------------------------------------------- what the rows resolve against

#[derive(Debug, Clone)]
struct CfInfo {
    name: String,
    /// The custom field type (`list`, `user`, `bool`, `date`, `int`, …).
    ty: String,
    options: Vec<(Uuid, Vec<String>)>,
}

/// Everything a row's values are looked up in, read as the owner: only
/// what they may use (R-I6). Nothing here says whether a value exists
/// elsewhere: unknown and out of reach are the same `unknown_value`.
struct Ctx {
    project: Uuid,
    kind: Kind,
    owner: Uuid,
    trackers: Vec<(Uuid, Vec<String>)>,
    default_status: HashMap<Uuid, Option<Uuid>>,
    statuses: Vec<(Uuid, Vec<String>)>,
    priorities: Vec<(Uuid, Vec<String>)>,
    versions: Vec<(Uuid, Vec<String>)>,
    assignees: Vec<(Uuid, Vec<String>)>,
    cf: BTreeMap<String, CfInfo>,
    activities: Vec<(Uuid, Vec<String>)>,
    loggers: Vec<(Uuid, Vec<String>)>,
    /// Display labels of every choice, for the wizard.
    labels: HashMap<Uuid, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Issues,
    TimeEntries,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "issues" => Some(Self::Issues),
            "time_entries" => Some(Self::TimeEntries),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::TimeEntries => "time_entries",
        }
    }
    fn permission(self) -> &'static str {
        match self {
            Self::Issues => IMPORT_ISSUES,
            Self::TimeEntries => IMPORT_TIME_ENTRIES,
        }
    }
    fn fields(
        self,
    ) -> &'static [(
        &'static str,
        &'static str,
        &'static str,
        &'static [&'static str],
    )] {
        match self {
            Self::Issues => ISSUE_FIELDS,
            Self::TimeEntries => TIME_FIELDS,
        }
    }
}

fn names(v: &[Option<&str>]) -> Vec<String> {
    v.iter()
        .flatten()
        .map(|s| s.trim().to_lowercase())
        .collect()
}

async fn load_ctx(
    conn: &mut PgConnection,
    owner: AuthContext,
    project: Uuid,
    kind: Kind,
) -> Result<Ctx> {
    let owner_id = owner
        .user_id()
        .ok_or_else(|| api(ErrorCode::Unauthenticated, "the import has no owner"))?;
    // The project as the owner sees it now (gone or hidden: not_found).
    describe::visible_project(&mut *conn, owner, &project.to_string()).await?;
    let mut labels: HashMap<Uuid, String> = HashMap::new();
    let mut named = |rows: Vec<(Uuid, String)>| -> Vec<(Uuid, Vec<String>)> {
        rows.into_iter()
            .map(|(id, name)| {
                labels.insert(id, name.clone());
                (id, names(&[Some(&name)]))
            })
            .collect()
    };
    let tracker_rows: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT t.id, t.name, t.default_status_id FROM trackers t
         JOIN project_trackers pt ON pt.tracker_id = t.id AND pt.project_id = $1
         ORDER BY t.position, t.name",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    let default_status = tracker_rows.iter().map(|t| (t.0, t.2)).collect();
    let trackers = named(tracker_rows.iter().map(|t| (t.0, t.1.clone())).collect());
    let statuses = named(
        sqlx::query_as("SELECT id, name FROM issue_statuses ORDER BY position, name")
            .fetch_all(&mut *conn)
            .await?,
    );
    let priorities = named(
        sqlx::query_as("SELECT id, name FROM issue_priorities WHERE active ORDER BY position")
            .fetch_all(&mut *conn)
            .await?,
    );
    let versions = named(
        sqlx::query_as(
            "SELECT id, name FROM versions
             WHERE project_id = $1 AND status = 'open' AND deleted_at IS NULL ORDER BY name",
        )
        .bind(project)
        .fetch_all(&mut *conn)
        .await?,
    );
    let activities = named(
        crate::time_entries::available_activities(&mut *conn, project)
            .await?
            .into_iter()
            .map(|(id, name, _)| (id, name))
            .collect(),
    );
    let people = describe::assignable(&mut *conn, project, Some(owner_id)).await?;
    let assignees: Vec<(Uuid, Vec<String>)> = people
        .iter()
        .map(|a| {
            labels.insert(a.id, a.name.clone());
            (a.id, names(&[Some(&a.name), a.login.as_deref()]))
        })
        .collect();
    // R-I6: who can log time here (an active user holding `log_time`,
    // directly or through a group).
    let logger_rows: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT p.id, p.name, p.login FROM principals p
         WHERE p.kind = 'user' AND p.status = 'active' AND EXISTS (
             SELECT 1 FROM members m
             JOIN member_roles mr ON mr.member_id = m.id
             JOIN role_permissions rp ON rp.role_id = mr.role_id AND rp.permission = 'log_time'
             WHERE m.project_id = $1
               AND (m.principal_id = p.id
                    OR m.principal_id IN (SELECT gm.group_id FROM group_members gm
                                          WHERE gm.user_id = p.id)))
         ORDER BY p.name, p.id",
    )
    .bind(project)
    .fetch_all(&mut *conn)
    .await?;
    let loggers: Vec<(Uuid, Vec<String>)> = logger_rows
        .iter()
        .map(|(id, name, login)| {
            labels.insert(*id, name.clone());
            (*id, names(&[Some(name), login.as_deref()]))
        })
        .collect();
    // R-I5: the custom fields the owner can write when creating an issue,
    // over the project's trackers.
    let mut cf: BTreeMap<String, CfInfo> = BTreeMap::new();
    if kind == Kind::Issues {
        for (tracker, _) in &trackers {
            let schema = describe::tracker_schema_in(
                &mut *conn,
                owner,
                &project.to_string(),
                &tracker.to_string(),
            )
            .await?;
            for f in schema.fields {
                if !f.key.starts_with("cf:") || !f.ops.iter().any(|o| o == "create") {
                    continue;
                }
                let options = f
                    .options
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|o| o.active)
                    .map(|o| {
                        labels.insert(o.id, o.label.clone());
                        (o.id, names(&[Some(&o.label)]))
                    })
                    .collect();
                cf.entry(f.key.clone()).or_insert(CfInfo {
                    name: f.name.unwrap_or_else(|| f.key.clone()),
                    ty: f.field_type,
                    options,
                });
            }
        }
    }
    Ok(Ctx {
        project,
        kind,
        owner: owner_id,
        trackers,
        default_status,
        statuses,
        priorities,
        versions,
        assignees,
        cf,
        activities,
        loggers,
        labels,
    })
}

impl Ctx {
    fn fields(&self) -> Vec<ImportField> {
        let mut out: Vec<ImportField> = self
            .kind
            .fields()
            .iter()
            .map(|(key, label, kind, _)| ImportField {
                key: key.to_string(),
                label: label.to_string(),
                kind: kind.to_string(),
                required: matches!(*key, "subject" | "hours"),
            })
            .collect();
        for (key, f) in &self.cf {
            let kind = match f.ty.as_str() {
                "list" => "list",
                "user" => "user",
                "bool" => "bool",
                "date" => "date",
                "int" | "float" => "number",
                _ => "text",
            };
            out.push(ImportField {
                key: key.clone(),
                label: f.name.clone(),
                kind: kind.into(),
                required: false,
            });
        }
        out
    }

    /// The choices of a list or user field; `None` for other fields.
    fn choices(&self, field: &str) -> Option<&[(Uuid, Vec<String>)]> {
        Some(match field {
            "tracker" => &self.trackers,
            "status" => &self.statuses,
            "priority" => &self.priorities,
            "fixed_version" => &self.versions,
            "assigned_to" => &self.assignees,
            "activity" => &self.activities,
            "user" => &self.loggers,
            _ => {
                let f = self.cf.get(field)?;
                match f.ty.as_str() {
                    "list" => &f.options,
                    "user" => &self.assignees,
                    _ => return None,
                }
            }
        })
    }

    /// R-I5: a column whose header is a field's name or alias, whatever
    /// the case.
    fn suggest(&self, header: &[String]) -> Vec<Option<String>> {
        let mut used: BTreeSet<String> = BTreeSet::new();
        header
            .iter()
            .map(|h| {
                let h = h.trim().to_lowercase();
                let builtin = self.kind.fields().iter().find(|(key, label, _, alias)| {
                    *key == h || label.to_lowercase() == h || alias.contains(&h.as_str())
                });
                let found = builtin.map(|f| f.0.to_string()).or_else(|| {
                    self.cf
                        .iter()
                        .find(|(_, f)| f.name.to_lowercase() == h)
                        .map(|(k, _)| k.clone())
                })?;
                used.insert(found.clone()).then_some(found)
            })
            .collect()
    }
}

/// A row that cannot become a command, or that the engine refused: a code
/// and a field key, never a value (R-I14).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowError {
    code: String,
    field: Option<String>,
}

fn row_err(code: &str, field: &str) -> RowError {
    RowError {
        code: code.into(),
        field: Some(field.into()),
    }
}

/// R-I6: an id, or a name whatever its case. Nothing found and out of
/// reach are one answer.
fn pick(choices: &[(Uuid, Vec<String>)], raw: &str) -> std::result::Result<Uuid, &'static str> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return if choices.iter().any(|c| c.0 == id) {
            Ok(id)
        } else {
            Err("unknown_value")
        };
    }
    let want = raw.to_lowercase();
    let found: BTreeSet<Uuid> = choices
        .iter()
        .filter(|c| c.1.contains(&want))
        .map(|c| c.0)
        .collect();
    match found.len() {
        0 => Err("unknown_value"),
        1 => Ok(found.into_iter().next().unwrap_or_default()),
        _ => Err("ambiguous_value"),
    }
}

/// R-I6: `ymd` takes `-` or `/`; `mdy` and `dmy` take `/`, `-` or `.`.
fn date(raw: &str, format: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split(['-', '/', '.']).collect();
    let [a, b, c] = parts[..] else { return None };
    let n = |s: &str| -> Option<u32> {
        (!s.is_empty() && s.len() <= 4 && s.bytes().all(|x| x.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let (y, m, d) = match format {
        "mdy" => (n(c)?, n(a)?, n(b)?),
        "dmy" => (n(c)?, n(b)?, n(a)?),
        _ => (n(a)?, n(b)?, n(c)?),
    };
    if !(1000..=9999).contains(&y) {
        return None;
    }
    NaiveDate::from_ymd_opt(y as i32, m, d).map(|d| d.to_string())
}

fn boolean(raw: &str) -> Option<bool> {
    match raw.to_lowercase().as_str() {
        "是" | "yes" | "true" | "1" | "y" => Some(true),
        "否" | "no" | "false" | "0" | "n" => Some(false),
        _ => None,
    }
}

/// R-I7 and trimming: the cell as a value.
fn cell(raw: &str, strip: bool) -> &str {
    let mut s = raw;
    if strip
        && let Some(rest) = s.strip_prefix('\'')
        && matches!(
            rest.chars().next(),
            Some('=' | '+' | '-' | '@' | '\t' | '\r' | '＝' | '＋' | '－' | '＠')
        )
    {
        s = rest;
    }
    s.trim()
}

/// A deterministic id of this import's row (R-I9). UUIDv8 from SHA-256
/// (the contract's UUIDv5 would need SHA-1, which the build does not
/// carry): the same import, row and purpose always give the same id.
fn derived_id(import: Uuid, row: i32, purpose: &str) -> Uuid {
    let mut h = Sha256::new();
    h.update(b"oxledger/import/v1\n");
    h.update(import.as_bytes());
    h.update(row.to_be_bytes());
    h.update(purpose.as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

/// The values of one row by field, after R-I7 and the value mapping; an
/// empty cell is no value.
fn row_values<'a>(set: &'a ImportSettings, cells: &'a [String]) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    for (i, field) in set.mapping.iter().enumerate() {
        let (Some(field), Some(raw)) = (field, cells.get(i)) else {
            continue;
        };
        let v = cell(raw, set.options.strip_formula_quote);
        let v = match set.value_map.get(field).and_then(|m| m.get(v)) {
            Some(mapped) => mapped.trim(),
            None => v,
        };
        if !v.is_empty() {
            out.push((field.as_str(), v));
        }
    }
    out
}

/// R-I9: the command document of one row.
fn build_doc(
    ctx: &Ctx,
    set: &ImportSettings,
    import: Uuid,
    row: i32,
    cells: &[String],
) -> std::result::Result<Json, RowError> {
    let values = row_values(set, cells);
    let lookup = |field: &str, v: &str| -> std::result::Result<Uuid, RowError> {
        let choices = ctx.choices(field).unwrap_or(&[]);
        pick(choices, v).map_err(|code| row_err(code, field))
    };
    let op = |purpose: &str| derived_id(import, row, purpose);
    match ctx.kind {
        Kind::Issues => {
            let mut fields = serde_json::Map::new();
            let mut tracker: Option<Uuid> = None;
            let mut status: Option<Uuid> = None;
            for (field, v) in values {
                match field {
                    "subject" | "description" => {
                        fields.insert(field.into(), json!(v));
                    }
                    "tracker" => tracker = Some(lookup(field, v)?),
                    "status" => status = Some(lookup(field, v)?),
                    "priority" => {
                        fields.insert("priority_id".into(), json!(lookup(field, v)?));
                    }
                    "assigned_to" => {
                        fields.insert("assigned_to_id".into(), json!(lookup(field, v)?));
                    }
                    "fixed_version" => {
                        fields.insert("fixed_version_id".into(), json!(lookup(field, v)?));
                    }
                    "start_date" | "due_date" => {
                        let d =
                            date(v, &set.date_format).ok_or_else(|| row_err("bad_date", field))?;
                        fields.insert(field.into(), json!(d));
                    }
                    "estimated_hours" => {
                        if matches!(commands::estimated_hours(v), Err(None)) {
                            return Err(row_err("bad_number", field));
                        }
                        fields.insert(field.into(), json!(v));
                    }
                    "is_private" => {
                        let b = boolean(v).ok_or_else(|| row_err("bad_boolean", field))?;
                        fields.insert(field.into(), json!(b));
                    }
                    cf => {
                        let Some(info) = ctx.cf.get(cf) else {
                            // A field the owner can no longer write.
                            return Err(row_err("unknown_field", cf));
                        };
                        let value = match info.ty.as_str() {
                            "bool" => {
                                json!({"bool": boolean(v).ok_or_else(|| row_err("bad_boolean", cf))?})
                            }
                            "date" => json!({"date": date(v, &set.date_format)
                                .ok_or_else(|| row_err("bad_date", cf))?}),
                            "int" | "float" => {
                                let n = commands::canonical_number(v, info.ty == "int")
                                    .ok_or_else(|| row_err("bad_number", cf))?;
                                json!({ info.ty.as_str(): n })
                            }
                            "list" | "user" => json!({ info.ty.as_str(): lookup(cf, v)? }),
                            ty => json!({ ty: v }),
                        };
                        fields.insert(cf.into(), value);
                    }
                }
            }
            if !fields.contains_key("subject") {
                return Err(row_err("required_field_missing", "subject"));
            }
            let tracker = tracker
                .or(set.options.default_tracker_id)
                .or(match &ctx.trackers[..] {
                    [only] => Some(only.0),
                    _ => None,
                })
                .ok_or_else(|| row_err("required_field_missing", "tracker"))?;
            if !ctx.trackers.iter().any(|t| t.0 == tracker) {
                return Err(row_err("unknown_value", "tracker"));
            }
            let mut ops = vec![json!({
                "op_id": op("create"), "kind": "create_issue", "bind": "row",
                "project_id": ctx.project, "tracker_id": tracker,
                "fields": fields, "format": "markdown",
            })];
            let initial = ctx.default_status.get(&tracker).copied().flatten();
            if let Some(s) = status
                && Some(s) != initial
            {
                ops.push(json!({
                    "op_id": op("status"), "kind": "transition",
                    "target": {"type": "temp", "ref": "row"}, "to_status_id": s,
                }));
            }
            Ok(json!({"command_schema_version": 1, "ops": ops}))
        }
        Kind::TimeEntries => {
            let mut fields = serde_json::Map::new();
            let mut issue: Option<String> = None;
            for (field, v) in values {
                match field {
                    "spent_on" => {
                        let d =
                            date(v, &set.date_format).ok_or_else(|| row_err("bad_date", field))?;
                        fields.insert(field.into(), json!(d));
                    }
                    "hours" => {
                        if crate::time_entries::parse_hours(&json!(v)).is_none() {
                            return Err(row_err("bad_number", field));
                        }
                        fields.insert(field.into(), json!(v));
                    }
                    "activity" => {
                        fields.insert("activity_id".into(), json!(lookup(field, v)?));
                    }
                    "user" => {
                        fields.insert("user_id".into(), json!(lookup(field, v)?));
                    }
                    "comments" => {
                        fields.insert(field.into(), json!(v));
                    }
                    "issue" => issue = Some(v.trim_start_matches('#').to_uppercase()),
                    other => return Err(row_err("unknown_field", other)),
                }
            }
            if !fields.contains_key("hours") {
                return Err(row_err("required_field_missing", "hours"));
            }
            let mut o = json!({
                "op_id": op("log"), "kind": "log_time", "project_id": ctx.project, "fields": fields,
            });
            if let Some(k) = issue {
                o["issue"] = json!({"type": "issue", "key": k});
            }
            Ok(json!({"command_schema_version": 1, "ops": [o]}))
        }
    }
}

/// What the engine refused, as a row error: its code, and the field's key
/// when the error names one. Never the message (it may quote a value).
fn engine_error(kind: Kind, e: &ApiError) -> RowError {
    let code = e.code.as_str();
    let d = e.details.as_ref();
    // `fields`: the engine's list of required fields that are empty.
    let one_of_list = d
        .and_then(|d| d.get("fields"))
        .and_then(Json::as_array)
        .and_then(|a| a.first())
        .and_then(Json::as_str);
    let named = d
        .and_then(|d| d.get("field").or_else(|| d.get("path")))
        .and_then(Json::as_str)
        .or(one_of_list)
        .map(|p| {
            let last = p.rsplit('.').next().unwrap_or(p);
            match last {
                "assigned_to_id" => "assigned_to",
                "fixed_version_id" => "fixed_version",
                "priority_id" => "priority",
                "to_status_id" => "status",
                "tracker_id" => "tracker",
                "activity_id" => "activity",
                "user_id" => "user",
                x => x,
            }
            .to_string()
        })
        // Only a field key: a path into the document says nothing to the person.
        .filter(|f| !f.contains('[') && f.len() <= 40);
    // R-I6, T-M2-98: an issue key that does not exist and one the owner
    // cannot see are the same unknown value.
    if kind == Kind::TimeEntries && e.code == ErrorCode::NotFound {
        return row_err("unknown_value", "issue");
    }
    RowError {
        code: code.into(),
        field: named,
    }
}

// ---------------------------------------------------------------- views of an import

/// An import's state and progress (`GET /imports/{id}`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportStatus {
    pub id: Uuid,
    /// The project's key.
    pub project: String,
    /// `issues` or `time_entries`.
    pub kind: String,
    /// `uploaded`, `ready`, `validating`, `validated`, `running`, `done`,
    /// `canceled`, `failed` or `expired`.
    pub state: String,
    pub encoding: String,
    /// The encoding was guessed (Big5 without a BOM): ask the person.
    pub encoding_guessed: bool,
    /// `,`, `;` or a tab.
    pub delimiter: String,
    /// Data rows of the file.
    pub row_count: i32,
    pub lock_version: i64,
    pub settings: ImportSettings,
    /// The file is still kept (R-I13).
    pub file_available: bool,
    /// Issues, once the import ended: the closed statuses `close-created`
    /// takes.
    pub undo_statuses: Vec<ImportChoice>,
    /// Rows the dry run or the run has finished.
    pub done: i64,
    pub ok: i64,
    pub errors: i64,
    pub skipped: i64,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// A value of the file for a list or user field, and what it resolves to.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValueGuess {
    pub value: String,
    /// The id it resolves to; null when nothing or several things match.
    pub target: Option<Uuid>,
    /// `unknown_value` or `ambiguous_value` when there is no target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// An import with what the wizard needs to map it (§6.12 steps 2–4).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportDetail {
    #[serde(flatten)]
    pub status: ImportStatus,
    pub header: Vec<String>,
    /// The first rows of the file.
    pub preview: Vec<Vec<String>>,
    pub fields: Vec<ImportField>,
    /// Per column: the field its header names, or null.
    pub suggested: Vec<Option<String>>,
    /// Per mapped list or user field: the choices.
    pub choices: BTreeMap<String, Vec<ImportChoice>>,
    /// Per mapped list or user field: the different values of the file
    /// (at most 200) and what each resolves to.
    pub values: BTreeMap<String, Vec<ValueGuess>>,
    /// The project's trackers (the default tracker of issue rows).
    pub trackers: Vec<ImportChoice>,
    /// The project uses Triage and you may decide there (`skip_triage`).
    pub can_skip_triage: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportList {
    pub items: Vec<ImportStatus>,
    /// What you may import here now: `issues`, `time_entries` (the web
    /// app shows its entry points by this).
    pub can_import: Vec<String>,
}

/// One row's result. Never a cell of the file.
#[derive(Debug, Clone, Serialize, JsonSchema, sqlx::FromRow)]
pub struct ImportRow {
    /// The data row in the file (the header is not counted).
    pub row_no: i32,
    /// `pending`, `ok`, `error` or `skipped`.
    pub state: String,
    pub error_code: Option<String>,
    /// The field's key (`subject`, `cf:<id>`, …).
    pub error_field: Option<String>,
    /// The issue the row created.
    pub issue_key: Option<String>,
    pub time_entry_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportRows {
    pub items: Vec<ImportRow>,
    pub next_cursor: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Loaded {
    id: Uuid,
    project_id: Uuid,
    project_key: String,
    kind: String,
    owner_id: Uuid,
    state: String,
    file_available: bool,
    encoding: String,
    encoding_guessed: bool,
    delimiter: String,
    row_count: i32,
    settings: Json,
    lock_version: i64,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

const LOAD: &str =
    "SELECT im.id, im.project_id, p.key AS project_key, im.kind, im.owner_id, im.state,
        im.file IS NOT NULL AS file_available, im.encoding, im.encoding_guessed, im.delimiter, im.row_count, im.settings,
        im.lock_version, im.created_at, im.started_at, im.finished_at
 FROM imports im JOIN projects p ON p.id = im.project_id";

impl Loaded {
    fn kind(&self) -> Kind {
        Kind::parse(&self.kind).unwrap_or(Kind::Issues)
    }
    fn settings(&self) -> ImportSettings {
        serde_json::from_value(self.settings.clone()).unwrap_or_default()
    }
    /// The file's bytes (read only where they are needed: up to 10 MB).
    async fn file(&self, conn: &mut PgConnection) -> Result<Vec<u8>> {
        sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT file FROM imports WHERE id = $1")
            .bind(self.id)
            .fetch_optional(&mut *conn)
            .await?
            .flatten()
            .ok_or_else(|| state_invalid("purged"))
    }
    async fn table(&self, conn: &mut PgConnection) -> Result<Table> {
        let bytes = self.file(&mut *conn).await?;
        let (text, _) = decode(&bytes, Some(&self.encoding))?;
        parse(&text, delimiter_of(&self.delimiter)?)
    }
}

async fn status_of(conn: &mut PgConnection, im: &Loaded) -> Result<ImportStatus> {
    let (done, ok, errors, skipped): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state <> 'pending'), count(*) FILTER (WHERE state = 'ok'),
                count(*) FILTER (WHERE state = 'error'), count(*) FILTER (WHERE state = 'skipped')
         FROM import_rows WHERE import_id = $1",
    )
    .bind(im.id)
    .fetch_one(&mut *conn)
    .await?;
    let undo_statuses = if im.kind() == Kind::Issues
        && matches!(im.state.as_str(), "done" | "canceled" | "failed")
    {
        sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, name FROM issue_statuses WHERE category = 'done' ORDER BY position, name",
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|(id, label)| ImportChoice { id, label })
        .collect()
    } else {
        Vec::new()
    };
    Ok(ImportStatus {
        undo_statuses,
        id: im.id,
        project: im.project_key.clone(),
        kind: im.kind.clone(),
        state: im.state.clone(),
        encoding: im.encoding.clone(),
        encoding_guessed: im.encoding_guessed,
        delimiter: im.delimiter.clone(),
        row_count: im.row_count,
        lock_version: im.lock_version,
        settings: im.settings(),
        file_available: im.file_available,
        done,
        ok,
        errors,
        skipped,
        created_at: im.created_at,
        started_at: im.started_at,
        finished_at: im.finished_at,
    })
}

/// The caller's own import (R-I14: someone else's is `not_found`, like one
/// that does not exist). `lock`: FOR UPDATE.
async fn own(conn: &mut PgConnection, caller: AuthContext, id: &str, lock: bool) -> Result<Loaded> {
    let (user, _) = caller.require_human_session()?;
    let id: Uuid = id.parse().map_err(|_| not_found())?;
    let sql = format!(
        "{LOAD} WHERE im.id = $1 AND im.owner_id = $2{}",
        if lock { " FOR UPDATE OF im" } else { "" }
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(not_found)
}

async fn can_decide_triage(
    conn: &mut PgConnection,
    caller: AuthContext,
    project: Uuid,
) -> Result<bool> {
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM project_triage_settings WHERE project_id = $1")
            .bind(project)
            .fetch_optional(&mut *conn)
            .await?;
    if enabled != Some(true) {
        return Ok(false);
    }
    let actor = perm::actor(&mut *conn, caller).await?;
    Ok(perm::project_perms(&mut *conn, &actor, project)
        .await?
        .is_some_and(|pp| pp.allowed_any(perm::TRIAGE_ISSUES)))
}

async fn detail(conn: &mut PgConnection, caller: AuthContext, im: &Loaded) -> Result<ImportDetail> {
    let status = status_of(&mut *conn, im).await?;
    let table = im.table(&mut *conn).await?;
    let ctx = load_ctx(&mut *conn, caller, im.project_id, im.kind()).await?;
    let set = im.settings();
    let mut choices: BTreeMap<String, Vec<ImportChoice>> = BTreeMap::new();
    let mut values: BTreeMap<String, Vec<ValueGuess>> = BTreeMap::new();
    for (col, field) in set.mapping.iter().enumerate() {
        let Some(field) = field else { continue };
        let Some(cs) = ctx.choices(field) else {
            continue;
        };
        choices.insert(
            field.clone(),
            cs.iter()
                .map(|c| ImportChoice {
                    id: c.0,
                    label: ctx.labels.get(&c.0).cloned().unwrap_or_default(),
                })
                .collect(),
        );
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut list = Vec::new();
        for (_, cells) in &table.rows {
            let Some(raw) = cells.get(col) else { continue };
            let v = cell(raw, set.options.strip_formula_quote);
            if v.is_empty() || !seen.insert(v) {
                continue;
            }
            if list.len() >= MAX_VALUES {
                break;
            }
            let mapped = set.value_map.get(field).and_then(|m| m.get(v));
            let guess = match mapped.map(|m| m.trim()) {
                Some("") => ValueGuess {
                    value: v.into(),
                    target: None,
                    problem: None,
                },
                other => match pick(cs, other.unwrap_or(v)) {
                    Ok(id) => ValueGuess {
                        value: v.into(),
                        target: Some(id),
                        problem: None,
                    },
                    Err(code) => ValueGuess {
                        value: v.into(),
                        target: None,
                        problem: Some(code.into()),
                    },
                },
            };
            list.push(guess);
        }
        values.insert(field.clone(), list);
    }
    Ok(ImportDetail {
        status,
        suggested: ctx.suggest(&table.header),
        fields: ctx.fields(),
        trackers: ctx
            .trackers
            .iter()
            .map(|t| ImportChoice {
                id: t.0,
                label: ctx.labels.get(&t.0).cloned().unwrap_or_default(),
            })
            .collect(),
        can_skip_triage: im.kind() == Kind::Issues
            && can_decide_triage(&mut *conn, caller, im.project_id).await?,
        preview: table
            .rows
            .iter()
            .take(PREVIEW_ROWS)
            .map(|r| r.1.clone())
            .collect(),
        header: table.header,
        choices,
        values,
    })
}

/// R-I5: the mapping names fields the owner may fill, each once, and
/// (issues) the subject. A field out of reach and one that does not exist
/// are the same answer.
fn check_mapping(ctx: &Ctx, set: &ImportSettings, columns: usize) -> Result<()> {
    if set.mapping.len() > columns.max(MAX_COLUMNS) {
        return Err(mapping_invalid("mapping", "too_many_columns"));
    }
    if !matches!(set.date_format.as_str(), "ymd" | "mdy" | "dmy") {
        return Err(bad("date_format", "ymd, mdy or dmy"));
    }
    let known: BTreeSet<String> = ctx.fields().into_iter().map(|f| f.key).collect();
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for f in set.mapping.iter().flatten() {
        if !known.contains(f) {
            return Err(mapping_invalid(f, "unknown_field"));
        }
        if !used.insert(f) {
            return Err(mapping_invalid(f, "mapped_twice"));
        }
    }
    for f in set.value_map.keys() {
        if !known.contains(f) {
            return Err(mapping_invalid(f, "unknown_field"));
        }
    }
    if let Some(t) = set.options.default_tracker_id
        && !ctx.trackers.iter().any(|x| x.0 == t)
    {
        return Err(mapping_invalid("tracker", "unknown_value"));
    }
    Ok(())
}

/// What a dry run needs on top of [`check_mapping`]: the required fields.
fn check_complete(kind: Kind, set: &ImportSettings) -> Result<()> {
    let need = match kind {
        Kind::Issues => "subject",
        Kind::TimeEntries => "hours",
    };
    if !set.mapping.iter().flatten().any(|f| f == need) {
        return Err(mapping_invalid(need, "required"));
    }
    Ok(())
}

// ---------------------------------------------------------------- the API

/// `POST /projects/{id}/imports?kind=`: the raw file (R-I1, R-I2).
pub async fn upload(
    app: &App,
    caller: AuthContext,
    project: &str,
    kind: &str,
    bytes: &[u8],
) -> Result<ImportDetail> {
    let (user, _) = caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    // Sight first: a project out of reach answers like one that is not there.
    let project_id = describe::visible_project(&mut tx, caller, project).await?;
    let kind = Kind::parse(kind).ok_or_else(|| bad("kind", "issues or time_entries"))?;
    let actor = perm::actor(&mut tx, caller).await?;
    let allowed = perm::project_perms(&mut tx, &actor, project_id)
        .await?
        .is_some_and(|pp| pp.status == "active" && pp.allowed_any(kind.permission()));
    if !allowed {
        return Err(ApiError::new(
            ErrorCode::ForbiddenOp,
            "you may not import into this project",
        )
        .with_details(json!({"permission": kind.permission()}))
        .into());
    }
    if bytes.len() > MAX_FILE {
        return Err(ApiError::new(
            ErrorCode::PayloadTooLarge,
            "an import file is at most 10 MB",
        )
        .with_details(json!({"limit": MAX_FILE, "kind": "import"}))
        .into());
    }
    let (text, enc) = decode(bytes, None)?;
    let delimiter = detect_delimiter(&text);
    let table = parse(&text, delimiter)?;
    let ctx = load_ctx(&mut tx, caller, project_id, kind).await?;
    let settings = ImportSettings {
        date_format: ymd(),
        mapping: ctx.suggest(&table.header),
        ..Default::default()
    };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO imports (project_id, kind, owner_id, state, file, file_sha256, encoding,
                              encoding_guessed, delimiter, row_count, settings)
         VALUES ($1, $2, $3, 'uploaded', $4, $5, $6, $7, $8, $9, $10) RETURNING id",
    )
    .bind(project_id)
    .bind(kind.as_str())
    .bind(user)
    .bind(bytes)
    .bind(&crate::auth::sha256(bytes)[..])
    .bind(enc.name)
    .bind(enc.guessed)
    .bind(delimiter.to_string())
    .bind(table.rows.len() as i32)
    .bind(serde_json::to_value(&settings)?)
    .fetch_one(&mut *tx)
    .await?;
    let im = own(&mut tx, caller, &id.to_string(), false).await?;
    let out = detail(&mut tx, caller, &im).await?;
    tx.commit().await?;
    Ok(out)
}

/// `GET /projects/{id}/imports`: the caller's imports here, newest first.
pub async fn list(app: &App, caller: AuthContext, project: &str) -> Result<ImportList> {
    let (user, _) = caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let project_id = describe::visible_project(&mut tx, caller, project).await?;
    let sql = format!(
        "{LOAD} WHERE im.project_id = $1 AND im.owner_id = $2
           AND im.created_at > now() - make_interval(days => $3)
         ORDER BY im.created_at DESC, im.id DESC LIMIT 100"
    );
    let rows: Vec<Loaded> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(project_id)
        .bind(user)
        .bind(KEEP_DAYS)
        .fetch_all(&mut *tx)
        .await?;
    let mut items = Vec::with_capacity(rows.len());
    for im in &rows {
        items.push(status_of(&mut tx, im).await?);
    }
    let actor = perm::actor(&mut tx, caller).await?;
    let pp = perm::project_perms(&mut tx, &actor, project_id).await?;
    let can_import = [Kind::Issues, Kind::TimeEntries]
        .into_iter()
        .filter(|k| {
            pp.as_ref()
                .is_some_and(|pp| pp.status == "active" && pp.allowed_any(k.permission()))
        })
        .map(|k| k.as_str().to_string())
        .collect();
    tx.commit().await?;
    Ok(ImportList { items, can_import })
}

/// `GET /imports/{id}`: state and progress (polled while it works).
pub async fn get(app: &App, caller: AuthContext, id: &str) -> Result<ImportStatus> {
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, false).await?;
    let out = status_of(&mut tx, &im).await?;
    tx.commit().await?;
    Ok(out)
}

/// `GET /imports/{id}/preview`: the import with the header, the first
/// rows, the fields and how the file's values resolve.
pub async fn preview(app: &App, caller: AuthContext, id: &str) -> Result<ImportDetail> {
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, false).await?;
    let out = detail(&mut tx, caller, &im).await?;
    tx.commit().await?;
    Ok(out)
}

fn stale() -> anyhow::Error {
    api(
        ErrorCode::Conflict,
        "the import changed since you read it; read it again",
    )
}

/// `PUT /imports/{id}/settings`: the state goes back to `ready`; a dry
/// run made before no longer counts.
pub async fn put_settings(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &ImportSettingsInput,
) -> Result<ImportDetail> {
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, true).await?;
    if !matches!(im.state.as_str(), "uploaded" | "ready" | "validated") {
        return Err(state_invalid(&im.state));
    }
    if im.lock_version != input.lock_version {
        return Err(stale());
    }
    let bytes = im.file(&mut tx).await?;
    let bytes = &bytes[..];
    // The file read again with what the person chose.
    let (text, enc) = match &input.encoding {
        Some(e) if *e != im.encoding || im.encoding_guessed => decode(bytes, Some(e))?,
        _ => (
            decode(bytes, Some(&im.encoding))?.0,
            Encoding {
                name: ENCODINGS
                    .iter()
                    .find(|e| **e == im.encoding)
                    .copied()
                    .unwrap_or("utf-8"),
                guessed: im.encoding_guessed,
            },
        ),
    };
    let delimiter = match &input.delimiter {
        Some(d) => delimiter_of(d)?,
        None => delimiter_of(&im.delimiter)?,
    };
    let table = parse(&text, delimiter)?;
    let ctx = load_ctx(&mut tx, caller, im.project_id, im.kind()).await?;
    let mut set = ImportSettings {
        date_format: input.date_format.clone(),
        mapping: input.mapping.clone(),
        value_map: input.value_map.clone(),
        options: input.options.clone().into(),
    };
    // Read another way, the file has other columns: what was mapped (and
    // the values mapped by hand) no longer means anything.
    if enc.name != im.encoding || delimiter.to_string() != im.delimiter {
        set.mapping = ctx.suggest(&table.header);
        set.value_map.clear();
    }
    set.mapping.truncate(table.header.len());
    check_mapping(&ctx, &set, table.header.len())?;
    if set.options.skip_triage && !can_decide_triage(&mut tx, caller, im.project_id).await? {
        set.options.skip_triage = false;
    }
    sqlx::query("DELETE FROM import_rows WHERE import_id = $1")
        .bind(im.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE imports SET state = 'ready', encoding = $2, encoding_guessed = $3, delimiter = $4,
                            row_count = $5, settings = $6, lock_version = lock_version + 1
         WHERE id = $1",
    )
    .bind(im.id)
    .bind(enc.name)
    .bind(enc.guessed)
    .bind(delimiter.to_string())
    .bind(table.rows.len() as i32)
    .bind(serde_json::to_value(&set)?)
    .execute(&mut *tx)
    .await?;
    let im = own(&mut tx, caller, id, false).await?;
    let out = detail(&mut tx, caller, &im).await?;
    tx.commit().await?;
    Ok(out)
}

async fn queue_rows(conn: &mut PgConnection, im: &Loaded) -> Result<()> {
    let table = im.table(&mut *conn).await?;
    let rows: Vec<i32> = table.rows.iter().map(|r| r.0).collect();
    sqlx::query("DELETE FROM import_rows WHERE import_id = $1")
        .bind(im.id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO import_rows (import_id, row_no, state) SELECT $1, unnest($2::int[]), 'pending'",
    )
    .bind(im.id)
    .bind(&rows)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `POST /imports/{id}/validate` (R-I8): the dry run starts; the worker
/// checks every row as the owner and writes nothing but the results.
pub async fn validate(app: &App, caller: AuthContext, id: &str) -> Result<ImportStatus> {
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, true).await?;
    if !matches!(im.state.as_str(), "uploaded" | "ready" | "validated") {
        return Err(state_invalid(&im.state));
    }
    check_complete(im.kind(), &im.settings())?;
    queue_rows(&mut tx, &im).await?;
    sqlx::query("UPDATE imports SET state = 'validating' WHERE id = $1")
        .bind(im.id)
        .execute(&mut *tx)
        .await?;
    let im = own(&mut tx, caller, id, false).await?;
    let out = status_of(&mut tx, &im).await?;
    tx.commit().await?;
    Ok(out)
}

/// `POST /imports/{id}/run`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportRunInput {
    /// The import's `lock_version` as the dry run saw it.
    pub lock_version: i64,
}

/// `POST /imports/{id}/run` (R-I10): the import must be `validated` with
/// the settings the dry run used. `operation_id` (the Idempotency-Key):
/// sending the request again starts nothing twice.
pub async fn run(
    app: &App,
    caller: AuthContext,
    id: &str,
    operation_id: Uuid,
    input: &ImportRunInput,
) -> Result<ImportStatus> {
    let (user, _) = caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, true).await?;
    let mut h = Sha256::new();
    h.update(im.id.as_bytes());
    h.update(input.lock_version.to_be_bytes());
    let fp = commands::fingerprint(ACTION_RUN, &h.finalize());
    let request = commands::open_request(&mut tx, user, operation_id, ACTION_RUN, &fp).await?;
    let Some(request) = request else {
        // The same request again: what the import is now.
        commands::stored_result(&mut tx, user, operation_id, ACTION_RUN, &fp).await?;
        let out = status_of(&mut tx, &im).await?;
        tx.commit().await?;
        return Ok(out);
    };
    if im.state != "validated" {
        return Err(state_invalid(&im.state));
    }
    if im.lock_version != input.lock_version {
        return Err(api(
            ErrorCode::ImportSettingsChanged,
            "the settings changed after the dry run; run it again",
        ));
    }
    queue_rows(&mut tx, &im).await?;
    sqlx::query(
        "UPDATE imports SET state = 'running', started_at = now(), operation_request_id = $2
         WHERE id = $1",
    )
    .bind(im.id)
    .bind(request)
    .execute(&mut *tx)
    .await?;
    commands::complete_request(&mut tx, request, &json!({"import_id": im.id})).await?;
    let im = own(&mut tx, caller, id, false).await?;
    let out = status_of(&mut tx, &im).await?;
    tx.commit().await?;
    Ok(out)
}

/// `POST /imports/{id}/cancel` (R-I10): the row being written finishes;
/// the rest is skipped. A dry run is simply dropped.
pub async fn cancel(app: &App, caller: AuthContext, id: &str) -> Result<ImportStatus> {
    let mut tx = app.pool.begin().await?;
    // The worker holds this row's lock while it writes a row.
    let im = own(&mut tx, caller, id, true).await?;
    match im.state.as_str() {
        "running" => {
            sqlx::query(
                "UPDATE import_rows SET state = 'skipped' WHERE import_id = $1 AND state = 'pending'",
            )
            .bind(im.id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE imports SET state = 'canceled', finished_at = now() WHERE id = $1")
                .bind(im.id)
                .execute(&mut *tx)
                .await?;
        }
        "validating" => {
            sqlx::query("DELETE FROM import_rows WHERE import_id = $1")
                .bind(im.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE imports SET state = 'ready' WHERE id = $1")
                .bind(im.id)
                .execute(&mut *tx)
                .await?;
        }
        s => return Err(state_invalid(s)),
    }
    let im = own(&mut tx, caller, id, false).await?;
    let out = status_of(&mut tx, &im).await?;
    tx.commit().await?;
    Ok(out)
}

const ROW_SELECT: &str =
    "SELECT r.row_no, r.state, r.error_code, r.error_field, k.key AS issue_key,
        r.time_entry_id
 FROM import_rows r
 LEFT JOIN issue_keys k ON k.issue_id = r.issue_id AND k.is_current";

/// `GET /imports/{id}/rows?state=&cursor=`: 100 a page, by row number.
pub async fn rows(
    app: &App,
    caller: AuthContext,
    id: &str,
    state: Option<&str>,
    cursor: Option<&str>,
) -> Result<ImportRows> {
    const PAGE: usize = 100;
    if let Some(s) = state
        && !matches!(s, "pending" | "ok" | "error" | "skipped")
    {
        return Err(bad("state", "pending, ok, error or skipped"));
    }
    let after: i32 = match cursor {
        None => 0,
        Some(c) => c
            .parse()
            .map_err(|_| api(ErrorCode::InvalidCursor, "not an import-rows cursor"))?,
    };
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, false).await?;
    let sql = format!(
        "{ROW_SELECT} WHERE r.import_id = $1 AND r.row_no > $2 AND ($3::text IS NULL OR r.state = $3)
         ORDER BY r.row_no LIMIT $4"
    );
    let mut items: Vec<ImportRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(im.id)
        .bind(after)
        .bind(state)
        .bind(PAGE as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let more = items.len() > PAGE;
    items.truncate(PAGE);
    Ok(ImportRows {
        next_cursor: more
            .then(|| items.last().map(|r| r.row_no.to_string()))
            .flatten(),
        items,
    })
}

/// `GET /imports/{id}/report.csv`: `row, status, key, error_code, field`.
/// The file name and the bytes (UTF-8 with BOM, CRLF).
pub async fn report_csv(app: &App, caller: AuthContext, id: &str) -> Result<(String, Vec<u8>)> {
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, false).await?;
    let sql = format!("{ROW_SELECT} WHERE r.import_id = $1 ORDER BY r.row_no");
    let items: Vec<ImportRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(im.id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut w = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(b"\xEF\xBB\xBF".to_vec());
    w.write_record(["row", "status", "key", "error_code", "field"])?;
    for r in &items {
        w.write_record([
            r.row_no.to_string().as_str(),
            r.state.as_str(),
            r.issue_key.as_deref().unwrap_or(""),
            r.error_code.as_deref().unwrap_or(""),
            r.error_field.as_deref().unwrap_or(""),
        ])?;
    }
    let bytes = w.into_inner().map_err(|e| anyhow::anyhow!("csv: {e}"))?;
    Ok((format!("{}-import-report.csv", im.project_key), bytes))
}

/// `DELETE /imports/{id}` (R-I13): the file and the settings go; what the
/// import created stays, and so does the record that it created it.
pub async fn delete(app: &App, caller: AuthContext, id: &str) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, true).await?;
    if matches!(im.state.as_str(), "validating" | "running") {
        return Err(state_invalid(&im.state));
    }
    let wrote: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM command_executions WHERE import_id = $1)")
            .bind(im.id)
            .fetch_one(&mut *tx)
            .await?;
    if wrote {
        sqlx::query(
            // Whether it notifies stays: its events may not be dispatched yet.
            "UPDATE imports SET file = NULL, purged_at = COALESCE(purged_at, now()),
                    settings = jsonb_build_object('options', jsonb_build_object(
                        'notify', COALESCE(settings->'options'->'notify', 'false'::jsonb)))
             WHERE id = $1",
        )
        .bind(im.id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("DELETE FROM imports WHERE id = $1")
            .bind(im.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- undo (R-I12)

/// `POST /imports/{id}/close-created`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportCloseInput {
    /// Issues: the closed status the created issues move to.
    #[serde(default)]
    pub status_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NotUndone {
    pub row_no: i32,
    pub issue_key: Option<String>,
    /// `changed_since` (someone wrote to it after the import), or the
    /// error code of the refused transition or deletion.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImportCloseResult {
    /// Issues closed, or time entries deleted, by this call.
    pub closed: u32,
    /// Already closed or deleted before.
    pub already: u32,
    pub failed: Vec<NotUndone>,
}

/// R-I12: every issue this import created that nothing else has written
/// to since moves to `status_id` (as the caller, by the workflow); every
/// time entry it logged is deleted. Within 30 days of the import's end.
pub async fn close_created(
    app: &App,
    caller: AuthContext,
    id: &str,
    input: &ImportCloseInput,
) -> Result<ImportCloseResult> {
    caller.require_human_session()?;
    let mut tx = app.pool.begin().await?;
    let im = own(&mut tx, caller, id, false).await?;
    // Sight first: out of the project, nothing about its issues is said.
    describe::visible_project(&mut tx, caller, &im.project_id.to_string()).await?;
    if !matches!(im.state.as_str(), "done" | "canceled" | "failed") {
        return Err(state_invalid(&im.state));
    }
    let fresh: bool = sqlx::query_scalar(
        "SELECT COALESCE(finished_at, created_at) > now() - make_interval(days => $2)
         FROM imports WHERE id = $1",
    )
    .bind(im.id)
    .bind(KEEP_DAYS)
    .fetch_one(&mut *tx)
    .await?;
    if !fresh {
        return Err(state_invalid("expired"));
    }
    let kind = im.kind();
    let quiet = !im.settings().options.notify;
    let status = match (kind, input.status_id) {
        (Kind::Issues, Some(s)) => {
            let closed: Option<bool> =
                sqlx::query_scalar("SELECT category = 'done' FROM issue_statuses WHERE id = $1")
                    .bind(s)
                    .fetch_optional(&mut *tx)
                    .await?;
            if closed != Some(true) {
                return Err(bad("status_id", "a closed status"));
            }
            Some(s)
        }
        (Kind::Issues, None) => return Err(bad("status_id", "a closed status")),
        (Kind::TimeEntries, _) => None,
    };
    let targets: Vec<(i32, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT row_no, issue_id, time_entry_id FROM import_rows
         WHERE import_id = $1 AND state = 'ok' ORDER BY row_no",
    )
    .bind(im.id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut out = ImportCloseResult {
        closed: 0,
        already: 0,
        failed: Vec::new(),
    };
    for (row_no, issue, entry) in targets {
        let mut tx = app.pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '10s'")
            .execute(&mut *tx)
            .await?;
        let mut key: Option<String> = None;
        let doc = if let (Some(issue), Some(status)) = (issue, status) {
            let row: Option<(i32, Uuid, String, bool)> = sqlx::query_as(
                "SELECT i.lock_version, i.status_id, k.key,
                        EXISTS (SELECT 1 FROM issue_events e
                                LEFT JOIN command_executions ce ON ce.id = e.command_execution_id
                                WHERE e.issue_id = i.id AND ce.import_id IS DISTINCT FROM $2)
                 FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
                 WHERE i.id = $1 AND i.archived_at IS NULL",
            )
            .bind(issue)
            .bind(im.id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((lv, current, k, touched)) = row else {
                out.failed.push(NotUndone {
                    row_no,
                    issue_key: None,
                    reason: "not_found".into(),
                });
                continue;
            };
            key = Some(k);
            if current == status {
                out.already += 1;
                continue;
            }
            if touched {
                out.failed.push(NotUndone {
                    row_no,
                    issue_key: key,
                    reason: "changed_since".into(),
                });
                continue;
            }
            let target = json!({"type": "issue", "id": issue});
            Some((
                derived_id(im.id, row_no, &format!("close:{lv}")),
                json!({"command_schema_version": 1,
                    "ops": [{"op_id": derived_id(im.id, row_no, "close-op"), "kind": "transition",
                             "target": target, "to_status_id": status}],
                    "preconditions": [{"target": target, "lock_version": lv}]}),
            ))
        } else if let Some(entry) = entry {
            let lv: Option<i32> = sqlx::query_scalar(
                "SELECT lock_version FROM time_entries WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(entry)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(lv) = lv else {
                out.already += 1;
                continue;
            };
            let target = json!({"type": "time_entry", "id": entry});
            Some((
                derived_id(im.id, row_no, &format!("delete:{lv}")),
                json!({"command_schema_version": 1,
                    "ops": [{"op_id": derived_id(im.id, row_no, "delete-op"),
                             "kind": "delete_time_entry", "target": target}],
                    "preconditions": [{"target": target, "lock_version": lv}]}),
            ))
        } else {
            None
        };
        let Some((operation, doc)) = doc else {
            continue;
        };
        match commands::run_for_import(&mut tx, caller, operation, &doc, im.id, quiet).await {
            Ok(_) => {
                tx.commit().await?;
                out.closed += 1;
            }
            Err(e) => {
                drop(tx);
                let Some(a) = classify(&e).filter(|a| !transient(a)) else {
                    return Err(e);
                };
                out.failed.push(NotUndone {
                    row_no,
                    issue_key: key,
                    reason: a.code.as_str().into(),
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- the worker (R-I8, R-I10)

fn lock_key(id: Uuid) -> i64 {
    let b = id.as_bytes();
    let mut k = [0u8; 8];
    k.copy_from_slice(&b[8..16]);
    i64::from_be_bytes(k) ^ 0x6f78_6c5f_696d_7031
}

/// The service cannot do it now (the pool, the connection): nothing about
/// the import or the row, so it is left for the next round.
fn transient(a: &ApiError) -> bool {
    a.code == ErrorCode::TemporarilyUnavailable
}

/// One import that waits for the worker, dry run or run, to its end (or
/// until it is canceled). `false`: nothing waited.
pub async fn work_round(app: &App) -> Result<bool> {
    match round(app, &BTreeSet::new()).await? {
        Some((_, worked)) => worked.map(|_| true),
        None => Ok(false),
    }
}

/// The first waiting import not in `skip`, with how its processing ended.
async fn round(app: &App, skip: &BTreeSet<Uuid>) -> Result<Option<(Uuid, Result<()>)>> {
    let waiting: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM imports WHERE state IN ('validating', 'running')
         ORDER BY created_at, id LIMIT 100",
    )
    .fetch_all(&app.pool)
    .await?;
    for id in waiting {
        if skip.contains(&id) {
            continue;
        }
        // One worker per import, across processes: a session lock on a
        // connection of its own, released whatever happens below.
        let mut lock = app.pool.acquire().await?;
        let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(lock_key(id))
            .fetch_one(&mut *lock)
            .await?;
        if !got {
            continue;
        }
        let worked = process(app, id).await;
        let unlocked = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(lock_key(id))
            .execute(&mut *lock)
            .await;
        if unlocked.is_err() {
            // A connection that may still hold the lock does not go back.
            lock.detach();
        }
        return Ok(Some((id, worked)));
    }
    Ok(None)
}

/// Every waiting import, until none is left (tests, and the worker loop).
/// An import that could not go on now is logged and left for the next
/// call: it does not keep the others waiting.
pub async fn work_all(app: &App) -> Result<usize> {
    let mut n = 0;
    let mut failed: BTreeSet<Uuid> = BTreeSet::new();
    while let Some((id, worked)) = round(app, &failed).await? {
        match worked {
            Ok(()) => n += 1,
            Err(e) => {
                tracing::warn!(import = %id, "import left for the next round: {e:#}");
                failed.insert(id);
            }
        }
    }
    Ok(n)
}

async fn fail(app: &App, id: Uuid, state_was: &str) -> Result<()> {
    let mut tx = app.pool.begin().await?;
    let done = sqlx::query(
        "UPDATE imports SET state = 'failed', finished_at = now() WHERE id = $1 AND state = $2",
    )
    .bind(id)
    .bind(state_was)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if done > 0 {
        sqlx::query(
            "UPDATE import_rows SET state = 'skipped' WHERE import_id = $1 AND state = 'pending'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

enum Step {
    Next,
    /// The import is no longer in the state the worker started in.
    Stop,
    /// The owner can no longer act at all: the import ends.
    Fail,
}

/// A row the engine could not handle for a reason that is neither the
/// row's nor the moment's (a defect): it gets an error of its own, so the
/// rows after it are not kept waiting.
async fn mark_internal(app: &App, id: Uuid, phase: &str, row_no: i32) -> Result<()> {
    sqlx::query(
        "UPDATE import_rows r SET state = 'error', error_code = 'internal_error'
         FROM imports im
         WHERE im.id = r.import_id AND im.state = $3 AND r.import_id = $1 AND r.row_no = $2
           AND r.state = 'pending'",
    )
    .bind(id)
    .bind(row_no)
    .bind(phase)
    .execute(&app.pool)
    .await?;
    Ok(())
}

struct RowDone {
    issue: Option<Uuid>,
    entry: Option<Uuid>,
    execution: Option<Uuid>,
}

async fn process(app: &App, id: Uuid) -> Result<()> {
    let sql = format!("{LOAD} WHERE im.id = $1");
    let Some(im): Option<Loaded> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&app.pool)
        .await?
    else {
        return Ok(());
    };
    let phase = im.state.clone();
    if !matches!(phase.as_str(), "validating" | "running") {
        return Ok(());
    }
    let owner = AuthContext::import_owner(im.owner_id);
    let kind = im.kind();
    let set = im.settings();
    // What cannot go on at all ends the import; each row's own problems
    // are the row's.
    let prepared = async {
        let mut tx = app.pool.begin().await?;
        let table = im.table(&mut tx).await?;
        let ctx = load_ctx(&mut tx, owner, im.project_id, kind).await?;
        let actor = perm::actor(&mut tx, owner).await?;
        let allowed = perm::project_perms(&mut tx, &actor, im.project_id)
            .await?
            .is_some_and(|pp| pp.status == "active" && pp.allowed_any(kind.permission()));
        tx.commit().await?;
        if !allowed {
            return Err(api(
                ErrorCode::ForbiddenOp,
                "the owner may no longer import here",
            ));
        }
        anyhow::Ok((table, ctx))
    }
    .await;
    let (table, ctx) = match prepared {
        Ok(x) => x,
        Err(e) if classify(&e).is_some_and(|a| !transient(&a)) => {
            tracing::warn!(import = %id, "import ended: {}", e.root_cause());
            return fail(app, id, &phase).await;
        }
        Err(e) => return Err(e),
    };
    let cells: HashMap<i32, &Vec<String>> = table.rows.iter().map(|r| (r.0, &r.1)).collect();
    let pending: Vec<i32> = sqlx::query_scalar(
        "SELECT row_no FROM import_rows WHERE import_id = $1 AND state = 'pending' ORDER BY row_no",
    )
    .bind(id)
    .fetch_all(&app.pool)
    .await?;
    for (worked, row_no) in pending.into_iter().enumerate() {
        let _ = worked;
        #[cfg(feature = "test-support")]
        {
            let stop = STOP_AFTER.load(std::sync::atomic::Ordering::Relaxed);
            if stop > 0 && worked >= stop {
                anyhow::bail!("stopped after {stop} rows (test)");
            }
        }
        let empty = Vec::new();
        let row = cells.get(&row_no).copied().unwrap_or(&empty);
        let mut attempt = 0;
        loop {
            match step(app, &im, &ctx, &set, owner, &phase, row_no, row).await {
                Ok(Step::Next) => break,
                Ok(Step::Stop) => return Ok(()),
                Ok(Step::Fail) => {
                    tracing::warn!(import = %id, "import ended: the owner can no longer act");
                    return fail(app, id, &phase).await;
                }
                Err(e)
                    if attempt < 3
                        && (commands::is_retryable_db(&e) || commands::is_lock_timeout(&e)) =>
                {
                    attempt += 1;
                }
                // Busy or unreachable: the next round goes on from here.
                Err(e)
                    if commands::is_retryable_db(&e)
                        || commands::is_lock_timeout(&e)
                        || classify(&e).is_some_and(|a| transient(&a))
                        || e.chain().any(|c| {
                            matches!(
                                c.downcast_ref::<sqlx::Error>(),
                                Some(
                                    sqlx::Error::Io(_)
                                        | sqlx::Error::PoolTimedOut
                                        | sqlx::Error::PoolClosed
                                )
                            )
                        }) =>
                {
                    return Err(e);
                }
                Err(e) => {
                    tracing::error!(import = %id, row = row_no, "import row failed: {e:#}");
                    mark_internal(app, id, &phase, row_no).await?;
                    break;
                }
            }
        }
    }
    // Every row has its result.
    let mut tx = app.pool.begin().await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM imports WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if state.as_deref() == Some(phase.as_str()) {
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM import_rows WHERE import_id = $1 AND state = 'pending'",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if left == 0 {
            let sql = if phase == "validating" {
                "UPDATE imports SET state = 'validated' WHERE id = $1"
            } else {
                "UPDATE imports SET state = 'done', finished_at = now() WHERE id = $1"
            };
            sqlx::query(sql).bind(id).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// One row, in one transaction (R-I10). The import's row lock is held
/// while the row is written: a cancel waits for it, and the next row sees
/// the new state.
#[allow(clippy::too_many_arguments)]
async fn step(
    app: &App,
    im: &Loaded,
    ctx: &Ctx,
    set: &ImportSettings,
    owner: AuthContext,
    phase: &str,
    row_no: i32,
    cells: &[String],
) -> Result<Step> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    // The state and the settings the worker started with: canceled, or
    // set up again and started anew, this run of it is over.
    let now: Option<(String, i64)> =
        sqlx::query_as("SELECT state, lock_version FROM imports WHERE id = $1 FOR UPDATE")
            .bind(im.id)
            .fetch_optional(&mut *tx)
            .await?;
    if now.as_ref().map(|n| (n.0.as_str(), n.1)) != Some((phase, im.lock_version)) {
        return Ok(Step::Stop);
    }
    let pending: Option<bool> = sqlx::query_scalar(
        "SELECT state = 'pending' FROM import_rows WHERE import_id = $1 AND row_no = $2 FOR UPDATE",
    )
    .bind(im.id)
    .bind(row_no)
    .fetch_optional(&mut *tx)
    .await?;
    if pending != Some(true) {
        tx.commit().await?;
        return Ok(Step::Next);
    }
    let result: std::result::Result<RowDone, RowError> =
        match build_doc(ctx, set, im.id, row_no, cells) {
            Err(e) => Err(e),
            Ok(doc) => {
                // The row's writes are one savepoint: a refused row leaves
                // nothing behind but its result.
                let mut sp = sqlx::Connection::begin(&mut *tx).await?;
                let r = if phase == "validating" {
                    commands::check_as(&mut sp, owner, &doc)
                        .await
                        .map(|_| RowDone {
                            issue: None,
                            entry: None,
                            execution: None,
                        })
                } else {
                    write_row(&mut sp, ctx, set, owner, im.id, row_no, &doc).await
                };
                match r {
                    Ok(done) => {
                        if phase == "validating" {
                            sp.rollback().await?;
                        } else {
                            sp.commit().await?;
                        }
                        Ok(done)
                    }
                    Err(e) => {
                        if commands::is_retryable_db(&e) || commands::is_lock_timeout(&e) {
                            return Err(e);
                        }
                        let Some(a) = classify(&e).filter(|a| !transient(a)) else {
                            return Err(e);
                        };
                        // Locked or gone: no row of theirs can be written.
                        if a.code == ErrorCode::Unauthenticated {
                            return Ok(Step::Fail);
                        }
                        sp.rollback().await?;
                        Err(engine_error(ctx.kind, &a))
                    }
                }
            }
        };
    let (state, code, field, done) = match result {
        Ok(d) => ("ok", None, None, Some(d)),
        Err(e) => ("error", Some(e.code), e.field, None),
    };
    sqlx::query(
        "UPDATE import_rows SET state = $3, error_code = $4, error_field = $5, issue_id = $6,
                                time_entry_id = $7, execution_id = $8
         WHERE import_id = $1 AND row_no = $2",
    )
    .bind(im.id)
    .bind(row_no)
    .bind(state)
    .bind(code)
    .bind(field)
    .bind(done.as_ref().and_then(|d| d.issue))
    .bind(done.as_ref().and_then(|d| d.entry))
    .bind(done.as_ref().and_then(|d| d.execution))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Step::Next)
}

/// R-I9: the row's document, and (issues, `skip_triage`) the decision that
/// takes the new issue out of the Triage inbox.
async fn write_row(
    conn: &mut PgConnection,
    ctx: &Ctx,
    set: &ImportSettings,
    owner: AuthContext,
    import: Uuid,
    row_no: i32,
    doc: &Json,
) -> Result<RowDone> {
    let quiet = !set.options.notify;
    let r = commands::run_for_import(
        &mut *conn,
        owner,
        derived_id(import, row_no, "row"),
        doc,
        import,
        quiet,
    )
    .await?;
    let created = r.effects.iter().find(|e| e.created);
    let done = RowDone {
        issue: created.map(|e| e.issue_id),
        entry: r.time_entries.first().map(|t| t.id),
        execution: Some(r.execution_id),
    };
    if let (Kind::Issues, true, Some(e)) = (ctx.kind, set.options.skip_triage, created) {
        let waiting: Option<bool> =
            sqlx::query_scalar("SELECT state = 'pending' FROM issue_triage WHERE issue_id = $1")
                .bind(e.issue_id)
                .fetch_optional(&mut *conn)
                .await?;
        if waiting == Some(true) {
            let target = json!({"type": "issue", "id": e.issue_id});
            let accept = json!({"command_schema_version": 1,
                "ops": [{"op_id": derived_id(import, row_no, "triage-op"), "kind": "triage_decide",
                         "target": target, "decision": "accept"}],
                "preconditions": [{"target": target, "lock_version": e.lock_version}]});
            let mut sp = sqlx::Connection::begin(&mut *conn).await?;
            let decided = commands::run_for_import(
                &mut sp,
                owner,
                derived_id(import, row_no, "triage"),
                &accept,
                import,
                quiet,
            )
            .await;
            match decided {
                Ok(_) => sp.commit().await?,
                // Without `triage_issues` the issue simply waits in the
                // inbox like any other new one.
                Err(e) if classify(&e).is_some_and(|a| a.code == ErrorCode::ForbiddenOp) => {
                    sp.rollback().await?
                }
                Err(e) => return Err(e),
            }
        }
    }
    let _ = ctx.owner;
    Ok(done)
}

// ---------------------------------------------------------------- retention (R-I13)

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Purged {
    pub files: u64,
    pub expired: u64,
    pub rows: u64,
}

/// R-I13: the file goes 24 hours after the import ended; an import nobody
/// ran expires after 7 days; row results go after 30 days.
pub async fn purge(pool: &sqlx::PgPool) -> Result<Purged> {
    let files = sqlx::query(
        "UPDATE imports SET file = NULL, purged_at = now()
         WHERE file IS NOT NULL AND state IN ('done', 'canceled', 'failed', 'expired')
           AND COALESCE(finished_at, created_at) < now() - interval '24 hours'",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let expired = sqlx::query(
        "UPDATE imports SET state = 'expired', file = NULL, purged_at = now(), finished_at = now()
         WHERE state IN ('uploaded', 'ready', 'validated')
           AND created_at < now() - interval '7 days'",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let rows = sqlx::query(
        "DELETE FROM import_rows r USING imports im
         WHERE im.id = r.import_id AND im.state IN ('done', 'canceled', 'failed', 'expired')
           AND COALESCE(im.finished_at, im.created_at) < now() - make_interval(days => $1)",
    )
    .bind(KEEP_DAYS)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(Purged {
        files,
        expired,
        rows,
    })
}

/// D12 (design 06 §3.13): a redaction in the project drops every file an
/// import of it still keeps (the rows hold no content).
pub(crate) async fn redact_project(conn: &mut PgConnection, project: Uuid) -> Result<u64> {
    // An import at work ends here: its worker stops at the next row.
    sqlx::query(
        "UPDATE import_rows r SET state = 'skipped' FROM imports im
         WHERE im.id = r.import_id AND im.project_id = $1 AND im.file IS NOT NULL
           AND im.state IN ('validating', 'running') AND r.state = 'pending'",
    )
    .bind(project)
    .execute(&mut *conn)
    .await?;
    // The values mapped by hand are cells of the file too.
    Ok(sqlx::query(
        "UPDATE imports SET file = NULL, purged_at = now(), settings = settings - 'value_map',
                finished_at = CASE WHEN state IN ('validating', 'running') THEN now()
                                   ELSE finished_at END,
                state = CASE WHEN state IN ('validating', 'running') THEN 'failed' ELSE state END
         WHERE project_id = $1 AND file IS NOT NULL",
    )
    .bind(project)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        let utf8 = "標題,狀態\n登入失敗,新建\n".as_bytes();
        assert_eq!(decode(utf8, None).unwrap().1.name, "utf-8");
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend(utf8);
        let (text, enc) = decode(&bom, None).unwrap();
        assert!(text.starts_with("標題") && !enc.guessed);
        let mut le = b"\xFF\xFE".to_vec();
        for u in "標題,狀態\n".encode_utf16() {
            le.extend(u.to_le_bytes());
        }
        let (text, enc) = decode(&le, None).unwrap();
        assert_eq!((text.as_str(), enc.name), ("標題,狀態\n", "utf-16le"));
        let (big5, _, _) = encoding_rs::BIG5.encode("標題,狀態\n登入失敗,新建\n");
        let (text, enc) = decode(&big5, None).unwrap();
        assert!(text.starts_with("標題,狀態") && enc.name == "big5" && enc.guessed);
        let junk = b"a,b\n\xff\xfe\xfd\x80\x80,x\n";
        let e = classify(&decode(junk, None).unwrap_err()).unwrap();
        assert_eq!(e.code, ErrorCode::ImportInvalidFile);
        assert_eq!(e.details.unwrap()["kind"], "encoding");
    }

    #[test]
    fn delimiters_and_rows() {
        assert_eq!(detect_delimiter("a;b;c\n1,2;3\n"), ';');
        assert_eq!(detect_delimiter("a\tb\n"), '\t');
        assert_eq!(detect_delimiter("\"a;b;c\",d\n"), ',');
        let t = parse("a;b\n\"line one\nline two\";2\n;\nlast\n", ';').unwrap();
        assert_eq!(t.header, ["a", "b"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(
            t.rows[0],
            (1, vec!["line one\nline two".into(), "2".into()])
        );
        // The empty row is counted but not kept; a short row is padded.
        assert_eq!(t.rows[1], (3, vec!["last".into(), String::new()]));
        let kind =
            |r: Result<Table>| classify(&r.unwrap_err()).unwrap().details.unwrap()["kind"].clone();
        assert_eq!(kind(parse("", ',')), "empty");
        assert_eq!(kind(parse("a,b\n", ',')), "empty");
        assert_eq!(kind(parse(",\n1,2\n", ',')), "no_header");
        // A line with nothing on it is a row too (R-I2: rows as in the file),
        // also after a cell that spans lines.
        let t = parse("a\n1\n\n\n2\n\"x\ny\"\n\n3\n", ',').unwrap();
        let nos: Vec<i32> = t.rows.iter().map(|r| r.0).collect();
        assert_eq!(nos, [1, 4, 5, 7]);
        let t = parse("a\r\n1\r\n\r\n2\r\n", ',').unwrap();
        assert_eq!(t.rows[1].0, 3);
        // UTF-16 without a BOM is not UTF-8 text.
        let nul = classify(&decode(b"a\0,\0b\0\n\0", None).unwrap_err()).unwrap();
        assert_eq!(nul.details.unwrap()["kind"], "encoding");
        let wide = vec!["x"; MAX_COLUMNS + 1].join(",");
        assert_eq!(
            kind(parse(&format!("{wide}\n{wide}\n"), ',')),
            "too_many_columns"
        );
    }

    #[test]
    fn values() {
        assert_eq!(date("2026-11-05", "ymd").as_deref(), Some("2026-11-05"));
        assert_eq!(date("2026/1/5", "ymd").as_deref(), Some("2026-01-05"));
        assert_eq!(date("11/05/2026", "mdy").as_deref(), Some("2026-11-05"));
        assert_eq!(date("05.11.2026", "dmy").as_deref(), Some("2026-11-05"));
        assert_eq!(date("2026-13-01", "ymd"), None);
        assert_eq!(date("yesterday", "ymd"), None);
        assert_eq!(boolean("是"), Some(true));
        assert_eq!(boolean("No"), Some(false));
        assert_eq!(boolean("maybe"), None);
        assert_eq!(cell("'=1+1", true), "=1+1");
        assert_eq!(cell("'=1+1", false), "'=1+1");
        assert_eq!(cell("'quoted'", true), "'quoted'");
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = vec![
            (a, vec!["amy".into(), "amy chen".into()]),
            (b, vec!["amy chen".into()]),
        ];
        assert_eq!(pick(&c, "AMY"), Ok(a));
        assert_eq!(pick(&c, "Amy Chen"), Err("ambiguous_value"));
        assert_eq!(pick(&c, "zed"), Err("unknown_value"));
        assert_eq!(pick(&c, &b.to_string()), Ok(b));
        assert_eq!(pick(&c, &Uuid::now_v7().to_string()), Err("unknown_value"));
        let i = Uuid::now_v7();
        assert_eq!(derived_id(i, 7, "row"), derived_id(i, 7, "row"));
        assert_ne!(derived_id(i, 7, "row"), derived_id(i, 8, "row"));
        assert_eq!(derived_id(i, 7, "row").get_version_num(), 8);
    }
}
