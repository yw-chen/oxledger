// SPDX-License-Identifier: GPL-2.0-or-later
// A stand-in OpenAI-compatible model for E2E and the dev preview (never a
// real service, nothing leaves the machine): `POST /v1/chat/completions`,
// always streamed. It answers by script:
//   - the connection test (system prompt says so): calls the tool `ping`;
//   - a user message containing 「留言」: calls `get` for AUTH-1's
//     lock_version, then `command_submit` to add a note, then a markdown
//     reply (FAKEAI_LOG=1 prints the tool results);
//   - anything else: a short reply with an issue key.
// The reply after a write carries an external link (the confirm dialog).
// The backend reaches it through OXLEDGER_AI_ENDPOINT_FAKE (http on
// loopback needs OXLEDGER_AI_ALLOW_HTTP=1).

import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';

const chunk = (delta, finish = null) => `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
const usage = (i, o) => `data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: i, completion_tokens: o } })}\n\n`;


function script(body) {
	const msgs = body.messages ?? [];
	const text = (m) => (typeof m.content === 'string' ? m.content : (m.content ?? []).map((c) => c.text ?? '').join(''));
	const all = msgs.map(text).join('\n');
	const last = msgs[msgs.length - 1] ?? {};
	if (/connection test/i.test(all)) return [chunk({ role: 'assistant', tool_calls: [{ index: 0, id: 'call_ping', type: 'function', function: { name: 'ping', arguments: '{}' } }] }), chunk({}, 'tool_calls')];
	const call = (id, name, args) => {
		const a = JSON.stringify(args);
		// Arguments arrive in two pieces, the way real providers stream them.
		return [chunk({ role: 'assistant', tool_calls: [{ index: 0, id, type: 'function', function: { name, arguments: a.slice(0, 12) } }] }), chunk({ tool_calls: [{ index: 0, function: { arguments: a.slice(12) } }] }), chunk({}, 'tool_calls')];
	};
	if (last.role === 'tool') {
		const prev = msgs[msgs.length - 2] ?? {};
		const called = prev.tool_calls?.[0]?.function?.name;
		if (called === 'get') {
			// Like a real model: read lock_version first, then write.
			let lock = 0;
			try {
				lock = JSON.parse(text(last)).lock_version ?? 0;
			} catch {
				/* keep 0: the server answers with a conflict */
			}
			return call('call_2', 'command_submit', {
				command: {
					command_schema_version: 1,
					ops: [{ kind: 'add_note', op_id: randomUUID(), target: { type: 'issue', key: 'AUTH-1' }, body: 'AI 助理的留言' }],
					preconditions: [{ target: { type: 'issue', key: 'AUTH-1' }, lock_version: lock }]
				}
			});
		}
		if (process.env.FAKEAI_LOG) console.error('[fakeai tool]', text(last));
		return [chunk({ role: 'assistant', content: '已處理。\n\n- **重點**：請看 AUTH-2\n' }), chunk({ content: '- 參考 [說明文件](https://docs.example.com/guide)' }), chunk({}, 'stop')];
	}
	if (last.role === 'user' && text(last).includes('留言')) return call('call_1', 'get', { key: 'AUTH-1', fields: ['lock_version'] });
	return [chunk({ role: 'assistant', content: '你好，我是 AI 助理。' }), chunk({ content: '相關的單：AUTH-1' }), chunk({}, 'stop')];
}

export function startFakeAi(port) {
	const server = createServer((req, res) => {
		if (req.method !== 'POST' || !req.url?.endsWith('/chat/completions')) {
			res.writeHead(404).end();
			return;
		}
		let raw = '';
		req.on('data', (d) => (raw += d));
		req.on('end', async () => {
			let body = {};
			try {
				body = JSON.parse(raw);
			} catch {
				res.writeHead(400).end();
				return;
			}
			res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' });
			for (const c of script(body)) {
				res.write(c);
				await new Promise((r) => setTimeout(r, 120));
			}
			res.write(usage(120, 30));
			res.end('data: [DONE]\n\n');
		});
	});
	// A port already in use (say the dev preview's) must not take the backend down with it.
	server.on('error', (e) => console.error(`[fakeai] port ${port}: ${e.message}; the built-in AI will not answer here`));
	server.listen(port, '127.0.0.1');
	return server;
}
