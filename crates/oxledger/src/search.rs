// SPDX-License-Identifier: GPL-2.0-or-later
//! Minimal BM25 index (M0) on tantivy.
//!
//! * CJK text is indexed as unigrams + overlapping bigrams, Latin text as
//!   lower-cased words — the dictionary-free baseline from research §5.4
//!   (no jieba). Experiment E1 decides whether to keep it.
//! * Visibility facts are indexed as raw terms so the same [`Pred`] that
//!   becomes SQL also becomes a tantivy filter (filter before ranking).
//! * The index follows the outbox (D11) and can always be rebuilt from SQL.

use anyhow::{Context, Result};
use ox_core::Pred;
use sqlx::PgPool;
use std::path::Path;
use std::sync::Mutex;
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

/// Splits text into tokens: alphanumeric runs → lower-cased words; CJK runs →
/// every character plus every adjacent pair.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();
    let flush_cjk = |cjk: &mut Vec<char>, out: &mut Vec<String>| {
        for (i, c) in cjk.iter().enumerate() {
            out.push(c.to_string());
            if let Some(n) = cjk.get(i + 1) {
                out.push([*c, *n].iter().collect());
            }
        }
        cjk.clear();
    };
    for c in text.chars() {
        if is_cjk(c) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            word.extend(c.to_lowercase());
        } else {
            flush_cjk(&mut cjk, &mut out);
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        }
    }
    flush_cjk(&mut cjk, &mut out);
    if !word.is_empty() {
        out.push(word);
    }
    out
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
    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecStream {
        let tokens = tokenize(text)
            .into_iter()
            .enumerate()
            .map(|(i, t)| Token {
                position: i,
                text: t,
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
    let all = tokenize(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let t = &all[i];
        let is_cjk_uni = t.chars().count() == 1 && t.chars().all(is_cjk);
        let next_is_bigram = all
            .get(i + 1)
            .is_some_and(|n| n.chars().count() == 2 && n.chars().all(is_cjk));
        if is_cjk_uni
            && (next_is_bigram
                || (i > 0 && all[i - 1].chars().count() == 2 && all[i - 1].chars().all(is_cjk)))
        {
            i += 1; // unigram inside a longer CJK run
            continue;
        }
        out.push(t.clone());
        i += 1;
    }
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
        })
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
    /// Empty `q` lists every visible document (used by tests).
    pub fn search(&self, q: &str, filter: &Pred, limit: usize) -> Result<Vec<Hit>> {
        let terms = {
            let mut t = tokenize_query(q);
            t.sort();
            t.dedup();
            t
        };
        let text: Box<dyn Query> = if terms.is_empty() {
            Box::new(AllQuery)
        } else {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for t in &terms {
                for (field, boost) in [(self.f.subject, 3.0), (self.f.description, 1.0)] {
                    let tq = TermQuery::new(
                        Term::from_field_text(field, t),
                        IndexRecordOption::WithFreqs,
                    );
                    clauses.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(Box::new(tq), boost)),
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
    pub async fn sync(&self, pool: &PgPool) -> Result<usize> {
        const CONSUMER: &str = "search";
        let mut applied = 0;
        loop {
            let rows: Vec<(i64, String, Uuid)> = sqlx::query_as(
                "SELECT o.id, o.xid::text, o.entity_id FROM outbox o
                 LEFT JOIN outbox_cursors c ON c.consumer = $1
                 WHERE o.topic = 'issue'
                   AND o.xid < pg_snapshot_xmin(pg_current_snapshot())
                   AND (o.xid, o.id) > (COALESCE(c.last_xid, '0'::xid8), COALESCE(c.last_id, 0))
                 ORDER BY o.xid, o.id LIMIT 500",
            )
            .bind(CONSUMER)
            .fetch_all(pool)
            .await?;
            let Some((last_id, last_xid, _)) = rows.last().cloned() else {
                break;
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
            self.reader.reload()?;
            sqlx::query(
                "INSERT INTO outbox_cursors (consumer, last_xid, last_id) VALUES ($1, $2::xid8, $3)
                 ON CONFLICT (consumer) DO UPDATE SET last_xid = EXCLUDED.last_xid, last_id = EXCLUDED.last_id",
            )
            .bind(CONSUMER)
            .bind(&last_xid)
            .bind(last_id)
            .execute(pool)
            .await?;
            applied += rows.len();
        }
        Ok(applied)
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

#[cfg(test)]
mod tests {
    use super::tokenize;

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
