<script lang="ts">
	// §15.2: the token is shown once, with a copy button and the MCP client
	// settings next to it. It lives only in this component's memory: never
	// in the URL, logs or browser storage.
	import type { IssuedCredential } from '$api/types';
	import { fmtShort } from '$lib/format';

	let { token, onclose }: { token: IssuedCredential; onclose: () => void } = $props();
	const url = $derived(`${location.origin}/mcp`);
	const config = $derived(JSON.stringify({ mcpServers: { oxledger: { type: 'http', url, headers: { Authorization: `Bearer ${token.token}` } } } }, null, 2));
	let copied = $state<'' | 'token' | 'config'>('');
	async function copy(what: 'token' | 'config') {
		try {
			await navigator.clipboard.writeText(what === 'token' ? token.token : config);
			copied = what;
		} catch {
			copied = '';
		}
	}
</script>

<div class="card p" data-testid="token-once">
	<b>agent 的 token（只顯示這一次）</b>
	<p class="hint" style="margin:4px 0">關掉之後就看不到了；遺失請在委派頁發新 token。到期 {fmtShort(token.expires_at)}。</p>
	<div class="row" style="gap:8px">
		<code class="mono" style="word-break:break-all" data-testid="token-value">{token.token}</code>
		<button class="btn sm" onclick={() => copy('token')}>{copied === 'token' ? '已複製' : '複製'}</button>
	</div>
	<details style="margin-top:6px">
		<summary>MCP 設定（Claude Code 等 client）</summary>
		<pre class="pre mono" style="font-size:12px">{config}</pre>
		<button class="btn sm" onclick={() => copy('config')}>{copied === 'config' ? '已複製' : '複製設定'}</button>
	</details>
	<div style="margin-top:8px"><button class="btn" onclick={onclose} data-testid="token-done">我已存好，關閉</button></div>
</div>
