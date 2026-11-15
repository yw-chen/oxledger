// SPDX-License-Identifier: GPL-2.0-or-later
// Wiki links in rendered Markdown (design 06 §3.2) point at
// `/projects/KEY/wiki/Title`; the SPA's project pages live under `/project/`.
import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';

export const load: PageLoad = ({ params, url }) => {
	const rest = params.rest ? `/${params.rest.split('/').map(encodeURIComponent).join('/')}` : '';
	redirect(307, `/project/${encodeURIComponent(params.key)}/wiki${rest}${url.search}${url.hash}`);
};
