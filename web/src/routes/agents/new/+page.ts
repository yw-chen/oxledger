// The 02 delegation wizard became the AI 團隊 wizard (contract 05 §6.18).
import { redirect } from '@sveltejs/kit';

export function load() {
	redirect(307, '/ai-team/new');
}
