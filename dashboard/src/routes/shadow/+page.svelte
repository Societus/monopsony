<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { ShadowPackage } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import Package from 'lucide-svelte/icons/package';
  import Plus from 'lucide-svelte/icons/plus';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Trash2 from 'lucide-svelte/icons/trash-2';
  import Search from 'lucide-svelte/icons/search';

  let packages = $state<ShadowPackage[]>([]);
  let loading = $state(true);
  let error = $state('');

  async function load() {
    loading = true;
    try {
      packages = await api.shadow.list();
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function unregister(id: string) {
    if (!confirm('Unregister this shadow package?')) return;
    try {
      await api.shadow.unregister(id);
      packages = packages.filter(p => p.id !== id);
    } catch (e: any) {
      error = e.message;
    }
  }

  onMount(load);
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">Shadow Packages</h1>
    <div class="flex gap-2">
      <button class="btn btn-ghost" onclick={load}>
        <RefreshCw size={14} />
        Refresh
      </button>
      <button class="btn btn-primary">
        <Plus size={14} />
        New Recipe
      </button>
    </div>
  </div>

  {#if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  <div class="card overflow-hidden">
    {#if loading}
      <div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>
    {:else if packages.length === 0}
      <div class="p-12 text-center">
        <Package size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
        <p class="text-sm" style="color: var(--color-text-muted);">No shadow packages registered yet</p>
        <p class="mt-1 text-xs" style="color: var(--color-text-muted);">Recipes will appear here once created</p>
      </div>
    {:else}
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
            <th class="px-4 py-2 font-medium">Name</th>
            <th class="px-4 py-2 font-medium">Version</th>
            <th class="px-4 py-2 font-medium">Source Type</th>
            <th class="px-4 py-2 font-medium">Source URL</th>
            <th class="px-4 py-2"></th>
          </tr>
        </thead>
        <tbody>
          {#each packages as pkg}
            <tr class="table-row border-b" style="border-color: var(--color-border-default);">
              <td class="px-4 py-2.5 font-medium">{pkg.name}</td>
              <td class="px-4 py-2.5">{pkg.version}</td>
              <td class="px-4 py-2.5">
                <span class="badge badge-info">{pkg.source_type}</span>
              </td>
              <td class="px-4 py-2.5 max-w-xs truncate font-mono text-xs">{pkg.source_url}</td>
              <td class="px-4 py-2.5">
                <button class="btn-ghost rounded p-1.5" onclick={() => unregister(pkg.id)} title="Unregister">
                  <Trash2 size={14} />
                </button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>

  <!-- Recipe editor link -->
  <div class="card p-5">
    <div class="flex items-center justify-between">
      <div class="flex items-center gap-2">
        <Search size={16} style="color: var(--color-accent);" />
        <div>
          <h2 class="text-sm font-semibold">Recipe Editor</h2>
          <p class="text-xs mt-0.5" style="color: var(--color-text-muted);">
            Interactive TOML editor with live validation and binary estimation
          </p>
        </div>
      </div>
      <a href="/shadow/recipes/new" class="btn btn-primary">
        <Plus size={14} />
        New Recipe
      </a>
    </div>
  </div>
</div>
