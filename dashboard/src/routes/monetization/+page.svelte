<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api/client';
  import type { MonetizationConfig } from '$lib/api/types';
  import Coins from 'lucide-svelte/icons/coins';
  import Bug from 'lucide-svelte/icons/bug';
  import Zap from 'lucide-svelte/icons/zap';
  import Save from 'lucide-svelte/icons/save';
  import Gift from 'lucide-svelte/icons/gift';
  import Info from 'lucide-svelte/icons/info';

  let config = $state<MonetizationConfig | null>(null);
  let loading = $state(true);
  let saving = $state(false);
  let error = $state('');
  let success = $state('');

  // Form state
  let publicMetrics = $state(false);
  let lightning = $state('');
  let bitcoin = $state('');
  let openalias = $state('');
  let bountyAddr = $state('');
  let bountyCritical = $state('');
  let bountyHigh = $state('');
  let bountyMedium = $state('');
  let v4vEnabled = $state(false);
  let v4vBoost = $state('');

  async function load() {
    loading = true;
    try {
      config = await api.features.monetization.get();
      publicMetrics = config.public_metrics_enabled;
      lightning = config.lightning_address || '';
      bitcoin = config.bitcoin_address || '';
      openalias = config.openalias || '';
      bountyAddr = config.bounty_address || '';
      bountyCritical = config.bounty_critical_sats?.toString() || '';
      bountyHigh = config.bounty_high_sats?.toString() || '';
      bountyMedium = config.bounty_medium_sats?.toString() || '';
      v4vEnabled = config.v4v_enabled;
      v4vBoost = config.v4v_default_boost_sats?.toString() || '';
      error = '';
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  async function save() {
    saving = true;
    error = '';
    success = '';
    try {
      await api.features.monetization.update({
        public_metrics_enabled: publicMetrics,
        lightning_address: lightning || null,
        bitcoin_address: bitcoin || null,
        openalias: openalias || null,
        bounty_address: bountyAddr || null,
        bounty_critical_sats: bountyCritical ? parseInt(bountyCritical) : null,
        bounty_high_sats: bountyHigh ? parseInt(bountyHigh) : null,
        bounty_medium_sats: bountyMedium ? parseInt(bountyMedium) : null,
        v4v_enabled: v4vEnabled,
        v4v_default_boost_sats: v4vBoost ? parseInt(v4vBoost) : null,
      });
      success = 'Monetization settings saved';
    } catch (e: any) {
      error = e.message;
    } finally {
      saving = false;
    }
  }

  onMount(load);
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <div>
      <h1 class="text-xl font-semibold">Monetization</h1>
      <p class="text-xs mt-1" style="color: var(--color-text-muted);">
        Configure funding mechanisms for public or federated nodes
      </p>
    </div>
    <button class="btn btn-primary" onclick={save} disabled={saving || loading}>
      <Save size={14} />
      {saving ? 'Saving...' : 'Save'}
    </button>
  </div>

  {#if error}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-error);">
      <span class="text-sm" style="color: var(--color-status-error);">{error}</span>
    </div>
  {/if}

  {#if success}
    <div class="card p-3 border-l-4" style="border-left-color: var(--color-status-success);">
      <span class="text-sm" style="color: var(--color-status-success);">{success}</span>
    </div>
  {/if}

  {#if loading}
    <div class="text-sm" style="color: var(--color-text-muted);">Loading...</div>
  {:else}
    <!-- Public metrics toggle -->
    <div class="card p-4">
      <label class="flex items-center gap-3">
        <input type="checkbox" bind:checked={publicMetrics} class="w-4 h-4 accent-[var(--color-accent)]" />
        <div>
          <div class="text-sm font-medium">Show funding stats to clients</div>
          <div class="text-xs" style="color: var(--color-text-muted);">
            When enabled, your public mirror page will display donation info and sustainability metrics
          </div>
        </div>
      </label>
    </div>

    <!-- Donations -->
    <div class="card p-5 space-y-4">
      <div class="flex items-center gap-2">
        <Gift size={16} style="color: var(--color-accent);" />
        <h2 class="text-sm font-semibold">Donations</h2>
      </div>
      <div class="grid grid-cols-2 gap-4">
        <div>
          <label for="mon-lightning" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Lightning Address</label>
          <input id="mon-lightning" class="input font-mono text-xs" bind:value={lightning} placeholder="node@walletofsatoshi.com" />
        </div>
        <div>
          <label for="mon-bitcoin" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Bitcoin Address</label>
          <input id="mon-bitcoin" class="input font-mono text-xs" bind:value={bitcoin} placeholder="bc1q..." />
        </div>
        <div>
          <label for="mon-openalias" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">OpenAlias</label>
          <input id="mon-openalias" class="input font-mono text-xs" bind:value={openalias} placeholder="donation.example.com" />
        </div>
      </div>
    </div>

    <!-- Bug Bounty -->
    <div class="card p-5 space-y-4">
      <div class="flex items-center gap-2">
        <Bug size={16} style="color: var(--color-status-warning);" />
        <h2 class="text-sm font-semibold">Bug Bounty Program</h2>
      </div>
      <p class="text-xs" style="color: var(--color-text-muted);">
        Fund a bounty pool that rewards security researchers for responsible disclosure.
      </p>
      <div class="grid grid-cols-2 gap-4">
        <div class="col-span-2">
          <label for="mon-bounty-addr" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Bounty Fund Address (Lightning)</label>
          <input id="mon-bounty-addr" class="input font-mono text-xs" bind:value={bountyAddr} placeholder="bounty@lightning.example.com" />
        </div>
        <div>
          <label for="mon-bounty-critical" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Critical (sats)</label>
          <input id="mon-bounty-critical" class="input text-xs" type="number" bind:value={bountyCritical} placeholder="1000000" />
        </div>
        <div>
          <label for="mon-bounty-high" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">High (sats)</label>
          <input id="mon-bounty-high" class="input text-xs" type="number" bind:value={bountyHigh} placeholder="250000" />
        </div>
        <div>
          <label for="mon-bounty-medium" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Medium (sats)</label>
          <input id="mon-bounty-medium" class="input text-xs" type="number" bind:value={bountyMedium} placeholder="50000" />
        </div>
      </div>
    </div>

    <!-- V4V (Value for Value) -->
    <div class="card p-5 space-y-4">
      <div class="flex items-center gap-2">
        <Zap size={16} style="color: var(--color-accent);" />
        <h2 class="text-sm font-semibold">Value for Value (V4V)</h2>
      </div>
      <p class="text-xs" style="color: var(--color-text-muted);">
        Allow clients to send micro-donations (boosts) when downloading packages.
      </p>
      <label class="flex items-center gap-3">
        <input type="checkbox" bind:checked={v4vEnabled} class="w-4 h-4 accent-[var(--color-accent)]" />
        <span class="text-sm">Enable V4V boosts</span>
      </label>
      {#if v4vEnabled}
        <div>
          <label for="mon-v4v-boost" class="text-xs font-medium block mb-1" style="color: var(--color-text-muted);">Default boost amount (sats)</label>
          <input id="mon-v4v-boost" class="input text-xs" type="number" bind:value={v4vBoost} placeholder="100" />
        </div>
      {/if}
    </div>

    <!-- Info note -->
    <div class="flex items-start gap-2 text-xs" style="color: var(--color-text-muted);">
      <Info size={12} class="mt-0.5 shrink-0" />
      <span>
        Monetization features are only visible when the node tier is set to "Public" or "Federated".
        Configure the node tier in Settings.
      </span>
    </div>
  {/if}
</div>
