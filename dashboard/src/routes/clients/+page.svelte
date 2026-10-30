<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { ClientInfo, ShadowContext, ReleaseGuideResponse } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import Users from 'lucide-svelte/icons/users';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Circle from 'lucide-svelte/icons/circle';
  import ChevronDown from 'lucide-svelte/icons/chevron-down';
  import Package from 'lucide-svelte/icons/package';

  let clients = $state<ClientInfo[]>([]);
  let loading = $state(true);
  let error = $state('');

  // Sprint 29 Phase 4: rendered upgrade-guide state, keyed by client id.
  // The guide is display-only text rendered by the server — nothing executes.
  let guideOpen = $state<Record<string, boolean>>({});
  let guideData = $state<Record<string, ReleaseGuideResponse | null>>({});
  let guideLoading = $state<Record<string, boolean>>({});
  let guideError = $state<Record<string, string>>({});
  let guideCopied = $state<Record<string, boolean>>({});

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

  async function loadGuide(id: string) {
    guideLoading[id] = true;
    guideError[id] = '';
    try {
      guideData[id] = await api.clients.releaseGuide(id);
    } catch (e: any) {
      guideError[id] = e.message;
    } finally {
      guideLoading[id] = false;
    }
  }

  function toggleGuide(id: string) {
    guideOpen[id] = !guideOpen[id];
    if (guideOpen[id] && !guideData[id]) {
      loadGuide(id);
    }
  }

  async function copyGuide(id: string) {
    const guide = guideData[id]?.guide;
    if (!guide) return;
    try {
      await navigator.clipboard.writeText(
        guide.steps.map((s) => `${s.order}. ${s.action}`).join('\n')
      );
      guideCopied[id] = true;
      setTimeout(() => {
        guideCopied[id] = false;
      }, 2000);
    } catch {
      guideError[id] = 'Clipboard unavailable — select the steps manually';
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
            <th class="px-4 py-2 font-medium">Updates</th>
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
              <td class="px-4 py-2.5">
                {#if client.updates}
                  <div class="text-xs">
                    <span class={client.updates.security > 0 ? 'font-semibold' : ''} style={client.updates.security > 0 ? 'color: var(--color-status-error);' : ''}>
                      {client.updates.security} security
                    </span>
                    / {client.updates.other} other
                  </div>
                  {#each client.updates.releases as release}
                    <div class="mt-0.5">
                      <span
                        class="badge"
                        class:badge-success={release.ready}
                        class:badge-error={!release.ready}
                      >
                        <!-- Sprint 29 Phase 4: PVE hosts report ID=debian; the
                             verdict-derived label presents them as Proxmox VE. -->
                        {client.updates.distro_label || release.distro} {release.from_version} &rarr; {release.to_version}
                        {release.ready ? 'ready' : 'not ready'}
                      </span>
                      {#if release.ready}
                        <button
                          class="btn btn-ghost text-xs ml-1"
                          aria-expanded={!!guideOpen[client.id]}
                          aria-controls={'guide-panel-' + client.id}
                          onclick={() => toggleGuide(client.id)}
                        >
                          <ChevronDown size={12} class={guideOpen[client.id] ? 'rotate-180' : ''} />
                          Upgrade guide
                        </button>
                      {:else}
                        <!-- Sprint 29 Phase 3: annotated reasons why this release is not ready -->
                        <ul class="mt-0.5 ml-3 list-disc text-xs" style="color: var(--color-text-muted);">
                          {#each release.readiness_notes as note}
                            <li>{note}</li>
                          {/each}
                        </ul>
                      {/if}
                    </div>
                  {/each}
                  {#if guideOpen[client.id]}
                    <div
                      id={'guide-panel-' + client.id}
                      class="mt-2 rounded border p-2"
                      style="border-color: var(--color-border-default);"
                    >
                      {#if guideLoading[client.id]}
                        <p class="text-xs" style="color: var(--color-text-muted);">Rendering guide&hellip;</p>
                      {:else if guideError[client.id]}
                        <p class="text-xs" style="color: var(--color-status-error);">{guideError[client.id]}</p>
                      {:else if guideData[client.id]}
                        {@const res = guideData[client.id]!}
                        {#if res.guide}
                          <div class="flex items-center justify-between gap-2">
                            <h4 class="text-xs font-semibold">{res.guide.title}</h4>
                            <button
                              class="btn btn-ghost text-xs"
                              aria-label="Copy upgrade guide to clipboard"
                              onclick={() => copyGuide(client.id)}
                            >
                              {guideCopied[client.id] ? 'Copied' : 'Copy'}
                            </button>
                          </div>
                          {#if res.guide.downgrade_unsafe}
                            <p class="mt-0.5 text-xs" style="color: var(--color-status-error);">
                              No rollback &mdash; an OS release upgrade cannot be undone. Review every step first.
                            </p>
                          {/if}
                          <ol class="mt-1 ml-4 list-decimal text-xs space-y-0.5 font-mono">
                            {#each res.guide.steps as step (step.order)}
                              <li>
                                <span class="badge">{step.group}</span>
                                {#if step.blocking}
                                  <span class="ml-0.5" style="color: var(--color-status-error);">review first</span>
                                {/if}
                                <span class="ml-1">{step.action}</span>
                              </li>
                            {/each}
                          </ol>
                          <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
                            Display only &mdash; Monopsony never executes these steps. Check against the vendor release notes before running anything.
                          </p>
                        {:else}
                          <p class="text-xs" style="color: var(--color-text-muted);">
                            {res.reason || 'No guide available for this client'}
                          </p>
                        {/if}
                      {/if}
                    </div>
                  {/if}
                {:else}
                  <span class="text-xs" style="color: var(--color-text-muted);">&mdash;</span>
                {/if}
              </td>
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
                <td colspan="7" class="px-4 py-3" id={'shadow-panel-' + client.id} style="background: var(--color-bg-subtle, transparent);">
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
