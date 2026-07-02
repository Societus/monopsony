<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { TrackerStatus } from '$lib/api/types';
  import MonitorSmartphone from 'lucide-svelte/icons/monitor-smartphone';
  import Circle from 'lucide-svelte/icons/circle';
  import ShieldAlert from 'lucide-svelte/icons/shield-alert';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';

  let status = $state<TrackerStatus | null>(null);
  let loading = $state(true);
  let error = $state('');

  async function load() {
    loading = true;
    try {
      status = await api.tracker.status();
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  onMount(load);
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">Tracker & Discovery</h1>
    <button class="btn btn-ghost" onclick={load}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  {#if loading}
    <div class="text-sm" style="color: var(--color-text-muted);">Loading...</div>
  {:else if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {:else if status}
    <!-- Status banner -->
    <div class="card p-5">
      <div class="flex items-center gap-3 mb-2">
        {#if status.running}
          <Circle size={10} fill="var(--color-status-success)" color="var(--color-status-success)" />
          <span class="text-sm font-semibold" style="color: var(--color-status-success);">Tracker Running</span>
        {:else}
          <Circle size={10} fill="var(--color-status-warning)" color="var(--color-status-warning)" />
          <span class="text-sm font-semibold" style="color: var(--color-status-warning);">Tracker Not Running</span>
        {/if}
        <span class="badge badge-info ml-auto">{status.auth_mode}</span>
      </div>
      {#if status.message}
        <p class="text-xs" style="color: var(--color-text-muted);">{status.message}</p>
      {/if}
    </div>

    {#if !status.running}
      <!-- Setup instructions -->
      <div class="card p-5">
        <div class="flex items-center gap-2 mb-3">
          <MonitorSmartphone size={16} style="color: var(--color-accent);" />
          <h2 class="text-sm font-semibold">Starting the Tracker</h2>
        </div>
        <p class="text-xs mb-3" style="color: var(--color-text-muted);">
          The tracker is a standalone HTTP service for decentralized mirror discovery.
          Mirrors announce themselves to the tracker; clients query it to find nearby mirrors.
        </p>
        <div class="rounded p-3 font-mono text-xs" style="background-color: var(--color-bg-base); color: var(--color-text-primary);">
          <div style="color: var(--color-text-muted);"># Start the tracker on default port 3131</div>
          <div>monopsony-tracker --addr 0.0.0.0:3131</div>
        </div>
      </div>
    {/if}

    <!-- Placeholder for announced mirrors -->
    <div class="card p-12 text-center">
      <MonitorSmartphone size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
      <p class="text-sm" style="color: var(--color-text-muted);">
        {#if status.running}
          No announced mirrors yet
        {:else}
          Start the tracker to see announced mirrors
        {/if}
      </p>
    </div>

    <!-- Auto-blacklist info -->
    <div class="card p-5">
      <div class="flex items-center gap-2 mb-2">
        <ShieldAlert size={16} style="color: var(--color-status-warning);" />
        <h2 class="text-sm font-semibold">Auto-Blacklist Policy</h2>
      </div>
      <p class="text-xs" style="color: var(--color-text-muted);">
        Mirrors receiving 5+ reports within a 24-hour window are automatically blacklisted.
        Configure the threshold in the Mirror feature toggles.
      </p>
    </div>
  {/if}
</div>
