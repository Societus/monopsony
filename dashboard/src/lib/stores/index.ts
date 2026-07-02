import { writable } from 'svelte/store';
import type { AuthStatus } from '$lib/api/types';

export const authStatus = writable<AuthStatus | null>(null);
export const sidebarCollapsed = writable(false);
