<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { api } from '$lib/api/client';
  import type { AuthStatus } from '$lib/api/types';
  import { authStatus } from '$lib/stores';
  import Logo from 'lucide-svelte/icons/boxes';
  import Lock from 'lucide-svelte/icons/lock';
  import User from 'lucide-svelte/icons/user';
  import Circle from 'lucide-svelte/icons/circle';

  let username = $state('');
  let password = $state('');
  let loading = $state(true);
  let submitting = $state(false);
  let error = $state('');
  let status = $state<AuthStatus | null>(null);

  onMount(async () => {
    try {
      status = await api.auth.status();
      authStatus.set(status);
      // Auto-redirect if not required
      if (!status.auth_required) {
        await goto('/');
        return;
      }
    } catch {
      // Continue to login form
    } finally {
      loading = false;
    }
  });

  async function submit(e: Event) {
    e.preventDefault();
    submitting = true;
    error = '';
    try {
      const result = await api.auth.login({ username, password });
      if (result.success) {
        await goto('/');
      } else {
        error = result.message || 'Login failed';
      }
    } catch (e: any) {
      error = e.message;
    } finally {
      submitting = false;
    }
  }
</script>

<div class="flex min-h-screen items-center justify-center" style="background-color: var(--color-bg-base);">
  <div class="w-full max-w-sm space-y-6">
    <!-- Logo -->
    <div class="text-center">
      <div class="inline-flex items-center gap-2.5">
        <Logo size={28} color="var(--color-accent)" />
        <span class="text-lg font-semibold">Monopsony</span>
      </div>
      <p class="mt-2 text-xs" style="color: var(--color-text-muted);">Dashboard Login</p>
    </div>

    {#if loading}
      <div class="text-center text-sm" style="color: var(--color-text-muted);">Checking access...</div>
    {:else}
      <!-- Tailscale context badge -->
      {#if status?.tailscale?.available}
        <div class="card p-3 text-center">
          {#if status.tailscale.funnel_mode}
            <div class="flex items-center justify-center gap-2 text-xs" style="color: var(--color-status-error);">
              <Circle size={8} fill="currentColor" />
              Public access via Tailscale Funnel
            </div>
            <p class="mt-1 text-xs" style="color: var(--color-text-muted);">Authentication required</p>
          {:else if status.tailscale.serve_mode}
            <div class="flex items-center justify-center gap-2 text-xs" style="color: var(--color-accent);">
              <Circle size={8} fill="currentColor" />
              Tailscale tailnet: {status.tailscale.tailnet_name || 'connected'}
            </div>
            <p class="mt-1 text-xs" style="color: var(--color-text-muted);">
              Login required (always_require_login is on)
            </p>
          {/if}
        </div>
      {/if}

      <!-- Login form -->
      {#if status && !status.credentials_configured}
        <div class="card p-5 text-center">
          <p class="text-sm" style="color: var(--color-status-warning);">
            Dashboard credentials not configured.
          </p>
          <p class="mt-2 text-xs" style="color: var(--color-text-muted);">
            Run <code class="rounded px-1.5 py-0.5" style="background-color: var(--color-bg-surface-2);">
              monopsony-server dashboard-set-password
            </code> to set up login.
          </p>
        </div>
      {:else}
        <form class="card p-6 space-y-4" onsubmit={submit}>
          {#if error}
            <div class="rounded px-3 py-2 text-sm" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
              {error}
            </div>
          {/if}

          <div class="space-y-1.5">
            <label class="text-xs font-medium" style="color: var(--color-text-muted);" for="username">Username</label>
            <div class="relative">
              <User size={14} class="absolute left-3 top-2.5" style="color: var(--color-text-muted);" />
              <input
                id="username"
                class="input pl-9"
                bind:value={username}
                placeholder="admin"
                autocomplete="username"
              />
            </div>
          </div>

          <div class="space-y-1.5">
            <label class="text-xs font-medium" style="color: var(--color-text-muted);" for="password">Password</label>
            <div class="relative">
              <Lock size={14} class="absolute left-3 top-2.5" style="color: var(--color-text-muted);" />
              <input
                id="password"
                type="password"
                class="input pl-9"
                bind:value={password}
                placeholder="••••••••"
                autocomplete="current-password"
              />
            </div>
          </div>

          <button type="submit" class="btn btn-primary w-full justify-center" disabled={submitting || !username || !password}>
            {submitting ? 'Signing in...' : 'Sign In'}
          </button>
        </form>
      {/if}
    {/if}
  </div>
</div>
