<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { ClientInfo } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import Users from 'lucide-svelte/icons/users';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Circle from 'lucide-svelte/icons/circle';

  let clients = $state<ClientInfo[]>([]);
  let loading = $state(true);
  let error = $state('');

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
          </tr>
        </thead>
        <tbody>
          {#each clients as client}
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
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
</div>
