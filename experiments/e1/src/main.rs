// SPDX-License-Identifier: GPL-2.0-or-later
//! Experiment E1: compares Chinese tokenization schemes for oxledger's BM25
//! search on a synthetic Traditional Chinese issue set.
//!
//! Every scheme uses the same tantivy schema and the same query shape as M0
//! (`crates/oxledger/src/search.rs`): a Should-clause per query term on
//! subject (boost 3) and description (boost 1). Only the tokenizers differ.
//!
//! Usage: `cargo run --release [--features icu,lindera] -- [--out DIR]`

mod schemes;

use anyhow::{Context, Result};
use schemes::{Scheme, TokenFn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term, doc};

// ---------------------------------------------------------------- data

#[derive(Deserialize)]
struct Issue {
    id: u64,
    subject: String,
    description: String,
}

#[derive(Deserialize)]
struct QueryCase {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    query: String,
    rels: BTreeMap<String, u8>,
}

#[derive(Deserialize)]
struct Alias {
    term: String,
    expand: Vec<String>,
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{}:{}", path.display(), i + 1))
        })
        .collect()
}

// ---------------------------------------------------------------- tantivy glue

#[derive(Clone)]
struct FnTokenizer(TokenFn);

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

impl Tokenizer for FnTokenizer {
    type TokenStream<'a> = VecStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecStream {
        let tokens = (self.0)(text)
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

struct Built {
    reader: IndexReader,
    id: Field,
    subject: Field,
    description: Field,
    size_bytes: u64,
    build_ms: f64,
    _dir: TempDir,
}

/// Removes the on-disk index when dropped.
struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn build(scheme: &Scheme, issues: &[Issue]) -> Result<Built> {
    const TOK: &str = "e1";
    let mut sb = Schema::builder();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOK)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let id = sb.add_u64_field("id", STORED);
    let subject = sb.add_text_field("subject", text.clone());
    let description = sb.add_text_field("description", text);
    let schema = sb.build();

    // On disk (temp dir, never a database) so the index size is measurable.
    let dir = std::env::temp_dir().join(format!("oxledger-e1-{}-{}", std::process::id(), scheme.key));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let guard = TempDir(dir.clone());
    let index = Index::create_in_dir(&dir, schema)?;
    index.tokenizers().register(
        TOK,
        TextAnalyzer::builder(FnTokenizer(scheme.index.clone())).build(),
    );
    let t0 = Instant::now();
    let mut w = index.writer_with_num_threads(1, 50_000_000)?;
    for i in issues {
        w.add_document(doc!(
            id => i.id,
            subject => i.subject.clone(),
            description => i.description.clone(),
        ))?;
    }
    w.commit()?;
    w.wait_merging_threads()?;
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    Ok(Built {
        reader,
        id,
        subject,
        description,
        size_bytes: dir_size(&dir),
        build_ms,
        _dir: guard,
    })
}

/// Weighted query terms → the M0 query shape. Later duplicates keep the max weight.
fn make_query(b: &Built, terms: &[(String, f32)]) -> Box<dyn Query> {
    let mut best: BTreeMap<&str, f32> = BTreeMap::new();
    for (t, w) in terms {
        let e = best.entry(t.as_str()).or_insert(0.0);
        *e = e.max(*w);
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for (t, w) in best {
        for (field, boost) in [(b.subject, 3.0f32), (b.description, 1.0)] {
            let tq = TermQuery::new(Term::from_field_text(field, t), IndexRecordOption::WithFreqs);
            clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(tq), boost * w))));
        }
    }
    Box::new(BooleanQuery::new(clauses))
}

/// Query terms with weights. `uni`: also add the CJK unigrams that M0's
/// query side drops, at weight `w` (only meaningful on an index that stores
/// unigrams, i.e. scheme a).
fn query_terms(
    scheme: &Scheme,
    q: &str,
    aliases: Option<(&[Alias], f32)>,
    uni: Option<f32>,
) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = (scheme.query)(q).into_iter().map(|t| (t, 1.0)).collect();
    if let Some(w) = uni {
        let unis: Vec<String> = oxledger::search::tokenize(q)
            .into_iter()
            .filter(|t| t.chars().count() == 1 && t.chars().all(schemes::is_cjk))
            .collect();
        out.extend(unis.into_iter().map(|t| (t, w)));
    }
    if let Some((table, w)) = aliases {
        for a in table.iter().filter(|a| q.contains(a.term.as_str())) {
            for e in &a.expand {
                out.extend((scheme.query)(e).into_iter().map(|t| (t, w)));
            }
        }
    }
    out
}

fn search(b: &Built, terms: &[(String, f32)], k: usize) -> Result<(Vec<u64>, usize)> {
    if terms.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let q = make_query(b, terms);
    let s = b.reader.searcher();
    let (top, count) = s.search(&q, &(TopDocs::with_limit(k).order_by_score(), Count))?;
    let mut ids = Vec::with_capacity(top.len());
    for (_, addr) in top {
        let d: TantivyDocument = s.doc(addr)?;
        if let Some(v) = d.get_first(b.id).and_then(|v| v.as_u64()) {
            ids.push(v);
        }
    }
    Ok((ids, count))
}

// ---------------------------------------------------------------- metrics

#[derive(Default, Clone, Copy, Serialize)]
struct M {
    ndcg10: f64,
    recall20: f64,
    mrr: f64,
    p10: f64,
}

fn metrics(ranked: &[u64], rels: &HashMap<u64, u8>) -> M {
    let gain = |g: u8| (2f64.powi(g as i32)) - 1.0;
    let dcg: f64 = ranked
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, d)| gain(*rels.get(d).unwrap_or(&0)) / ((i + 2) as f64).log2())
        .sum();
    let mut ideal: Vec<u8> = rels.values().copied().filter(|g| *g > 0).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, g)| gain(*g) / ((i + 2) as f64).log2())
        .sum();
    let n_rel = ideal.len().max(1) as f64;
    let is_rel = |d: &u64| rels.get(d).is_some_and(|g| *g > 0);
    let recall20 = ranked.iter().take(20).filter(|d| is_rel(d)).count() as f64 / n_rel;
    let mrr = ranked
        .iter()
        .position(is_rel)
        .map(|p| 1.0 / (p + 1) as f64)
        .unwrap_or(0.0);
    let p10 = ranked.iter().take(10).filter(|d| is_rel(d)).count() as f64 / 10.0;
    M {
        // `+ 0.0` turns the -0.0 of an empty f64 sum into 0.0 for printing.
        ndcg10: if idcg > 0.0 { dcg / idcg + 0.0 } else { 0.0 },
        recall20,
        mrr,
        p10,
    }
}

fn mean(ms: &[M]) -> M {
    let n = ms.len().max(1) as f64;
    M {
        ndcg10: ms.iter().map(|m| m.ndcg10).sum::<f64>() / n,
        recall20: ms.iter().map(|m| m.recall20).sum::<f64>() / n,
        mrr: ms.iter().map(|m| m.mrr).sum::<f64>() / n,
        p10: ms.iter().map(|m| m.p10).sum::<f64>() / n,
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

// ---------------------------------------------------------------- run

#[derive(Serialize)]
struct PerQuery<'a> {
    run: &'a str,
    qid: &'a str,
    kind: &'a str,
    query: &'a str,
    terms: Vec<String>,
    matched: usize,
    #[serde(flatten)]
    m: M,
    /// top 10 as (issue id, grade)
    top10: Vec<(u64, u8)>,
    /// relevant issues not in the top 20
    missed: Vec<(u64, u8)>,
}

struct RunResult {
    name: String,
    /// per-query nDCG@10, in query order (for paired comparisons)
    ndcg_per_q: Vec<f64>,
    by_kind: BTreeMap<String, M>,
    all: M,
    size_bytes: u64,
    build_ms: f64,
    lat_p50_us: f64,
    lat_p95_us: f64,
    avg_matched: f64,
    avg_qterms: f64,
}

const KINDS: [&str; 5] = ["exact", "synonym", "mixed", "single", "decoy"];
const LAT_REPS: usize = 30;
const SCALE: usize = 50;
const BOOT: usize = 2000;

/// Paired comparison of nDCG@10 against `base`: wins/ties/losses and a
/// percentile-bootstrap 95% CI of the mean difference (fixed-seed LCG).
fn paired(base: &[f64], other: &[f64]) -> (usize, usize, usize, f64, f64, f64) {
    let d: Vec<f64> = other.iter().zip(base).map(|(o, b)| o - b).collect();
    let eps = 1e-9;
    let wins = d.iter().filter(|x| **x > eps).count();
    let losses = d.iter().filter(|x| **x < -eps).count();
    let ties = d.len() - wins - losses;
    let mean = d.iter().sum::<f64>() / d.len() as f64;
    let mut seed: u64 = 0x5eed_e1;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let mut means: Vec<f64> = (0..BOOT)
        .map(|_| (0..d.len()).map(|_| d[next() % d.len()]).sum::<f64>() / d.len() as f64)
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (wins, ties, losses, mean, pct(&means, 0.025), pct(&means, 0.975))
}

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out_dir = root.join("results");
    let args: Vec<String> = std::env::args().collect();
    // `--tokens TEXT...`: show how each scheme tokenizes, then exit.
    if args.get(1).is_some_and(|a| a == "--tokens") {
        for text in &args[2..] {
            println!("{text}");
            for s in schemes::all()? {
                println!("  {:<3} index: {:?}", s.key, (s.index)(text));
                println!("  {:<3} query: {:?}", "", (s.query)(text));
            }
        }
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--out") {
        out_dir = PathBuf::from(args.get(i + 1).context("--out needs a dir")?);
    }
    std::fs::create_dir_all(&out_dir)?;

    let issues: Vec<Issue> = read_jsonl(&root.join("data/issues.jsonl"))?;
    let queries: Vec<QueryCase> = read_jsonl(&root.join("data/queries.jsonl"))?;
    let aliases: Vec<Alias> = read_jsonl(&root.join("data/aliases.jsonl"))?;
    // Sanity: every judged id exists.
    let known: std::collections::HashSet<u64> = issues.iter().map(|i| i.id).collect();
    for q in &queries {
        for k in q.rels.keys() {
            let id: u64 = k.parse()?;
            anyhow::ensure!(known.contains(&id), "{}: unknown issue {id}", q.id);
        }
        anyhow::ensure!(KINDS.contains(&q.kind.as_str()), "{}: bad type {}", q.id, q.kind);
    }
    eprintln!("{} issues, {} queries, {} aliases", issues.len(), queries.len(), aliases.len());

    let schemes = schemes::all()?;
    // (run name, scheme index, alias weight)
    // (run name, scheme index, alias weight, extra-unigram weight)
    let mut runs: Vec<(String, usize, Option<f32>, Option<f32>)> = Vec::new();
    for (i, s) in schemes.iter().enumerate() {
        runs.push((s.key.to_string(), i, None, None));
    }
    // a + low-weight unigrams: recall fallback for queries whose bigrams
    // never occur (e.g. 付不了錢), without letting unigrams dominate ranking.
    for w in [0.1f32, 0.2, 0.5] {
        runs.push((format!("a+uni{w}"), 0, None, Some(w)));
    }
    for (i, s) in schemes.iter().enumerate() {
        for w in [1.0f32, 0.5] {
            runs.push((format!("{}+alias{w}", s.key), i, Some(w), None));
        }
    }
    runs.push(("a+uni0.2+alias1".into(), 0, Some(1.0), Some(0.2)));

    let mut built: Vec<Built> = Vec::new();
    for s in &schemes {
        eprintln!("building index: {} ({})", s.key, s.label);
        built.push(build(s, &issues)?);
    }

    let mut per_query_lines = String::new();
    let mut results: Vec<RunResult> = Vec::new();
    for (name, si, alias_w, uni_w) in &runs {
        let s = &schemes[*si];
        let b = &built[*si];
        let mut ms_by_kind: BTreeMap<String, Vec<M>> = BTreeMap::new();
        let mut all = Vec::new();
        let mut lat = Vec::new();
        let mut matched_sum = 0usize;
        let mut qterms_sum = 0usize;
        for q in &queries {
            let rels: HashMap<u64, u8> = q.rels.iter().map(|(k, v)| (k.parse().unwrap(), *v)).collect();
            let al = alias_w.map(|w| (aliases.as_slice(), w));
            let terms = query_terms(s, &q.query, al, *uni_w);
            let (ranked, matched) = search(b, &terms, 100)?;
            // latency: tokenization + search + stored-field fetch, end to end
            for _ in 0..LAT_REPS {
                let t0 = Instant::now();
                let terms = query_terms(s, &q.query, al, *uni_w);
                let r = search(b, &terms, 20)?;
                std::hint::black_box(r);
                lat.push(t0.elapsed().as_secs_f64() * 1e6);
            }
            let m = metrics(&ranked, &rels);
            matched_sum += matched;
            let mut uniq: Vec<String> = terms.iter().map(|t| t.0.clone()).collect();
            uniq.sort();
            uniq.dedup();
            qterms_sum += uniq.len();
            ms_by_kind.entry(q.kind.clone()).or_default().push(m);
            all.push(m);
            let top10 = ranked.iter().take(10).map(|d| (*d, *rels.get(d).unwrap_or(&0))).collect();
            let top20: std::collections::HashSet<u64> = ranked.iter().take(20).copied().collect();
            let mut missed: Vec<(u64, u8)> = rels
                .iter()
                .filter(|(d, g)| **g > 0 && !top20.contains(d))
                .map(|(d, g)| (*d, *g))
                .collect();
            missed.sort();
            let pq = PerQuery {
                run: name,
                qid: &q.id,
                kind: &q.kind,
                query: &q.query,
                terms: uniq,
                matched,
                m,
                top10,
                missed,
            };
            per_query_lines.push_str(&serde_json::to_string(&pq)?);
            per_query_lines.push('\n');
        }
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        results.push(RunResult {
            name: name.clone(),
            ndcg_per_q: all.iter().map(|m| m.ndcg10).collect(),
            by_kind: ms_by_kind.iter().map(|(k, v)| (k.clone(), mean(v))).collect(),
            all: mean(&all),
            size_bytes: b.size_bytes,
            build_ms: b.build_ms,
            lat_p50_us: pct(&lat, 0.5),
            lat_p95_us: pct(&lat, 0.95),
            avg_matched: matched_sum as f64 / queries.len() as f64,
            avg_qterms: qterms_sum as f64 / queries.len() as f64,
        });
    }

    drop(built);

    // Cost at a more realistic size: the corpus replicated SCALE times.
    let mut big: Vec<Issue> = Vec::with_capacity(issues.len() * SCALE);
    for r in 0..SCALE as u64 {
        big.extend(issues.iter().map(|i| Issue {
            id: r * 1000 + i.id,
            subject: i.subject.clone(),
            description: i.description.clone(),
        }));
    }
    let mut scaled: Vec<(String, u64, f64, f64, f64)> = Vec::new();
    for s in &schemes {
        eprintln!("scaled index ({} docs): {}", big.len(), s.key);
        let b = build(s, &big)?;
        let mut lat = Vec::new();
        for q in &queries {
            let terms = query_terms(s, &q.query, None, None);
            search(&b, &terms, 20)?; // warm
            for _ in 0..LAT_REPS {
                let t0 = Instant::now();
                let terms = query_terms(s, &q.query, None, None);
                std::hint::black_box(search(&b, &terms, 20)?);
                lat.push(t0.elapsed().as_secs_f64() * 1e6);
            }
        }
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        scaled.push((s.key.to_string(), b.size_bytes, b.build_ms, pct(&lat, 0.5), pct(&lat, 0.95)));
    }

    let mut md = report(&schemes, &results, &queries, issues.len());
    let _ = writeln!(md, "\n## Cost at {} docs (corpus × {SCALE})\n", big.len());
    let _ = writeln!(md, "| scheme | index size (KiB) | build (ms) | query p50 (µs) | query p95 (µs) |");
    let _ = writeln!(md, "|---|---|---|---|---|");
    for (k, size, ms, p50, p95) in &scaled {
        let _ = writeln!(md, "| {k} | {:.1} | {ms:.0} | {p50:.0} | {p95:.0} |", *size as f64 / 1024.0);
    }
    std::fs::write(out_dir.join("summary.md"), &md)?;
    std::fs::write(out_dir.join("per_query.jsonl"), per_query_lines)?;
    println!("{md}");
    eprintln!("wrote {}", out_dir.display());
    Ok(())
}

fn report(schemes: &[Scheme], rs: &[RunResult], qs: &[QueryCase], n_issues: usize) -> String {
    let mut s = String::new();
    let count = |k: &str| qs.iter().filter(|q| q.kind == k).count();
    let _ = writeln!(s, "# E1 results\n");
    let _ = writeln!(s, "{} issues, {} queries ({}).\n", n_issues, qs.len(),
        KINDS.iter().map(|k| format!("{k} {}", count(k))).collect::<Vec<_>>().join(", "));
    let _ = writeln!(s, "Schemes:\n");
    for sc in schemes {
        let _ = writeln!(s, "- `{}`: {}", sc.key, sc.label);
    }
    for (title, pick) in [
        ("nDCG@10", (|m: &M| m.ndcg10) as fn(&M) -> f64),
        ("recall@20", |m: &M| m.recall20),
        ("MRR", |m: &M| m.mrr),
        ("P@10", |m: &M| m.p10),
    ] {
        let _ = writeln!(s, "\n## {title}\n");
        let _ = writeln!(s, "| run | all | {} |", KINDS.join(" | "));
        let _ = writeln!(s, "|---|---|{}", "---|".repeat(KINDS.len()));
        for r in rs {
            let cells: Vec<String> = KINDS
                .iter()
                .map(|k| r.by_kind.get(*k).map(|m| format!("{:.3}", pick(m))).unwrap_or_default())
                .collect();
            let _ = writeln!(s, "| {} | **{:.3}** | {} |", r.name, pick(&r.all), cells.join(" | "));
        }
    }
    if let Some(base) = rs.iter().find(|r| r.name == "a") {
        let _ = writeln!(s, "\n## Paired nDCG@10 vs `a` (60 queries, bootstrap 95% CI)\n");
        let _ = writeln!(s, "| run | better | same | worse | mean Δ | 95% CI |");
        let _ = writeln!(s, "|---|---|---|---|---|---|");
        for r in rs.iter().filter(|r| r.name != "a") {
            let (w, t, l, m, lo, hi) = paired(&base.ndcg_per_q, &r.ndcg_per_q);
            let _ = writeln!(s, "| {} | {w} | {t} | {l} | {m:+.3} | [{lo:+.3}, {hi:+.3}] |", r.name);
        }
    }
    let _ = writeln!(s, "\n## Cost (200 docs)\n");
    let _ = writeln!(s, "| run | index size (KiB) | build (ms) | query p50 (µs) | query p95 (µs) | avg query terms | avg matched docs |");
    let _ = writeln!(s, "|---|---|---|---|---|---|---|");
    for r in rs {
        let _ = writeln!(
            s,
            "| {} | {:.1} | {:.1} | {:.0} | {:.0} | {:.1} | {:.1} |",
            r.name,
            r.size_bytes as f64 / 1024.0,
            r.build_ms,
            r.lat_p50_us,
            r.lat_p95_us,
            r.avg_qterms,
            r.avg_matched
        );
    }
    s
}
