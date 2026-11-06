# oxledger

A Rust reimplementation of an issue tracker, derived from [Redmine](https://github.com/redmine/redmine) (GPL-2.0-or-later), designed for AI agents as first-class users.

> Status: research / design phase. No code yet.

## Goals

1. **Native MCP** — a built-in Model Context Protocol server ([rmcp](https://github.com/modelcontextprotocol/rust-sdk)), sharing one permission and query core with the REST API and the UI.
2. **Highly customizable UI** — lists, boards, dashboards and issue forms are declarative definitions stored in the database; people and AI edit the same definitions.
3. **Ontology + BM25 retrieval** — typed relations and an alias vocabulary on PostgreSQL, BM25F search with [tantivy](https://github.com/quickwit-oss/tantivy), so agents can find and understand records in one call.

## Stack (planned)

Rust (axum, sqlx) · PostgreSQL 18+ · tantivy · rmcp · SvelteKit SPA

## Design docs

- [docs/research/01-redmine-rust-rewrite.md](docs/research/01-redmine-rust-rewrite.md) — research, decisions D1–D13, architecture, experiments (Traditional Chinese)

## Relationship to Redmine

oxledger is an independent project, not affiliated with or endorsed by the Redmine project. It reuses Redmine's design and may adapt its source, and is therefore distributed under the same license. "Redmine" is used here only to describe the project's origin.

## License

GNU General Public License v2.0 or (at your option) any later version — see [LICENSE](LICENSE).

SPDX-License-Identifier: GPL-2.0-or-later
