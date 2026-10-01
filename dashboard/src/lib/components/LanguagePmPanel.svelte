<script lang="ts">
  import { onMount } from 'svelte';
  import Package from 'lucide-svelte/icons/package';

  type PmName = 'npm' | 'pip' | 'cargo';
  type PmSource = 'default' | 'auto' | 'manual';
  type PmState = { enabled: boolean; source: PmSource };
  type LanguagePms = Record<PmName, PmState>;

  const BASE: string = import.meta.env.VITE_API_BASE || '/api/v1';
  const PMS: readonly PmName[] = ['npm', 'pip', 'cargo'];

  let states = $state<LanguagePms | null>(null);
  let loading = $state(true);
  let error = $state('');
  let savingPm = $state<PmName | null>(null);

  function badgeColor(source: PmSource): string {
    if (source === 'manual') return 'var(--color-status-success)';
    if (source === 'auto') return 'var(--color-accent)';
    return 'var(--color-text-muted)';
  }

  async function load() {
    loading = true;
    try {
      const res = await fetch(`${BASE}/admin/language-pms`, { credentials: 'same-origin' });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body.message || body.reason || body.error || `HTTP ${res.status}`);
      }
      states = await res.json();
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function toggle(pm: PmName) {
    if (!states || savingPm) return;
    const prev = states[pm].enabled;
    // Optimistic update; revert on failure
    states[pm].enabled = !prev;
    savingPm = pm;
    error = '';
    try {
      const res = await fetch(`${BASE}/admin/language-pms`, {
        method: 'PATCH',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pm, enabled: !prev }),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body.message || body.reason || body.error || `HTTP ${res.status}`);
      }
      states = await res.json();
    } catch (e: any) {
      if (states) states[pm].enabled = prev;
      error = e.message;
    } finally {
      savingPm = null;
    }
  }

  onMount(load);
</script>

<div class="card p-5 space-y-4">
  <div class="flex items-center gap-2">
    <Package size={16} style="color: var(--color-accent);" />
    <h2 class="text-sm font-semibold">Language package managers</h2>
  </div>
  <p class="text-xs" style="color: var(--color-text-muted);">
    Enable caching of language-ecosystem packages (npm registry, PyPI, crates.io). Changes apply immediately — no restart required.
  </p>

  {#if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  {#if loading}
    <div class="text-sm" style="color: var(--color-text-muted);">Loading...</div>
  {:else if states}
    <div class="space-y-2">
      {#each PMS as pm (pm)}
        <div class="flex items-center justify-between gap-3 rounded px-2 py-2" style="background-color: var(--color-bg-surface-2);">
          <div class="flex items-center gap-2">
            <label for={`langpm-${pm}`} class="text-sm font-medium cursor-pointer">{pm}</label>
            <span
              class="badge"
              style={`color: ${badgeColor(states[pm].source)}; border: 1px solid ${badgeColor(states[pm].source)};`}
            >
              {states[pm].source}
            </span>
          </div>
          <button
            id={`langpm-${pm}`}
            type="button"
            role="switch"
            aria-checked={states[pm].enabled}
            aria-label={`Enable ${pm} caching`}
            disabled={savingPm !== null}
            class="relative inline-flex h-5 w-9 items-center rounded-full transition-colors shrink-0 cursor-pointer"
            style={`background-color: ${states[pm].enabled ? 'var(--color-accent)' : 'var(--color-text-muted)'}; opacity: ${savingPm === pm ? 0.6 : 1};`}
            onclick={() => toggle(pm)}
          >
            <span
              class="inline-block h-4 w-4 transform rounded-full bg-white transition-transform"
              style={`transform: translateX(${states[pm].enabled ? '18px' : '2px'});`}
            ></span>
          </button>
        </div>
      {/each}
    </div>
  {/if}
</div>
