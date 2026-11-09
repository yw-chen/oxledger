// SPDX-License-Identifier: GPL-2.0-or-later
// A minimal MCP client (Streamable HTTP) playing the agent side in E2E and
// manual tests: `new Agent(token).call('changeset_propose', {...})`.
// CLI: node e2e/agent.mjs <token> <tool> '<json args>'

export const MCP_URL = process.env.OX_MCP ?? 'http://127.0.0.1:18080/mcp';

export class Agent {
	constructor(token, url = MCP_URL) {
		this.token = token;
		this.url = url;
		this.session = null;
		this.id = 1;
	}
	async post(body) {
		const headers = {
			'Content-Type': 'application/json',
			Accept: 'application/json, text/event-stream',
			Authorization: `Bearer ${this.token}`,
			Host: 'localhost'
		};
		if (this.session) headers['mcp-session-id'] = this.session;
		const r = await fetch(this.url, { method: 'POST', headers, body: JSON.stringify(body) });
		const sid = r.headers.get('mcp-session-id');
		if (sid && !this.session) this.session = sid;
		return r.text();
	}
	async rpc(method, params) {
		const id = this.id++;
		const text = await this.post({ jsonrpc: '2.0', id, method, params });
		try {
			return JSON.parse(text);
		} catch {
			for (const l of text.split('\n')) {
				if (!l.startsWith('data:')) continue;
				try {
					const v = JSON.parse(l.slice(5).trim());
					if (v.id === id) return v;
				} catch {
					/* next line */
				}
			}
			throw new Error(`no answer: ${text.slice(0, 300)}`);
		}
	}
	async connect() {
		const r = await this.rpc('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'oxledger-e2e', version: '0' } });
		if (!r.result?.serverInfo) throw new Error(`initialize failed: ${JSON.stringify(r)}`);
		await this.post({ jsonrpc: '2.0', method: 'notifications/initialized' });
		return this;
	}
	/** Structured content, or throws with the error object. */
	async call(name, args) {
		if (!this.session) await this.connect();
		const r = await this.rpc('tools/call', { name, arguments: args });
		if (!r.result) throw Object.assign(new Error(JSON.stringify(r.error)), { body: r.error });
		if (r.result.isError) throw Object.assign(new Error(JSON.stringify(r.result.structuredContent)), { body: r.result.structuredContent });
		return r.result.structuredContent;
	}
}

if (import.meta.url === `file://${process.argv[1].replace(/\\/g, '/')}` || process.argv[1]?.endsWith('agent.mjs')) {
	const [token, tool, args] = process.argv.slice(2);
	if (token && tool) {
		new Agent(token)
			.call(tool, JSON.parse(args ?? '{}'))
			.then((v) => console.log(JSON.stringify(v, null, 2)))
			.catch((e) => {
				console.error(e.message);
				process.exit(1);
			});
	}
}
