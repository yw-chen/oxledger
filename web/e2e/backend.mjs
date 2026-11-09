// SPDX-License-Identifier: GPL-2.0-or-later
// Local backend for the SPA (dev and E2E). Only ever touches databases named
// oxledger_fe_* on the docker PostgreSQL (playbook).
//
//   node e2e/backend.mjs reset [db]     drop + create db, seed-demo, fixture.sql,
//                                       passwords, the "Claude Code" agent client
//   node e2e/backend.mjs serve [db]     oxledger serve on 127.0.0.1:18080
//                                       (SPA origin http://localhost:5174)
//
// Demo password for admin/alice/bob/carol: see PASSWORD below (test value).

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..', '..');
export const PASSWORD = 'oxledger-demo-pass';
export const PORT = Number(process.env.OX_BACKEND_PORT ?? 18080);
export const SPA_ORIGIN = process.env.OX_SPA_ORIGIN ?? 'http://localhost:5174';
const PG = 'postgres://oxledger:oxledger_dev@127.0.0.1:55432';
const bin = join(root, 'target', 'debug', process.platform === 'win32' ? 'oxledger.exe' : 'oxledger');

function check(db) {
	if (!/^oxledger_fe_[a-z0-9_]+$/.test(db)) throw new Error(`refusing database ${db}: only oxledger_fe_* (playbook)`);
	return db;
}

function run(cmd, args, opts = {}) {
	const r = spawnSync(cmd, args, { encoding: 'utf8', ...opts });
	if (r.status !== 0) throw new Error(`${cmd} ${args.join(' ')} failed:\n${r.stdout}\n${r.stderr}`);
	return r.stdout;
}

const psql = (db, sql) => run('docker', ['exec', '-i', 'oxledger-pg', 'psql', '-v', 'ON_ERROR_STOP=1', '-q', '-U', 'oxledger', '-d', db], { input: sql });

export function reset(db = 'oxledger_fe_dev') {
	check(db);
	if (!existsSync(bin)) throw new Error(`build the backend first: cargo build --features test-support (${bin})`);
	psql('postgres', `DROP DATABASE IF EXISTS ${db} WITH (FORCE);\nCREATE DATABASE ${db};\n`);
	const env = { ...process.env, DATABASE_URL: `${PG}/${db}` };
	const keys = run(bin, ['seed-demo'], { env, cwd: root })
		.trim()
		.split(/\r?\n/)
		.map((l) => l.split('\t'));
	psql(db, readFileSync(join(here, 'fixture.sql'), 'utf8'));
	for (const login of ['admin', 'alice', 'bob', 'carol']) run(bin, ['user', 'set-password', login], { env, cwd: root, input: PASSWORD + '\n' });
	run(bin, ['agent-client', 'add', 'Claude Code', 'policy-2026-01'], { env, cwd: root });
	run(bin, ['agent-client', 'enable', 'Claude Code'], { env, cwd: root });
	mkdirSync(join(here, '.state'), { recursive: true });
	writeFileSync(join(here, '.state', `${db}.json`), JSON.stringify({ db, keys: Object.fromEntries(keys) }, null, 2));
	return db;
}

export function serve(db = 'oxledger_fe_dev', { inherit = true } = {}) {
	check(db);
	const index = join(here, '.state', `index-${db}`);
	const child = spawn(bin, ['serve'], {
		cwd: root,
		stdio: inherit ? 'inherit' : 'ignore',
		env: {
			...process.env,
			DATABASE_URL: `${PG}/${db}`,
			OXLEDGER_BIND: `127.0.0.1:${PORT}`,
			OXLEDGER_SPA_ORIGIN: SPA_ORIGIN,
			OXLEDGER_INDEX_DIR: index,
			RUST_LOG: process.env.RUST_LOG ?? 'warn'
		}
	});
	return child;
}

export async function waitHealthy(ms = 60000) {
	const end = Date.now() + ms;
	while (Date.now() < end) {
		try {
			const r = await fetch(`http://127.0.0.1:${PORT}/healthz`);
			if (r.ok) return;
		} catch {
			/* not yet */
		}
		await new Promise((r) => setTimeout(r, 300));
	}
	throw new Error('backend did not become healthy');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	const [cmd, db] = process.argv.slice(2);
	if (cmd === 'reset') console.log(`reset ${reset(db)}`);
	else if (cmd === 'serve') {
		const c = serve(db);
		c.on('exit', (code) => process.exit(code ?? 0));
	} else {
		console.error('usage: node e2e/backend.mjs reset|serve [oxledger_fe_*]');
		process.exit(2);
	}
}
