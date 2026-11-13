<script lang="ts">
	// AI 團隊名單 (contract 05 §6.18): every role the viewer may see, or, in a
	// project frame, the ones delegated in that project. The numbers count
	// only what the viewer can read (§3.12.5).
	import type { TeamMember } from '$api/types';
	import { api } from '$lib/api';
	import { STATUS_LABEL, TEMPLATE_LABEL } from '$lib/aiteam';
	import ErrorBox from './ErrorBox.svelte';

	let { projectId = null }: { projectId?: string | null } = $props();
	let items = $state<TeamMember[] | null>(null);
	let error = $state<unknown>(null);
	async function load() {
		error = null;
		try {
			items = (await api('GET /ai-team', { query: { project: projectId ?? undefined } })).items;
		} catch (e) {
			error = e;
		}
	}
	$effect(() => {
		void projectId;
		load();
	});
	const href = (m: TeamMember) => `/ai-team/${encodeURIComponent(m.handle)}`;
</script>

<ErrorBox {error} onaction={load} />
{#if items === null}
	{#if !error}<div class="sync"><span class="spin"></span>載入中</div>{/if}
{:else if !items.length}
	<div class="empty" data-testid="team-empty">
		{#if projectId}這個專案沒有委派給任何 AI 角色。{:else}還沒有 AI 團隊成員。<a class="btn sm pri" href="/ai-team/new">建立角色</a>{/if}
	</div>
{:else}
	<div style="overflow-x:auto">
		<table class="t" data-testid="team-table">
			<thead>
				<tr><th>角色</th><th>sponsor</th><th>狀態</th><th class="num">今日寫入</th><th class="num">待審</th><th class="num">7 天被擋</th><th class="num">7 天被復原</th><th class="num">連續寫入轉送審（7 天）</th></tr>
			</thead>
			<tbody>
				{#each items as m (m.id)}
					{@const s = STATUS_LABEL[m.status] ?? { t: m.status, c: 'c-dim' }}
					<tr data-testid="team-row" data-handle={m.handle}>
						<td><a href={href(m)} class="agname">{m.handle}</a> <span class="agent-label">agent</span> <span class="dim">{TEMPLATE_LABEL[m.role.template] ?? m.role.template}</span></td>
						<td>{m.sponsor.name}</td>
						<td><span class="chip {s.c}" title={s.tip} data-testid="team-status">{s.t}</span></td>
						<td class="num">{m.stats.writes_today}</td>
						<td class="num">{m.stats.pending_reviews}</td>
						<td class="num">{m.stats.blocked_7d ?? '—'}</td>
						<td class="num">{m.stats.undo_rate_7d.undone}／{m.stats.undo_rate_7d.total}</td>
						<td class="num">{m.stats.chain_reviews_7d}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
	<p class="hint">數字只包含你看得到的專案與議題。「連續寫入轉送審」只限制同一張議題上的連續 agent 寫入。</p>
{/if}

<style>
	.num { text-align: right; white-space: nowrap; }
</style>
