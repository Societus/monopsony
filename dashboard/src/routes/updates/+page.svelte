<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type {
    RolloutAssignment,
    RolloutsResponse,
    SelfApplyGuardConflict,
    UpdateRelease,
    UpdatesListResponse,
  } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import {
    compareReleaseVersions,
    isLegacyVersion,
    sortReleasesAscending,
  } from '$lib/utils/version';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import CircleArrowUp from 'lucide-svelte/icons/circle-arrow-up';

  let releases = $state<UpdateRelease[]>([]);
  let rollouts = $state<RolloutsResponse | null>(null);
  let loading = $state(true);
  let error = $state('');

  let checkBusy = $state(false);
  let checkMsg = $state('');

  let rolloutVersion = $state('');
  let allClients = $state(false);
  let selectedClients = $state<Record<string, boolean>>({});
  let rolloutBusy = $state(false);
  let rolloutMsg = $state<{ ok: boolean; text: string } | null>(null);

  let selfApplyVersion = $state('');
  let confirmOpen = $state(false);
  let selfApplyBusy = $state(false);
  let selfApplyMsg = $state<{ ok: boolean; text: string } | null>(null);
  let guardConflict = $state<SelfApplyGuardConflict | null>(null);

  let releasesDesc = $derived([...sortReleasesAscending(releases)].reverse());
  let latest = $derived(releasesDesc.length > 0 ? releasesDesc[0] : null);
  let completeDesc = $derived(releasesDesc.filter((r) => r.assets_complete));

  let serverUpToDate = $derived.by(() => {
    if (latest === null || rollouts === null) return false;
    const cmp = compareReleaseVersions(rollouts.server_version, latest.version);
    return cmp !== null && cmp >= 0;
  });

  let selectedIds = $derived.by(() => {
    const fleet = rollouts?.fleet ?? [];
    return fleet.filter((f) => selectedClients[f.client_id]).map((f) => f.client_id);
  });

  async function load(resetSelections = true) {
    loading = true;
    error = '';
    let loaded: UpdateRelease[] = [];
    try {
      const res = (await api.updates.list()) as Partial<UpdatesListResponse> & {
        error?: string;
      };
      if (res.error) {
        error = res.error;
      } else {
        loaded = res.releases ?? [];
      }
    } catch (e: any) {
      error = e.message;
    }
    releases = loaded;
    try {
      rollouts = await api.updates.rollouts();
    } catch {
      // Rollout/ack state requires the db — the releases view stays usable.
      rollouts = null;
      if (!error) error = 'Rollout state unavailable (requires database)';
    }
    if (resetSelections) {
      const complete = sortReleasesAscending(loaded.filter((r) => r.assets_complete));
      const newest = complete.length > 0 ? complete[complete.length - 1] : undefined;
      rolloutVersion = newest?.version ?? '';
      selfApplyVersion = newest?.version ?? '';
      selectedClients = {};
      allClients = false;
    }
    loading = false;
  }

  async function checkNow() {
    checkBusy = true;
    checkMsg = '';
    try {
      const res = await api.updates.check();
      checkMsg = res.message || (res.success ? 'Update check triggered' : 'Distributor unavailable');
    } catch (e: any) {
      checkMsg = e.message;
    } finally {
      checkBusy = false;
    }
  }

  async function submitRollout() {
    if (!rolloutVersion) return;
    rolloutBusy = true;
    rolloutMsg = null;
    try {
      const res = allClients
        ? await api.updates.createRollout({ version: rolloutVersion, all: true })
        : await api.updates.createRollout({ version: rolloutVersion, client_ids: selectedIds });
      rolloutMsg = {
        ok: true,
        text: `Assigned ${res.version} to ${res.assigned} client${res.assigned === 1 ? '' : 's'} — clients pick it up on their next control-stream tick`,
      };
      await load(false);
    } catch (e: any) {
      rolloutMsg = { ok: false, text: e.message };
    } finally {
      rolloutBusy = false;
    }
  }

  function beginSelfApply() {
    if (!selfApplyVersion) return;
    guardConflict = null;
    selfApplyMsg = null;
    confirmOpen = true;
  }

  function readGuardConflict(e: unknown): SelfApplyGuardConflict | null {
    if (!(e instanceof Error)) return null;
    const err = e as Error & { status?: number; body?: unknown };
    if (err.status !== 409 || typeof err.body !== 'object' || err.body === null) return null;
    const body = err.body as { guard_triggered?: unknown };
    if (body.guard_triggered !== true) return null;
    return err.body as SelfApplyGuardConflict;
  }

  async function submitSelfApply(overrideGuard: boolean) {
    selfApplyBusy = true;
    selfApplyMsg = null;
    try {
      const res = await api.updates.selfApply({
        version: selfApplyVersion,
        override_guard: overrideGuard || undefined,
      });
      if (res.staged) {
        selfApplyMsg = {
          ok: true,
          text: `Staged ${res.from_version} → ${res.to_version}. ${res.note} — this dashboard reconnects once the server is back.`,
        };
      } else {
        selfApplyMsg = {
          ok: true,
          text: `Nothing to do: ${res.reason.split('_').join(' ')}`,
        };
      }
      confirmOpen = false;
      guardConflict = null;
    } catch (e: any) {
      const conflict = readGuardConflict(e);
      if (conflict !== null) {
        confirmOpen = false;
        guardConflict = conflict;
      } else {
        selfApplyMsg = { ok: false, text: e.message };
      }
    } finally {
      selfApplyBusy = false;
    }
  }

  function statusBadge(status: RolloutAssignment['status']): { text: string; cls: string } {
    switch (status) {
      case 'pending':
        return { text: 'Pending', cls: 'badge-info' };
      case 'delivered':
        return { text: 'Delivered', cls: 'badge-warning' };
      case 'applied':
        return { text: 'Applied', cls: 'badge-success' };
      case 'refused':
        return { text: 'Refused', cls: 'badge-error' };
    }
  }

  onMount(() => {
    load();
  });
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="flex items-center gap-2 text-xl font-semibold">
      <CircleArrowUp size={20} />
      Updates
    </h1>
    <button class="btn btn-ghost" onclick={() => load()}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  {#if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  {#if loading}
    <div class="card p-8 text-center text-sm" style="color: var(--color-text-muted);">
      Loading...
    </div>
  {:else}
    <!-- Latest release -->
    <div class="card p-4">
      {#if latest}
        <div class="flex items-start justify-between gap-4">
          <div>
            <div class="flex flex-wrap items-center gap-2">
              <h2 class="text-sm font-semibold">Latest release</h2>
              <span class="badge badge-info font-mono">{latest.version}</span>
              {#if latest.prerelease}
                <span class="badge badge-warning">pre-release</span>
              {/if}
              {#if latest.assets_complete}
                <span class="badge badge-success">assets complete</span>
              {:else}
                <span class="badge badge-warning">assets incomplete</span>
              {/if}
            </div>
            <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
              Fetched {formatTimestamp(latest.fetched_at)} &middot; {latest.artifact_count} artifacts
              {#if latest.changelog_url}
                &middot;
                <a class="underline" href={latest.changelog_url} target="_blank" rel="noopener noreferrer">changelog</a>
              {/if}
            </p>
            {#if rollouts}
              <p class="mt-1.5 flex items-center gap-2 text-xs">
                <span style="color: var(--color-text-muted);">Server running</span>
                <span class="font-mono">{rollouts.server_version}</span>
                {#if serverUpToDate}
                  <span class="badge badge-success">up to date</span>
                {:else}
                  <span class="badge badge-info">update available</span>
                {/if}
              </p>
            {/if}
          </div>
          <button class="btn btn-ghost text-xs" onclick={checkNow} disabled={checkBusy}>
            <RefreshCw size={12} class={checkBusy ? 'animate-spin' : ''} />
            {checkBusy ? 'Checking…' : 'Check upstream now'}
          </button>
        </div>
        <div class="mt-3 grid gap-2 text-xs md:grid-cols-2">
          <div>
            <span class="font-semibold">Server platforms</span>
            <div class="mt-1 flex flex-wrap gap-1">
              {#each latest.server_platforms as platform (platform)}
                <span class="badge badge-muted font-mono">{platform}</span>
              {:else}
                <span style="color: var(--color-text-muted);">none stored</span>
              {/each}
            </div>
          </div>
          <div>
            <span class="font-semibold">Client platforms</span>
            <div class="mt-1 flex flex-wrap gap-1">
              {#each latest.client_platforms as platform (platform)}
                <span class="badge badge-muted font-mono">{platform}</span>
              {:else}
                <span style="color: var(--color-text-muted);">none stored</span>
              {/each}
            </div>
          </div>
        </div>
        {#if checkMsg}
          <p class="mt-2 text-xs" style="color: var(--color-text-muted);">{checkMsg}</p>
        {/if}
      {:else}
        <p class="text-sm" style="color: var(--color-text-muted);">
          No releases fetched yet — the distributor fills this in on its next poll, or use
          &ldquo;Check upstream now&rdquo;.
        </p>
        <button class="btn btn-ghost mt-2 text-xs" onclick={checkNow} disabled={checkBusy}>
          <RefreshCw size={12} class={checkBusy ? 'animate-spin' : ''} />
          {checkBusy ? 'Checking…' : 'Check upstream now'}
        </button>
      {/if}
    </div>

    <!-- Stored releases -->
    {#if releasesDesc.length > 0}
      <div class="card overflow-hidden">
        <table class="w-full text-sm">
          <thead>
            <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
              <th class="px-4 py-2 font-medium">Version</th>
              <th class="px-4 py-2 font-medium">Fetched</th>
              <th class="px-4 py-2 font-medium">Server platforms</th>
              <th class="px-4 py-2 font-medium">Client platforms</th>
              <th class="px-4 py-2 font-medium">Assets</th>
            </tr>
          </thead>
          <tbody>
            {#each releasesDesc as release (release.version)}
              <tr class="table-row border-b" style="border-color: var(--color-border-default);">
                <td class="px-4 py-2.5">
                  <span class="font-mono text-xs">{release.version}</span>
                  {#if release.prerelease}
                    <span class="badge badge-warning ml-1">pre</span>
                  {/if}
                </td>
                <td class="px-4 py-2.5 text-xs">{formatTimestamp(release.fetched_at)}</td>
                <td class="px-4 py-2.5">
                  <div class="flex flex-wrap gap-1">
                    {#each release.server_platforms as platform (platform)}
                      <span class="badge badge-muted font-mono text-[10px]">{platform}</span>
                    {:else}
                      <span class="text-xs" style="color: var(--color-text-muted);">&mdash;</span>
                    {/each}
                  </div>
                </td>
                <td class="px-4 py-2.5">
                  <div class="flex flex-wrap gap-1">
                    {#each release.client_platforms as platform (platform)}
                      <span class="badge badge-muted font-mono text-[10px]">{platform}</span>
                    {:else}
                      <span class="text-xs" style="color: var(--color-text-muted);">&mdash;</span>
                    {/each}
                  </div>
                </td>
                <td class="px-4 py-2.5">
                  {#if release.assets_complete}
                    <span class="badge badge-success">complete</span>
                  {:else}
                    <span class="badge badge-warning">{release.artifact_count} artifacts</span>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}

    <!-- Rollout -->
    <div class="card p-4">
      <h2 class="text-sm font-semibold">Roll out a release to clients</h2>
      {#if rollouts === null}
        <p class="mt-2 text-xs" style="color: var(--color-text-muted);">
          Rollout control and ack states require the database (no-DB mode keeps serving cached
          packages only).
        </p>
      {:else}
        <div class="mt-3 flex flex-wrap items-end gap-3">
          <div>
            <label class="text-xs" for="rollout-version">Release</label>
            <select id="rollout-version" class="input mt-1 w-56 text-xs" bind:value={rolloutVersion}>
              {#each completeDesc as release (release.version)}
                <option value={release.version}>
                  {release.version}{release === latest ? ' (latest)' : ''}
                </option>
              {/each}
            </select>
            <p class="mt-1 text-[10px]" style="color: var(--color-text-muted);">
              Only complete releases are offered — clients can only download those.
            </p>
          </div>
          <label class="flex items-center gap-2 pb-5 text-xs">
            <input type="checkbox" bind:checked={allClients} />
            All registered clients
          </label>
          <button
            class="btn btn-primary mb-1 text-xs"
            disabled={!rolloutVersion || rolloutBusy || (!allClients && selectedIds.length === 0)}
            onclick={submitRollout}
          >
            {rolloutBusy ? 'Assigning…' : 'Create rollout'}
          </button>
        </div>

        {#if rollouts.fleet.length === 0}
          <p class="mt-2 text-xs" style="color: var(--color-text-muted);">No registered clients.</p>
        {:else}
          <div class="mt-3 grid gap-1.5 md:grid-cols-3">
            {#each rollouts.fleet as client (client.client_id)}
              <label class="flex items-center gap-2 text-xs">
                <input
                  type="checkbox"
                  bind:checked={selectedClients[client.client_id]}
                  disabled={allClients}
                />
                <span class="font-medium">{client.hostname}</span>
                {#if isLegacyVersion(client.monopsony_version)}
                  <span style="color: var(--color-text-muted);">legacy</span>
                {:else}
                  <span class="font-mono" style="color: var(--color-text-muted);">{client.monopsony_version}</span>
                {/if}
              </label>
            {/each}
          </div>
        {/if}

        {#if rolloutMsg}
          <p
            class="mt-2 text-xs"
            style="color: {rolloutMsg.ok ? 'var(--color-status-success)' : 'var(--color-status-error)'};"
          >
            {rolloutMsg.text}
          </p>
        {/if}

        <h3 class="mt-5 text-xs font-semibold">Rollout / ack state</h3>
        {#if rollouts.assignments.length === 0}
          <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
            No rollout assignments yet.
          </p>
        {:else}
          <table class="mt-2 w-full text-sm">
            <thead>
              <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
                <th class="px-3 py-2 font-medium">Client</th>
                <th class="px-3 py-2 font-medium">Release</th>
                <th class="px-3 py-2 font-medium">Status</th>
                <th class="px-3 py-2 font-medium">Created</th>
                <th class="px-3 py-2 font-medium">Delivered</th>
                <th class="px-3 py-2 font-medium">Resolved</th>
              </tr>
            </thead>
            <tbody>
              {#each rollouts.assignments as assignment (assignment.client_id + ':' + assignment.version)}
                {@const badge = statusBadge(assignment.status)}
                <tr class="table-row border-b" style="border-color: var(--color-border-default);">
                  <td class="px-3 py-2 text-xs font-medium">
                    {assignment.hostname || assignment.client_id}
                  </td>
                  <td class="px-3 py-2 font-mono text-xs">{assignment.version}</td>
                  <td class="px-3 py-2"><span class="badge {badge.cls}">{badge.text}</span></td>
                  <td class="px-3 py-2 text-xs">{formatTimestamp(assignment.created_at)}</td>
                  <td class="px-3 py-2 text-xs">{formatTimestamp(assignment.delivered_at)}</td>
                  <td class="px-3 py-2 text-xs">{formatTimestamp(assignment.resolved_at)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      {/if}
    </div>

    <!-- Self-apply -->
    <div class="card p-4">
      <h2 class="text-sm font-semibold">Apply a release to this server</h2>
      <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
        Order of operations: roll out to clients first, self-apply last — the guard refuses while
        any active client still reports below the target.
      </p>
      <div class="mt-3 flex flex-wrap items-end gap-3">
        <div>
          <label class="text-xs" for="self-apply-version">Release</label>
          <select id="self-apply-version" class="input mt-1 w-56 text-xs" bind:value={selfApplyVersion}>
            {#each completeDesc as release (release.version)}
              <option value={release.version}>
                {release.version}{release === latest ? ' (latest)' : ''}
              </option>
            {/each}
          </select>
        </div>
        <button
          class="btn btn-secondary mb-1 text-xs"
          disabled={!selfApplyVersion || selfApplyBusy}
          onclick={beginSelfApply}
        >
          Self-apply server
        </button>
      </div>

      {#if confirmOpen}
        <div
          class="mt-3 rounded border p-3"
          style="border-color: var(--color-status-warning); background: color-mix(in srgb, var(--color-status-warning) 8%, transparent);"
        >
          <p class="text-xs font-semibold" style="color: var(--color-status-warning);">
            Confirm self-apply of {selfApplyVersion}
          </p>
          <ul class="mt-1.5 ml-4 list-disc text-xs" style="color: var(--color-text-muted);">
            <li>The update is staged from the local blob store, then a detached finalize swaps
              the binary, restarts the server and health-gates it (automatic rollback on a
              failed health check).</li>
            <li>The control plane goes down for the restart — pairing, heartbeats and the
              dashboard pause until it is back.</li>
            <li>Roll it out to clients and let them converge first; the guard blocks this
              otherwise.</li>
          </ul>
          <div class="mt-2.5 flex gap-2">
            <button
              class="btn btn-primary text-xs"
              disabled={selfApplyBusy}
              onclick={() => submitSelfApply(false)}
            >
              {selfApplyBusy ? 'Staging…' : 'Confirm self-apply'}
            </button>
            <button
              class="btn btn-ghost text-xs"
              disabled={selfApplyBusy}
              onclick={() => (confirmOpen = false)}
            >
              Cancel
            </button>
          </div>
        </div>
      {/if}

      {#if guardConflict}
        <div
          class="mt-3 rounded border p-3"
          style="border-color: var(--color-status-error); background: color-mix(in srgb, var(--color-status-error) 8%, transparent);"
        >
          <p class="text-xs font-semibold" style="color: var(--color-status-error);">
            Fleet-convergence guard triggered — {guardConflict.lagging_clients.length}
            client{guardConflict.lagging_clients.length === 1 ? '' : 's'} still below
            {guardConflict.target_version}
          </p>
          <p class="mt-1 text-xs" style="color: var(--color-text-muted);">{guardConflict.error}</p>
          <div class="mt-2 grid gap-3 text-xs md:grid-cols-3">
            <div>
              <span class="font-semibold" style="color: var(--color-status-error);">Lagging</span>
              <ul class="mt-1 space-y-0.5">
                {#each guardConflict.lagging_clients as row (row.client_id)}
                  <li>{row.hostname} <span class="font-mono" style="color: var(--color-text-muted);">{row.monopsony_version || 'legacy'}</span></li>
                {:else}
                  <li style="color: var(--color-text-muted);">none</li>
                {/each}
              </ul>
            </div>
            <div>
              <span class="font-semibold">Unknown version</span>
              <ul class="mt-1 space-y-0.5">
                {#each guardConflict.unknown_clients as row (row.client_id)}
                  <li>{row.hostname} <span class="font-mono" style="color: var(--color-text-muted);">legacy</span></li>
                {:else}
                  <li style="color: var(--color-text-muted);">none</li>
                {/each}
              </ul>
            </div>
            <div>
              <span class="font-semibold" style="color: var(--color-status-success);">Converged</span>
              <ul class="mt-1 space-y-0.5">
                {#each guardConflict.converged_clients as row (row.client_id)}
                  <li>{row.hostname} <span class="font-mono" style="color: var(--color-text-muted);">{row.monopsony_version}</span></li>
                {:else}
                  <li style="color: var(--color-text-muted);">none</li>
                {/each}
              </ul>
            </div>
          </div>
          <div class="mt-2.5 flex gap-2">
            <button
              class="btn btn-secondary text-xs"
              disabled={selfApplyBusy}
              onclick={() => submitSelfApply(true)}
            >
              {selfApplyBusy ? 'Staging…' : 'Override guard and self-apply anyway'}
            </button>
            <button
              class="btn btn-ghost text-xs"
              disabled={selfApplyBusy}
              onclick={() => (guardConflict = null)}
            >
              Cancel
            </button>
          </div>
        </div>
      {/if}

      {#if selfApplyMsg}
        <p
          class="mt-2 text-xs"
          style="color: {selfApplyMsg.ok ? 'var(--color-status-success)' : 'var(--color-status-error)'};"
        >
          {selfApplyMsg.text}
        </p>
      {/if}
    </div>
  {/if}
</div>
