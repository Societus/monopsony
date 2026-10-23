// Typed API client for Monopsony admin endpoints

import { goto } from '$app/navigation';
import { page } from '$app/stores';
import { get } from 'svelte/store';
import type {
  AuthStatus, LoginRequest, LoginResponse,
  HealthResponse, StatsResponse,
  CacheEntry, CacheStats,
  ShadowPackage,
  RecipeValidationResult, EstimateResponse,
  ClientInfo, ConfigResponse,
  ShadowContext, AssignRecipeResponse,
  MirrorConfig, MirrorStatus,
  TrackerStatus,
  FeatureFlags, MonetizationConfig,
  PendingPairRequest, PairApproveResponse, PairAuditEntry,
} from './types';

const BASE = import.meta.env.VITE_API_BASE || '/api/v1';

// Track whether we're already redirecting to avoid loops
let redirecting = false;

async function request<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json', ...options?.headers },
    ...options,
  });

  if (res.status === 401) {
    // For write endpoints (POST/PUT/DELETE) or config reads, redirect to login
    const method = options?.method || 'GET';
    const isWrite = method !== 'GET' || path.includes('/config');

    if (isWrite) {
      throw new Error('__AUTH_REQUIRED__');
    }

    // Read endpoint returned 401 — means always_require_login or funnel mode
    // Redirect to login (unless we're already there)
    const currentPath = get(page).url.pathname;
    if (!redirecting && currentPath !== '/login') {
      redirecting = true;
      await goto('/login');
      redirecting = false;
    }
    throw new Error('Unauthorized');
  }

  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(body.message || body.reason || body.error || `HTTP ${res.status}`);
  }

  return res.json();
}

// ─── Auth ──────────────────────────────────────────────────────────

export const api = {
  auth: {
    status: () => request<AuthStatus>('/auth/status'),
    login: (req: LoginRequest) =>
      request<LoginResponse>('/auth/login', { method: 'POST', body: JSON.stringify(req) }),
    logout: () =>
      request<{ success: boolean }>('/auth/logout', { method: 'POST' }),
  },

  health: {
    get: () => request<HealthResponse>('/health'),
    stats: () => request<StatsResponse>('/stats'),
  },

  cache: {
    list: () => request<CacheEntry[]>('/cache'),
    stats: () => request<CacheStats>('/cache/stats'),
    purge: (key: string) =>
      request<{ success: boolean }>(`/cache/${encodeURIComponent(key)}`, { method: 'DELETE' }),
  },

  shadow: {
    list: () => request<ShadowPackage[]>('/shadow/packages'),
    register: (pkg: { name: string; source_type: string; source_url: string; client_id?: string }) =>
      request<{ success: boolean }>('/shadow/packages', { method: 'POST', body: JSON.stringify(pkg) }),
    unregister: (id: string) =>
      request<{ success: boolean }>(`/shadow/packages/${id}`, { method: 'DELETE' }),
    checkUpdates: (id: string) =>
      request<{ success: boolean }>(`/shadow/packages/${id}/check`, { method: 'POST' }),
    recipe: {
      validate: (toml: string) =>
        request<RecipeValidationResult>('/shadow/recipe/validate', { method: 'POST', body: JSON.stringify({ recipe_toml: toml }) }),
      estimate: (data: { binary_path: string; source_url?: string; client_id?: string }) =>
        request<EstimateResponse>('/shadow/recipe/estimate', { method: 'POST', body: JSON.stringify(data) }),
    },
  },

  config: {
    get: () => request<ConfigResponse>('/config'),
    update: (toml: string) =>
      request<{ success: boolean }>('/config', { method: 'POST', body: JSON.stringify({ toml }) }),
  },

  clients: {
    list: () => request<ClientInfo[]>('/clients'),
    get: (id: string) => request<ClientInfo>(`/clients/${id}`),
    inventory: (id: string) => request<{ packages: { name: string; version: string }[] }>(`/clients/${id}/inventory`),
    getShadowContext: (id: string) =>
      request<ShadowContext>(`/admin/clients/${encodeURIComponent(id)}/shadow-context`),
    assignRecipe: (id: string, recipeName: string) =>
      request<AssignRecipeResponse>(`/admin/clients/${encodeURIComponent(id)}/assign-recipe`, {
        method: 'POST',
        body: JSON.stringify({ recipe_name: recipeName }),
      }),
  },

  // Pairing v2 admin approval (auth-gated — pending/approve/deny/audit).
  pairing: {
    pending: () => request<PendingPairRequest[]>('/pair/pending'),
    approve: (requestId: string, pin: string) =>
      request<PairApproveResponse>('/pair/approve', {
        method: 'POST',
        body: JSON.stringify({ request_id: requestId, pin }),
      }),
    deny: (requestId: string) =>
      request<{ status: string }>(`/pair/deny/${encodeURIComponent(requestId)}`, { method: 'POST' }),
    audit: () => request<PairAuditEntry[]>('/pair/audit'),
  },

  mirrors: {
    list: () => request<MirrorConfig[]>('/mirrors'),
    upsert: (config: { distro: string; upstream_url: string; sync_enabled?: boolean; sync_interval_secs?: number; bandwidth_limit_bps?: number; public?: boolean }) =>
      request<{ success: boolean }>('/mirrors', { method: 'POST', body: JSON.stringify(config) }),
    delete: (distro: string) =>
      request<{ success: boolean }>(`/mirrors/${distro}`, { method: 'DELETE' }),
    sync: (distro: string) =>
      request<{ success: boolean }>(`/mirrors/${distro}/sync`, { method: 'POST' }),
    status: (distro: string) =>
      request<MirrorStatus>(`/mirrors/${distro}/status`),
  },

  tracker: {
    status: () => request<TrackerStatus>('/tracker/status'),
    mirrors: () => request<{ mirrors: any[]; message: string }>('/tracker/mirrors'),
    reports: () => request<{ reports: any[]; message: string }>('/tracker/reports'),
  },

  features: {
    get: () => request<FeatureFlags>('/features'),
    update: (flags: FeatureFlags) =>
      request<{ success: boolean }>('/features', { method: 'PUT', body: JSON.stringify(flags) }),
    monetization: {
      get: () => request<MonetizationConfig>('/features/monetization'),
      update: (config: MonetizationConfig) =>
        request<{ success: boolean }>('/features/monetization', { method: 'PUT', body: JSON.stringify(config) }),
    },
  },
};
