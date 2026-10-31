<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import { formatTimestamp } from '$lib/utils/format';
  import type { ClientCertInfo, ClientInfo } from '$lib/api/types';
  import Shield from 'lucide-svelte/icons/shield';
  import ShieldOff from 'lucide-svelte/icons/shield-off';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import AlertTriangle from 'lucide-svelte/icons/alert-triangle';
  import Check from 'lucide-svelte/icons/check';
  import LogIn from 'lucide-svelte/icons/log-in';
  import ChevronDown from 'lucide-svelte/icons/chevron-down';

  let clients = $state<ClientInfo[]>([]);
  let loading = $state(true);
  let error = $state('');
  let authRequired = $state(false);

  // Per-client certificate list
  let selectedClientId = $state('');
  let certs = $state<ClientCertInfo[]>([]);
  let certsLoading = $state(false);
  let certsError = $state('');

  // Revocation confirm (per-cert rows and the manual fallback share one flow)
  let confirming = $state<{ serial: string; manual: boolean } | null>(null);
  let revokeBusy = $state(false);
  let formError = $state('');
  let manualSerial = $state('');

  // Fleet-wide revocation audit
  let auditRows = $state<ClientCertInfo[]>([]);
  let auditLoading = $state(false);
  let auditError = $state('');

  let toast = $state<{ ok: boolean; text: string } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  function isAuthError(e: unknown): boolean {
    const msg = e instanceof Error ? e.message : String(e);
    return msg === 'Unauthorized' || msg === '__AUTH_REQUIRED__';
  }

  async function load() {
    loading = true;
    try {
      clients = await api.clients.list();
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

  async function loadAudit() {
    auditLoading = true;
    auditError = '';
    try {
      auditRows = await api.clients.revokedCerts();
    } catch (e: any) {
      if (!isAuthError(e)) auditError = e.message;
    } finally {
      auditLoading = false;
    }
  }

  async function loadCerts() {
    if (!selectedClientId) {
      certs = [];
      return;
    }
    certsLoading = true;
    certsError = '';
    try {
      const res = await api.clients.certs(selectedClientId);
      certs = res.certs;
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        certsError = e.message;
        certs = [];
      }
    } finally {
      certsLoading = false;
    }
  }

  function onClientSelected() {
    confirming = null;
    formError = '';
    manualSerial = '';
    loadCerts();
  }

  function showToast(ok: boolean, text: string) {
    toast = { ok, text };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 6000);
  }

  function badgeMeta(badge: ClientCertInfo['badge']): { text: string; cls: string } {
    switch (badge) {
      case 'valid':
        return { text: 'Valid', cls: 'badge-success' };
      case 'expiring':
        return { text: 'Expiring', cls: 'badge-warning' };
      case 'revoked':
        return { text: 'Revoked', cls: 'badge-error' };
      case 'expired':
        return { text: 'Expired', cls: 'badge-muted' };
    }
  }

  function clientLabel(id: string): string {
    const c = clients.find((cl) => cl.id === id);
    if (!c) return id;
    return `${c.hostname} (${c.os}) — ${c.id.slice(0, 8)}`;
  }

  function clientHostname(id: string): string {
    const c = clients.find((cl) => cl.id === id);
    return c ? c.hostname : id.slice(0, 8);
  }

  function askRevoke(serial: string, manual = false) {
    formError = '';
    confirming = { serial, manual };
  }

  function cancelConfirm() {
    confirming = null;
    formError = '';
  }

  async function doRevoke() {
    if (!selectedClientId || !confirming) return;
    revokeBusy = true;
    formError = '';
    try {
      const res = await api.clients.revokeCert(selectedClientId, confirming.serial);
      showToast(true, `Certificate ${res.serial} revoked — the client is cut off at TLS accept immediately.`);
      confirming = null;
      manualSerial = '';
      await Promise.all([loadCerts(), loadAudit()]);
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        formError = e.message;
      }
    } finally {
      revokeBusy = false;
    }
  }

  onMount(() => {
    load();
    loadAudit();
    return () => clearTimeout(toastTimer);
  });
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold flex items-center gap-2">
      <Shield size={20} />
      Security
    </h1>
    <button class="btn btn-ghost" onclick={() => { load(); loadAudit(); loadCerts(); }}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  {#if authRequired}
    <div class="card p-8 text-center">
      <LogIn size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
      <p class="text-sm font-medium">Login required</p>
      <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
        Certificate management needs an authenticated admin session. Your session may have
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

    <!-- Client certificates -->
    <section class="space-y-3">
      <h2 class="text-sm font-semibold flex items-center gap-2">
        <ShieldOff size={14} />
        Client certificate revocation
      </h2>

      <div class="card p-3 text-xs space-y-1" style="color: var(--color-text-muted);">
        <p>
          Client certificates are short-lived (90 days) and serial-tracked. Revoking a serial
          takes effect <strong>immediately at TLS accept</strong> — for direct gRPC and
          <code>/ws/tunnel</code>-relayed connections alike. A revoked client keeps failing to
          connect until it re-pairs. Revocation is permanent.
        </p>
      </div>

      {#if loading}
        <div class="card p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading clients...</div>
      {:else if clients.length === 0 && error}
        <div class="card p-8 text-center text-sm" style="color: var(--color-text-muted);">
          Couldn't load clients — {error}
        </div>
      {:else if clients.length === 0}
        <div class="card p-8 text-center">
          <p class="text-sm" style="color: var(--color-text-muted);">No registered clients</p>
          <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
            Clients appear here after they pair with the server
          </p>
        </div>
      {:else}
        <div class="card p-4 space-y-3">
          <div class="grid gap-3 sm:grid-cols-2">
            <div class="flex flex-col gap-1">
              <label class="text-xs font-medium" for="cert-client">Client</label>
              <select id="cert-client" class="input" bind:value={selectedClientId} onchange={onClientSelected}>
                <option value="" disabled>Select a client...</option>
                {#each clients as c (c.id)}
                  <option value={c.id}>{c.hostname} ({c.os}) — {c.id.slice(0, 8)}</option>
                {/each}
              </select>
            </div>
          </div>

          {#if selectedClientId}
            {#if certsLoading}
              <div class="p-4 text-center text-sm" style="color: var(--color-text-muted);">Loading certificates...</div>
            {:else if certsError}
              <div class="rounded px-3 py-2 text-xs" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
                Couldn't load certificates — {certsError}
              </div>
            {:else if certs.length === 0}
              <div class="p-4 text-center text-xs" style="color: var(--color-text-muted);">
                No certificates issued to this client
              </div>
            {:else}
              <div class="overflow-x-auto">
                <table class="w-full text-sm">
                  <thead>
                    <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
                      <th class="px-4 py-2 font-medium" scope="col">Serial</th>
                      <th class="px-4 py-2 font-medium" scope="col">Issued</th>
                      <th class="px-4 py-2 font-medium" scope="col">Expires</th>
                      <th class="px-4 py-2 font-medium" scope="col">Status</th>
                      <th class="px-4 py-2 font-medium" scope="col"><span class="sr-only">Actions</span></th>
                    </tr>
                  </thead>
                  <tbody>
                    {#each certs as cert (cert.serial)}
                      {@const status = badgeMeta(cert.badge)}
                      <tr class="border-b" style="border-color: var(--color-border-default);">
                        <td class="px-4 py-2 font-mono text-xs">{cert.serial}</td>
                        <td class="px-4 py-2 text-xs">{formatTimestamp(cert.issued_at)}</td>
                        <td class="px-4 py-2 text-xs">
                          {formatTimestamp(cert.expires_at)}
                          {#if cert.days_to_expiry !== null}
                            <span style="color: var(--color-text-muted);">({cert.days_to_expiry}d)</span>
                          {/if}
                        </td>
                        <td class="px-4 py-2"><span class="badge {status.cls}">{status.text}</span></td>
                        <td class="px-4 py-2 text-right">
                          {#if cert.badge !== 'revoked'}
                            <button
                              class="btn btn-ghost text-xs"
                              type="button"
                              disabled={revokeBusy}
                              onclick={() => askRevoke(cert.serial)}
                            >
                              <ShieldOff size={12} />
                              Revoke
                            </button>
                          {/if}
                        </td>
                      </tr>
                    {/each}
                  </tbody>
                </table>
              </div>
            {/if}

            <!-- Manual serial entry (fallback when the list is unavailable) -->
            <details class="text-xs" style="color: var(--color-text-muted);">
              <summary class="cursor-pointer select-none inline-flex items-center gap-1">
                <ChevronDown size={12} />
                Enter a serial manually
              </summary>
              <div class="mt-2 flex flex-wrap items-end gap-2">
                <div class="flex flex-col gap-1">
                  <label class="text-xs font-medium" for="manual-serial">Certificate serial (hex)</label>
                  <input
                    id="manual-serial"
                    class="input font-mono"
                    type="text"
                    bind:value={manualSerial}
                    placeholder="e.g. 1a2b3c4d"
                    autocomplete="off"
                    spellcheck="false"
                  />
                </div>
                <button
                  class="btn"
                  type="button"
                  style="background: var(--color-status-error); color: white;"
                  disabled={!manualSerial.trim()}
                  onclick={() => askRevoke(manualSerial.trim(), true)}
                >
                  <ShieldOff size={14} />
                  Revoke serial
                </button>
              </div>
            </details>
          {/if}

          {#if confirming}
            <div class="rounded px-3 py-2 text-xs" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
              Revoke serial <span class="font-mono">{confirming.serial}</span> for
              <strong>{clientLabel(selectedClientId)}</strong>? The device loses server access
              immediately and must re-pair to reconnect.
            </div>
            <div class="flex items-center gap-2">
              <button
                class="btn"
                type="button"
                style="background: var(--color-status-error); color: white;"
                disabled={revokeBusy}
                onclick={doRevoke}
              >
                {revokeBusy ? 'Revoking...' : 'Confirm revoke'}
              </button>
              <button class="btn btn-ghost" type="button" onclick={cancelConfirm} disabled={revokeBusy}>
                Cancel
              </button>
            </div>
          {/if}
          {#if formError}
            <div class="rounded px-3 py-2 text-xs" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
              {formError}
            </div>
          {/if}
        </div>
      {/if}
    </section>

    <!-- Fleet-wide revocation audit -->
    <section class="space-y-3">
      <h2 class="text-sm font-semibold flex items-center gap-2">
        <RefreshCw size={14} class="rotate-90" />
        Revocation audit
      </h2>
      <div class="card p-4 space-y-3">
        {#if auditLoading}
          <div class="p-2 text-center text-sm" style="color: var(--color-text-muted);">Loading audit...</div>
        {:else if auditError}
          <div class="rounded px-3 py-2 text-xs" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
            Couldn't load audit — {auditError}
          </div>
        {:else if auditRows.length === 0}
          <p class="text-xs" style="color: var(--color-text-muted);">
            No certificates have been revoked.
          </p>
        {:else}
          <div class="overflow-x-auto">
            <table class="w-full text-sm">
              <thead>
                <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
                  <th class="px-4 py-2 font-medium" scope="col">Serial</th>
                  <th class="px-4 py-2 font-medium" scope="col">Client</th>
                  <th class="px-4 py-2 font-medium" scope="col">Revoked</th>
                  <th class="px-4 py-2 font-medium" scope="col">Status</th>
                </tr>
              </thead>
              <tbody>
                {#each auditRows as cert (cert.serial)}
                  <tr class="border-b" style="border-color: var(--color-border-default);">
                    <td class="px-4 py-2 font-mono text-xs">{cert.serial}</td>
                    <td class="px-4 py-2 text-xs">{clientHostname(cert.client_id)}</td>
                    <td class="px-4 py-2 text-xs">{formatTimestamp(cert.revoked_at)}</td>
                    <td class="px-4 py-2"><span class="badge badge-error">Revoked</span></td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
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
