// SPDX-License-Identifier: GPL-2.0-or-later
//! Checks claims about tantivy 0.27 used in docs/design/03-search-consistency-explain.md.
use std::time::Instant;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, ConstScoreQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, STRING, STORED, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{Index, TantivyDocument, Term, doc};

fn is_cjk(c: char) -> bool { matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF) }

/// oxledger-like tokens (CJK unigram + bigram, lower-cased alnum words), with byte spans.
fn spanned(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let cs: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < cs.len() {
        let (b, c) = cs[i];
        if is_cjk(c) {
            let mut j = i;
            while j < cs.len() && is_cjk(cs[j].1) { j += 1; }
            for k in i..j {
                let end = |k: usize| cs[k].0 + cs[k].1.len_utf8();
                out.push((cs[k].1.to_string(), cs[k].0, end(k)));
                if k + 1 < j { out.push((format!("{}{}", cs[k].1, cs[k + 1].1), cs[k].0, end(k + 1))); }
            }
            i = j;
        } else if c.is_alphanumeric() || c == '_' {
            let mut j = i;
            while j < cs.len() && (cs[j].1.is_alphanumeric() || cs[j].1 == '_') && !is_cjk(cs[j].1) { j += 1; }
            let e = if j < cs.len() { cs[j].0 } else { text.len() };
            out.push((text[b..e].to_lowercase(), b, e));
            i = j;
        } else { i += 1; }
    }
    out
}

#[derive(Clone)]
struct Tok { offsets: bool }
struct VecStream { tokens: Vec<Token>, pos: usize }
impl TokenStream for VecStream {
    fn advance(&mut self) -> bool { if self.pos < self.tokens.len() { self.pos += 1; true } else { false } }
    fn token(&self) -> &Token { &self.tokens[self.pos - 1] }
    fn token_mut(&mut self) -> &mut Token { &mut self.tokens[self.pos - 1] }
}
impl Tokenizer for Tok {
    type TokenStream<'a> = VecStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecStream {
        let off = self.offsets;
        let tokens = spanned(text).into_iter().enumerate().map(|(i, (t, f, e))| Token {
            position: i, text: t,
            offset_from: if off { f } else { 0 }, offset_to: if off { e } else { 0 },
            ..Token::default()
        }).collect();
        VecStream { tokens, pos: 0 }
    }
}

fn main() -> tantivy::Result<()> {
    for offsets in [false, true] {
        let mut sb = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default().set_tokenizer("t").set_index_option(IndexRecordOption::WithFreqsAndPositions));
        let id = sb.add_text_field("id", STRING | STORED);
        let private = sb.add_text_field("private", STRING);
        let desc = sb.add_text_field("description", text);
        let index = Index::create_in_ram(sb.build());
        index.tokenizers().register("t", TextAnalyzer::builder(Tok { offsets }).build());
        let mut w = index.writer(50_000_000)?;
        let long = format!("{}登入失敗{}", "這是一段很長的中文說明文字".repeat(10), "後面還有其他內容".repeat(5));
        let docs = [
            ("d1", "輸入正確密碼仍無法登入，出現 invalid_grant"),
            ("d2", "首頁的橫幅圖片要十秒以上才載入完成"),
            ("d3", long.as_str()),
        ];
        for (i, t) in docs { w.add_document(doc!(id => i, private => "false", desc => t))?; }
        // filler so df differs
        for n in 0..20_000 { w.add_document(doc!(id => format!("f{n}"), private => "false",
            desc => format!("填充文件 {n} 關於報表匯出與系統設定的描述，偶爾提到輸入"))) ?; }
        let mut pc = w.prepare_commit()?;
        pc.set_payload("{\"applied_through_xid\":\"645406\"}");
        pc.commit()?;
        println!("== offsets={offsets}  payload after commit: {:?}", index.load_metas()?.payload);
        let reader = index.reader()?;
        let s = reader.searcher();
        // Query 登入: bigram ×1 + unigrams 登/入 ×0.1, AND a ConstScore filter (like search.rs).
        let tq = |t: &str, b: f32| -> (Occur, Box<dyn Query>) {
            (Occur::Should, Box::new(BoostQuery::new(Box::new(TermQuery::new(
                Term::from_field_text(desc, t), IndexRecordOption::WithFreqs)), b)))
        };
        let text_q = BooleanQuery::new(vec![tq("登入", 1.0), tq("登", 0.1), tq("入", 0.1)]);
        let filter = ConstScoreQuery::new(Box::new(TermQuery::new(Term::from_field_text(private, "false"), IndexRecordOption::Basic)), 0.0);
        let q = BooleanQuery::new(vec![(Occur::Must, Box::new(text_q)), (Occur::Must, Box::new(filter))]);
        let top = s.search(&q, &TopDocs::with_limit(5).order_by_score())?;
        let mut sg = SnippetGenerator::create(&s, &q, desc)?;
        println!("snippet terms (term -> weight; boosts ignored?): see highlights below");
        for (score, addr) in &top {
            let d: TantivyDocument = s.doc(*addr)?;
            let did = d.get_first(id).and_then(|v| v.as_str()).unwrap().to_string();
            let src = docs.iter().find(|x| x.0 == did).map(|x| x.1).unwrap_or("(filler)");
            if src == "(filler)" { continue; }
            sg.set_max_num_chars(150);
            let sn = sg.snippet(src);
            let frag = sn.fragment();
            let hl: Vec<&str> = sn.highlighted().iter().map(|r| &frag[r.clone()]).collect();
            println!("  {did} score={score:.4} fragment_bytes={} fragment_chars={} highlighted={hl:?}",
                     frag.len(), frag.chars().count());
        }
        if offsets {
            // Explanation shape for the top hit
            let ex = q.explain(&s, top[0].1)?;
            let js = ex.to_pretty_json();
            println!("explain json ({} bytes):\n{}", js.len(), js);
            // cost: explain for top 20 of a broader query
            let q2 = BooleanQuery::new(vec![tq("輸入", 1.0), tq("報表", 1.0)]);
            let t0 = Instant::now();
            let top2 = s.search(&q2, &TopDocs::with_limit(20).order_by_score())?;
            let t_search = t0.elapsed();
            let t1 = Instant::now();
            let w2 = q2.weight(tantivy::query::EnableScoring::enabled_from_searcher(&s))?;
            for (_, a) in &top2 { let _ = w2.explain(s.segment_reader(a.segment_ord), a.doc_id)?; }
            println!("search top20 {:?}, explain x20 (one weight) {:?}", t_search, t1.elapsed());
        }
    }
    Ok(())
}
