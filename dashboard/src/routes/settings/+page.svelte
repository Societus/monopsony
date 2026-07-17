<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { ConfigResponse } from '$lib/api/types';
  import Save from 'lucide-svelte/icons/save';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import AlertTriangle from 'lucide-svelte/icons/alert-triangle';
  import CheckCircle from 'lucide-svelte/icons/check-circle';
  import HelpTooltip from '$lib/components/HelpTooltip.svelte';

  let config = $state<ConfigResponse | null>(null);
  let editedToml = $state('');
  let loading = $state(true);
  let saving = $state(false);
  let error = $state('');
  let saveMsg = $state('');

  async function load() {
    loading = true;
    try {
      config = await api.config.get();
      editedToml = config.toml || '';
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function save() {
    if (!confirm('Save config? The server will need a restart to apply changes.')) return;
    saving = true;
    saveMsg = '';
    try {
      const result = await api.config.update(editedToml);
      if (result.success) {
        saveMsg = 'Config saved. Restart server to apply.';
      } else {
        error = 'Save failed';
      }
    } catch (e: any) {
      error = e.message;
    } finally {
      saving = false;
    }
  }

  onMount(load);

  const isDirty = $derived(editedToml !== (config?.toml || ''));
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <div class="flex items-center gap-1.5">
      <h1 class="text-xl font-semibold">Settings</h1>
      <HelpTooltip helpKey="gui:settings.editor" label="Settings page" />
    </div>
    <div class="flex gap-2">
      <button class="btn btn-ghost" onclick={load} disabled={loading}>
        <RefreshCw size={14} />
        Reset
      </button>
      <button class="btn btn-primary" onclick={save} disabled={!isDirty || saving}>
        <Save size={14} />
        {saving ? 'Saving...' : 'Save'}
      </button>
    </div>
  </div>

  {#if saveMsg}
    <div class="card p-3 flex items-center gap-2 border-l-4" style="border-left-color: var(--color-status-success);">
      <CheckCircle size={16} style="color: var(--color-status-success);" />
      <span class="text-sm">{saveMsg}</span>
    </div>
  {/if}

  {#if error}
    <div class="card p-3 flex items-center gap-2 border-l-4" style="border-left-color: var(--color-status-error);">
      <AlertTriangle size={16} style="color: var(--color-status-error);" />
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  {#if loading}
    <div class="text-sm" style="color: var(--color-text-muted);">Loading config...</div>
  {:else}
    <!-- Config path -->
    {#if config?.path}
      <div class="flex items-center gap-2 text-xs" style="color: var(--color-text-muted);">
        Config file:
        <code class="rounded px-2 py-1" style="background-color: var(--color-bg-surface-2);">{config.path}</code>
      </div>
    {/if}

    <!-- TOML editor -->
    <div class="card p-4">
      <div class="mb-2 flex items-center justify-between">
        <h2 class="text-sm font-semibold">server.toml</h2>
        {#if isDirty}
          <span class="badge badge-warning">Unsaved changes</span>
        {/if}
      </div>
      <textarea
        class="input font-mono text-xs"
        rows="24"
        bind:value={editedToml}
        spellcheck="false"
      ></textarea>
    </div>

    <!-- Restart warning -->
    {#if isDirty}
      <div class="flex items-center gap-2 text-xs" style="color: var(--color-status-warning);">
        <AlertTriangle size={14} />
        Changes require a server restart to take effect.
      </div>
    {/if}
  {/if}
</div>
