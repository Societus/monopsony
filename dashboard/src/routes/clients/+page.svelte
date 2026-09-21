<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { ClientInfo, ShadowContext } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import Users from 'lucide-svelte/icons/users';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Circle from 'lucide-svelte/icons/circle';
  import ChevronDown from 'lucide-svelte/icons/chevron-down';
  import Package from 'lucide-svelte/icons/package';

  let clients = $state<ClientInfo[]>([]);
  let loading = $state(true);
  let error = $state('');

  // Shadow context state, keyed by client id
  let expanded = $state<Record<string, boolean>>({});
  let shadowCtx = $state<Record<string, ShadowContext | null>>({});
  let ctxLoading = $state<Record<string, boolean>>({});
  let ctxError = $state<Record<string, string>>({});
  let selectedRecipe = $state<Record<string, string>>({});
  let assignBusy = $state<Record<string, boolean>>({});
  let assignMsg = $state<Record<string, { ok: boolean; text: string }>>({});

  async function load() {
    loading = true;
    try {
      clients = await api.clients.list();
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function loadShadowContext(id: string) {
    ctxLoading[id] = true;
    ctxError[id] = '';
    try {
      shadowCtx[id] = await api.clients.getShadowContext(id);
    } catch (e: any) {
      ctxError[id] = e.message;
    } finally {
      ctxLoading[id] = false;
    }
  }

  function toggleShadow(id: string) {
    expanded[id] = !expanded[id];
    if (expanded[id] && !shadowCtx[id]) {
      loadShadowContext(id);
    }
  }

  async function assignRecipe(id: string) {
    const recipe = selectedRecipe[id];
    if (!recipe) return;
    assignBusy[id] = true;
    assignMsg[id] = null as unknown as { ok: boolean; text: string };
    try {
      const res = await api.clients.assignRecipe(id, recipe);
      if (res.accepted) {
        assignMsg[id] = { ok: true, text: `Recipe "${recipe}" assigned` };
      } else {
        assignMsg[id] = { ok: false, text: res.reason || 'Rejected' };
      }
    } catch (e: any) {
      assignMsg[id] = { ok: false, text: e.message };
    } finally {
      assignBusy[id] = false;
    }
  }

  onMount(load);
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold">Clients</h1>
    <button class="btn btn-ghost" onclick={load}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  {#if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  <div class="card overflow-hidden">
    {#if loading}
      <div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>
    {:else if clients.length === 0}
      <div class="p-12 text-center">
        <Users size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
        <p class="text-sm" style="color: var(--color-text-muted);">No clients registered</p>
        <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
          Pair clients with <code>monopsony-client pair</code>
        </p>
      </div>
    {:else}
      <table class="w-full text-sm">
        <thead>
          <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
            <th class="px-4 py-2 font-medium">Hostname</th>
            <th class="px-4 py-2 font-medium">OS</th>
            <th class="px-4 py-2 font-medium">Registered</th>
            <th class="px-4 py-2 font-medium">Last Seen</th>
            <th class="px-4 py-2 font-medium">Status</th>
            <th class="px-4 py-2 font-medium">Shadow</th>
          </tr>
        </thead>
        <tbody>
          {#each clients as client (client.id)}
            <tr class="table-row border-b" style="border-color: var(--color-border-default);">
              <td class="px-4 py-2.5 font-medium">{client.hostname}</td>
              <td class="px-4 py-2.5">{client.os}</td>
              <td class="px-4 py-2.5 text-xs">{formatTimestamp(client.registered_at)}</td>
              <td class="px-4 py-2.5 text-xs">{formatTimestamp(client.last_seen)}</td>
              <td class="px-4 py-2.5">
                <span class="badge badge-success">
                  <Circle size={8} fill="currentColor" />
                  Active
                </span>
              </td>
              <td class="px-4 py-2.5">
                <button
                  class="btn btn-ghost"
                  aria-expanded={!!expanded[client.id]}
                  aria-controls={'shadow-panel-' + client.id}
                  onclick={() => toggleShadow(client.id)}
                >
                  <ChevronDown size={14} class={expanded[client.id] ? 'rotate-180' : ''} />
                  Shadow packages
                </button>
              </td>
            </tr>
            {#if expanded[client.id]}
              <tr>
                <td colspan="6" class="px-4 py-3" id={'shadow-panel-' + client.id} style="background: var(--color-bg-subtle, transparent);">
                  {#if ctxLoading[client.id]}
                    <div class="text-xs" style="color: var(--color-text-muted);">Loading shadow context...</div>
                  {:else if ctxError[client.id]}
                    <div class="text-xs" style="color: var(--color-status-error);">{ctxError[client.id]}</div>
                  {:else if shadowCtx[client.id]}
                    {@const ctx = shadowCtx[client.id]!}
                    <div class="grid gap-4 md:grid-cols-2">
                      <div>
                        <h3 class="text-xs font-semibold mb-1">Inventory</h3>
                        {#if ctx.inventory.length === 0}
                          <p class="text-xs" style="color: var(--color-text-muted);">No inventory reported</p>
                        {:else}
                          <ul class="text-xs space-y-0.5 font-mono">
                            {#each ctx.inventory as item}
                              <li>{item.name}@{item.version} <span style="color: var(--color-text-muted);">via {item.manager}</span></li>
                            {/each}
                          </ul>
                        {/if}
                      </div>
                      <div>
                        <h3 class="text-xs font-semibold mb-1">Scan findings</h3>
                        {#if ctx.scan_findings.length === 0}
                          <p class="text-xs" style="color: var(--color-text-muted);">No findings</p>
                        {:else}
                          <ul class="text-xs space-y-0.5">
                            {#each ctx.scan_findings as finding}
                              <li>
                                <span class="badge" class:badge-success={finding.installed}>{finding.recipe_name}</span>
                                {finding.installed ? 'installed' : 'not installed'}
                                {#if finding.detected_version}
                                  &middot; {finding.detected_version}
                                {/if}
                                <span style="color: var(--color-text-muted);">({finding.method})</span>
                              </li>
                            {/each}
                          </ul>
                        {/if}
                      </div>
                    </div>
                    <div class="mt-3 flex items-center gap-2">
                      <Package size={14} style="color: var(--color-text-muted);" />
                      <label class="text-xs" for={'assign-recipe-' + client.id}>Assign recipe</label>
                      <select
                        id={'assign-recipe-' + client.id}
                        class="input text-xs"
                        bind:value={selectedRecipe[client.id]}
                      >
                        <option value="" disabled selected>Select a recipe</option>
                        {#each ctx.catalog as recipe}
                          <option value={recipe}>{recipe}</option>
                        {/each}
                      </select>
                      <button
                        class="btn btn-primary text-xs"
                        disabled={!selectedRecipe[client.id] || assignBusy[client.id]}
                        onclick={() => assignRecipe(client.id)}
                      >
                        {assignBusy[client.id] ? 'Assigning...' : 'Assign'}
                      </button>
                    </div>
                    {#if assignMsg[client.id]}
                      <p class="mt-2 text-xs" style="color: {assignMsg[client.id].ok ? 'var(--color-status-success)' : 'var(--color-status-error)'};">
                        {assignMsg[client.id].text}
                      </p>
                    {/if}
                  {/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
</div>
