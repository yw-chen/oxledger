// SPDX-License-Identifier: GPL-2.0-or-later
//! The wiki search index and `GET /wiki/search` (design 06 §3.14, M2.3).
//!
//! * Its own tantivy index, one document per live page: the current
//!   version's title (×3.0) and the plain text of its body (×1.0, the
//!   Markdown extraction of R-M11), with the project as a filter term. The
//!   tokenizer and the query side (D15 weights) are the issue index's
//!   ([`crate::search`]); the issue index's layout does not change.
//! * It follows the outbox like the issue index, with the same generation
//!   machinery (0018, B-49): consumer `wiki@<generation>`, the same states,
//!   lease, blue-green rebuild, health check, payload and `CURRENT` (under
//!   `<index dir>/wiki`). It reads **every** outbox row and moves its
//!   cursor over the ones that are not `wiki_page` (as the design asks of
//!   it), so it never holds back the outbox GC. The coordination SQL and
//!   the frontier waiters are shared with the issue index
//!   ([`crate::search::mark_active_consumer`],
//!   [`crate::search::FrontierGate`] …); the document side is this file's
//!   (decisions.tsv M2.3).
//! * A search filters on the wikis the caller reads (R-W1,
//!   [`crate::wiki::readable_wikis`]) inside the index, then re-checks the
//!   top hits in SQL: the page is live, its project still readable, and its
//!   current title or text still matches (a hit on text the index has not
//!   caught up with, e.g. a redacted version, is dropped). Snippets come
//!   from the current text only, as `[{text, hit}]` parts (no HTML).
//! * `consistent_after` waits for this index's frontier (write receipts of
//!   wiki writes carry the same `c1.<epoch>.<xid>` token).

// Row tuples straight from sqlx.
#![allow(clippy::type_complexity)]

use crate::auth::AuthContext;
use crate::cursor::{self, Position};
use crate::error::{ApiError, ErrorCode};
use crate::evidence::{self, CpText, Terms};
use crate::search::{
    self, ANALYZER_VERSION, BASELINE_PAGE, Cursor, DURABLE_INTERVAL, FrontierGate, GenState,
    HEALTH_SAMPLE, Payload, RebuildReport, SYNC_BATCH, TOKENIZER, Wait, lock, switch_ready,
};
use crate::service::{App, Consistency, LAG_RETRY_MS, read_tx, search_unavailable};
use crate::wiki::{self, WikiProjectRef};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, ConstScoreQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};
use uuid::Uuid;

/// Payload `schema_version` of a wiki generation (its own numbering).
pub const WIKI_SCHEMA_VERSION: u32 = 1;
/// Payload `query_semantics_version` of a wiki generation.
pub const WIKI_QUERY_SEMANTICS_VERSION: u32 = 1;
/// Field weights (design 06 §3.14).
pub const TITLE_WEIGHT: f32 = 3.0;
pub const BODY_WEIGHT: f32 = 1.0;
/// Consumer names of wiki index generations (B-49).
const CONSUMER_PREFIX: &str = "wiki@";
/// The outbox topic of wiki pages (0034, R-W11).
const TOPIC: &str = "wiki_page";
/// Results per page of `GET /wiki/search`.
pub const PAGE_SIZE: usize = 20;
/// Index candidates asked for first, and the most a page asks for.
const FIRST_CANDIDATES: usize = 100;
const MAX_CANDIDATES: usize = 6400;
/// Snippet length in code points (about one line).
const SNIPPET_CHARS: usize = 80;

fn versions() -> (u32, u32, u32) {
    (
        WIKI_SCHEMA_VERSION,
        ANALYZER_VERSION,
        WIKI_QUERY_SEMANTICS_VERSION,
    )
}

/// The text a page's body is indexed and matched as (R-M11).
pub fn plain_body(body: &str) -> String {
    crate::markdown::plain_text(body, crate::commands::TextFormat::Markdown)
}

/// sha256("w1" ‖ len ‖ title ‖ len ‖ plain body), lengths as big-endian
/// u64 byte counts: what a document was built from (health check).
pub fn page_rev(title: &str, body: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"w1");
    h.update((title.len() as u64).to_be_bytes());
    h.update(title.as_bytes());
    h.update((body.len() as u64).to_be_bytes());
    h.update(body.as_bytes());
    h.finalize().into()
}

// ---------------------------------------------------------------- generation

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    project: Field,
    title: Field,
    body: Field,
    rev: Field,
}

impl Fields {
    fn build() -> (Schema, Fields) {
        let mut sb = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let f = Fields {
            id: sb.add_text_field("id", STRING | STORED),
            project: sb.add_text_field("project", STRING),
            title: sb.add_text_field("title", text.clone()),
            body: sb.add_text_field("body", text),
            rev: sb.add_bytes_field("rev", STORED),
        };
        (sb.build(), f)
    }
}

/// A live page as the index holds it: the body already as plain text.
#[derive(sqlx::FromRow)]
struct PageDoc {
    id: Uuid,
    project_id: Uuid,
    title: String,
    body: String,
}

impl PageDoc {
    fn plain(mut self) -> PageDoc {
        self.body = plain_body(&self.body);
        self
    }
}

const DOC_SELECT: &str = "SELECT p.id, p.project_id, p.title, v.body
 FROM wiki_pages p JOIN wiki_page_versions v ON v.page_id = p.id AND v.version = p.current_version";

struct Gen {
    id: Uuid,
    dir: Option<PathBuf>,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    f: Fields,
    st: Mutex<GenState>,
}

impl Gen {
    fn consumer(&self) -> String {
        format!("{CONSUMER_PREFIX}{}", self.id)
    }

    fn create(root: Option<&Path>, epoch: Uuid) -> Result<Gen> {
        let id = Uuid::now_v7();
        let (schema, f) = Fields::build();
        let (index, dir) = match root {
            None => (Index::create_in_ram(schema), None),
            Some(r) => {
                let d = r.join(format!("gen-{id}"));
                std::fs::create_dir_all(&d)?;
                (Index::create_in_dir(&d, schema)?, Some(d))
            }
        };
        Gen::wrap(
            id,
            dir,
            index,
            f,
            GenState {
                epoch,
                cursor: Cursor::default(),
                f: 0,
                durable_f: 0,
                durable_at: Instant::now(),
            },
        )
    }

    fn wrap(id: Uuid, dir: Option<PathBuf>, index: Index, f: Fields, st: GenState) -> Result<Gen> {
        search::register_tokenizer(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = Mutex::new(index.writer(15_000_000)?);
        Ok(Gen {
            id,
            dir,
            reader,
            writer,
            f,
            st: Mutex::new(st),
        })
    }

    /// `root/CURRENT`'s generation if its payload and schema are this
    /// build's (its epoch is checked against PostgreSQL before it serves).
    fn open_current(root: &Path) -> Result<Option<Gen>> {
        let Ok(name) = std::fs::read_to_string(root.join("CURRENT")) else {
            return Ok(None);
        };
        let name = name.trim();
        let Some(id) = name
            .strip_prefix("gen-")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            tracing::warn!("wiki index CURRENT is not a generation: {name:?}");
            return Ok(None);
        };
        let dir = root.join(name);
        let index = match Index::open_in_dir(&dir) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("wiki generation {id} does not open: {e:#}");
                return Ok(None);
            }
        };
        let payload = index.load_metas()?.payload;
        let Some((p, v, cursor, fr)) = payload.as_deref().and_then(Payload::parse_raw) else {
            tracing::warn!("wiki generation {id}: payload missing or unsupported; rebuilding");
            return Ok(None);
        };
        let (want, f) = Fields::build();
        if v != versions() || index.schema() != want || p.generation != id {
            tracing::warn!("wiki generation {id} is of another version; rebuilding");
            return Ok(None);
        }
        Gen::wrap(
            id,
            Some(dir),
            index,
            f,
            GenState {
                epoch: p.data_epoch,
                cursor,
                f: fr,
                durable_f: fr,
                durable_at: Instant::now(),
            },
        )
        .map(Some)
    }

    /// Commits pending writes with the payload; on failure the writer rolls
    /// back (the durable state is the last payload).
    fn commit(&self, epoch: Uuid, c: Cursor, f: u64) -> Result<()> {
        let payload = serde_json::to_string(&Payload::new(epoch, self.id, versions(), c, f))?;
        let mut w = lock(&self.writer)?;
        let r = (|| -> Result<()> {
            let mut prep = w.prepare_commit()?;
            prep.set_payload(&payload);
            prep.commit()?;
            Ok(())
        })();
        if let Err(e) = r {
            let _ = w.rollback();
            return Err(e);
        }
        Ok(())
    }

    fn make_doc(&self, d: &PageDoc) -> TantivyDocument {
        let f = self.f;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.id, d.id.to_string());
        doc.add_text(f.project, d.project_id.to_string());
        doc.add_text(f.title, &d.title);
        doc.add_text(f.body, &d.body);
        doc.add_bytes(f.rev, page_rev(&d.title, &d.body).as_slice());
        doc
    }

    fn view(&self, f: u64) -> Arc<WikiView> {
        Arc::new(WikiView {
            searcher: self.reader.searcher(),
            f,
            fields: self.f,
            generation: self.id,
        })
    }
}

/// What one search reads: a reader pinned with the frontier it was
/// published with.
pub struct WikiView {
    searcher: Searcher,
    f: u64,
    fields: Fields,
    generation: Uuid,
}

#[derive(Debug, Clone, Copy)]
pub struct WikiHit {
    pub page_id: Uuid,
    pub score: f32,
}

impl WikiView {
    pub fn frontier(&self) -> u64 {
        self.f
    }

    pub fn generation(&self) -> Uuid {
        self.generation
    }

    pub fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }

    /// BM25 over title (×3) and body (×1) with weighted query terms
    /// ([`crate::search::query_terms`]), restricted to `projects`.
    pub fn search(
        &self,
        terms: &[(String, f32)],
        projects: &[Uuid],
        limit: usize,
    ) -> Result<Vec<WikiHit>> {
        if terms.is_empty() || projects.is_empty() {
            return Ok(Vec::new());
        }
        let f = self.fields;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (t, w) in terms {
            for (field, boost) in [(f.title, TITLE_WEIGHT), (f.body, BODY_WEIGHT)] {
                let tq = TermQuery::new(
                    Term::from_field_text(field, t),
                    IndexRecordOption::WithFreqs,
                );
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(tq), boost * w)),
                ));
            }
        }
        let allowed: Box<dyn Query> = Box::new(BooleanQuery::new(
            projects
                .iter()
                .map(|p| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(f.project, &p.to_string()),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect(),
        ));
        // The filter must not change BM25 scores.
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>,
            ),
            (Occur::Must, Box::new(ConstScoreQuery::new(allowed, 0.0))),
        ]);
        let top = self
            .searcher
            .search(&query, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = self.searcher.doc(addr)?;
            if let Some(id) = d.get_first(f.id).and_then(|v| v.as_str()) {
                hits.push(WikiHit {
                    page_id: id.parse()?,
                    score,
                });
            }
        }
        Ok(hits)
    }

    /// Does exactly one document for `d` carry its project and `rev`?
    fn holds(&self, d: &PageDoc) -> Result<bool> {
        let f = self.fields;
        let q = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.id, &d.id.to_string()),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.project, &d.project_id.to_string()),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let top = self
            .searcher
            .search(&q, &TopDocs::with_limit(2).order_by_score())?;
        let [(_, addr)] = top.as_slice() else {
            return Ok(false);
        };
        let doc: TantivyDocument = self.searcher.doc(*addr)?;
        let rev = doc
            .get_first(f.rev)
            .and_then(|v| v.as_bytes())
            .and_then(|b| <[u8; 32]>::try_from(b).ok());
        Ok(rev == Some(page_rev(&d.title, &d.body)))
    }
}

/// Outbox rows a round reads: (id, xid8 as text, entity id, topic).
type Events = sqlx::types::Json<Vec<(i64, String, Uuid, String)>>;

/// The wiki index (design 06 §3.14): generations, frontier and waiters as
/// [`crate::search::SearchIndex`], over wiki pages.
pub struct WikiIndex {
    root: Option<PathBuf>,
    active: RwLock<Option<Arc<Gen>>>,
    loaded: Mutex<Option<Arc<Gen>>>,
    pending: Mutex<Option<Arc<Gen>>>,
    view: RwLock<Option<Arc<WikiView>>>,
    gate: FrontierGate,
    /// One round at a time; the switch also runs under it.
    round: tokio::sync::Mutex<()>,
    rebuilding: tokio::sync::Mutex<()>,
    durable_interval: Mutex<Duration>,
    /// Rounds, switches, consistency outcomes and the stall check
    /// (`search_unavailable`), as for the issue index.
    pub metrics: crate::metrics::SearchMetrics,
}

impl WikiIndex {
    /// Opens the index under `dir` (`None`: in memory, for tests). Nothing
    /// serves until the first sync round validated or built a generation.
    pub fn open(dir: Option<&Path>) -> Result<Self> {
        let loaded = match dir {
            None => None,
            Some(d) => {
                std::fs::create_dir_all(d)?;
                Gen::open_current(d)?.map(Arc::new)
            }
        };
        Ok(WikiIndex {
            root: dir.map(Path::to_path_buf),
            active: RwLock::new(None),
            loaded: Mutex::new(loaded),
            pending: Mutex::new(None),
            view: RwLock::new(None),
            gate: FrontierGate::default(),
            round: tokio::sync::Mutex::new(()),
            rebuilding: tokio::sync::Mutex::new(()),
            durable_interval: Mutex::new(DURABLE_INTERVAL),
            metrics: Default::default(),
        })
    }

    /// Tests: how often frontier-only progress is made durable.
    pub fn set_durable_interval(&self, d: Duration) {
        if let Ok(mut g) = self.durable_interval.lock() {
            *g = d;
        }
    }

    /// The published frontier F.
    pub fn frontier(&self) -> u64 {
        self.gate.frontier()
    }

    pub fn view(&self) -> Option<Arc<WikiView>> {
        self.view.read().ok().and_then(|v| v.clone())
    }

    /// The serving generation's id.
    pub fn generation(&self) -> Option<Uuid> {
        self.active
            .read()
            .ok()
            .and_then(|a| a.as_ref().map(|g| g.id))
    }

    /// Durable (cursor, F) of the serving generation.
    pub fn durable_state(&self) -> Option<(Cursor, u64)> {
        let g = self.active.read().ok()?.clone()?;
        let st = g.st.lock().ok()?;
        Some((st.cursor, st.durable_f))
    }

    /// Waits at most `wait` until `x < F` (03 §4.4), with the same waiter
    /// limits as the issue index (its own slots).
    pub async fn wait_for(&self, who: Option<Uuid>, x: u64, wait: Duration) -> Wait {
        self.gate.wait_for(who, x, wait).await
    }

    /// Tests: waiters currently holding a slot.
    pub fn waiting(&self) -> usize {
        self.gate.waiting()
    }

    fn publish(&self, g: &Gen, f: u64) -> Result<()> {
        let f = f.max(self.frontier());
        let v = g.view(f);
        *self
            .view
            .write()
            .map_err(|_| anyhow::anyhow!("wiki view lock poisoned"))? = Some(v);
        self.gate.advance(f);
        self.metrics.published(f);
        Ok(())
    }

    /// Tests: sync until every outbox row written so far (any topic) is
    /// passed (see `SearchIndex::sync_all`).
    #[cfg(feature = "test-support")]
    pub async fn sync_all(&self, pool: &PgPool) -> Result<usize> {
        let target: Option<(String, i64)> =
            sqlx::query_as("SELECT xid::text, id FROM outbox ORDER BY xid DESC, id DESC LIMIT 1")
                .fetch_optional(pool)
                .await?;
        let mut applied = 0;
        for _ in 0..1000 {
            applied += self.sync(pool).await?;
            let Some((xid, id)) = &target else {
                return Ok(applied);
            };
            let want = Cursor {
                xid: xid.parse()?,
                id: *id,
            };
            let at = self
                .active
                .read()
                .ok()
                .and_then(|a| a.clone())
                .and_then(|g| g.st.lock().ok().map(|s| (s.cursor, s.f)));
            if let Some((c, f)) = at
                && (c >= want || want.xid < f)
            {
                return Ok(applied);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!(
            "{}",
            crate::metrics::horizon_timeout_message(pool, "wiki index").await
        )
    }

    /// Rounds until the outbox below H is drained.
    pub async fn sync(&self, pool: &PgPool) -> Result<usize> {
        let mut applied = 0;
        loop {
            let (n, more) = self.sync_round(pool).await?;
            applied += n;
            if !more {
                return Ok(applied);
            }
        }
    }

    /// One round (03 §4.2) for the serving generation and a generation
    /// being rebuilt: (rows passed, a full batch so more may follow).
    pub async fn sync_round(&self, pool: &PgPool) -> Result<(usize, bool)> {
        let _round = self.round.lock().await;
        let started = Instant::now();
        let r = self.sync_round_locked(pool).await;
        self.metrics.round(started.elapsed(), r.is_ok());
        r
    }

    async fn sync_round_locked(&self, pool: &PgPool) -> Result<(usize, bool)> {
        let g = match self.active.read().ok().and_then(|a| a.clone()) {
            Some(g) => g,
            None => self.activate(pool).await?,
        };
        let r = self.round_on(pool, &g, true).await?;
        let pending = lock(&self.pending)?.clone();
        if let Some(p) = pending
            && let Err(e) = self.round_on(pool, &p, false).await
        {
            tracing::warn!("wiki rebuild catch-up failed: {e:#}");
        }
        Ok(r)
    }

    async fn activate(&self, pool: &PgPool) -> Result<Arc<Gen>> {
        let epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
            .fetch_one(pool)
            .await?;
        let loaded = lock(&self.loaded)?.clone();
        let g = match loaded {
            Some(g) if lock(&g.st)?.epoch == epoch => g,
            Some(g) => {
                tracing::warn!(
                    "wiki generation {} belongs to another data_epoch; rebuilding",
                    g.id
                );
                Arc::new(self.build_baseline(pool).await?)
            }
            None => Arc::new(self.build_baseline(pool).await?),
        };
        let (cursor, f) = {
            let st = lock(&g.st)?;
            (st.cursor, st.f)
        };
        let ready = async {
            self.mark_active(pool, &g, cursor).await?;
            if let Some(root) = &self.root {
                search::write_current(root, g.id)?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = ready {
            *lock(&self.loaded)? = Some(g);
            return Err(e);
        }
        *lock(&self.loaded)? = None;
        *self
            .active
            .write()
            .map_err(|_| anyhow::anyhow!("wiki active lock poisoned"))? = Some(g.clone());
        self.publish(&g, f)?;
        Ok(g)
    }

    async fn mark_active(&self, pool: &PgPool, g: &Gen, c: Cursor) -> Result<()> {
        let pending = lock(&self.pending)?.as_ref().map(|p| p.consumer());
        search::mark_active_consumer(pool, CONSUMER_PREFIX, &g.consumer(), pending, c).await
    }

    /// One 03 §4.2 round on `g`. Every outbox row moves the cursor; only
    /// `wiki_page` rows touch documents.
    async fn round_on(&self, pool: &PgPool, g: &Gen, serve: bool) -> Result<(usize, bool)> {
        let (epoch, cursor, f_old, durable_f, durable_at) = {
            let st = lock(&g.st)?;
            (st.epoch, st.cursor, st.f, st.durable_f, st.durable_at)
        };
        let (h, events, db_epoch): (String, Events, Uuid) = sqlx::query_as(
            "WITH h AS MATERIALIZED (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS h),
                  ev AS (SELECT o.id, o.xid, o.entity_id, o.topic FROM outbox o, h
                         WHERE o.xid < h.h AND (o.xid, o.id) > ($1::text::xid8, $2::bigint)
                         ORDER BY o.xid, o.id LIMIT $3)
             SELECT h.h::text,
                    COALESCE(json_agg(json_build_array(ev.id, ev.xid::text, ev.entity_id, ev.topic)
                                      ORDER BY ev.xid, ev.id)
                             FILTER (WHERE ev.id IS NOT NULL), '[]'::json),
                    (SELECT data_epoch FROM search_settings)
             FROM h LEFT JOIN ev ON true GROUP BY h.h",
        )
        .bind(cursor.xid.to_string())
        .bind(cursor.id)
        .bind(SYNC_BATCH)
        .fetch_one(pool)
        .await?;
        if db_epoch != epoch {
            anyhow::bail!("search data_epoch changed; restart to rebuild the wiki index");
        }
        let rows = events.0;
        let h: u64 = h.parse().context("snapshot xmin")?;
        let Some((last_id, last_xid, _, _)) = rows.last().cloned() else {
            let f_new = f_old.max(h);
            let interval = *lock(&self.durable_interval)?;
            if f_new > durable_f && durable_at.elapsed() >= interval {
                g.commit(epoch, cursor, f_new)?;
                let mut st = lock(&g.st)?;
                st.durable_f = f_new;
                st.durable_at = Instant::now();
            }
            lock(&g.st)?.f = f_new;
            if serve {
                g.reader.reload()?;
                self.publish(g, f_new)?;
            }
            return Ok((0, false));
        };
        let mut ids: Vec<Uuid> = rows.iter().filter(|r| r.3 == TOPIC).map(|r| r.2).collect();
        ids.sort();
        ids.dedup();
        if !ids.is_empty() {
            let docs: Vec<PageDoc> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{DOC_SELECT} WHERE p.id = ANY($1) AND p.deleted_at IS NULL"
            )))
            .bind(&ids)
            .fetch_all(pool)
            .await?;
            let w = lock(&g.writer)?;
            for id in &ids {
                w.delete_term(Term::from_field_text(g.f.id, &id.to_string()));
            }
            for d in docs {
                w.add_document(g.make_doc(&d.plain()))?;
            }
        }
        let full = rows.len() as i64 == SYNC_BATCH;
        let f_cand = if full {
            last_xid.parse().context("outbox xid")?
        } else {
            h
        };
        let f_new = f_old.max(f_cand);
        let c_new = Cursor {
            xid: last_xid.parse().context("outbox xid")?,
            id: last_id,
        };
        g.commit(epoch, c_new, f_new)?;
        {
            let mut st = lock(&g.st)?;
            st.cursor = c_new;
            st.f = f_new;
            st.durable_f = f_new;
            st.durable_at = Instant::now();
        }
        g.reader.reload()?;
        sqlx::query(
            "UPDATE outbox_cursors SET last_xid = $2::text::xid8, last_id = $3, updated_at = now()
             WHERE consumer = $1",
        )
        .bind(g.consumer())
        .bind(c_new.xid.to_string())
        .bind(c_new.id)
        .execute(pool)
        .await?;
        if serve {
            self.publish(g, f_new)?;
        }
        Ok((rows.len(), full))
    }

    /// A generation from a snapshot (03 §4.8 steps 1–4): lease, snapshot,
    /// payload `(F0, 0)` / `F0`.
    async fn build_baseline(&self, pool: &PgPool) -> Result<Gen> {
        let epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
            .fetch_one(pool)
            .await?;
        let g = Gen::create(self.root.as_deref(), epoch)?;
        search::lease_consumer(pool, &g.consumer()).await?;
        match self.scan_snapshot(pool, &g).await {
            Ok(()) => Ok(g),
            Err(e) => {
                search::retire_consumer(pool, &g.consumer(), g.dir.as_deref()).await;
                Err(e)
            }
        }
    }

    async fn scan_snapshot(&self, pool: &PgPool, g: &Gen) -> Result<()> {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let f0: String = sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
            .fetch_one(&mut *tx)
            .await?;
        let f0: u64 = f0.parse().context("snapshot xmin")?;
        let mut after = Uuid::nil();
        let mut n: u64 = 0;
        loop {
            let page: Vec<PageDoc> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{DOC_SELECT} WHERE p.deleted_at IS NULL AND p.id > $1 ORDER BY p.id LIMIT $2"
            )))
            .bind(after)
            .bind(BASELINE_PAGE)
            .fetch_all(&mut *tx)
            .await?;
            let Some(last) = page.last() else { break };
            after = last.id;
            n += page.len() as u64;
            let w = lock(&g.writer)?;
            for d in page {
                w.add_document(g.make_doc(&d.plain()))?;
            }
        }
        let expected: i64 =
            sqlx::query_scalar("SELECT count(*) FROM wiki_pages WHERE deleted_at IS NULL")
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        let c0 = Cursor { xid: f0, id: 0 };
        let epoch = lock(&g.st)?.epoch;
        g.commit(epoch, c0, f0)?;
        g.reader.reload()?;
        {
            let mut st = lock(&g.st)?;
            st.cursor = c0;
            st.f = f0;
            st.durable_f = f0;
            st.durable_at = Instant::now();
        }
        let docs = g.reader.searcher().num_docs();
        if docs != n || docs != expected as u64 {
            anyhow::bail!(
                "wiki baseline has {docs} documents, the snapshot {expected} pages (scanned {n})"
            );
        }
        sqlx::query(
            "UPDATE outbox_cursors SET last_xid = $2::text::xid8, last_id = 0, updated_at = now()
             WHERE consumer = $1",
        )
        .bind(g.consumer())
        .bind(f0.to_string())
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Blue-green rebuild (03 §4.8): the serving generation keeps serving
    /// while a new one is built and catches up; it switches only when not
    /// behind (Q-S8) and healthy.
    pub async fn rebuild(&self, pool: &PgPool) -> Result<RebuildReport> {
        let _one = self.rebuilding.lock().await;
        self.sync_round(pool).await?;
        let g = Arc::new(self.build_baseline(pool).await?);
        let (baseline_docs, baseline_frontier) = (g.reader.searcher().num_docs(), lock(&g.st)?.f);
        *lock(&self.pending)? = Some(g.clone());
        let r = self.catch_up_and_switch(pool, &g).await;
        *lock(&self.pending)? = None;
        match r {
            Ok((previous, frontier)) => Ok(RebuildReport {
                generation: g.id,
                previous,
                baseline_docs,
                baseline_frontier,
                frontier,
            }),
            Err(e) => {
                search::retire_consumer(pool, &g.consumer(), g.dir.as_deref()).await;
                Err(e)
            }
        }
    }

    async fn catch_up_and_switch(
        &self,
        pool: &PgPool,
        g: &Arc<Gen>,
    ) -> Result<(Option<Uuid>, u64)> {
        for _ in 0..100_000 {
            let _round = self.round.lock().await;
            let (_, more) = self.round_on(pool, g, false).await?;
            if !more {
                break;
            }
        }
        let _round = self.round.lock().await;
        let old = self.active.read().ok().and_then(|a| a.clone());
        let (old_f, old_c) = match &old {
            Some(o) => {
                let st = lock(&o.st)?;
                (st.f, st.cursor)
            }
            None => (0, Cursor::default()),
        };
        let mut ready = false;
        for _ in 0..1000 {
            let (nf, nc) = {
                let st = lock(&g.st)?;
                (st.f, st.cursor)
            };
            if switch_ready(nf, nc, old_f, old_c) {
                ready = true;
                break;
            }
            self.round_on(pool, g, false).await?;
        }
        if !ready {
            anyhow::bail!("rebuilt wiki generation did not catch up with the serving one");
        }
        self.health_check(pool, g).await?;
        let (epoch, c, f) = {
            let st = lock(&g.st)?;
            (st.epoch, st.cursor, st.f)
        };
        g.commit(epoch, c, f)?;
        {
            let mut st = lock(&g.st)?;
            st.durable_f = f;
            st.durable_at = Instant::now();
        }
        g.reader.reload()?;
        self.mark_active(pool, g, c).await?;
        if let Some(root) = &self.root
            && let Err(e) = search::write_current(root, g.id)
        {
            tracing::error!(
                "wiki generation {} serves but CURRENT was not written: {e:#}",
                g.id
            );
        }
        *self
            .active
            .write()
            .map_err(|_| anyhow::anyhow!("wiki active lock poisoned"))? = Some(g.clone());
        self.publish(g, f)?;
        self.metrics.switched();
        Ok((old.map(|o| o.id), f.max(old_f)))
    }

    /// 03 §4.8 step 6 health check: a sample of live pages must be in the
    /// new generation as they are now, or have a `wiki_page` event it has
    /// not applied yet.
    async fn health_check(&self, pool: &PgPool, g: &Gen) -> Result<()> {
        let c = lock(&g.st)?.cursor;
        let view = g.view(0);
        let sample: Vec<PageDoc> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{DOC_SELECT} WHERE p.deleted_at IS NULL ORDER BY random() LIMIT {HEALTH_SAMPLE}"
        )))
        .fetch_all(pool)
        .await?;
        let mut bad: Vec<Uuid> = Vec::new();
        for d in sample {
            let d = d.plain();
            if !view.holds(&d)? {
                bad.push(d.id);
            }
        }
        if bad.is_empty() {
            return Ok(());
        }
        let unexplained: Vec<Uuid> = sqlx::query_scalar(
            "SELECT b FROM unnest($1::uuid[]) AS b
             WHERE NOT EXISTS (SELECT 1 FROM outbox o WHERE o.topic = 'wiki_page' AND o.entity_id = b
                                 AND (o.xid, o.id) > ($2::text::xid8, $3::bigint))",
        )
        .bind(&bad)
        .bind(c.xid.to_string())
        .bind(c.id)
        .fetch_all(pool)
        .await?;
        if unexplained.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "rebuilt wiki generation differs from PostgreSQL for {} sampled pages (e.g. {}); switch aborted",
                unexplained.len(),
                unexplained[0]
            )
        }
    }

    /// Outbox GC and the wiki generations past their window (same rules as
    /// the issue index's; either one's GC removes outbox rows below every
    /// live consumer).
    pub async fn gc(&self, pool: &PgPool) -> Result<(u64, usize)> {
        let live: Vec<Uuid> = [
            self.generation(),
            lock(&self.pending)?.as_ref().map(|g| g.id),
        ]
        .into_iter()
        .flatten()
        .collect();
        search::gc_generations(pool, CONSUMER_PREFIX, self.root.as_deref(), &live).await
    }
}

// ---------------------------------------------------------------- GET /wiki/search

/// `GET /wiki/search` query parameters.
#[derive(Debug, Default, Clone, Deserialize, JsonSchema)]
pub struct WikiSearchParams {
    /// Free text. Absent or blank: no search, only `available`.
    pub q: Option<String>,
    /// One project (id or key). A project whose wiki you cannot read and
    /// a project that does not exist give the same empty result.
    pub project: Option<String>,
    /// `next_cursor` of the previous page (same q and project).
    pub cursor: Option<String>,
    /// A wiki write receipt's `search.consistent_after`: wait until this
    /// index includes that write.
    pub consistent_after: Option<String>,
    /// How long to wait for `consistent_after`, 0–2000 ms (default 1000).
    pub wait_ms: Option<u32>,
    /// "return" (default: `outcome: "search_lagging"`, no items) or
    /// "error" (`search_not_caught_up`).
    pub on_lag: Option<String>,
}

/// One part of a snippet: plain text, `hit` where a query term matched.
/// Render the parts in order as text (no HTML), bold where `hit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SnippetPart {
    pub text: String,
    pub hit: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WikiSearchItem {
    pub project: WikiProjectRef,
    pub page_id: Uuid,
    /// The current title.
    pub title: String,
    /// The title as the page URL writes it: `/projects/{project.key}/wiki/{url_title}`.
    pub url_title: String,
    /// About one line of the current text around the best match (the start
    /// of the page when only the title matched); empty for an empty page.
    pub snippet: Vec<SnippetPart>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WikiSearchResult {
    /// "ok", or "search_lagging": the wiki index has not caught up with
    /// `consistent_after` yet (no items; the write itself succeeded).
    pub outcome: String,
    /// You read at least one wiki (whatever `q` and `project` say): the
    /// web app shows the Wiki tab of the search page only then.
    pub available: bool,
    /// eventual (no token) | satisfied | lagging | not_applicable (no `q`).
    pub consistency: Consistency,
    /// Best first, at most 20.
    pub items: Vec<WikiSearchItem>,
    pub next_cursor: Option<String>,
}

fn invalid_query() -> anyhow::Error {
    ApiError::new(
        ErrorCode::InvalidQuery,
        "q has no searchable words or characters (only punctuation?)",
    )
    .with_details(serde_json::json!({"suggestion": "use words, identifiers or CJK characters"}))
    .into()
}

/// The snippet of a page: a window of the body around its best match, or
/// the start of the body when only the title matched.
fn snippet(terms: &Terms, body: &str) -> Vec<SnippetPart> {
    let cp = CpText::new(body);
    if cp.is_empty() {
        return Vec::new();
    }
    let (w, spans) = match evidence::match_field(terms, body, &cp) {
        Some(m) => match evidence::windows(&cp, &m, SNIPPET_CHARS, 1).first() {
            Some(w) => {
                let r = (w.start, w.end);
                (r, evidence::highlights_in(&m, r))
            }
            None => ((0, cp.len().min(SNIPPET_CHARS)), Vec::new()),
        },
        None => ((0, cp.len().min(SNIPPET_CHARS)), Vec::new()),
    };
    let text = cp.slice(w.0, w.1);
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<SnippetPart> = Vec::new();
    let mut at = 0;
    for (s, e) in spans {
        let (s, e) = (s.min(chars.len()), e.min(chars.len()));
        if s > at {
            out.push(SnippetPart {
                text: chars[at..s].iter().collect(),
                hit: false,
            });
        }
        if e > s.max(at) {
            out.push(SnippetPart {
                text: chars[s.max(at)..e].iter().collect(),
                hit: true,
            });
        }
        at = at.max(e);
    }
    if at < chars.len() {
        out.push(SnippetPart {
            text: chars[at..].iter().collect(),
            hit: false,
        });
    }
    out
}

/// `GET /wiki/search` (design 06 §3.14, §6.7): wiki pages only, best
/// first, 20 per page. Only wikis the caller reads (R-W1) are searched; a
/// `project` you cannot read and one that does not exist give the same
/// empty result (Q-S1). `consistent_after` waits for the wiki index.
pub async fn search(
    app: &App,
    caller: AuthContext,
    p: WikiSearchParams,
) -> Result<WikiSearchResult> {
    // Input first (03 §4.11), then the wait, outside any transaction.
    let target = crate::service::consistency_target(
        &app.pool,
        p.wait_ms,
        p.on_lag.as_deref(),
        p.consistent_after.as_deref(),
    )
    .await;
    if let Err(e) = &target
        && e.downcast_ref::<ApiError>()
            .is_some_and(|a| a.code == ErrorCode::ConsistencyEpochChanged)
    {
        app.wiki
            .metrics
            .consistency(crate::metrics::Outcome::EpochChanged, None);
    }
    let target = target?;
    let q = p.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let terms = q.map(Terms::new);
    if terms.as_ref().is_some_and(Terms::is_empty) {
        return Err(invalid_query());
    }
    let hash = cursor::query_hash(&serde_json::json!({
        "kind": "wiki_search",
        "q": q,
        "project": p.project.as_deref().map(str::trim),
    }));
    let offset = match p.cursor.as_deref() {
        None => 0,
        Some(c) => match cursor::decode(&app.pool, c, caller.principal_id(), &hash).await? {
            Position::Rank { o } => o as usize,
            _ => return Err(cursor::mismatch()),
        },
    };
    let mut consistency = Consistency {
        status: if q.is_some() {
            "eventual"
        } else {
            "not_applicable"
        }
        .into(),
        reason: q.is_none().then(|| "no_query".to_string()),
        waited_ms: None,
        retry_after_ms: None,
    };
    let mut lagging = false;
    if let (Some((x, wait)), Some(_)) = (target, q) {
        use crate::metrics::Outcome;
        match app.wiki.wait_for(caller.principal_id(), x, wait).await {
            Wait::Satisfied { waited_ms } => {
                app.wiki
                    .metrics
                    .consistency(Outcome::Satisfied, Some(Duration::from_millis(waited_ms)));
                consistency.status = "satisfied".into();
                consistency.waited_ms = Some(waited_ms);
            }
            Wait::Lagging { waited_ms } => {
                app.wiki
                    .metrics
                    .consistency(Outcome::Lagging, Some(Duration::from_millis(waited_ms)));
                if p.on_lag.as_deref() == Some("error") {
                    return Err(ApiError::new(
                        ErrorCode::SearchNotCaughtUp,
                        "the wiki index has not caught up with consistent_after yet",
                    )
                    .with_retry_after(LAG_RETRY_MS)
                    .into());
                }
                lagging = true;
                consistency.status = "lagging".into();
                consistency.waited_ms = Some(waited_ms);
                consistency.retry_after_ms = Some(LAG_RETRY_MS);
            }
        }
    }
    let mut tx = read_tx(app).await?;
    let readable = wiki::readable_wikis(&mut tx, caller).await?;
    let available = !readable.is_empty();
    let mut out = WikiSearchResult {
        outcome: if lagging { "search_lagging" } else { "ok" }.into(),
        available,
        consistency,
        items: Vec::new(),
        next_cursor: None,
    };
    let (Some(terms), false) = (terms, lagging) else {
        tx.commit().await?;
        return Ok(out);
    };
    // The wikis to search: every readable one, or the one `project` names
    // if readable (else none, exactly as for a missing project).
    let projects: Vec<Uuid> = match p.project.as_deref() {
        None => readable.keys().copied().collect(),
        Some(text) => match wiki::project_id(&mut tx, text).await? {
            Some(id) if readable.contains_key(&id) => vec![id],
            _ => Vec::new(),
        },
    };
    if projects.is_empty() {
        tx.commit().await?;
        return Ok(out);
    }
    let view = match app.wiki.view() {
        Some(v) if !app.wiki.metrics.sync_stalled() => v,
        _ => return Err(search_unavailable()),
    };
    let weighted = terms.weighted();
    let need = offset + PAGE_SIZE + 1;
    let mut k = FIRST_CANDIDATES.max(2 * need);
    // (score, page id, project, title, plain body, updated_at)
    let survivors: Vec<(f32, Uuid, Uuid, String, String, DateTime<Utc>)> = loop {
        let hits = view.search(&weighted, &projects, k).map_err(|e| {
            tracing::error!("wiki index failed: {e:#}");
            search_unavailable()
        })?;
        let exhausted = hits.len() < k;
        let score_of: HashMap<Uuid, f32> = hits.iter().map(|h| (h.page_id, h.score)).collect();
        let ids: Vec<Uuid> = hits.iter().map(|h| h.page_id).collect();
        let rows: Vec<(Uuid, Uuid, String, String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT p.id, p.project_id, p.title, v.body, p.updated_at
             FROM wiki_pages p
             JOIN wiki_page_versions v ON v.page_id = p.id AND v.version = p.current_version
             WHERE p.id = ANY($1) AND p.deleted_at IS NULL AND p.project_id = ANY($2)",
        )
        .bind(&ids)
        .bind(&projects)
        .fetch_all(&mut *tx)
        .await?;
        let mut kept = Vec::with_capacity(rows.len());
        for (id, project, title, body, updated_at) in rows {
            let body = plain_body(&body);
            // The current text must still match: the index may still hold
            // text that has changed since (an edit, a D12 redaction).
            let matches = evidence::match_field(&terms, &title, &CpText::new(&title)).is_some()
                || evidence::match_field(&terms, &body, &CpText::new(&body)).is_some();
            if matches {
                kept.push((score_of[&id], id, project, title, body, updated_at));
            }
        }
        if exhausted || kept.len() >= need || k >= MAX_CANDIDATES {
            break kept;
        }
        k = (k * 4).min(MAX_CANDIDATES);
    };
    let mut survivors = survivors;
    survivors.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let more = survivors.len() > offset + PAGE_SIZE;
    for (_, id, project, title, body, updated_at) in
        survivors.into_iter().skip(offset).take(PAGE_SIZE)
    {
        let Some(a) = readable.get(&project) else {
            continue;
        };
        out.items.push(WikiSearchItem {
            project: wiki::project_ref(a),
            page_id: id,
            url_title: wiki::url_title(&title),
            snippet: snippet(&terms, &body),
            title,
            updated_at,
        });
    }
    tx.commit().await?;
    if more {
        out.next_cursor = Some(
            cursor::encode(
                &app.pool,
                caller.principal_id(),
                &hash,
                Position::Rank {
                    o: (offset + PAGE_SIZE) as u32,
                },
            )
            .await?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{SnippetPart, Terms, snippet};

    fn parts(q: &str, body: &str) -> Vec<(String, bool)> {
        snippet(&Terms::new(q), body)
            .into_iter()
            .map(|SnippetPart { text, hit }| (text, hit))
            .collect()
    }

    #[test]
    fn snippet_marks_hits_as_parts() {
        assert_eq!(
            parts("登入", "請先登入系統"),
            vec![
                ("請先".to_string(), false),
                ("登入".to_string(), true),
                ("系統".to_string(), false)
            ]
        );
        // No body match (title only): the start of the page, nothing marked.
        assert_eq!(parts("deploy", "hello"), vec![("hello".to_string(), false)]);
        assert!(parts("x", "").is_empty());
        // A long text is cut to about one line around the hit.
        let long = format!("{} needle {}", "a ".repeat(200), "b ".repeat(200));
        let p = parts("needle", &long);
        assert!(p.iter().any(|(t, h)| *h && t == "needle"));
        let n: usize = p.iter().map(|(t, _)| t.chars().count()).sum();
        assert!(n <= 120, "{n}");
        // Markup-looking text stays text (the caller never renders HTML).
        assert_eq!(
            parts("tag", "<b>tag</b>"),
            vec![
                ("<b>".to_string(), false),
                ("tag".to_string(), true),
                ("</b>".to_string(), false)
            ]
        );
    }
}
