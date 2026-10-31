<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { EnrollmentKey, CreateEnrollmentKeyResponse } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import KeyRound from 'lucide-svelte/icons/key-round';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Plus from 'lucide-svelte/icons/plus';
  import Copy from 'lucide-svelte/icons/copy';
  import Check from 'lucide-svelte/icons/check';
  import X from 'lucide-svelte/icons/x';
  import AlertTriangle from 'lucide-svelte/icons/alert-triangle';
  import ShieldOff from 'lucide-svelte/icons/shield-off';
  import LogIn from 'lucide-svelte/icons/log-in';

  let keys = $state<EnrollmentKey[]>([]);
  let loading = $state(true);
  let error = $state('');
  let authRequired = $state(false);

  // Create form
  let newLabel = $state('');
  let newMaxUses = $state(1);
  let newExpiryHours = $state<number | null>(null);
  let createBusy = $state(false);
  let createError = $state('');

  // One-time plaintext reveal after create
  let revealed = $state<CreateEnrollmentKeyResponse | null>(null);
  let copied = $state(false);
  let copyTimer: ReturnType<typeof setTimeout> | undefined;

  // Per-key revoke state
  let confirmRevoke = $state<Record<string, boolean>>({});
  let revokeBusy = $state<Record<string, boolean>>({});

  let toast = $state<{ ok: boolean; text: string } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  function isAuthError(e: unknown): boolean {
    const msg = e instanceof Error ? e.message : String(e);
    return msg === 'Unauthorized' || msg === '__AUTH_REQUIRED__';
  }

  async function loadKeys() {
    loading = true;
    try {
      keys = await api.enrollmentKeys.list();
      authRequired = false;
      error = '';
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        error = e.message;
      }
    } finally {
      loading = false;
    }
  }

  function showToast(ok: boolean, text: string) {
    toast = { ok, text };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 6000);
  }

  async function createKey(e: Event) {
    e.preventDefault();
    const label = newLabel.trim();
    if (!label || newMaxUses < 1) return;
    createBusy = true;
    createError = '';
    try {
      revealed = await api.enrollmentKeys.create({
        label,
        max_uses: newMaxUses,
        expires_in_hours: newExpiryHours && newExpiryHours > 0 ? newExpiryHours : null,
      });
      copied = false;
      newLabel = '';
      newMaxUses = 1;
      newExpiryHours = null;
      showToast(true, `Enrollment key "${label}" created. Copy the key now — it is never shown again.`);
      await loadKeys();
    } catch (err: any) {
      if (isAuthError(err)) {
        authRequired = true;
      } else {
        createError = err.message;
      }
    } finally {
      createBusy = false;
    }
  }

  async function copyKey() {
    if (!revealed) return;
    try {
      await navigator.clipboard.writeText(revealed.key);
      copied = true;
      clearTimeout(copyTimer);
      copyTimer = setTimeout(() => (copied = false), 2000);
    } catch {
      // Clipboard unavailable (insecure context) — the key stays selectable.
    }
  }

  function dismissReveal() {
    revealed = null;
  }

  async function revoke(key: EnrollmentKey) {
    revokeBusy[key.key_id] = true;
    try {
      await api.enrollmentKeys.revoke(key.key_id);
      showToast(true, `Enrollment key "${key.label}" revoked.`);
      await loadKeys();
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        showToast(false, e.message);
        await loadKeys();
      }
    } finally {
      revokeBusy[key.key_id] = false;
      confirmRevoke[key.key_id] = false;
    }
  }

  function keyStatus(key: EnrollmentKey): { text: string; cls: string } {
    if (key.revoked_at) return { text: 'Revoked', cls: 'badge-error' };
    if (key.expires_at !== null && key.expires_at * 1000 < Date.now())
      return { text: 'Expired', cls: 'badge-warning' };
    if (key.use_count >= key.max_uses) return { text: 'Exhausted', cls: 'badge-warning' };
    return { text: 'Active', cls: 'badge-success' };
  }

  function keyUsable(key: EnrollmentKey): boolean {
    return !key.revoked_at && keyStatus(key).text === 'Active';
  }

  onMount(() => {
    loadKeys();
    return () => {
      clearTimeout(toastTimer);
      clearTimeout(copyTimer);
    };
  });
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold flex items-center gap-2">
      <KeyRound size={20} />
      Enrollment Keys
    </h1>
    <button class="btn btn-ghost" onclick={loadKeys}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  <div class="card p-3 text-xs" style="color: var(--color-text-muted);">
    Enrollment keys pre-approve pairing for fleet installs — a client presenting a valid key
    pairs without interactive admin approval (<code>monopsony-client pair --enrollment-key mopk-…</code>).
    Keys are stored hashed; the plaintext is shown <strong>once</strong> at creation.
  </div>

  {#if authRequired}
    <div class="card p-8 text-center">
      <LogIn size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
      <p class="text-sm font-medium">Login required</p>
      <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
        Managing enrollment keys needs an authenticated admin session. Your session may have
        expired — sign in again to continue.
      </p>
      <a href="/login" class="btn btn-primary mt-4 inline-flex items-center gap-2">
        <LogIn size={14} />
        Go to login
      </a>
    </div>
  {:else}
    {#if error}
      <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
        <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
      </div>
    {/if}

    <!-- One-time plaintext reveal -->
    {#if revealed}
      <div
        class="card p-4 space-y-2"
        style="border-left: 4px solid var(--color-status-warning);"
        role="alert"
      >
        <div class="flex items-center gap-2 text-sm font-medium">
          <AlertTriangle size={14} style="color: var(--color-status-warning);" />
          Copy this key now — it is shown only once
        </div>
        <div class="flex items-center gap-2 flex-wrap">
          <code class="input font-mono text-sm flex-1 min-w-72 select-all">{revealed.key}</code>
          <button class="btn btn-secondary" type="button" onclick={copyKey} aria-label="Copy enrollment key to clipboard">
            {#if copied}
              <Check size={14} />
            {:else}
              <Copy size={14} />
            {/if}
            {copied ? 'Copied' : 'Copy'}
          </button>
          <button class="btn btn-ghost" type="button" onclick={dismissReveal}>
            Done
          </button>
        </div>
        <p class="text-xs" style="color: var(--color-text-muted);">
          Only a SHA-256 hash is stored server-side. If you lose the plaintext, revoke this key
          and create a new one. Label: <strong>{revealed.label}</strong>
          &middot; {revealed.max_uses} use{revealed.max_uses === 1 ? '' : 's'}
          {#if revealed.expires_at !== null}
            &middot; expires {formatTimestamp(revealed.expires_at)}
          {/if}
        </p>
      </div>
    {/if}

    <!-- Create form -->
    <section class="space-y-3">
      <h2 class="text-sm font-semibold flex items-center gap-2">
        <Plus size={14} />
        Create key
      </h2>
      <form class="card p-4 space-y-3" onsubmit={createKey}>
        {#if createError}
          <div class="rounded px-3 py-2 text-xs" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
            {createError}
          </div>
        {/if}
        <div class="grid gap-3 sm:grid-cols-3">
          <div class="flex flex-col gap-1">
            <label class="text-xs font-medium" for="ek-label">Label</label>
            <input
              id="ek-label"
              class="input"
              type="text"
              bind:value={newLabel}
              placeholder="e.g. workshop laptops Q4"
              autocomplete="off"
              required
            />
          </div>
          <div class="flex flex-col gap-1">
            <label class="text-xs font-medium" for="ek-max-uses">Max uses</label>
            <input
              id="ek-max-uses"
              class="input"
              type="number"
              min={1}
              step={1}
              bind:value={newMaxUses}
            />
          </div>
          <div class="flex flex-col gap-1">
            <label class="text-xs font-medium" for="ek-expiry">Expires in (hours, blank = never)</label>
            <input
              id="ek-expiry"
              class="input"
              type="number"
              min={1}
              step={1}
              placeholder="e.g. 168"
              bind:value={newExpiryHours}
            />
          </div>
        </div>
        <div>
          <button
            class="btn btn-primary"
            type="submit"
            disabled={createBusy || !newLabel.trim() || newMaxUses < 1}
          >
            {createBusy ? 'Creating...' : 'Create key'}
          </button>
        </div>
      </form>
    </section>

    <!-- Key list -->
    <section class="space-y-3">
      <h2 class="text-sm font-semibold">Keys</h2>
      <div class="card overflow-hidden">
        {#if loading}
          <div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>
        {:else if keys.length === 0}
          <div class="p-12 text-center">
            <KeyRound size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
            <p class="text-sm" style="color: var(--color-text-muted);">No enrollment keys</p>
            <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
              Create a key above to pre-approve fleet pairing
            </p>
          </div>
        {:else}
          <table class="w-full text-sm">
            <thead>
              <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
                <th class="px-4 py-2 font-medium" scope="col">Label</th>
                <th class="px-4 py-2 font-medium" scope="col">Key hash</th>
                <th class="px-4 py-2 font-medium" scope="col">Usage</th>
                <th class="px-4 py-2 font-medium" scope="col">Created</th>
                <th class="px-4 py-2 font-medium" scope="col">Expires</th>
                <th class="px-4 py-2 font-medium" scope="col">Status</th>
                <th class="px-4 py-2 font-medium" scope="col"><span class="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {#each keys as key (key.key_id)}
                {@const status = keyStatus(key)}
                <tr class="border-b table-row" style="border-color: var(--color-border-default);">
                  <td class="px-4 py-2">{key.label}</td>
                  <td class="px-4 py-2 font-mono text-xs" title="SHA-256 prefix">{key.key_hash_prefix}…</td>
                  <td class="px-4 py-2 font-mono text-xs">{key.use_count}/{key.max_uses}</td>
                  <td class="px-4 py-2 text-xs">{formatTimestamp(key.created_at)}</td>
                  <td class="px-4 py-2 text-xs">{key.expires_at !== null ? formatTimestamp(key.expires_at) : 'Never'}</td>
                  <td class="px-4 py-2"><span class="badge {status.cls}">{status.text}</span></td>
                  <td class="px-4 py-2 text-right">
                    {#if keyUsable(key)}
                      {#if confirmRevoke[key.key_id]}
                        <span class="flex items-center justify-end gap-1.5">
                          <span class="text-xs" style="color: var(--color-status-error);">Revoke?</span>
                          <button
                            class="btn text-xs"
                            type="button"
                            style="background: var(--color-status-error); color: white;"
                            disabled={revokeBusy[key.key_id]}
                            onclick={() => revoke(key)}
                          >
                            {revokeBusy[key.key_id] ? 'Revoking...' : 'Confirm'}
                          </button>
                          <button class="btn btn-ghost text-xs" type="button" onclick={() => (confirmRevoke[key.key_id] = false)}>
                            Cancel
                          </button>
                        </span>
                      {:else}
                        <button class="btn btn-ghost text-xs" type="button" onclick={() => (confirmRevoke[key.key_id] = true)}>
                          <ShieldOff size={12} />
                          Revoke
                        </button>
                      {/if}
                    {:else}
                      <span class="text-xs" style="color: var(--color-text-muted);">—</span>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      </div>
    </section>
  {/if}
</div>

{#if toast}
  <div
    class="fixed bottom-6 right-6 z-50 card px-4 py-3 text-sm flex items-center gap-2 shadow-lg"
    style="border-left: 4px solid {toast.ok ? 'var(--color-status-success)' : 'var(--color-status-error)'};"
    role="status"
  >
    {#if toast.ok}
      <Check size={16} style="color: var(--color-status-success);" />
    {:else}
      <AlertTriangle size={16} style="color: var(--color-status-error);" />
    {/if}
    {toast.text}
  </div>
{/if}
