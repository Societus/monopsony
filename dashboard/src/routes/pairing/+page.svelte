<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { PendingPairRequest, PairAuditEntry } from '$lib/api/types';
  import { formatTimestamp } from '$lib/utils/format';
  import LinkIcon from 'lucide-svelte/icons/link';
  import RefreshCw from 'lucide-svelte/icons/refresh-cw';
  import Globe from 'lucide-svelte/icons/globe';
  import Monitor from 'lucide-svelte/icons/monitor';
  import KeyRound from 'lucide-svelte/icons/key-round';
  import AlertTriangle from 'lucide-svelte/icons/alert-triangle';
  import Check from 'lucide-svelte/icons/check';
  import X from 'lucide-svelte/icons/x';
  import History from 'lucide-svelte/icons/history';
  import LogIn from 'lucide-svelte/icons/log-in';

  const MAX_PIN_ATTEMPTS = 3;
  const POLL_MS = 10_000;

  let pending = $state<PendingPairRequest[]>([]);
  let audit = $state<PairAuditEntry[]>([]);
  let loading = $state(true);
  let error = $state('');
  let authRequired = $state(false);

  // Live countdown tick (seconds resolution)
  let now = $state(Math.floor(Date.now() / 1000));

  // Per-request UI state
  let pins = $state<Record<string, string>>({});
  let approveBusy = $state<Record<string, boolean>>({});
  let cardMsg = $state<Record<string, { ok: boolean; text: string }>>({});
  let confirmDeny = $state<Record<string, boolean>>({});
  let denyBusy = $state<Record<string, boolean>>({});

  let toast = $state<{ ok: boolean; text: string } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  function isAuthError(e: unknown): boolean {
    const msg = e instanceof Error ? e.message : String(e);
    return msg === 'Unauthorized' || msg === '__AUTH_REQUIRED__';
  }

  async function loadPending() {
    try {
      pending = await api.pairing.pending();
      authRequired = false;
      error = '';
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        error = e.message;
      }
    }
  }

  async function loadAudit() {
    try {
      audit = await api.pairing.audit();
    } catch (e: any) {
      if (isAuthError(e)) authRequired = true;
      // Audit failures are non-fatal — keep the last snapshot.
    }
  }

  async function loadAll() {
    await Promise.all([loadPending(), loadAudit()]);
    loading = false;
  }

  function showToast(ok: boolean, text: string) {
    toast = { ok, text };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 5000);
  }

  async function approve(req: PendingPairRequest) {
    const pin = (pins[req.request_id] || '').trim().toUpperCase();
    if (pin.length !== 6) {
      cardMsg[req.request_id] = { ok: false, text: 'Enter the 6-character PIN shown on the client console.' };
      return;
    }
    approveBusy[req.request_id] = true;
    cardMsg[req.request_id] = null as unknown as { ok: boolean; text: string };
    try {
      await api.pairing.approve(req.request_id, pin);
      pending = pending.filter((p) => p.request_id !== req.request_id);
      pins[req.request_id] = '';
      showToast(true, `Approved ${req.claimed_hostname} (${req.source_ip}) — waiting for client to collect its certificate.`);
      await loadAudit();
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        // Wrong PIN or invalidated — refresh to get updated pin_attempts/status.
        await loadPending();
        const updated = pending.find((p) => p.request_id === req.request_id);
        if (updated && updated.status === 'invalidated') {
          cardMsg[req.request_id] = {
            ok: false,
            text: 'Request invalidated: too many failed PIN attempts. The client must pair again.',
          };
        } else {
          const remaining = updated ? MAX_PIN_ATTEMPTS - updated.pin_attempts : null;
          cardMsg[req.request_id] = {
            ok: false,
            text: remaining !== null && remaining > 0
              ? `${e.message} ${remaining} attempt${remaining === 1 ? '' : 's'} remaining before the request is invalidated.`
              : e.message,
          };
        }
      }
    } finally {
      approveBusy[req.request_id] = false;
    }
  }

  async function deny(req: PendingPairRequest) {
    denyBusy[req.request_id] = true;
    try {
      await api.pairing.deny(req.request_id);
      pending = pending.filter((p) => p.request_id !== req.request_id);
      showToast(true, `Denied pairing request from ${req.source_ip}.`);
      await loadAudit();
    } catch (e: any) {
      if (isAuthError(e)) {
        authRequired = true;
      } else {
        cardMsg[req.request_id] = { ok: false, text: e.message };
        await loadPending();
      }
    } finally {
      denyBusy[req.request_id] = false;
      confirmDeny[req.request_id] = false;
    }
  }

  function secondsLeft(req: PendingPairRequest): number {
    return Math.max(0, req.expires_at - now);
  }

  function countdown(req: PendingPairRequest): string {
    const s = secondsLeft(req);
    if (s <= 0) return 'expired';
    const m = Math.floor(s / 60);
    const sec = s % 60;
    return m > 0 ? `${m}m ${sec.toString().padStart(2, '0')}s` : `${sec}s`;
  }

  function reqStatusBadge(req: PendingPairRequest): { text: string; cls: string } {
    if (req.status === 'invalidated') return { text: 'Invalidated', cls: 'badge-error' };
    if (secondsLeft(req) <= 0) return { text: 'Expired', cls: 'badge-error' };
    if (req.pin_attempts > 0)
      return { text: `${MAX_PIN_ATTEMPTS - req.pin_attempts} PIN attempt${MAX_PIN_ATTEMPTS - req.pin_attempts === 1 ? '' : 's'} left`, cls: 'badge-warning' };
    return { text: 'Awaiting PIN', cls: 'badge-info' };
  }

  onMount(() => {
    loadAll();
    const tick = setInterval(() => {
      now = Math.floor(Date.now() / 1000);
    }, 1000);
    const poll = setInterval(() => {
      if (!authRequired) loadAll();
    }, POLL_MS);
    return () => {
      clearInterval(tick);
      clearInterval(poll);
      clearTimeout(toastTimer);
    };
  });
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <h1 class="text-xl font-semibold flex items-center gap-2">
      <LinkIcon size={20} />
      Pairing
    </h1>
    <button class="btn btn-ghost" onclick={loadAll}>
      <RefreshCw size={14} />
      Refresh
    </button>
  </div>

  <div
    class="card p-3 flex items-start gap-2 text-xs"
    style="border-left: 4px solid var(--color-status-warning, var(--color-accent));"
  >
    <AlertTriangle size={14} class="mt-0.5 shrink-0" style="color: var(--color-status-warning, var(--color-accent));" />
    <span style="color: var(--color-text-muted);">
      The claimed hostname is <strong>self-reported and forgeable</strong>. Verify the
      <strong>source IP</strong> (shown first on each card) before approving. The PIN is
      displayed out-of-band on the <strong>client's console</strong> — type it in yourself;
      it is never pre-filled here.
    </span>
  </div>

  {#if authRequired}
    <div class="card p-8 text-center">
      <LogIn size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
      <p class="text-sm font-medium">Login required</p>
      <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
        Pairing approval needs an authenticated admin session. Your session may have
        expired — sign in again to review pending requests.
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

    <!-- Pending requests -->
    <section class="space-y-3">
      <h2 class="text-sm font-semibold">Pending requests</h2>
      {#if loading}
        <div class="card p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>
      {:else if pending.length === 0}
        <div class="card p-12 text-center">
          <Monitor size={32} class="mx-auto mb-3" style="color: var(--color-text-muted);" />
          <p class="text-sm" style="color: var(--color-text-muted);">No pending pairing requests</p>
          <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
            Clients initiate pairing with <code>monopsony-client pair</code>
          </p>
        </div>
      {:else}
        {#each pending as req (req.request_id)}
          {@const badge = reqStatusBadge(req)}
          <div class="card p-4 space-y-3">
            <div class="flex items-start justify-between gap-4 flex-wrap">
              <div class="space-y-1">
                <!-- source_ip FIRST: the trusted identity signal -->
                <div class="flex items-center gap-2 font-mono text-base font-semibold">
                  <Globe size={16} style="color: var(--color-accent);" />
                  {req.source_ip}
                </div>
                <div class="text-xs" style="color: var(--color-text-muted);">
                  claims hostname
                  <span class="font-mono" style="color: var(--color-text-default, inherit);">{req.claimed_hostname}</span>
                  &middot; {req.claimed_os}{req.claimed_os_version ? ` ${req.claimed_os_version}` : ''}
                </div>
                <div class="text-xs" style="color: var(--color-text-muted);">
                  <span title="request id" class="font-mono">{req.request_id}</span>
                  &middot; requested {formatTimestamp(req.created_at)}
                </div>
              </div>
              <div class="flex flex-col items-end gap-1.5">
                <span class="badge {badge.cls}">{badge.text}</span>
                <span
                  class="badge {secondsLeft(req) <= 0 ? 'badge-error' : 'badge-muted'} font-mono"
                  aria-label="time until request expires"
                >
                  {countdown(req)}
                </span>
              </div>
            </div>

            {#if req.status === 'invalidated'}
              <div class="text-xs p-2 rounded" style="background: var(--color-bg-subtle, transparent); color: var(--color-status-error);">
                Invalidated after {MAX_PIN_ATTEMPTS} failed PIN attempts. The client must start a new pairing request.
              </div>
            {:else}
              <form
                class="flex items-end gap-2 flex-wrap"
                onsubmit={(e) => { e.preventDefault(); approve(req); }}
              >
                <div class="flex flex-col gap-1">
                  <label class="text-xs font-medium" for={'pin-' + req.request_id}>
                    PIN (shown on the client console)
                  </label>
                  <input
                    id={'pin-' + req.request_id}
                    class="input font-mono w-36 tracking-[0.3em] uppercase"
                    type="text"
                    inputmode="text"
                    autocomplete="off"
                    spellcheck="false"
                    maxlength={6}
                    placeholder="••••••"
                    bind:value={pins[req.request_id]}
                  />
                </div>
                <button
                  class="btn btn-primary"
                  type="submit"
                  disabled={approveBusy[req.request_id] || (pins[req.request_id] || '').trim().length !== 6}
                >
                  <Check size={14} />
                  {approveBusy[req.request_id] ? 'Approving...' : 'Approve'}
                </button>
                {#if confirmDeny[req.request_id]}
                  <span class="flex items-center gap-1.5">
                    <span class="text-xs" style="color: var(--color-status-error);">Deny this request?</span>
                    <button
                      class="btn text-xs"
                      type="button"
                      style="background: var(--color-status-error); color: white;"
                      disabled={denyBusy[req.request_id]}
                      onclick={() => deny(req)}
                    >
                      {denyBusy[req.request_id] ? 'Denying...' : 'Confirm deny'}
                    </button>
                    <button class="btn btn-ghost text-xs" type="button" onclick={() => (confirmDeny[req.request_id] = false)}>
                      Cancel
                    </button>
                  </span>
                {:else}
                  <button class="btn btn-ghost" type="button" onclick={() => (confirmDeny[req.request_id] = true)}>
                    <X size={14} />
                    Deny
                  </button>
                {/if}
              </form>
            {/if}

            {#if cardMsg[req.request_id]}
              <p
                class="text-xs"
                style="color: {cardMsg[req.request_id].ok ? 'var(--color-status-success)' : 'var(--color-status-error)'};"
              >
                {cardMsg[req.request_id].text}
              </p>
            {/if}
          </div>
        {/each}
      {/if}
    </section>

    <!-- Audit trail -->
    <section class="space-y-3">
      <h2 class="text-sm font-semibold flex items-center gap-2">
        <History size={14} />
        Approval audit trail
      </h2>
      <div class="card overflow-hidden">
        {#if audit.length === 0}
          <div class="p-6 text-center text-xs" style="color: var(--color-text-muted);">No approval events recorded</div>
        {:else}
          <table class="w-full text-sm">
            <thead>
              <tr class="border-b text-left" style="border-color: var(--color-border-default); color: var(--color-text-muted);">
                <th class="px-4 py-2 font-medium">Time</th>
                <th class="px-4 py-2 font-medium">Action</th>
                <th class="px-4 py-2 font-medium">Request</th>
                <th class="px-4 py-2 font-medium">Approver</th>
                <th class="px-4 py-2 font-medium">Detail</th>
              </tr>
            </thead>
            <tbody>
              {#each audit as entry (entry.request_id + ':' + entry.action + ':' + entry.created_at)}
                <tr class="border-b" style="border-color: var(--color-border-default);">
                  <td class="px-4 py-2 text-xs">{formatTimestamp(entry.created_at)}</td>
                  <td class="px-4 py-2">
                    <span class="badge {entry.action === 'approved' ? 'badge-success' : 'badge-error'}">
                      {entry.action}
                    </span>
                  </td>
                  <td class="px-4 py-2 text-xs font-mono">{entry.request_id}</td>
                  <td class="px-4 py-2 text-xs">{entry.approver}</td>
                  <td class="px-4 py-2 text-xs" style="color: var(--color-text-muted);">{entry.detail ?? '—'}</td>
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
