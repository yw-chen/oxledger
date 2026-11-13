// The 02 delegation page became AI 團隊 (contract 05 §6.18; `/agent-contexts*` goes away in M2.0).
import { redirect } from '@sveltejs/kit';

export function load() {
	redirect(307, '/ai-team');
}
