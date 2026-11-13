<script lang="ts">
	// Layout B, left: navigation, collapsible to icons.
	import { page } from '$app/state';
	import { session } from '$lib/session.svelte';
	import { prefs, savePrefs } from '$lib/prefs.svelte';
	import { counts, awaitingLabel, attentionRoles, triageLabel } from '$lib/counts.svelte';
	import { current } from '$lib/current.svelte';
	import { me, shortcutHref, SHORTCUT_ICON, SHORTCUT_LABEL } from '$lib/me.svelte';

	const path = $derived(page.url.pathname as string);
	const search = $derived(page.url.search);
	const mini = $derived(prefs.navMini);
	// The project the middle column is showing (a project tab or an issue in it).
	const projectKey = $derived(current.projectKey);
	const attention = $derived(attentionRoles().length);

	function toggle() {
		prefs.navMini = !prefs.navMini;
		savePrefs();
	}
	const items = $derived([
		{ href: '/', ic: '⌂', label: '首頁', on: path === '/' },
		{ href: '/search?assignee=me&status=open', ic: '◉', label: '我的工作', on: path === '/search' && search.includes('assignee=me') },
		{ href: '/review', ic: '✓', label: '待審', on: path.startsWith('/review') || path.startsWith('/cs/'), badge: counts.awaiting.length ? awaitingLabel() : '' },
		{ href: '/search', ic: '☰', label: '全部議題', on: path === '/search' && !search.includes('assignee=me') },
		{ href: '/views', ic: '▤', label: '我的視圖', on: path.startsWith('/views') },
		...(counts.triage.length ? [{ href: '/triage', ic: '⇥', label: '分流', on: path.startsWith('/triage'), badge: triageLabel(), testid: 'triage-badge' }] : []),
		{ href: '/time', ic: '◷', label: '工時', on: path.startsWith('/time') }
	]);
	/** §6.12 左欄捷徑 (saved per person); board and timeline open the current project. */
	const shortcuts = $derived(
		(me.prefs?.nav_shortcuts ?? [])
			.filter((s) => s.available)
			.map((s) => ({ s, href: shortcutHref(s, projectKey ?? session.projects[0]?.key ?? null) }))
			.filter((x): x is { s: typeof x.s; href: string } => !!x.href)
	);
</script>

<nav class="lnav" class:mini aria-label="主要導覽">
	<a href="/new" class="newbtn" title="回報問題"><span class="ic">＋</span>{#if !mini}<span class="lb">回報問題</span>{/if}</a>
	{#if mini}<div class="sep"></div>{:else}<div class="sec">工作</div>{/if}
	{#each items as it (it.href)}
		<a href={it.href} class:on={it.on} title={it.label} aria-current={it.on ? 'page' : undefined}>
			<span class="ic">{it.ic}</span>{#if !mini}<span class="lb">{it.label}</span>{/if}
			{#if it.badge}<span class="badge" data-testid={it.testid ?? 'awaiting-badge'} title="{it.label} {it.badge}">{it.badge}</span>{/if}
		</a>
	{/each}
	{#if shortcuts.length}
		{#if mini}<div class="sep"></div>{:else}<div class="sec">捷徑</div>{/if}
		{#each shortcuts as { s, href }, i (i)}
			{@const label = s.kind === 'view' ? (s.name ?? '視圖') : (SHORTCUT_LABEL[s.kind] ?? s.kind)}
			<a {href} class:on={path + search === href || path === href} title={label} data-testid="nav-shortcut"><span class="ic">{SHORTCUT_ICON[s.kind] ?? '★'}</span>{#if !mini}<span class="lb">{label}</span>{/if}</a>
		{/each}
	{/if}
	{#if mini}<div class="sep"></div>{:else}<div class="sec">專案</div>{/if}
	{#each session.projects as p (p.id)}
		<a href="/project/{p.key}" class:on={projectKey === p.key} title="{p.key} {p.name}">
			<span class="ic">▣</span>{#if !mini}<span class="lb"><span class="mono">{p.key}</span> {p.name}</span>{/if}
		</a>
	{/each}
	{#if mini}<div class="sep"></div>{:else}<div class="sec">agent</div>{/if}
	<a href="/ai-team" class:on={path.startsWith('/ai-team') || path.startsWith('/agents')} title="AI 團隊" data-testid="nav-ai-team">
		<span class="ic">⚙</span>{#if !mini}<span class="lb">AI 團隊</span>{/if}
		{#if attention}<span class="badge w" title="有 AI 角色需要處理" data-testid="ai-attention">!</span>{/if}
	</a>
	{#if session.user?.is_admin}
		{#if mini}<div class="sep"></div>{:else}<div class="sec">管理</div>{/if}
		<a href="/admin/timelog" class:on={path.startsWith('/admin/timelog')} title="工時設定"><span class="ic">◷</span>{#if !mini}<span class="lb">工時設定</span>{/if}</a>
		<a href="/admin/custom-fields" class:on={path.startsWith('/admin/custom-fields')} title="自訂欄位"><span class="ic">▤</span>{#if !mini}<span class="lb">自訂欄位</span>{/if}</a>
		<a href="/admin/forms" class:on={path.startsWith('/admin/forms')} title="表單"><span class="ic">▦</span>{#if !mini}<span class="lb">表單</span>{/if}</a>
		<a href="/admin/concepts" class:on={path.startsWith('/admin/concepts')} title="概念與別名"><span class="ic">≈</span>{#if !mini}<span class="lb">概念與別名</span>{/if}</a>
		<a href="/admin/github" class:on={path.startsWith('/admin/github')} title="GitHub 連動"><span class="ic">⚒</span>{#if !mini}<span class="lb">GitHub 連動</span>{/if}</a>
		<a href="/admin/ai" class:on={path.startsWith('/admin/ai')} title="AI 設定" data-testid="nav-admin-ai"><span class="ic">✦</span>{#if !mini}<span class="lb">AI 設定</span>{/if}</a>
	{/if}
	<span style="flex:1"></span>
	<button class="btn sm ghost" onclick={toggle} title={mini ? '展開導覽' : '收合成圖示'}>{mini ? '»' : '« 收合'}</button>
</nav>
