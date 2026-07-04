<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { MirrorConfig, FeatureFlags } from '$lib/api/types';
  import { formatBytes, formatTimestamp } from '$lib/utils/format';
  import Server from 'lucide-svelte/icons/server';
  import Plus from 'lucide-svelte/icons/plus';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Trash2 from 'lucide-svelte/icons/trash-2';
  import Zap from 'lucide-svelte/icons/zap';
  import Save from 'lucide-svelte/icons/save';
  import X from 'lucide-svelte/icons/x';

  let mirrors = $state<MirrorConfig[]>([]);
  let features = $state<FeatureFlags | null>(null);
  let loading = $state(true);
  let error = $state('');
  let showAdd = $state(false);
  let saving = $state(false);

  // Add mirror form
  let newDistro = $state('');
  let newUpstream = $state('');
  let newSyncInterval = $state('3600');
  let newPublic = $state(false);

  // Feature toggles
  let shadowEnabled = $state(false);
  let storageQuota = $state(false);
  let storageQuotaGb = $state('100');
  let bandwidthQuota = $state(false);
  let bandwidthQuotaMbps = $state('100');
  let trustMode = $state('open');
  let savingFeatures = $state(false);
  let featureMsg = $state('');

  async function load() {
    loading = true;
    try {
      [mirrors, features] = await Promise.all([api.mirrors.list(), api.features.get()]);
      if (features) {
        shadowEnabled = features.shadow_enabled;
        storageQuota = features.storage_quota_enabled;
        storageQuotaGb = String(features.storage_quota_gb);
        bandwidthQuota = features.bandwidth_quota_enabled;
        bandwidthQuotaMbps = String(features.bandwidth_quota_mbps);
        trustMode = features.trust_mode;
      }
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function addMirror() {
    if (!newDistro.trim() || !newUpstream.trim()) {
      error = 'Distro and upstream URL are required';
      return;
    }
    saving = true;
    try {
      await api.mirrors.upsert({
        distro: newDistro,
        upstream_url: newUpstream,
        sync_interval_secs: parseInt(newSyncInterval) || 3600,
        public: newPublic,
      });
      showAdd = false;
      newDistro = '';
      newUpstream = '';
      newSyncInterval = '3600';
      newPublic = false;
      await load();
    } catch (e: any) {
      error = e.message;
    } finally {
      saving = false;
    }
  }

  async function removeMirror(distro: string) {
    if (!confirm(`Remove mirror "${distro}"?`)) return;
    try {
      await api.mirrors.delete(distro);
      mirrors = mirrors.filter(m => m.distro !== distro);
    } catch (e: any) {
      error = e.message;
    }
  }

  async function triggerSync(distro: string) {
    const mirror = mirrors.find(m => m.distro === distro);
    if (mirror) {
      mirror.sync_status = 'syncing';
    }
    try {
      const res = await api.mirrors.sync(distro);
      // Refresh after a short delay to show updated status
      setTimeout(() => load(), 3000);
    } catch (e: any) {
      error = e.message;
      // Restore status on error
      if (mirror) {
        mirror.sync_status = 'idle';
      }
    }
  }

  async function saveFeatures() {
    savingFeatures = true;
    featureMsg = '';
    try {
      await api.features.update({
        shadow_enabled: shadowEnabled,
        storage_quota_enabled: storageQuota,
        storage_quota_gb: parseInt(storageQuotaGb) || 0,
        bandwidth_quota_enabled: bandwidthQuota,
        bandwidth_quota_mbps: parseInt(bandwidthQuotaMbps) || 0,
        trust_mode: trustMode,
        auto_blacklist_threshold: 5,
      });
      featureMsg = 'Feature flags saved';
    } catch (e: any) {
      error = e.message;
    } finally {
      savingFeatures = false;
    }
  }

  onMount(load);
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">Mirrors</h1>
    <div class="flex gap-2">
      <button class="btn btn-ghost" onclick={load}>
        <RefreshCw size={14} />
        Refresh
      </button>
      <button class="btn btn-primary" onclick={() => (showAdd = true)}>
        <Plus size={14} />
        Add Mirror
      </button>
    </div>
  </div>

  {#if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  <!-- Mirror table -->
  <div class="card overflow-hidden">
    {#if loading}
      <div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>
    {:else if mirrors.length === 0}
      <div class="p-12 text-center">
        <Server size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
        <p class="text-sm" style="color: var(--color-text-muted);">No mirrors configured</p>
      </div>
    {:else}
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
            <th class="px-4 py-2 font-medium">Distro</th>
            <th class="px-4 py-2 font-medium">Upstream</th>
            <th class="px-4 py-2 font-medium">Sync</th>
            <th class="px-4 py-2 font-medium">Size</th>
            <th class="px-4 py-2 font-medium">Last Sync</th>
            <th class="px-4 py-2 font-medium">Access</th>
            <th class="px-4 py-2"></th>
          </tr>
        </thead>
        <tbody>
          {#each mirrors as m}
            <tr class="table-row border-b" style="border-color: var(--color-border-default);">
              <td class="px-4 py-2.5 font-medium">{m.distro}</td>
              <td class="px-4 py-2.5 max-w-xs truncate font-mono text-xs">{m.upstream_url}</td>
              <td class="px-4 py-2.5">
                {#if m.sync_enabled || m.sync_status === 'syncing'}
                  {#if m.sync_status === 'syncing'}
                    <span class="badge badge-info animate-pulse">syncing…</span>
                  {:else}
                    <span class="badge badge-success">{m.sync_status || 'enabled'}</span>
                  {/if}
                {:else}
                  <span class="badge badge-muted">disabled</span>
                {/if}
              </td>
              <td class="px-4 py-2.5 whitespace-nowrap">{formatBytes(m.total_bytes)}</td>
              <td class="px-4 py-2.5 text-xs whitespace-nowrap">{formatTimestamp(m.last_sync)}</td>
              <td class="px-4 py-2.5">
                {#if m.public}
                  <span class="badge badge-info">public</span>
                {:else}
                  <span class="badge badge-muted">private</span>
                {/if}
              </td>
              <td class="px-4 py-2.5">
                <div class="flex gap-1">
                  <button class="btn-ghost rounded p-1.5" onclick={() => triggerSync(m.distro)} title="Sync now" disabled={m.sync_status === 'syncing'}>
                    {#if m.sync_status === 'syncing'}
                      <RefreshCw size={14} class="animate-spin" />
                    {:else}
                      <Zap size={14} />
                    {/if}
                  </button>
                  <button class="btn-ghost rounded p-1.5" onclick={() => removeMirror(m.distro)} title="Remove">
                    <Trash2 size={14} />
                  </button>
                </div>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>

  <!-- Feature toggles -->
  <div class="card p-5 space-y-4">
    <div class="flex items-center justify-between">
      <h2 class="text-sm font-semibold">Feature Toggles</h2>
      <button class="btn btn-primary" onclick={saveFeatures} disabled={savingFeatures}>
        <Save size={14} />
        {savingFeatures ? 'Saving...' : 'Save'}
      </button>
    </div>

    {#if featureMsg}
      <p class="text-xs" style="color: var(--color-status-success);">{featureMsg}</p>
    {/if}

    <div class="grid grid-cols-2 gap-4">
      <!-- Shadow support -->
      <label class="flex items-center gap-3 rounded p-3" style="background-color: var(--color-bg-surface-2);">
        <input type="checkbox" bind:checked={shadowEnabled} class="w-4 h-4 accent-[var(--color-accent)]" />
        <div>
          <div class="text-sm font-medium">Shadow Packages</div>
          <div class="text-xs" style="color: var(--color-text-muted);">Enable shadow package distribution</div>
        </div>
      </label>

      <!-- Trust mode -->
      <div class="rounded p-3" style="background-color: var(--color-bg-surface-2);">
        <div class="text-sm font-medium mb-1">Client Trust Mode</div>
        <select class="input text-xs" bind:value={trustMode}>
          <option value="open">Open (no auth)</option>
          <option value="token">Token</option>
          <option value="jwt">JWT</option>
          <option value="mtls">mTLS</option>
        </select>
      </div>

      <!-- Storage quota -->
      <div class="rounded p-3" style="background-color: var(--color-bg-surface-2);">
        <label class="flex items-center gap-2 mb-2">
          <input type="checkbox" bind:checked={storageQuota} class="w-4 h-4 accent-[var(--color-accent)]" />
          <span class="text-sm font-medium">Storage Quota</span>
        </label>
        {#if storageQuota}
          <input type="number" class="input text-xs" bind:value={storageQuotaGb} placeholder="GB" />
        {/if}
      </div>

      <!-- Bandwidth quota -->
      <div class="rounded p-3" style="background-color: var(--color-bg-surface-2);">
        <label class="flex items-center gap-2 mb-2">
          <input type="checkbox" bind:checked={bandwidthQuota} class="w-4 h-4 accent-[var(--color-accent)]" />
          <span class="text-sm font-medium">Bandwidth Quota</span>
        </label>
        {#if bandwidthQuota}
          <input type="number" class="input text-xs" bind:value={bandwidthQuotaMbps} placeholder="Mbps" />
        {/if}
      </div>
    </div>
  </div>
</div>

<!-- Add mirror modal -->
{#if showAdd}
  <div class="fixed inset-0 z-50 flex items-center justify-center" style="background-color: rgba(0,0,0,0.6);" onclick={() => (showAdd = false)} role="presentation">
    <div class="card w-full max-w-md p-6 space-y-4" style="background-color: var(--color-bg-surface);" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
      <div class="flex items-center justify-between">
        <h2 class="text-sm font-semibold">Add Mirror</h2>
        <button class="btn-ghost rounded p-1" onclick={() => (showAdd = false)}><X size={16} /></button>
      </div>
      <div class="space-y-3">
        <div>
          <label class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Distro name</label>
          <input class="input" bind:value={newDistro} placeholder="archlinux" />
        </div>
        <div>
          <label class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Upstream URL</label>
          <input class="input font-mono text-xs" bind:value={newUpstream} placeholder="https://mirror.example.com/archlinux" />
        </div>
        <div>
          <label class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Sync interval (seconds)</label>
          <input class="input" type="number" bind:value={newSyncInterval} />
        </div>
        <label class="flex items-center gap-2">
          <input type="checkbox" bind:checked={newPublic} class="w-4 h-4 accent-[var(--color-accent)]" />
          <span class="text-sm">Public mirror (no auth required)</span>
        </label>
      </div>
      <div class="flex justify-end gap-2">
        <button class="btn btn-secondary" onclick={() => (showAdd = false)}>Cancel</button>
        <button class="btn btn-primary" onclick={addMirror} disabled={saving}>
          {saving ? 'Adding...' : 'Add'}
        </button>
      </div>
    </div>
  </div>
{/if}
