<script lang="ts">
	// Layout B, left: navigation, collapsible to icons.
	import { page } from '$app/state';
	import { session } from '$lib/session.svelte';
	import { prefs, savePrefs } from '$lib/prefs.svelte';
	import { counts, awaitingLabel, attentionContexts } from '$lib/counts.svelte';
	import { current } from '$lib/current.svelte';

	const path = $derived(page.url.pathname as string);
	const search = $derived(page.url.search);
	const mini = $derived(prefs.navMini);
	// The project the middle column is showing (a project tab or an issue in it).
	const projectKey = $derived(current.projectKey);
	const attention = $derived(attentionContexts().length);

	function toggle() {
		prefs.navMini = !prefs.navMini;
		savePrefs();
	}
	const items = $derived([
		{ href: '/', ic: '⌂', label: '首頁', on: path === '/' },
		{ href: '/search?assignee=me&status=open', ic: '◉', label: '我的工作', on: path === '/search' && search.includes('assignee=me') },
		{ href: '/review', ic: '✓', label: '待審', on: path.startsWith('/review') || path.startsWith('/cs/'), badge: counts.awaiting.length ? awaitingLabel() : '' },
		{ href: '/search', ic: '☰', label: '全部議題', on: path === '/search' && !search.includes('assignee=me') },
		{ href: '/views', ic: '▤', label: '我的視圖', on: path.startsWith('/views') }
	]);
</script>

<nav class="lnav" class:mini aria-label="主要導覽">
	<a href="/new" class="newbtn" title="回報問題"><span class="ic">＋</span>{#if !mini}<span class="lb">回報問題</span>{/if}</a>
	{#if mini}<div class="sep"></div>{:else}<div class="sec">工作</div>{/if}
	{#each items as it (it.href)}
		<a href={it.href} class:on={it.on} title={it.label} aria-current={it.on ? 'page' : undefined}>
			<span class="ic">{it.ic}</span>{#if !mini}<span class="lb">{it.label}</span>{/if}
			{#if it.badge}<span class="badge" data-testid="awaiting-badge" title="等我審 {it.badge}">{it.badge}</span>{/if}
		</a>
	{/each}
	{#if mini}<div class="sep"></div>{:else}<div class="sec">專案</div>{/if}
	{#each session.projects as p (p.id)}
		<a href="/project/{p.key}" class:on={projectKey === p.key} title="{p.key} {p.name}">
			<span class="ic">▣</span>{#if !mini}<span class="lb"><span class="mono">{p.key}</span> {p.name}</span>{/if}
		</a>
	{/each}
	{#if mini}<div class="sep"></div>{:else}<div class="sec">agent</div>{/if}
	<a href="/agents" class:on={path.startsWith('/agents')} title="我的 agent 委派">
		<span class="ic">⚙</span>{#if !mini}<span class="lb">我的 agent 委派</span>{/if}
		{#if attention}<span class="badge w" title="有 agent 需要處理">!</span>{/if}
	</a>
	{#if session.user?.is_admin}
		{#if mini}<div class="sep"></div>{:else}<div class="sec">管理</div>{/if}
		<a href="/admin/github" class:on={path.startsWith('/admin')} title="GitHub 連動"><span class="ic">⚒</span>{#if !mini}<span class="lb">GitHub 連動</span>{/if}</a>
	{/if}
	<span style="flex:1"></span>
	<button class="btn sm ghost" onclick={toggle} title={mini ? '展開導覽' : '收合成圖示'}>{mini ? '»' : '« 收合'}</button>
</nav>
