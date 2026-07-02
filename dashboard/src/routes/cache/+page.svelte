<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { CacheEntry, CacheStats } from '$lib/api/types';
  import { formatBytes, formatTimestamp } from '$lib/utils/format';
  import Trash2 from 'lucide-svelte/icons/trash-2';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Database from 'lucide-svelte/icons/database';

  let entries = $state<CacheEntry[]>([]);
  let cacheStats = $state<CacheStats | null>(null);
  let loading = $state(true);
  let error = $state('');
  let searchQuery = $state('');

  async function load() {
    loading = true;
    try {
      const [e, s] = await Promise.all([api.cache.list(), api.cache.stats()]);
      entries = e;
      cacheStats = s;
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function purge(key: string) {
    if (!confirm(`Purge cache entry?\n${key}`)) return;
    try {
      await api.cache.purge(key);
      entries = entries.filter(e => e.url !== key);
    } catch (e: any) {
      error = e.message;
    }
  }

  onMount(load);

  const filtered = $derived(
    searchQuery
      ? entries.filter(e => e.url.toLowerCase().includes(searchQuery.toLowerCase()))
      : entries
  );
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">Proxy Cache</h1>
    <button class="btn btn-ghost" onclick={load}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  <!-- Stats bar -->
  {#if cacheStats}
    <div class="grid grid-cols-3 gap-4">
      <div class="card p-4">
        <span class="text-xs uppercase tracking-wide" style="color: var(--color-text-muted);">Total Entries</span>
        <p class="mt-1 text-xl font-semibold">{cacheStats.total_entries}</p>
      </div>
      <div class="card p-4">
        <span class="text-xs uppercase tracking-wide" style="color: var(--color-text-muted);">Total Size</span>
        <p class="mt-1 text-xl font-semibold">{formatBytes(cacheStats.total_size_bytes)}</p>
      </div>
      <div class="card p-4">
        <span class="text-xs uppercase tracking-wide" style="color: var(--color-text-muted);">Meta Files</span>
        <p class="mt-1 text-xl font-semibold">{cacheStats.total_meta_files}</p>
      </div>
    </div>
  {/if}

  {#if error}
    <div class="card p-4 border-l-4" style="border-left-color: var(--color-status-error);">
      <p class="text-sm" style="color: var(--color-status-error);">{error}</p>
    </div>
  {/if}

  <!-- Cache table -->
  <div class="card overflow-hidden">
    <div class="flex items-center gap-3 border-b px-4 py-3" style="border-color: var(--color-border-default);">
      <Database size={16} style="color: var(--color-text-muted);" />
      <input
        class="input flex-1"
        placeholder="Filter by URL..."
        bind:value={searchQuery}
      />
    </div>

    {#if loading}
      <div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>
    {:else if filtered.length === 0}
      <div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">
        {entries.length === 0 ? 'No cached entries' : 'No matches'}
      </div>
    {:else}
      <div class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
              <th class="px-4 py-2 font-medium">URL</th>
              <th class="px-4 py-2 font-medium">Size</th>
              <th class="px-4 py-2 font-medium">Hits</th>
              <th class="px-4 py-2 font-medium">Cached</th>
              <th class="px-4 py-2 font-medium">Last Access</th>
              <th class="px-4 py-2"></th>
            </tr>
          </thead>
          <tbody>
            {#each filtered.slice(0, 100) as entry}
              <tr class="table-row border-b" style="border-color: var(--color-border-default); opacity: 0.9;">
                <td class="px-4 py-2.5 max-w-md truncate font-mono text-xs">{entry.url}</td>
                <td class="px-4 py-2.5 whitespace-nowrap">{formatBytes(entry.content_length)}</td>
                <td class="px-4 py-2.5">{entry.hit_count}</td>
                <td class="px-4 py-2.5 whitespace-nowrap text-xs">{formatTimestamp(entry.cached_at)}</td>
                <td class="px-4 py-2.5 whitespace-nowrap text-xs">{formatTimestamp(entry.last_accessed)}</td>
                <td class="px-4 py-2.5">
                  <button class="btn-ghost rounded p-1.5" onclick={() => purge(entry.url)} title="Purge">
                    <Trash2 size={14} />
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if filtered.length > 100}
        <div class="px-4 py-2 text-xs text-center" style="color: var(--color-text-muted);">
          Showing 100 of {filtered.length} entries
        </div>
      {/if}
    {/if}
  </div>
</div>
