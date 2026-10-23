<script lang="ts">
  import '../app.css';
  import { page } from '$app/stores';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { api } from '$lib/api/client';
  import { authStatus } from '$lib/stores';
  import type { AuthStatus } from '$lib/api/types';
  import Activity from 'lucide-svelte/icons/activity';
  import Database from 'lucide-svelte/icons/database';
  import Package from 'lucide-svelte/icons/package';
  import Settings from 'lucide-svelte/icons/settings';
  import Server from 'lucide-svelte/icons/server';
  import Shield from 'lucide-svelte/icons/shield';
  import MonitorSmartphone from 'lucide-svelte/icons/monitor-smartphone';
  import Users from 'lucide-svelte/icons/users';
  import Coins from 'lucide-svelte/icons/coins';
  import LinkIcon from 'lucide-svelte/icons/link';
  import Circle from 'lucide-svelte/icons/circle';
  import Logo from 'lucide-svelte/icons/boxes';
  import Lock from 'lucide-svelte/icons/lock';

  let { children } = $props();

  let authLoaded = $state(false);
  let currentPath = $state('');

  // Check auth status on mount and on route changes
  async function checkAuth() {
    const path = $page.url.pathname;

    // Never redirect away from /login
    if (path === '/login') {
      authLoaded = true;
      return;
    }

    try {
      const status = await api.auth.status();
      authStatus.set(status);

      // If auth is required (funnel mode or always_require_login) and
      // no session cookie exists, redirect to login.
      // The /auth/status endpoint tells us if auth is required.
      if (status.auth_required) {
        // Check if we have a VALID session by probing an endpoint the server
        // always auth-gates. GET /api/v1/health is NOT usable for this: in
        // tailnet-serve / direct-LAN mode it is an optional-auth read that
        // returns 200 even with no/expired session — probing it made the SPA
        // render as logged in while every write (POST) 401'd. GET /config is
        // write-class (always requires auth) per auth::is_write_endpoint.
        try {
          await api.config.get();
          // Success — we're authenticated (session cookie works)
        } catch {
          // 401 on an always-auth read means we need to login
          await goto('/login');
          return;
        }
      }
    } catch {
      // Auth status endpoint itself failed — try loading page anyway
    }

    authLoaded = true;
  }

  onMount(() => {
    checkAuth();

    // Re-check auth when route changes
    const unsubscribe = page.subscribe((p) => {
      const newPath = p.url.pathname;
      if (newPath !== currentPath) {
        currentPath = newPath;
        if (newPath !== '/login') {
          checkAuth();
        }
      }
    });

    return unsubscribe;
  });

  const navItems = [
    { href: '', label: 'Overview', icon: Activity },
    { href: 'cache', label: 'Proxy Cache', icon: Database },
    { href: 'shadow', label: 'Shadow Packages', icon: Package },
    { href: 'settings', label: 'Settings', icon: Settings, requiresAuth: true },
    { href: 'mirrors', label: 'Mirrors', icon: Server },
    { href: 'tracker', label: 'Tracker', icon: MonitorSmartphone },
    { href: 'monetization', label: 'Monetization', icon: Coins },
    { href: 'security', label: 'Security', icon: Shield },
    { href: 'pairing', label: 'Pairing', icon: LinkIcon },
    { href: 'clients', label: 'Clients', icon: Users },
  ];

  function isActive(href: string): boolean {
    const path = $page.url.pathname.replace(/^\/?/, '');
    if (href === '') return path === '' || path === '/';
    return path.startsWith(href);
  }

  function getTailscaleBadge($auth: any): { text: string; cls: string } {
    if (!$auth?.tailscale?.available) {
      return { text: 'Direct (LAN)', cls: 'badge-muted' };
    }
    if ($auth.tailscale.funnel_mode) {
      return { text: `Funnel (public)`, cls: 'badge-error' };
    }
    if ($auth.tailscale.serve_mode) {
      return { text: `Tailnet (${$auth.tailscale.tailnet_name || 'connected'})`, cls: 'badge-success' };
    }
    return { text: 'Tailscale', cls: 'badge-info' };
  }
</script>

{#if !authLoaded && $page.url.pathname !== '/login'}
  <!-- Loading screen while auth is being checked -->
  <div class="flex min-h-screen items-center justify-center" style="background-color: var(--color-bg-base);">
    <div class="flex items-center gap-3">
      <div class="w-5 h-5 rounded-full border-2 border-t-transparent animate-spin" style="border-color: var(--color-accent); border-top-color: transparent;"></div>
      <span class="text-sm" style="color: var(--color-text-muted);">Loading...</span>
    </div>
  </div>
{:else if $page.url.pathname === '/login'}
  <!-- Login page renders without the shell -->
  {@render children()}
{:else}
  <div class="flex min-h-screen">
    <!-- Sidebar -->
    <aside class="flex w-56 flex-col border-r" style="background-color: var(--color-bg-surface); border-color: var(--color-border-default);">
      <!-- Logo -->
      <div class="flex h-14 items-center gap-2.5 px-4 border-b" style="border-color: var(--color-border-default);">
        <Logo size={20} color="var(--color-accent)" />
        <span class="font-semibold text-sm tracking-wide">Monopsony</span>
      </div>

      <!-- Nav -->
      <nav class="flex-1 space-y-0.5 p-2">
        {#each navItems as item}
          <a
            href="/{item.href}"
            class="nav-link {isActive(item.href) ? 'nav-link-active' : ''}"
          >
            <item.icon size={16} />
            {item.label}
            {#if item.requiresAuth && $authStatus && !$authStatus.auth_method.includes('trust') && $authStatus.auth_required}
              <Lock size={11} class="ml-auto opacity-50" />
            {/if}
          </a>
        {/each}
      </nav>

      <!-- Version -->
      <div class="px-4 py-3 text-xs" style="color: var(--color-text-muted);">
        v1.0.0-rc1
      </div>
    </aside>

    <!-- Main content -->
    <div class="flex flex-1 flex-col">
      <!-- Topbar -->
      <header class="flex h-14 items-center justify-between border-b px-6" style="background-color: var(--color-bg-surface); border-color: var(--color-border-default);">
        <div class="text-sm font-medium" style="color: var(--color-text-muted);">
          {$page.url.pathname.replace('/', '').replace(/^\//, '') || 'Overview'}
        </div>

        <div class="flex items-center gap-3">
          {#if $authStatus}
            {@const badge = getTailscaleBadge($authStatus)}
            <span class="badge {badge.cls}">
              <Circle size={8} fill="currentColor" />
              {badge.text}
            </span>
            {#if $authStatus.auth_required && $authStatus.credentials_configured}
              <button
                class="btn btn-ghost text-xs"
                onclick={async () => { await api.auth.logout(); await goto('/login'); }}
              >
                <Lock size={12} />
                Logout
              </button>
            {/if}
          {/if}
        </div>
      </header>

      <!-- Page content -->
      <main class="flex-1 overflow-auto p-6" style="background-color: var(--color-bg-base);">
        {@render children()}
      </main>
    </div>
  </div>
{/if}
