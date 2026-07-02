<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { HealthResponse, StatsResponse } from '$lib/api/types';
  import { formatBytes, formatDuration } from '$lib/utils/format';
  import Circle from 'lucide-svelte/icons/circle';
  import Server from 'lucide-svelte/icons/server';
  import Database from 'lucide-svelte/icons/database';
  import Package from 'lucide-svelte/icons/package';
  import HardDrive from 'lucide-svelte/icons/hard-drive';
  import Users from 'lucide-svelte/icons/users';

  let health = $state<HealthResponse | null>(null);
  let stats = $state<StatsResponse | null>(null);
  let loading = $state(true);
  let error = $state('');

  async function load() {
    loading = true;
    try {
      [health, stats] = await Promise.all([api.health.get(), api.health.stats()]);
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  onMount(load);

  const serviceIcons: Record<string, any> = {
    database: Database,
    shadow: Package,
    mirror: Server,
    tls: Server,
  };

  const statCards = $derived([
    { label: 'Clients', value: stats?.total_clients ?? '—', icon: Users },
    { label: 'Packages Tracked', value: stats?.total_packages ?? '—', icon: Package },
    { label: 'Shadow Packages', value: stats?.shadow_package_count ?? '—', icon: Package },
    { label: 'Cache Dir', value: stats?.cache_exists ? 'Active' : 'Missing', icon: HardDrive },
  ]);
</script>

<div class="space-y-6">
  <h1 class="text-xl font-semibold">System Overview</h1>

  {#if loading}
    <div class="text-sm" style="color: var(--color-text-muted);">Loading...</div>
  {:else if error}
    <div class="card p-4">
      <p class="text-sm" style="color: var(--color-status-error);">{error}</p>
      <button class="btn btn-secondary mt-3" onclick={load}>Retry</button>
    </div>
  {:else}
    <!-- Stat cards -->
    <div class="grid grid-cols-4 gap-4">
      {#each statCards as card}
        <div class="card p-4">
          <div class="flex items-center justify-between">
            <span class="text-xs font-medium uppercase tracking-wide" style="color: var(--color-text-muted);">
              {card.label}
            </span>
            <card.icon size={16} style="color: var(--color-text-muted);" />
          </div>
          <p class="mt-2 text-2xl font-semibold">{card.value}</p>
        </div>
      {/each}
    </div>

    <!-- Service health -->
    <div class="card p-5">
      <h2 class="mb-4 text-sm font-semibold uppercase tracking-wide" style="color: var(--color-text-muted);">
        Service Health
      </h2>
      <div class="grid grid-cols-2 gap-4">
        {#if health}
          {#each Object.entries(health.services) as [name, svc]}
            {@const Cmp = serviceIcons[name] || Server}
            <div class="flex items-center justify-between rounded px-4 py-3" style="background-color: var(--color-bg-surface-2);">
              <div class="flex items-center gap-3">
                <Cmp size={16} style="color: var(--color-text-muted);" />
                <span class="text-sm font-medium capitalize">{name}</span>
              </div>
              <div class="flex items-center gap-2">
                <span class="text-xs" style="color: var(--color-text-muted);">{svc.status}</span>
                {#if svc.healthy}
                  <Circle size={8} fill="var(--color-status-success)" color="var(--color-status-success)" />
                {:else}
                  <Circle size={8} fill="var(--color-status-error)" color="var(--color-status-error)" />
                {/if}
              </div>
            </div>
          {/each}
        {/if}
      </div>
    </div>

    <!-- Cache info -->
    {#if stats}
      <div class="card p-5">
        <h2 class="mb-3 text-sm font-semibold uppercase tracking-wide" style="color: var(--color-text-muted);">
          Cache
        </h2>
        <div class="flex items-center gap-2 text-sm">
          <HardDrive size={14} style="color: var(--color-text-muted);" />
          <span style="color: var(--color-text-muted);">Directory:</span>
          <code class="text-xs">{stats.cache_dir}</code>
          {#if stats.cache_exists}
            <span class="badge badge-success">Active</span>
          {:else}
            <span class="badge badge-error">Missing</span>
          {/if}
        </div>
      </div>
    {/if}
  {/if}
</div>
