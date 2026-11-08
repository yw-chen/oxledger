// SPDX-License-Identifier: GPL-2.0-or-later
//! Minimal BM25 index (M0) on tantivy.
//!
//! * CJK text is indexed as unigrams + overlapping bigrams, Latin text as
//!   lower-cased words — the dictionary-free baseline from research §5.4
//!   (no jieba), kept after experiment E1. Identifiers glued by `_` / `.`
//!   are indexed whole as well as split.
//! * Visibility facts are indexed as raw terms so the same [`Pred`] that
//!   becomes SQL also becomes a tantivy filter (filter before ranking).
//! * The index follows the outbox (D11) and can always be rebuilt from SQL.
//! * Read-your-writes (design 03 §4.1–4.3): each sync round reads the
//!   snapshot horizon H and the outbox rows below it in one statement, and
//!   publishes the exclusive frontier F only after the reader reloaded. A
//!   token `c1.<epoch>.<X>` is satisfied when `X < F`. F lives in memory
//!   (S8: no durable payload yet); after a restart it starts at 0 and moves
//!   to H on the first round, which only makes waiters wait longer.

use anyhow::{Context, Result};
use ox_core::Pred;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, EmptyQuery, Occur, Query, TermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};
use uuid::Uuid;

const TOKENIZER: &str = "cjk_bigram";
/// Waiters per caller and in all (design 03 §4.3); beyond them a search with
/// a token reports lag at once.
pub const MAX_WAITERS_PER_CALLER: usize = 4;
pub const MAX_WAITERS: usize = 1000;
/// An outbox row as a sync round reads it: (id, xid8 as text, issue id).
type OutboxEvent = (i64, String, Uuid);
/// Outbox rows per sync round (03 §4.2).
const SYNC_BATCH: i64 = 500;
const NONE_TERM: &str = "none";

// ---------------------------------------------------------------- tokenizer

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF |   // Hiragana, Katakana
        0x3400..=0x4DBF |   // CJK Ext A
        0x4E00..=0x9FFF |   // CJK Unified
        0xAC00..=0xD7AF |   // Hangul
        0xF900..=0xFAFF |   // CJK Compatibility
        0x20000..=0x2FFFF) // CJK Ext B+
}

/// Characters that glue non-CJK alphanumeric segments into one identifier
/// (`invalid_grant`, `v3.2.0`, `foo.bar`). Other punctuation, `-` included,
/// stays a plain separator. Decided with GPT, see `docs/playbook.md`
/// 「識別碼斷詞」.
const COMPOUND_JOINERS: [char; 2] = ['_', '.'];
/// A longer compound is not emitted as a whole (its parts still are).
const MAX_COMPOUND_BYTES: usize = 128;

/// What a [`Lexeme`] is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Non-CJK alphanumeric run that is not part of a compound.
    Word,
    /// A segment of a compound (`invalid` in `invalid_grant`).
    Part,
    /// The whole compound (`invalid_grant`).
    Compound,
    /// A CJK character standing alone.
    CjkAlone,
    /// A CJK character inside a run of two or more.
    CjkInRun,
    /// Two adjacent CJK characters.
    CjkBigram,
}

/// One token with where it came from (design 03 §3.3): `from..to` is the
/// UTF-8 byte range in the original text. Lower-casing changes only `term`:
/// a character that lower-cases to several still maps to its own full range,
/// and a compound spans its first segment's start to its last segment's end
/// (joiners included).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lexeme {
    pub term: String,
    pub kind: Kind,
    pub from: usize,
    pub to: usize,
}

#[derive(Default)]
struct Lexer {
    out: Vec<Lexeme>,
    /// The current CJK run: (char, byte offset).
    cjk: Vec<(char, usize)>,
    /// Current segment (lower-cased) and its byte range.
    word: String,
    word_from: usize,
    word_to: usize,
    /// The segments of the compound so far.
    parts: Vec<(String, usize, usize)>,
    /// The compound as written (lower-cased), joiners included.
    whole: String,
    joiner: Option<char>,
}

impl Lexer {
    fn end_cjk(&mut self) {
        let alone = self.cjk.len() == 1;
        for (i, (c, at)) in self.cjk.iter().enumerate() {
            let kind = if alone {
                Kind::CjkAlone
            } else {
                Kind::CjkInRun
            };
            let end = at + c.len_utf8();
            self.out.push(Lexeme {
                term: c.to_string(),
                kind,
                from: *at,
                to: end,
            });
            if let Some((n, n_at)) = self.cjk.get(i + 1) {
                self.out.push(Lexeme {
                    term: [*c, *n].iter().collect(),
                    kind: Kind::CjkBigram,
                    from: *at,
                    to: n_at + n.len_utf8(),
                });
            }
        }
        self.cjk.clear();
    }

    fn latin_char(&mut self, c: char, at: usize) {
        if let Some(j) = self.joiner.take() {
            self.whole.push(j);
        }
        if self.word.is_empty() {
            self.word_from = at;
        }
        self.word_to = at + c.len_utf8();
        for l in c.to_lowercase() {
            self.word.push(l);
            self.whole.push(l);
        }
    }

    fn take_word(&mut self) {
        let w = std::mem::take(&mut self.word);
        self.parts.push((w, self.word_from, self.word_to));
    }

    /// A joiner right after a segment may start or extend a compound;
    /// anywhere else (`__init`, `a..b`, `a._b`) it ends the run.
    fn joiner(&mut self, j: char) {
        if self.word.is_empty() {
            self.end_latin();
        } else {
            self.take_word();
            self.joiner = Some(j);
        }
    }

    fn end_latin(&mut self) {
        if !self.word.is_empty() {
            self.take_word();
        }
        self.joiner = None; // a trailing joiner (`invalid_grant.`) is dropped
        let whole = std::mem::take(&mut self.whole);
        let compound = self.parts.len() >= 2 && whole.len() <= MAX_COMPOUND_BYTES;
        let kind = if compound { Kind::Part } else { Kind::Word };
        let span = match (self.parts.first(), self.parts.last()) {
            (Some(f), Some(l)) => (f.1, l.2),
            _ => (0, 0),
        };
        self.out
            .extend(self.parts.drain(..).map(|(term, from, to)| Lexeme {
                term,
                kind,
                from,
                to,
            }));
        if compound {
            self.out.push(Lexeme {
                term: whole,
                kind: Kind::Compound,
                from: span.0,
                to: span.1,
            });
        }
    }
}

/// The spanned lexer (design 03 §3.3): one implementation for the index
/// tokenizer, [`tokenize`], [`query_terms`] and evidence matching.
pub fn lexemes(text: &str) -> Vec<Lexeme> {
    let mut lx = Lexer::default();
    for (at, c) in text.char_indices() {
        if is_cjk(c) {
            lx.end_latin();
            lx.cjk.push((c, at));
        } else if c.is_alphanumeric() {
            lx.end_cjk();
            lx.latin_char(c, at);
        } else if COMPOUND_JOINERS.contains(&c) {
            lx.end_cjk();
            lx.joiner(c);
        } else {
            lx.end_cjk();
            lx.end_latin();
        }
    }
    lx.end_cjk();
    lx.end_latin();
    lx.out
}

/// Splits text into tokens: alphanumeric runs → lower-cased words; runs glued
/// by `_` or `.` → each segment plus the whole (`invalid_grant` → `invalid`,
/// `grant`, `invalid_grant`); CJK runs → every character plus every adjacent
/// pair.
pub fn tokenize(text: &str) -> Vec<String> {
    lexemes(text).into_iter().map(|l| l.term).collect()
}

fn tokens(text: &str) -> Vec<(String, Kind)> {
    lexemes(text)
        .into_iter()
        .map(|l| (l.term, l.kind))
        .collect()
}

#[derive(Clone, Default)]
struct CjkBigram;

struct VecStream {
    tokens: Vec<Token>,
    pos: usize,
}

impl TokenStream for VecStream {
    fn advance(&mut self) -> bool {
        if self.pos < self.tokens.len() {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn token(&self) -> &Token {
        &self.tokens[self.pos - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.pos - 1]
    }
}

impl Tokenizer for CjkBigram {
    type TokenStream<'a> = VecStream;
    /// Positions stay the expanded token ordinal (the index format does not
    /// change); offsets are the original byte range (design 03 §3.3).
    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecStream {
        let tokens = lexemes(text)
            .into_iter()
            .enumerate()
            .map(|(i, l)| Token {
                position: i,
                offset_from: l.from,
                offset_to: l.to,
                text: l.term,
                ..Token::default()
            })
            .collect();
        VecStream { tokens, pos: 0 }
    }
}

/// Query-side tokens: a CJK run of two or more characters is matched by its
/// bigrams only (so 登入 does not hit 載入 through the unigram 入); a single
/// CJK character falls back to its unigram. The index keeps both.
pub fn tokenize_query(text: &str) -> Vec<String> {
    tokens(text)
        .into_iter()
        .filter(|(_, k)| *k != Kind::CjkInRun)
        .map(|(t, _)| t)
        .collect()
}

/// Weight of the extra CJK unigrams on the query side, relative to the
/// bigram/word terms (D15). They rescue queries whose bigrams never occur in
/// the corpus (付不了錢 → 付, 錢) without letting a shared character such as
/// 入 pull 載入 up next to 登入. E1 tried 0.1 / 0.2 / 0.5: 0.1 was best
/// (nDCG@10 0.781 → 0.810), 0.5 lost the gain. See
/// `docs/research/03-e1-zh-retrieval.md` (摘要第 2 點、§6 建議 2).
pub const CJK_UNIGRAM_WEIGHT: f32 = 0.1;

/// Weight of a compound's segments on the query side: `invalid_grant` is
/// searched as `invalid_grant` ×1 plus `invalid`, `grant` ×0.2, so the exact
/// identifier ranks first while `invalid grant` written apart is still found.
/// Not calibrated (E1 has no identifier queries); chosen with GPT as a
/// conservative start, see `docs/playbook.md`「識別碼斷詞」.
pub const COMPOUND_PART_WEIGHT: f32 = 0.2;

/// Weighted query terms: words, compounds, CJK bigrams and lone CJK
/// characters at weight 1; compound segments at [`COMPOUND_PART_WEIGHT`];
/// CJK characters inside a longer run (which [`tokenize_query`] drops) at
/// [`CJK_UNIGRAM_WEIGHT`].
/// Sorted by term, no duplicates (a term keeps its highest weight).
pub fn query_terms(text: &str) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = tokens(text)
        .into_iter()
        .map(|(t, k)| {
            let w = match k {
                Kind::Part => COMPOUND_PART_WEIGHT,
                Kind::CjkInRun => CJK_UNIGRAM_WEIGHT,
                Kind::Word | Kind::Compound | Kind::CjkAlone | Kind::CjkBigram => 1.0,
            };
            (t, w)
        })
        .collect();
    // Highest weight first within a term, so dedup keeps it.
    out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
    out.dedup_by(|later, kept| later.0 == kept.0);
    out
}

// ---------------------------------------------------------------- index

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    key: Field,
    project: Field,
    tracker: Field,
    private: Field,
    author: Field,
    assignee: Field,
    subject: Field,
    description: Field,
}

pub struct SearchIndex {
    #[allow(dead_code)] // kept for upcoming schema introspection / merges
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    f: Fields,
    /// The published exclusive frontier F (03 §4.1): every transaction with
    /// xid < F has its search events applied to the current reader.
    frontier: tokio::sync::watch::Sender<u64>,
    waiters: AtomicUsize,
    per_caller: Mutex<HashMap<Option<Uuid>, usize>>,
    /// One round at a time (03 §4.2 has a single writer): two concurrent
    /// rounds could overwrite newer documents with an older snapshot.
    round: tokio::sync::Mutex<()>,
}

/// How a wait for `X < F` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Satisfied { waited_ms: u64 },
    Lagging { waited_ms: u64 },
}

/// Releases a waiter slot however the wait ends (also on cancellation).
struct Slot<'a> {
    index: &'a SearchIndex,
    who: Option<Uuid>,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.index.waiters.fetch_sub(1, Ordering::SeqCst);
        if let Ok(mut m) = self.index.per_caller.lock()
            && let Some(n) = m.get_mut(&self.who)
        {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.who);
            }
        }
    }
}

pub struct Hit {
    pub issue_id: Uuid,
    pub score: f32,
}

#[derive(sqlx::FromRow)]
struct IssueDoc {
    id: Uuid,
    key: String,
    project_id: Uuid,
    tracker_id: Uuid,
    is_private: bool,
    author_id: Uuid,
    assigned_to_id: Option<Uuid>,
    subject: String,
    description: String,
}

impl SearchIndex {
    /// Opens (or creates) the index in `dir`; `None` keeps it in memory (tests).
    pub fn open(dir: Option<&Path>) -> Result<Self> {
        let mut sb = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let f = Fields {
            id: sb.add_text_field("id", STRING | STORED),
            key: sb.add_text_field("key", STRING | STORED),
            project: sb.add_text_field("project", STRING),
            tracker: sb.add_text_field("tracker", STRING),
            private: sb.add_text_field("private", STRING),
            author: sb.add_text_field("author", STRING),
            assignee: sb.add_text_field("assignee", STRING),
            subject: sb.add_text_field("subject", text.clone()),
            description: sb.add_text_field("description", text),
        };
        let schema = sb.build();
        let index = match dir {
            None => Index::create_in_ram(schema),
            Some(d) => {
                std::fs::create_dir_all(d)?;
                let mmap = tantivy::directory::MmapDirectory::open(d)?;
                Index::open_or_create(mmap, schema)
                    .context("open search index (schema change? delete the index dir to rebuild)")?
            }
        };
        index
            .tokenizers()
            .register(TOKENIZER, TextAnalyzer::builder(CjkBigram).build());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = Mutex::new(index.writer(50_000_000)?);
        Ok(SearchIndex {
            index,
            reader,
            writer,
            f,
            frontier: tokio::sync::watch::Sender::new(0),
            waiters: AtomicUsize::new(0),
            round: tokio::sync::Mutex::new(()),
            per_caller: Mutex::new(HashMap::new()),
        })
    }

    /// The published frontier F.
    pub fn frontier(&self) -> u64 {
        *self.frontier.borrow()
    }

    fn publish(&self, f: u64) {
        self.frontier.send_if_modified(|cur| {
            if f > *cur {
                *cur = f;
                true
            } else {
                false
            }
        });
    }

    fn take_slot(&self, who: Option<Uuid>) -> Option<Slot<'_>> {
        let mut m = self.per_caller.lock().ok()?;
        let mine = m.get(&who).copied().unwrap_or(0);
        if mine >= MAX_WAITERS_PER_CALLER {
            return None;
        }
        if self.waiters.fetch_add(1, Ordering::SeqCst) >= MAX_WAITERS {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        *m.entry(who).or_insert(0) += 1;
        Some(Slot { index: self, who })
    }

    /// Waits at most `wait` until `x < F` (03 §4.4). All waiters share one
    /// `watch` notification; over the waiter limits it reports lag at once.
    pub async fn wait_for(&self, who: Option<Uuid>, x: u64, wait: Duration) -> Wait {
        let start = Instant::now();
        let ms = |s: Instant| s.elapsed().as_millis() as u64;
        if x < self.frontier() {
            return Wait::Satisfied { waited_ms: 0 };
        }
        if wait.is_zero() {
            return Wait::Lagging { waited_ms: 0 };
        }
        let Some(_slot) = self.take_slot(who) else {
            return Wait::Lagging { waited_ms: 0 };
        };
        let mut rx = self.frontier.subscribe();
        match tokio::time::timeout(wait, rx.wait_for(|f| x < *f)).await {
            Ok(Ok(_)) => Wait::Satisfied {
                waited_ms: ms(start),
            },
            _ => Wait::Lagging {
                waited_ms: ms(start),
            },
        }
    }

    /// Tests: waiters currently holding a slot.
    pub fn waiting(&self) -> usize {
        self.waiters.load(Ordering::SeqCst)
    }

    fn uuid_term(&self, field: Field, id: Uuid) -> Term {
        Term::from_field_text(field, &id.to_string())
    }

    fn filter(&self, pred: &Pred) -> Box<dyn Query> {
        let f = self.f;
        let term = |field: Field, v: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, v),
                IndexRecordOption::Basic,
            ))
        };
        let any_of = |field: Field, ids: &[Uuid]| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(
                ids.iter()
                    .map(|i| (Occur::Should, term(field, &i.to_string())))
                    .collect(),
            ))
        };
        match pred {
            Pred::True => Box::new(AllQuery),
            Pred::False => Box::new(EmptyQuery),
            Pred::And(ps) => Box::new(BooleanQuery::new(
                ps.iter().map(|p| (Occur::Must, self.filter(p))).collect(),
            )),
            Pred::Or(ps) => Box::new(BooleanQuery::new(
                ps.iter().map(|p| (Occur::Should, self.filter(p))).collect(),
            )),
            Pred::ProjectIs(id) => term(f.project, &id.to_string()),
            Pred::TrackerIn(ids) => any_of(f.tracker, ids),
            Pred::NotPrivate => term(f.private, "false"),
            Pred::AuthorIs(id) => term(f.author, &id.to_string()),
            Pred::AssigneeIn(ids) => any_of(f.assignee, ids),
        }
    }

    /// BM25 over subject (×3) and description (×1), restricted to `filter`.
    /// Each term's boost is further scaled by its [`query_terms`] weight.
    /// Empty `q` lists every visible document (used by tests).
    pub fn search(&self, q: &str, filter: &Pred, limit: usize) -> Result<Vec<Hit>> {
        let terms = query_terms(q);
        let text: Box<dyn Query> = if terms.is_empty() {
            Box::new(AllQuery)
        } else {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (t, w) in &terms {
                for (field, boost) in [(self.f.subject, 3.0), (self.f.description, 1.0)] {
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
            Box::new(BooleanQuery::new(clauses))
        };
        // The filter must not change BM25 scores, so it scores a constant 0.
        let filter_q: Box<dyn Query> = Box::new(ConstScoreQuery::new(self.filter(filter), 0.0));
        let query = BooleanQuery::new(vec![(Occur::Must, text), (Occur::Must, filter_q)]);
        let searcher = self.reader.searcher();
        let top = searcher.search(&query, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            if let Some(id) = d.get_first(self.f.id).and_then(|v| v.as_str()) {
                hits.push(Hit {
                    issue_id: id.parse()?,
                    score,
                });
            }
        }
        Ok(hits)
    }

    /// Applies committed outbox messages (xid8 snapshot rule, D11).
    /// Returns how many messages were applied. At-least-once: re-applying is harmless.
    /// Tests: `sync` until every outbox row written so far is indexed.
    /// `sync` alone skips rows newer than the cluster-wide xmin, so another
    /// database's open transaction can leave a just-committed row for the
    /// next round (playbook).
    #[cfg(feature = "test-support")]
    pub async fn sync_all(&self, pool: &PgPool) -> Result<usize> {
        let target: Option<(String, i64)> = sqlx::query_as(
            "SELECT xid::text, id FROM outbox WHERE topic = 'issue' ORDER BY xid DESC, id DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
        let mut applied = 0;
        for _ in 0..1000 {
            applied += self.sync(pool).await?;
            let Some((xid, id)) = &target else {
                return Ok(applied);
            };
            let done: bool = sqlx::query_scalar(
                "SELECT COALESCE((c.last_xid, c.last_id) >= ($1::xid8, $2), false)
                   FROM (SELECT 1) one
                   LEFT JOIN outbox_cursors c ON c.consumer = 'search'",
            )
            .bind(xid)
            .bind(id)
            .fetch_one(pool)
            .await?;
            if done {
                return Ok(applied);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        anyhow::bail!("search index did not catch up with the outbox within 20 s")
    }

    /// Rounds of 03 §4.2 until the outbox below H is drained.
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

    /// One round of 03 §4.2: (rows applied, a full batch so more may follow).
    pub async fn sync_round(&self, pool: &PgPool) -> Result<(usize, bool)> {
        const CONSUMER: &str = "search";
        let _round = self.round.lock().await;
        {
            // One statement: H and the visible events come from one snapshot,
            // and H is there even with zero events (03 §4.2; FILTER keeps
            // the empty case `[]`).
            let (h, events): (String, sqlx::types::Json<Vec<OutboxEvent>>) = sqlx::query_as(
                "WITH h AS MATERIALIZED (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS h),
                          c AS (SELECT COALESCE(max(last_xid::text), '0')::xid8 AS x,
                                       COALESCE(max(last_id), 0) AS i
                                FROM outbox_cursors WHERE consumer = $1),
                          ev AS (SELECT o.id, o.xid, o.entity_id FROM outbox o, h, c
                                 WHERE o.topic = 'issue' AND o.xid < h.h
                                   AND (o.xid, o.id) > (c.x, c.i)
                                 ORDER BY o.xid, o.id LIMIT $2)
                     SELECT h.h::text,
                            COALESCE(json_agg(json_build_array(ev.id, ev.xid::text, ev.entity_id)
                                              ORDER BY ev.xid, ev.id)
                                     FILTER (WHERE ev.id IS NOT NULL), '[]'::json)
                     FROM h LEFT JOIN ev ON true GROUP BY h.h",
            )
            .bind(CONSUMER)
            .bind(SYNC_BATCH)
            .fetch_one(pool)
            .await?;
            let rows = events.0;
            let h: u64 = h.parse().context("snapshot xmin")?;
            let Some((last_id, last_xid, _)) = rows.last().cloned() else {
                // Drained: everything below H is applied.
                self.publish(h);
                return Ok((0, false));
            };
            let mut ids: Vec<Uuid> = rows.iter().map(|r| r.2).collect();
            ids.sort();
            ids.dedup();
            let docs: Vec<IssueDoc> = sqlx::query_as(
                "SELECT i.id, k.key, i.project_id, i.tracker_id, i.is_private, i.author_id,
                        i.assigned_to_id, i.subject, i.description
                 FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current
                 WHERE i.id = ANY($1) AND i.archived_at IS NULL",
            )
            .bind(&ids)
            .fetch_all(pool)
            .await?;
            {
                let mut w = self
                    .writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("index writer poisoned"))?;
                for id in &ids {
                    w.delete_term(self.uuid_term(self.f.id, *id));
                }
                for d in docs {
                    let f = self.f;
                    w.add_document(doc!(
                        f.id => d.id.to_string(),
                        f.key => d.key,
                        f.project => d.project_id.to_string(),
                        f.tracker => d.tracker_id.to_string(),
                        f.private => if d.is_private { "true" } else { "false" },
                        f.author => d.author_id.to_string(),
                        f.assignee => d.assigned_to_id.map(|a| a.to_string()).unwrap_or_else(|| NONE_TERM.into()),
                        f.subject => d.subject,
                        f.description => d.description,
                    ))?;
                }
                w.commit()?;
            }
            // Publish F only after the reader shows the documents (03 §4.2
            // step 6): never an old searcher with a new F.
            self.reader.reload()?;
            let full = rows.len() as i64 == SYNC_BATCH;
            // A full batch may stop inside one xid: F' = last.xid, not +1.
            let f_new = if full {
                last_xid.parse().context("outbox xid")?
            } else {
                h
            };
            sqlx::query(
                "INSERT INTO outbox_cursors (consumer, last_xid, last_id) VALUES ($1, $2::xid8, $3)
                 ON CONFLICT (consumer) DO UPDATE SET last_xid = EXCLUDED.last_xid, last_id = EXCLUDED.last_id",
            )
            .bind(CONSUMER)
            .bind(&last_xid)
            .bind(last_id)
            .execute(pool)
            .await?;
            #[cfg(feature = "test-support")]
            crate::changesets::faults::hit("search.before_publish").await?;
            self.publish(f_new);
            Ok((rows.len(), full))
        }
    }

    /// Drops everything and replays from the start of the outbox's issues.
    pub async fn rebuild(&self, pool: &PgPool) -> Result<usize> {
        {
            let mut w = self
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("index writer poisoned"))?;
            w.delete_all_documents()?;
            w.commit()?;
        }
        sqlx::query("DELETE FROM outbox_cursors WHERE consumer = 'search'")
            .execute(pool)
            .await?;
        // Ensure every live issue has a message even if old outbox rows were pruned.
        sqlx::query("INSERT INTO outbox (topic, entity_id) SELECT 'issue', id FROM issues")
            .execute(pool)
            .await?;
        self.sync(pool).await
    }
}

/// The lexer before design 03 (no offsets), kept to prove the spanned one
/// emits exactly the same terms.
#[cfg(test)]
mod v0 {
    use super::{COMPOUND_JOINERS, MAX_COMPOUND_BYTES, is_cjk};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Kind {
        /// Non-CJK alphanumeric run that is not part of a compound.
        Word,
        /// A segment of a compound (`invalid` in `invalid_grant`).
        Part,
        /// The whole compound (`invalid_grant`).
        Compound,
        /// A CJK character standing alone.
        CjkAlone,
        /// A CJK character inside a run of two or more.
        CjkInRun,
        /// Two adjacent CJK characters.
        CjkBigram,
    }

    #[derive(Default)]
    struct Lexer {
        out: Vec<(String, Kind)>,
        cjk: Vec<char>,
        /// Current segment, and the segments of the compound so far.
        word: String,
        parts: Vec<String>,
        /// The compound as written (lower-cased), joiners included.
        whole: String,
        joiner: Option<char>,
    }

    impl Lexer {
        fn end_cjk(&mut self) {
            let alone = self.cjk.len() == 1;
            for (i, c) in self.cjk.iter().enumerate() {
                let kind = if alone {
                    Kind::CjkAlone
                } else {
                    Kind::CjkInRun
                };
                self.out.push((c.to_string(), kind));
                if let Some(n) = self.cjk.get(i + 1) {
                    self.out.push(([*c, *n].iter().collect(), Kind::CjkBigram));
                }
            }
            self.cjk.clear();
        }

        fn latin_char(&mut self, c: char) {
            if let Some(j) = self.joiner.take() {
                self.whole.push(j);
            }
            for l in c.to_lowercase() {
                self.word.push(l);
                self.whole.push(l);
            }
        }

        /// A joiner right after a segment may start or extend a compound;
        /// anywhere else (`__init`, `a..b`, `a._b`) it ends the run.
        fn joiner(&mut self, j: char) {
            if self.word.is_empty() {
                self.end_latin();
            } else {
                self.parts.push(std::mem::take(&mut self.word));
                self.joiner = Some(j);
            }
        }

        fn end_latin(&mut self) {
            if !self.word.is_empty() {
                self.parts.push(std::mem::take(&mut self.word));
            }
            self.joiner = None; // a trailing joiner (`invalid_grant.`) is dropped
            let whole = std::mem::take(&mut self.whole);
            let compound = self.parts.len() >= 2 && whole.len() <= MAX_COMPOUND_BYTES;
            let kind = if compound { Kind::Part } else { Kind::Word };
            self.out.extend(self.parts.drain(..).map(|p| (p, kind)));
            if compound {
                self.out.push((whole, Kind::Compound));
            }
        }
    }

    pub fn tokens(text: &str) -> Vec<(String, Kind)> {
        let mut lx = Lexer::default();
        for c in text.chars() {
            if is_cjk(c) {
                lx.end_latin();
                lx.cjk.push(c);
            } else if c.is_alphanumeric() {
                lx.end_cjk();
                lx.latin_char(c);
            } else if COMPOUND_JOINERS.contains(&c) {
                lx.end_cjk();
                lx.joiner(c);
            } else {
                lx.end_cjk();
                lx.end_latin();
            }
        }
        lx.end_cjk();
        lx.end_latin();
        lx.out
    }
}

#[cfg(test)]
mod tests {
    use super::{CJK_UNIGRAM_WEIGHT, Hit, NONE_TERM, SearchIndex, query_terms, tokenize};
    use ox_core::Pred;
    use std::collections::BTreeSet;
    use tantivy::doc;
    use uuid::Uuid;

    /// Texts for the lexer properties: the E1 corpus (every subject and
    /// description) plus Unicode edge cases.
    fn lexer_corpus() -> Vec<String> {
        let mut out: Vec<String> = include_str!("../../../experiments/e1/data/issues.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .flat_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                [
                    v["subject"].as_str().unwrap_or_default().to_string(),
                    v["description"].as_str().unwrap_or_default().to_string(),
                ]
            })
            .collect();
        out.extend(
            [
                "İstanbul İİ_x ǅemal ẞtraße",
                "👨\u{200D}👩\u{200D}👧 家族 e\u{0301}clair café",
                "line1\r\nline2\r\n登入\r\n",
                "__invalid_grant__ a..b a._b e.g. v3.2.0. foo_bar.baz",
                "ＡＢＣ全形１２３ ﬁle",
            ]
            .map(String::from),
        );
        out.push(format!("{}_x", "a".repeat(200)));
        out
    }

    /// Design 03 §3.9 lexer: the same term sequence as before, offsets on
    /// char boundaries, and each range lower-cased gives back its term.
    #[test]
    fn spanned_lexer_matches_old_tokens_and_offsets() {
        use super::{lexemes, v0};
        let corpus = lexer_corpus();
        assert!(corpus.len() > 100, "E1 corpus loaded");
        for text in &corpus {
            let new: Vec<String> = lexemes(text).into_iter().map(|l| l.term).collect();
            let old: Vec<String> = v0::tokens(text).into_iter().map(|(t, _)| t).collect();
            assert_eq!(new, old, "{text}");
            for l in lexemes(text) {
                assert!(l.from < l.to, "{l:?}");
                assert!(text.is_char_boundary(l.from) && text.is_char_boundary(l.to));
                assert_eq!(text[l.from..l.to].to_lowercase(), l.term, "{text}: {l:?}");
            }
        }
    }

    #[test]
    fn lexeme_ranges_cover_whole_identifiers_and_long_lowercase() {
        use super::{Kind, lexemes};
        let text = "x İnvalid_Grant.v2 y";
        let ls = lexemes(text);
        let whole = ls.iter().find(|l| l.kind == Kind::Compound).unwrap();
        assert_eq!(&text[whole.from..whole.to], "İnvalid_Grant.v2");
        assert_eq!(whole.term, "i\u{307}nvalid_grant.v2");
        // İ lower-cases to two chars but keeps its own 2-byte range.
        let first = ls.iter().find(|l| l.kind == Kind::Part).unwrap();
        assert_eq!(&text[first.from..first.to], "İnvalid");
        // The tokenizer now reports the offsets; positions are unchanged.
        use tantivy::tokenizer::{TokenStream, Tokenizer};
        let mut tk = super::CjkBigram;
        let mut s = tk.token_stream("登入 ok");
        let mut seen = Vec::new();
        while s.advance() {
            let t = s.token();
            seen.push((t.position, t.offset_from, t.offset_to, t.text.clone()));
        }
        assert_eq!(
            seen,
            vec![
                (0, 0, 3, "登".into()),
                (1, 0, 6, "登入".into()),
                (2, 3, 6, "入".into()),
                (3, 7, 9, "ok".into())
            ]
        );
    }

    #[test]
    fn identifiers_glued_by_underscore_or_dot_are_kept_whole() {
        let t = |s: &str| tokenize(s);
        assert_eq!(
            t("invalid_grant"),
            vec!["invalid", "grant", "invalid_grant"]
        );
        assert_eq!(t("v3.2.0"), vec!["v3", "2", "0", "v3.2.0"]);
        assert_eq!(t("EXC_BAD_ACCESS"), t("exc_bad_access"));
        // One maximal compound, not every sub-compound.
        assert_eq!(t("foo_bar.baz"), vec!["foo", "bar", "baz", "foo_bar.baz"]);
        assert_eq!(t("foo.bar()"), vec!["foo", "bar", "foo.bar"]);
        // Trailing / leading / doubled / mixed-adjacent joiners do not glue.
        assert_eq!(
            t("invalid_grant."),
            vec!["invalid", "grant", "invalid_grant"]
        );
        assert_eq!(t("__init__"), vec!["init"]);
        assert_eq!(t("a..b"), vec!["a", "b"]);
        assert_eq!(t("a._b"), vec!["a", "b"]);
        assert_eq!(t("e.g."), vec!["e", "g", "e.g"]);
        // `-`, `/`, Unicode dashes and spaces stay separators.
        assert_eq!(t("SSO-Login"), vec!["sso", "login"]);
        assert_eq!(t("foo—bar"), vec!["foo", "bar"]);
        assert_eq!(t("api/v2"), vec!["api", "v2"]);
        // CJK is a hard boundary.
        assert_eq!(
            t("版本v3.2.0修正"),
            vec![
                "版", "版本", "本", "v3", "2", "0", "v3.2.0", "修", "修正", "正"
            ]
        );
        assert_eq!(t("登入.失敗"), vec!["登", "登入", "入", "失", "失敗", "敗"]);
        // Repeats keep their counts.
        assert_eq!(t("a_b a_b"), vec!["a", "b", "a_b", "a", "b", "a_b"]);
        // Over the cap: parts only, as plain words.
        let long = format!("{}_x", "a".repeat(200));
        assert_eq!(t(&long), vec!["a".repeat(200), "x".to_string()]);
        let w = super::COMPOUND_PART_WEIGHT;
        assert_eq!(
            query_terms(&long),
            vec![("a".repeat(200), 1.0), ("x".to_string(), 1.0)]
        );
        assert_eq!(
            query_terms("invalid_grant"),
            vec![
                ("grant".to_string(), w),
                ("invalid".to_string(), w),
                ("invalid_grant".to_string(), 1.0)
            ]
        );
    }

    /// The exact identifier outranks a sibling sharing a segment, and a
    /// version outranks issues that merely contain its digits.
    #[test]
    fn exact_identifier_ranks_first() {
        let (idx, id) = index_with(&[
            ("OAuth 回傳 invalid_request", "token 端點回 invalid_request"),
            (
                "OAuth 回傳 invalid_grant",
                "refresh token 過期後回 invalid_grant",
            ),
            ("匯出 0 筆", "報表只有 2 欄，筆數 0"),
            ("升級到 v3.2.0 後當機", "v3.2.0 啟動就閃退"),
            ("v3.1.0 版本說明", "列出 2 個新功能"),
        ]);
        for (q, exact) in [
            ("invalid_grant", id[1]),
            ("invalid_request", id[0]),
            ("v3.2.0", id[3]),
        ] {
            let hits = idx.search(q, &Pred::True, 5).unwrap();
            assert_eq!(hits[0].issue_id, exact, "{q}");
            // Segment-only matches stay far below (≈0.1× at part weight
            // 0.2; ≈0.3× if parts weighed 1.0 like the whole).
            for h in &hits[1..] {
                assert!(h.score < hits[0].score * 0.15, "{q}: {}", h.score);
            }
        }
        // Written apart, the segments still find it.
        assert!(ids(&idx.search("invalid grant", &Pred::True, 5).unwrap()).contains(&id[1]));
    }

    #[test]
    fn query_terms_add_low_weight_unigrams() {
        let w = CJK_UNIGRAM_WEIGHT;
        let t = |s: &str, w: f32| (s.to_string(), w);
        assert_eq!(
            query_terms("登入 SSO"),
            vec![t("sso", 1.0), t("入", w), t("登", w), t("登入", 1.0)]
        );
        // A lone CJK character is already a full-weight term.
        assert_eq!(query_terms("信"), vec![t("信", 1.0)]);
        assert!(query_terms(" ,. ").is_empty());
    }

    /// In-memory index with public `(subject, description)` docs in one
    /// project; returns it with the doc ids in input order.
    fn index_with(docs: &[(&str, &str)]) -> (SearchIndex, Vec<Uuid>) {
        let idx = SearchIndex::open(None).unwrap();
        let project = Uuid::new_v4().to_string();
        let mut ids = Vec::new();
        {
            let mut w = idx.writer.lock().unwrap();
            let f = idx.f;
            for (i, (subject, description)) in docs.iter().enumerate() {
                let id = Uuid::new_v4();
                ids.push(id);
                w.add_document(doc!(
                    f.id => id.to_string(),
                    f.key => format!("T-{}", i + 1),
                    f.project => project.clone(),
                    f.tracker => Uuid::new_v4().to_string(),
                    f.private => "false",
                    f.author => Uuid::new_v4().to_string(),
                    f.assignee => NONE_TERM,
                    f.subject => *subject,
                    f.description => *description,
                ))
                .unwrap();
            }
            w.commit().unwrap();
        }
        idx.reader.reload().unwrap();
        (idx, ids)
    }

    fn ids(hits: &[Hit]) -> Vec<Uuid> {
        hits.iter().map(|h| h.issue_id).collect()
    }

    const CORPUS: &[(&str, &str)] = &[
        ("結帳頁付款失敗", "按下付款後顯示錯誤，錢沒有扣到"),
        ("登入失敗", "輸入正確密碼仍無法登入，出現 invalid_grant"),
        ("首頁圖片載入太慢", "首頁的橫幅圖片要十秒以上才載入完成"),
        ("SSO 登入後跳回登入頁", "從公司入口登入後又被導回登入頁"),
        ("報表匯出亂碼", "CSV 匯出後中文變成亂碼"),
        (
            "帳號被鎖定",
            "連續輸錯密碼五次後帳號被鎖住，之後就無法登入，要等管理員手動解鎖才能繼續使用系統，客服那邊也查不到被鎖的原因",
        ),
    ];

    /// 付不了錢: none of its bigrams (付不、不了、了錢) occur, so before D15
    /// it had no hits. The unigrams 付／錢 now find the payment issue.
    #[test]
    fn colloquial_query_falls_back_to_unigrams() {
        let (idx, id) = index_with(CORPUS);
        let hits = idx.search("付不了錢", &Pred::True, 5).unwrap();
        assert_eq!(
            ids(&hits).first(),
            Some(&id[0]),
            "付款失敗 should rank first"
        );
    }

    /// 登入 must not put an issue that only shares 入 (via 載入) near the
    /// top: the unigram weight is low enough that every real 登入 match —
    /// including one buried in a long description — still ranks above it.
    #[test]
    fn login_does_not_rank_loading_issue_high() {
        let (idx, id) = index_with(CORPUS);
        let hits = idx.search("登入", &Pred::True, 10).unwrap();
        let ranked = ids(&hits);
        let top3: BTreeSet<Uuid> = ranked.iter().take(3).copied().collect();
        assert_eq!(
            top3,
            BTreeSet::from([id[1], id[3], id[5]]),
            "the three 登入 issues should be the top 3"
        );
        assert!(
            ranked.contains(&id[2]),
            "載入 issue still matches 入 at low weight"
        );
    }

    #[test]
    fn cjk_unigram_bigram_and_words() {
        assert_eq!(
            tokenize("登入失敗 SSO-Login"),
            vec![
                "登", "登入", "入", "入失", "失", "失敗", "敗", "sso", "login"
            ]
        );
        assert_eq!(tokenize("v2版本"), vec!["v2", "版", "版本", "本"]);
        assert!(tokenize("  ,.!").is_empty());
    }

    #[test]
    fn query_uses_bigrams_for_runs() {
        use super::tokenize_query;
        assert_eq!(tokenize_query("登入"), vec!["登入"]);
        assert_eq!(
            tokenize_query("登入失敗 sso"),
            vec!["登入", "入失", "失敗", "sso"]
        );
        assert_eq!(tokenize_query("信"), vec!["信"]);
        assert_eq!(tokenize_query("寄信 v2"), vec!["寄信", "v2"]);
    }
}
