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
//!   token `c1.<epoch>.<X>` is satisfied when `X < F`.
//! * Generations (03 §4.2, §4.8): `<dir>/CURRENT` names `gen-<uuid>/`; each
//!   commit carries a payload (cursor, F, epoch, versions), so a restart
//!   resumes from what the index durably holds. A rebuild builds a new
//!   generation from a snapshot while the old one serves, then switches.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use ox_core::Pred;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, EmptyQuery, Occur, Query, RangeQuery,
    TermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, STORED, STRING, Schema, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term, doc,
};
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

/// Index schema version (design 03 §4.8 payload `schema_version`):
/// 2 added `search_rev` and the §17.15 fast fields.
pub const SCHEMA_VERSION: u32 = 2;
/// Bump when the lexer changes what it emits.
pub const ANALYZER_VERSION: u32 = 1;
/// Bump when query weights or field boosts change.
pub const QUERY_SEMANTICS_VERSION: u32 = 1;
const PAYLOAD_V: u32 = 1;
/// Serialises lease registration, retirement and the outbox GC bound (03 §4.8 step 1, §4.9).
const LEASE_LOCK: i64 = 0x6f78_7365_6172_6368; // "oxsearch"
/// Issues per page of the snapshot scan.
const BASELINE_PAGE: i64 = 2000;
/// A rebuilt generation stays on disk this long after the switch (03 §4.8 step 7).
const RETAIN_HOURS: i32 = 24;
/// Durable payload for frontier-only progress at most this often (03 §4.2).
const DURABLE_INTERVAL: Duration = Duration::from_secs(10);
/// Issues the switch health check samples (03 §4.8 step 6).
const HEALTH_SAMPLE: i64 = 200;

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
    /// §17.15: the version id, or `none`.
    fixed_version: Field,
    /// §17.15: days since 1970-01-01; no value when NULL.
    start_date: Field,
    due_date: Field,
    /// Names of the date fields that are NULL (`start_date`, `due_date`).
    nulls: Field,
    /// 03 §3.2: sha256 over the indexed text, STORED.
    search_rev: Field,
}

impl Fields {
    fn build() -> (Schema, Fields) {
        let mut sb = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let date = NumericOptions::default().set_indexed().set_fast();
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
            fixed_version: sb.add_text_field("fixed_version", STRING),
            start_date: sb.add_i64_field("start_date", date.clone()),
            due_date: sb.add_i64_field("due_date", date),
            nulls: sb.add_text_field("nulls", STRING),
            search_rev: sb.add_bytes_field("search_rev", STORED),
        };
        (sb.build(), f)
    }

    /// The fields of an existing index, or `None` if its schema is not ours.
    fn from_schema(s: &Schema) -> Option<Fields> {
        let (want, f) = Fields::build();
        (s == &want).then_some(f)
    }
}

/// `search_rev` (design 03 §3.2): does an index document still hold the
/// text SQL has now? sha256("v1" ‖ len ‖ subject ‖ len ‖ description), the
/// lengths as big-endian u64 byte counts.
pub fn search_rev(subject: &str, description: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"v1");
    h.update((subject.len() as u64).to_be_bytes());
    h.update(subject.as_bytes());
    h.update((description.len() as u64).to_be_bytes());
    h.update(description.as_bytes());
    h.finalize().into()
}

fn epoch_day(d: NaiveDate) -> i64 {
    (d - NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch")).num_days()
}

/// A date column the index can filter on (§17.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateField {
    Start,
    Due,
}

impl DateField {
    fn null_term(self) -> &'static str {
        match self {
            DateField::Start => "start_date",
            DateField::Due => "due_date",
        }
    }
}

/// Index-side pre-filter on the §17.15 fast fields. It only narrows the
/// candidates: the SQL re-check stays the authority, so a translation may
/// be a superset ([`IxFilter::All`] when unsure) but never a subset.
#[derive(Debug, Clone, PartialEq)]
pub enum IxFilter {
    All,
    And(Vec<IxFilter>),
    Or(Vec<IxFilter>),
    /// Documents the inner filter does not match.
    Not(Box<IxFilter>),
    /// `fixed_version_id` equals this (`None`: no version).
    Version(Option<Uuid>),
    /// The date is set and inside the bounds.
    Date {
        field: DateField,
        lo: Bound<NaiveDate>,
        hi: Bound<NaiveDate>,
    },
    /// The date is NULL.
    DateNull(DateField),
}

impl IxFilter {
    pub fn and(parts: Vec<IxFilter>) -> IxFilter {
        let parts: Vec<IxFilter> = parts.into_iter().filter(|p| *p != IxFilter::All).collect();
        match parts.len() {
            0 => IxFilter::All,
            1 => parts.into_iter().next().expect("one"),
            _ => IxFilter::And(parts),
        }
    }
}

/// Where a generation's replay stands: the last applied outbox row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Cursor {
    pub xid: u64,
    pub id: i64,
}

/// The commit payload (03 §4.2 step 5, §4.8): what this generation durably holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Payload {
    v: u32,
    data_epoch: Uuid,
    generation: Uuid,
    schema_version: u32,
    analyzer_version: u32,
    query_semantics_version: u32,
    /// xid8 as decimal strings (JSON numbers lose precision past 2^53 in JS).
    cursor_xid: String,
    cursor_id: i64,
    complete_before_xid: String,
}

impl Payload {
    fn parse(s: &str) -> Option<(Payload, Cursor, u64)> {
        let p: Payload = serde_json::from_str(s).ok()?;
        if p.v != PAYLOAD_V
            || p.schema_version != SCHEMA_VERSION
            || p.analyzer_version != ANALYZER_VERSION
            || p.query_semantics_version != QUERY_SEMANTICS_VERSION
        {
            return None;
        }
        let c = Cursor {
            xid: p.cursor_xid.parse().ok()?,
            id: p.cursor_id,
        };
        let f = p.complete_before_xid.parse().ok()?;
        Some((p, c, f))
    }
}

struct GenState {
    epoch: Uuid,
    /// Committed replay position (in the last durable payload).
    cursor: Cursor,
    /// Exclusive frontier of what the reader shows (may run ahead of `durable_f`).
    f: u64,
    durable_f: u64,
    durable_at: Instant,
}

/// One index generation (03 §4.8): its own directory, writer and progress.
struct Gen {
    id: Uuid,
    dir: Option<PathBuf>,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    f: Fields,
    st: Mutex<GenState>,
}

fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| anyhow::anyhow!("search index lock poisoned"))
}

fn register_tokenizer(index: &Index) {
    index
        .tokenizers()
        .register(TOKENIZER, TextAnalyzer::builder(CjkBigram).build());
}

impl Gen {
    fn consumer(&self) -> String {
        format!("search@{}", self.id)
    }

    /// A new, empty generation (`root` None: in memory).
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
        register_tokenizer(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = Mutex::new(index.writer(50_000_000)?);
        Ok(Gen {
            id,
            dir,
            reader,
            writer,
            f,
            st: Mutex::new(st),
        })
    }

    /// Opens `root/CURRENT`'s generation if its payload is one this build
    /// understands. Its `data_epoch` is checked against PostgreSQL before it
    /// serves (03 §4.2 restart table).
    fn open_current(root: &Path) -> Result<Option<Gen>> {
        let Ok(name) = std::fs::read_to_string(root.join("CURRENT")) else {
            return Ok(None);
        };
        let name = name.trim();
        let Some(id) = name
            .strip_prefix("gen-")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            tracing::warn!("search index CURRENT is not a generation: {name:?}");
            return Ok(None);
        };
        let dir = root.join(name);
        let index = match Index::open_in_dir(&dir) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("search generation {id} does not open: {e:#}");
                return Ok(None);
            }
        };
        let Some(f) = Fields::from_schema(&index.schema()) else {
            tracing::warn!("search generation {id} has another schema; rebuilding");
            return Ok(None);
        };
        let payload = index.load_metas()?.payload;
        let Some((p, cursor, fr)) = payload.as_deref().and_then(Payload::parse) else {
            tracing::warn!("search generation {id}: payload missing or unsupported; rebuilding");
            return Ok(None);
        };
        if p.generation != id {
            tracing::warn!("search generation {id}: payload names {}", p.generation);
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

    fn payload(&self, epoch: Uuid, c: Cursor, f: u64) -> String {
        serde_json::to_string(&Payload {
            v: PAYLOAD_V,
            data_epoch: epoch,
            generation: self.id,
            schema_version: SCHEMA_VERSION,
            analyzer_version: ANALYZER_VERSION,
            query_semantics_version: QUERY_SEMANTICS_VERSION,
            cursor_xid: c.xid.to_string(),
            cursor_id: c.id,
            complete_before_xid: f.to_string(),
        })
        .expect("payload serialises")
    }

    /// Commits pending writes together with the payload (03 §4.2 step 5).
    /// On failure the writer rolls back: the durable state is the last payload.
    fn commit(&self, epoch: Uuid, c: Cursor, f: u64) -> Result<()> {
        let payload = self.payload(epoch, c, f);
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

    fn make_doc(&self, d: &IssueDoc) -> TantivyDocument {
        let f = self.f;
        let mut doc = doc!(
            f.id => d.id.to_string(),
            f.key => d.key.clone(),
            f.project => d.project_id.to_string(),
            f.tracker => d.tracker_id.to_string(),
            f.private => if d.is_private { "true" } else { "false" },
            f.author => d.author_id.to_string(),
            f.assignee => d.assigned_to_id.map(|a| a.to_string()).unwrap_or_else(|| NONE_TERM.into()),
            f.subject => d.subject.clone(),
            f.description => d.description.clone(),
            f.fixed_version => d.fixed_version_id.map(|v| v.to_string()).unwrap_or_else(|| NONE_TERM.into()),
            f.search_rev => search_rev(&d.subject, &d.description).to_vec(),
        );
        for (field, v, which) in [
            (f.start_date, d.start_date, DateField::Start),
            (f.due_date, d.due_date, DateField::Due),
        ] {
            match v {
                Some(day) => doc.add_i64(field, epoch_day(day)),
                None => doc.add_text(f.nulls, which.null_term()),
            }
        }
        doc
    }

    fn view(&self, f: u64) -> Arc<SearchView> {
        Arc::new(SearchView {
            searcher: self.reader.searcher(),
            f,
            fields: self.f,
            generation: self.id,
        })
    }
}

/// What one search reads (03 §4.4): a reader pinned with the frontier it was
/// published with. A search keeps the view it started with, so it never
/// switches reader half-way (also across a blue-green switch).
pub struct SearchView {
    searcher: Searcher,
    f: u64,
    fields: Fields,
    generation: Uuid,
}

pub struct Hit {
    pub issue_id: Uuid,
    pub score: f32,
    /// The indexed text's `search_rev`.
    pub search_rev: Option<[u8; 32]>,
}

impl SearchView {
    /// Exclusive frontier F of this view.
    pub fn frontier(&self) -> u64 {
        self.f
    }

    pub fn generation(&self) -> Uuid {
        self.generation
    }

    /// Documents in this view.
    pub fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }

    fn term(&self, field: Field, v: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, v),
            IndexRecordOption::Basic,
        ))
    }

    fn filter(&self, pred: &Pred) -> Box<dyn Query> {
        let f = self.fields;
        let any_of = |field: Field, ids: &[Uuid]| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(
                ids.iter()
                    .map(|i| (Occur::Should, self.term(field, &i.to_string())))
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
            Pred::ProjectIs(id) => self.term(f.project, &id.to_string()),
            Pred::TrackerIn(ids) => any_of(f.tracker, ids),
            Pred::NotPrivate => self.term(f.private, "false"),
            Pred::AuthorIs(id) => self.term(f.author, &id.to_string()),
            Pred::AssigneeIn(ids) => any_of(f.assignee, ids),
        }
    }

    fn ix_query(&self, x: &IxFilter) -> Box<dyn Query> {
        let f = self.fields;
        match x {
            IxFilter::All => Box::new(AllQuery),
            IxFilter::And(ps) => Box::new(BooleanQuery::new(
                ps.iter().map(|p| (Occur::Must, self.ix_query(p))).collect(),
            )),
            IxFilter::Or(ps) => Box::new(BooleanQuery::new(
                ps.iter()
                    .map(|p| (Occur::Should, self.ix_query(p)))
                    .collect(),
            )),
            IxFilter::Not(p) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
                (Occur::MustNot, self.ix_query(p)),
            ])),
            IxFilter::Version(v) => self.term(
                f.fixed_version,
                &v.map(|v| v.to_string())
                    .unwrap_or_else(|| NONE_TERM.to_string()),
            ),
            IxFilter::DateNull(d) => self.term(f.nulls, d.null_term()),
            IxFilter::Date { field, lo, hi } => {
                let fld = match field {
                    DateField::Start => f.start_date,
                    DateField::Due => f.due_date,
                };
                let t = |d: &NaiveDate| Term::from_field_i64(fld, epoch_day(*d));
                Box::new(RangeQuery::new(lo.as_ref().map(t), hi.as_ref().map(t)))
            }
        }
    }

    /// BM25 over subject (×3) and description (×1), restricted to `filter`
    /// and the fast-field pre-filter `ix`. Each term's boost is further
    /// scaled by its [`query_terms`] weight. Empty `q` lists every visible
    /// document (used by tests).
    pub fn search(&self, q: &str, filter: &Pred, ix: &IxFilter, limit: usize) -> Result<Vec<Hit>> {
        let terms = query_terms(q);
        let text: Box<dyn Query> = if terms.is_empty() {
            Box::new(AllQuery)
        } else {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (t, w) in &terms {
                for (field, boost) in [(self.fields.subject, 3.0), (self.fields.description, 1.0)] {
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
        // Filters must not change BM25 scores, so they score a constant 0.
        let mut must: Vec<(Occur, Box<dyn Query>)> = vec![
            (Occur::Must, text),
            (
                Occur::Must,
                Box::new(ConstScoreQuery::new(self.filter(filter), 0.0)),
            ),
        ];
        if *ix != IxFilter::All {
            must.push((
                Occur::Must,
                Box::new(ConstScoreQuery::new(self.ix_query(ix), 0.0)),
            ));
        }
        let query = BooleanQuery::new(must);
        let top = self
            .searcher
            .search(&query, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = self.searcher.doc(addr)?;
            if let Some(id) = d.get_first(self.fields.id).and_then(|v| v.as_str()) {
                let rev = d
                    .get_first(self.fields.search_rev)
                    .and_then(|v| v.as_bytes())
                    .and_then(|b| <[u8; 32]>::try_from(b).ok());
                hits.push(Hit {
                    issue_id: id.parse()?,
                    score,
                    search_rev: rev,
                });
            }
        }
        Ok(hits)
    }

    /// The stored `search_rev` of one issue's document, if it has one.
    fn rev_of(&self, id: Uuid) -> Result<Option<[u8; 32]>> {
        let q = TermQuery::new(
            Term::from_field_text(self.fields.id, &id.to_string()),
            IndexRecordOption::Basic,
        );
        let top = self
            .searcher
            .search(&q, &TopDocs::with_limit(2).order_by_score())?;
        let Some((_, addr)) = top.first() else {
            return Ok(None);
        };
        let d: TantivyDocument = self.searcher.doc(*addr)?;
        Ok(d.get_first(self.fields.search_rev)
            .and_then(|v| v.as_bytes())
            .and_then(|b| <[u8; 32]>::try_from(b).ok()))
    }

    /// Does exactly one document for `d.id` carry `d`'s visibility and
    /// §17.15 facts? (switch health check)
    fn facts_match(&self, d: &IssueDoc) -> Result<bool> {
        let f = self.fields;
        let mut parts: Vec<(Occur, Box<dyn Query>)> = vec![
            (Occur::Must, self.term(f.id, &d.id.to_string())),
            (Occur::Must, self.term(f.key, &d.key)),
            (Occur::Must, self.term(f.project, &d.project_id.to_string())),
            (Occur::Must, self.term(f.tracker, &d.tracker_id.to_string())),
            (
                Occur::Must,
                self.term(f.private, if d.is_private { "true" } else { "false" }),
            ),
            (Occur::Must, self.term(f.author, &d.author_id.to_string())),
            (
                Occur::Must,
                self.term(
                    f.assignee,
                    &d.assigned_to_id
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| NONE_TERM.into()),
                ),
            ),
            (
                Occur::Must,
                self.ix_query(&IxFilter::Version(d.fixed_version_id)),
            ),
        ];
        for (v, which) in [
            (d.start_date, DateField::Start),
            (d.due_date, DateField::Due),
        ] {
            let q = match v {
                Some(day) => IxFilter::Date {
                    field: which,
                    lo: Bound::Included(day),
                    hi: Bound::Included(day),
                },
                None => IxFilter::DateNull(which),
            };
            parts.push((Occur::Must, self.ix_query(&q)));
        }
        let n = self
            .searcher
            .search(&BooleanQuery::new(parts), &tantivy::collector::Count)?;
        Ok(n == 1)
    }
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
    fixed_version_id: Option<Uuid>,
    start_date: Option<NaiveDate>,
    due_date: Option<NaiveDate>,
}

const DOC_SELECT: &str =
    "SELECT i.id, k.key, i.project_id, i.tracker_id, i.is_private, i.author_id,
        i.assigned_to_id, i.subject, i.description, i.fixed_version_id, i.start_date, i.due_date
 FROM issues i JOIN issue_keys k ON k.issue_id = i.id AND k.is_current";

/// Result of a blue-green rebuild (03 §4.8).
#[derive(Debug, Clone, Serialize)]
pub struct RebuildReport {
    pub generation: Uuid,
    pub previous: Option<Uuid>,
    /// Issues in the snapshot baseline.
    pub baseline_docs: u64,
    /// F0: the baseline is complete for every xid below it.
    pub baseline_frontier: u64,
    /// F of the new generation when it started serving.
    pub frontier: u64,
}

/// Is a rebuilt generation far enough to replace the serving one (03 §4.8
/// step 6, Q-S8)? Its F must not be behind, and it must have replayed at
/// least every event the old one applied: equal F with a shorter cursor
/// could still miss content the old view already shows.
pub fn switch_ready(new_f: u64, new_c: Cursor, old_f: u64, old_c: Cursor) -> bool {
    new_f >= old_f && new_c >= old_c
}

/// Outbox rows a round reads: (id, xid8 as text, issue id).
type Events = sqlx::types::Json<Vec<OutboxEvent>>;

pub struct SearchIndex {
    root: Option<PathBuf>,
    /// The generation that serves, once validated against PostgreSQL.
    active: RwLock<Option<Arc<Gen>>>,
    /// Read from `CURRENT` at open; serves only after its epoch is checked.
    loaded: Mutex<Option<Arc<Gen>>>,
    /// A generation being rebuilt (baseline done, catching up).
    pending: Mutex<Option<Arc<Gen>>>,
    view: RwLock<Option<Arc<SearchView>>>,
    /// The published exclusive frontier F (03 §4.1): every transaction with
    /// xid < F has its search events applied to the current view. Never
    /// moves backwards, also across a switch.
    frontier: tokio::sync::watch::Sender<u64>,
    waiters: AtomicUsize,
    per_caller: Mutex<HashMap<Option<Uuid>, usize>>,
    /// One round at a time (03 §4.2 has a single writer per generation);
    /// the switch also runs under it, so the old generation cannot move
    /// while the new one is compared with it.
    round: tokio::sync::Mutex<()>,
    /// One rebuild at a time.
    rebuilding: tokio::sync::Mutex<()>,
    durable_interval: Mutex<Duration>,
    pub metrics: crate::metrics::SearchMetrics,
}

impl SearchIndex {
    /// Opens the index under `dir` (`None`: in memory, for tests). Nothing
    /// serves until the first sync round has checked the generation in
    /// `CURRENT` against PostgreSQL, or built one from a snapshot.
    pub fn open(dir: Option<&Path>) -> Result<Self> {
        let loaded = match dir {
            None => None,
            Some(d) => {
                std::fs::create_dir_all(d)?;
                Gen::open_current(d)?.map(Arc::new)
            }
        };
        Ok(SearchIndex {
            root: dir.map(Path::to_path_buf),
            active: RwLock::new(None),
            loaded: Mutex::new(loaded),
            pending: Mutex::new(None),
            view: RwLock::new(None),
            frontier: tokio::sync::watch::Sender::new(0),
            waiters: AtomicUsize::new(0),
            per_caller: Mutex::new(HashMap::new()),
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
        *self.frontier.borrow()
    }

    /// The view searches read now; `None` while no generation can serve
    /// (first start, or a rebuild after an unusable index).
    pub fn view(&self) -> Option<Arc<SearchView>> {
        self.view.read().ok().and_then(|v| v.clone())
    }

    /// The serving generation's id.
    pub fn generation(&self) -> Option<Uuid> {
        self.active
            .read()
            .ok()
            .and_then(|a| a.as_ref().map(|g| g.id))
    }

    /// Durable (cursor, F) of the serving generation, as its payload has it.
    pub fn durable_state(&self) -> Option<(Cursor, u64)> {
        let g = self.active.read().ok()?.clone()?;
        let st = g.st.lock().ok()?;
        Some((st.cursor, st.durable_f))
    }

    /// Searches the current view (tests and tools; services pin a view).
    pub fn search(&self, q: &str, filter: &Pred, limit: usize) -> Result<Vec<Hit>> {
        match self.view() {
            Some(v) => v.search(q, filter, &IxFilter::All, limit),
            None => anyhow::bail!("search index not ready"),
        }
    }

    /// Publishes `gen` at frontier `f`: the view first, then F, so a waiter
    /// that sees F > X always gets a view whose F > X.
    fn publish(&self, g: &Gen, f: u64) -> Result<()> {
        let f = f.max(self.frontier());
        let v = g.view(f);
        *self
            .view
            .write()
            .map_err(|_| anyhow::anyhow!("view lock poisoned"))? = Some(v);
        self.frontier.send_if_modified(|cur| {
            if f > *cur {
                *cur = f;
                true
            } else {
                false
            }
        });
        self.metrics.published(f);
        Ok(())
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

    /// Tests: sync until every outbox row written so far is indexed.
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
            // Done when the cursor passed the target row, or the baseline
            // snapshot already covered it (F above its xid).
            if let Some((c, f)) = at
                && (c >= want || want.xid < f)
            {
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

    /// One round of 03 §4.2 for the serving generation (and for a
    /// generation being rebuilt): (rows applied, a full batch so more may
    /// follow). The first round validates the generation found on disk or
    /// builds one from a snapshot.
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
        if let Some(p) = pending {
            // Keep the rebuilt generation close behind; its errors do not
            // stop the serving one.
            if let Err(e) = self.round_on(pool, &p, false).await {
                tracing::warn!("search rebuild catch-up failed: {e:#}");
            }
        }
        Ok(r)
    }

    /// Validates the generation from `CURRENT` (03 §4.2 restart table) or
    /// builds one from a snapshot, and makes it serve.
    async fn activate(&self, pool: &PgPool) -> Result<Arc<Gen>> {
        let epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
            .fetch_one(pool)
            .await?;
        // `loaded` stays until the generation serves: a failed activation
        // (a transient DB error) retries it instead of rebuilding.
        let loaded = lock(&self.loaded)?.clone();
        let g = match loaded {
            Some(g) if lock(&g.st)?.epoch == epoch => g,
            Some(g) => {
                tracing::warn!(
                    "search generation {} belongs to another data_epoch; rebuilding",
                    g.id
                );
                Arc::new(self.build_baseline(pool).await?)
            }
            None => Arc::new(self.build_baseline(pool).await?),
        };
        // PostgreSQL's cursor follows the payload, ahead or behind (03 §4.2):
        // the payload is what the index durably holds.
        let (cursor, f) = {
            let st = lock(&g.st)?;
            (st.cursor, st.f)
        };
        let ready = async {
            self.mark_active(pool, &g, cursor).await?;
            if let Some(root) = &self.root {
                write_current(root, g.id)?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = ready {
            // Keep what was opened or built for the next round.
            *lock(&self.loaded)? = Some(g);
            return Err(e);
        }
        *lock(&self.loaded)? = None;
        *self
            .active
            .write()
            .map_err(|_| anyhow::anyhow!("active lock poisoned"))? = Some(g.clone());
        self.publish(&g, f)?;
        Ok(g)
    }

    /// This generation's consumer row becomes `active`; any other active
    /// search generation (an interrupted switch) is retained, and leftover
    /// `bootstrapping` rows (an interrupted rebuild) retire.
    async fn mark_active(&self, pool: &PgPool, g: &Gen, c: Cursor) -> Result<()> {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(LEASE_LOCK)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE outbox_cursors SET state = 'retained', updated_at = now(),
                    retained_until = now() + make_interval(hours => $2)
             WHERE consumer LIKE 'search@%' AND consumer <> $1 AND state = 'active'",
        )
        .bind(g.consumer())
        .bind(RETAIN_HOURS)
        .execute(&mut *tx)
        .await?;
        let pending = lock(&self.pending)?.as_ref().map(|p| p.consumer());
        sqlx::query(
            "UPDATE outbox_cursors SET state = 'retired', updated_at = now()
             WHERE consumer LIKE 'search@%' AND consumer <> $1 AND state = 'bootstrapping'
               AND ($2::text IS NULL OR consumer <> $2)",
        )
        .bind(g.consumer())
        .bind(pending)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO outbox_cursors (consumer, last_xid, last_id, state)
             VALUES ($1, $2::text::xid8, $3, 'active')
             ON CONFLICT (consumer) DO UPDATE SET last_xid = EXCLUDED.last_xid,
                 last_id = EXCLUDED.last_id, state = 'active', retained_until = NULL, updated_at = now()",
        )
        .bind(g.consumer())
        .bind(c.xid.to_string())
        .bind(c.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// One 03 §4.2 round on `gen`; `serve` publishes the result.
    async fn round_on(&self, pool: &PgPool, g: &Gen, serve: bool) -> Result<(usize, bool)> {
        let (epoch, cursor, f_old, durable_f, durable_at) = {
            let st = lock(&g.st)?;
            (st.epoch, st.cursor, st.f, st.durable_f, st.durable_at)
        };
        // One statement: H and the visible events come from one snapshot,
        // and H is there even with zero events (03 §4.2; FILTER keeps the
        // empty case `[]`). The cursor is the payload's, not PostgreSQL's.
        let (h, events, db_epoch): (String, Events, Uuid) = sqlx::query_as(
            "WITH h AS MATERIALIZED (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS h),
                  ev AS (SELECT o.id, o.xid, o.entity_id FROM outbox o, h
                         WHERE o.topic = 'issue' AND o.xid < h.h
                           AND (o.xid, o.id) > ($1::text::xid8, $2::bigint)
                         ORDER BY o.xid, o.id LIMIT $3)
             SELECT h.h::text,
                    COALESCE(json_agg(json_build_array(ev.id, ev.xid::text, ev.entity_id)
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
            // The data was reset under us (03 §4.3): stop advancing; the
            // next start rebuilds. Tokens of the new epoch keep lagging.
            anyhow::bail!("search data_epoch changed; restart to rebuild the index");
        }
        let rows = events.0;
        let h: u64 = h.parse().context("snapshot xmin")?;
        let Some((last_id, last_xid, _)) = rows.last().cloned() else {
            // Drained: everything below H is applied. The view keeps its
            // searcher; the payload records F only now and then.
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
        let mut ids: Vec<Uuid> = rows.iter().map(|r| r.2).collect();
        ids.sort();
        ids.dedup();
        let docs: Vec<IssueDoc> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{DOC_SELECT} WHERE i.id = ANY($1) AND i.archived_at IS NULL"
        )))
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        {
            let w = lock(&g.writer)?;
            for id in &ids {
                w.delete_term(Term::from_field_text(g.f.id, &id.to_string()));
            }
            for d in &docs {
                w.add_document(g.make_doc(d))?;
            }
        }
        let full = rows.len() as i64 == SYNC_BATCH;
        // A full batch may stop inside one xid: F' = last.xid, not +1.
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
        // Durable first (payload with the documents), then the reader, then
        // the view: never an old searcher with a new F.
        g.commit(epoch, c_new, f_new)?;
        {
            let mut st = lock(&g.st)?;
            st.cursor = c_new;
            st.f = f_new;
            st.durable_f = f_new;
            st.durable_at = Instant::now();
        }
        g.reader.reload()?;
        // PostgreSQL's cursor last: coordination and GC only (03 §4.2 step 7).
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
            #[cfg(feature = "test-support")]
            crate::changesets::faults::hit("search.before_publish").await?;
            self.publish(g, f_new)?;
            self.metrics.applied(pool, &rows).await;
        }
        Ok((rows.len(), full))
    }

    /// Builds a generation from a snapshot (03 §4.8 steps 1–4): lease
    /// first, then the snapshot, then the payload `(F0, 0)` / `F0`.
    async fn build_baseline(&self, pool: &PgPool) -> Result<Gen> {
        let epoch: Uuid = sqlx::query_scalar("SELECT data_epoch FROM search_settings")
            .fetch_one(pool)
            .await?;
        let g = Gen::create(self.root.as_deref(), epoch)?;
        // 1. The lease, before the snapshot: the GC sees it and keeps every
        //    row from the slowest live consumer on.
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(LEASE_LOCK)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO outbox_cursors (consumer, last_xid, last_id, state)
             VALUES ($1, COALESCE((SELECT last_xid FROM outbox_cursors
                                   WHERE state IN ('active', 'bootstrapping', 'retained')
                                   ORDER BY last_xid LIMIT 1), '0'::xid8), 0, 'bootstrapping')",
        )
        .bind(g.consumer())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let r = self.scan_snapshot(pool, &g).await;
        match r {
            Ok(()) => Ok(g),
            Err(e) => {
                self.retire(pool, &g).await;
                Err(e)
            }
        }
    }

    async fn scan_snapshot(&self, pool: &PgPool, g: &Gen) -> Result<()> {
        #[cfg(feature = "test-support")]
        crate::changesets::faults::hit("search.before_snapshot").await?;
        // 2–3. One read-only snapshot (no xid, so it does not hold back the
        //      frontier, 03 §4.7); issues stream out page by page.
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
            let page: Vec<IssueDoc> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{DOC_SELECT} WHERE i.archived_at IS NULL AND i.id > $1 ORDER BY i.id LIMIT $2"
            )))
            .bind(after)
            .bind(BASELINE_PAGE)
            .fetch_all(&mut *tx)
            .await?;
            let Some(last) = page.last() else { break };
            after = last.id;
            let w = lock(&g.writer)?;
            for d in &page {
                w.add_document(g.make_doc(d))?;
            }
            n += page.len() as u64;
        }
        // The same set the scan reads (an issue without a current key, e.g.
        // from an import, is not indexed; it must not block the baseline).
        let (expected, all): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT (SELECT count(*) FROM ({DOC_SELECT} WHERE i.archived_at IS NULL) d),
                    (SELECT count(*) FROM issues WHERE archived_at IS NULL)"
        )))
        .fetch_one(&mut *tx)
        .await?;
        if all != expected {
            tracing::warn!(
                "{} unarchived issues have no current key and are not searchable",
                all - expected
            );
        }
        tx.commit().await?;
        // 4. The generation now claims everything below F0.
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
                "search baseline has {docs} documents, the snapshot {expected} issues (scanned {n})"
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

    /// A failed rebuild: the lease retires (stops holding back the GC) and
    /// the directory goes.
    async fn retire(&self, pool: &PgPool, g: &Gen) {
        let r = sqlx::query(
            "UPDATE outbox_cursors SET state = 'retired', retained_until = NULL, updated_at = now()
             WHERE consumer = $1",
        )
        .bind(g.consumer())
        .execute(pool)
        .await;
        if let Err(e) = r {
            tracing::warn!("retiring search generation {}: {e:#}", g.id);
        }
        if let Some(d) = &g.dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Blue-green rebuild (03 §4.8): the serving generation keeps serving
    /// while a new one is built from a snapshot and catches up; the switch
    /// happens only when the new one is not behind (Q-S8) and passes the
    /// health check. Replaces the old `rebuild()` that emptied the index.
    pub async fn rebuild(&self, pool: &PgPool) -> Result<RebuildReport> {
        let _one = self.rebuilding.lock().await;
        // Something must serve first (a fresh start builds it here).
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
                self.retire(pool, &g).await;
                Err(e)
            }
        }
    }

    async fn catch_up_and_switch(
        &self,
        pool: &PgPool,
        g: &Arc<Gen>,
    ) -> Result<(Option<Uuid>, u64)> {
        // 5. Catch up outside the switch: other rounds keep serving.
        for _ in 0..100_000 {
            let _round = self.round.lock().await;
            let (_, more) = self.round_on(pool, g, false).await?;
            if !more {
                break;
            }
        }
        #[cfg(feature = "test-support")]
        crate::changesets::faults::hit("search.before_switch").await?;
        // 6. The switch, under the round lock: the old generation stands still.
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
            anyhow::bail!("rebuilt search generation did not catch up with the serving one");
        }
        self.health_check(pool, g).await?;
        // Durable before CURRENT names it.
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
        // PostgreSQL first: if it fails nothing has switched (the caller
        // retires the new generation, the old one keeps serving). From here
        // on the switch is done and nothing undoes it.
        self.mark_active(pool, g, c).await?;
        if let Some(root) = &self.root
            && let Err(e) = write_current(root, g.id)
        {
            // A restart then opens the old generation, which re-marks
            // itself active: safe, only this switch is lost.
            tracing::error!(
                "search generation {} serves but CURRENT was not written: {e:#}",
                g.id
            );
        }
        *self
            .active
            .write()
            .map_err(|_| anyhow::anyhow!("active lock poisoned"))? = Some(g.clone());
        self.publish(g, f)?;
        self.metrics.switched();
        Ok((old.map(|o| o.id), f.max(old_f)))
    }

    /// 03 §4.8 step 6 health check (not a correctness proof): a sample of
    /// issues must match the new generation, or be explained by events it
    /// has not applied yet.
    async fn health_check(&self, pool: &PgPool, g: &Gen) -> Result<()> {
        let c = lock(&g.st)?.cursor;
        let view = g.view(0);
        let sample: Vec<IssueDoc> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{DOC_SELECT} WHERE i.archived_at IS NULL ORDER BY random() LIMIT {HEALTH_SAMPLE}"
        )))
        .fetch_all(pool)
        .await?;
        let mut bad: Vec<Uuid> = Vec::new();
        for d in &sample {
            let rev_ok = view.rev_of(d.id)? == Some(search_rev(&d.subject, &d.description));
            if !rev_ok || !view.facts_match(d)? {
                bad.push(d.id);
            }
        }
        if bad.is_empty() {
            return Ok(());
        }
        let unexplained: Vec<Uuid> = sqlx::query_scalar(
            "SELECT b FROM unnest($1::uuid[]) AS b
             WHERE NOT EXISTS (SELECT 1 FROM outbox o WHERE o.topic = 'issue' AND o.entity_id = b
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
                "rebuilt search generation differs from PostgreSQL for {} sampled issues (e.g. {}); switch aborted",
                unexplained.len(),
                unexplained[0]
            )
        }
    }

    /// Outbox GC (03 §4.9) and retained generations past their window
    /// (§4.8 step 7). Returns (outbox rows deleted, generations removed).
    pub async fn gc(&self, pool: &PgPool) -> Result<(u64, usize)> {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(LEASE_LOCK)
            .execute(&mut *tx)
            .await?;
        let expired: Vec<String> = sqlx::query_scalar(
            "UPDATE outbox_cursors SET state = 'retired', retained_until = NULL, updated_at = now()
             WHERE consumer LIKE 'search@%' AND state = 'retained' AND retained_until < now()
             RETURNING consumer",
        )
        .fetch_all(&mut *tx)
        .await?;
        // Only durable progress counts (PostgreSQL's cursor trails the
        // payload); no consumer at all means nothing may go.
        let deleted = sqlx::query(
            "WITH b AS (SELECT last_xid FROM outbox_cursors
                        WHERE state IN ('active', 'bootstrapping', 'retained')
                        ORDER BY last_xid LIMIT 1)
             DELETE FROM outbox o USING b
             WHERE o.xid < b.last_xid AND o.created_at < clock_timestamp() - interval '1 hour'",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let retired: Vec<String> = sqlx::query_scalar(
            "DELETE FROM outbox_cursors WHERE consumer LIKE 'search@%' AND state = 'retired'
             RETURNING consumer",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut removed = 0;
        if let Some(root) = &self.root {
            let live: Vec<Uuid> = [
                self.generation(),
                lock(&self.pending)?.as_ref().map(|g| g.id),
            ]
            .into_iter()
            .flatten()
            .collect();
            for c in retired.iter().chain(expired.iter()) {
                if let Some(id) = c
                    .strip_prefix("search@")
                    .and_then(|s| Uuid::parse_str(s).ok())
                    && !live.contains(&id)
                {
                    let d = root.join(format!("gen-{id}"));
                    if d.exists() && std::fs::remove_dir_all(&d).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        Ok((deleted, removed))
    }
}

/// `CURRENT` names the serving generation: temp file, fsync, rename, fsync
/// the directory (03 §4.8 step 6.2).
fn write_current(root: &Path, id: Uuid) -> Result<()> {
    use std::io::Write as _;
    let tmp = root.join("CURRENT.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("gen-{id}\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, root.join("CURRENT"))?;
    // Directories cannot be fsynced on every platform (Windows).
    if let Ok(d) = std::fs::File::open(root) {
        let _ = d.sync_all();
    }
    Ok(())
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
        let g = std::sync::Arc::new(super::Gen::create(None, Uuid::nil()).unwrap());
        let project = Uuid::new_v4().to_string();
        let mut ids = Vec::new();
        {
            let w = g.writer.lock().unwrap();
            let f = g.f;
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
        }
        g.commit(Uuid::nil(), super::Cursor::default(), 1).unwrap();
        g.reader.reload().unwrap();
        *idx.active.write().unwrap() = Some(g.clone());
        idx.publish(&g, 1).unwrap();
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
