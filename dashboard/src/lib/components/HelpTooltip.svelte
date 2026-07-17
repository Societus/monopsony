<script lang="ts">
  import { onMount } from 'svelte';
  import HelpCircle from 'lucide-svelte/icons/help-circle';
  import X from 'lucide-svelte/icons/x';

  export let field: string;
  export let trigger: 'hover' | 'click' = 'hover';
  export let position: 'above' | 'below' | 'right' | 'left' = 'above';

  let tooltipVisible = $state(false);
  let helpText = $state('');
  let loading = $state(false);
  let error = $state('');

  async function fetchHelp() {
    if (helpText) return; // Already loaded
    
    loading = true;
    try {
      const response = await fetch(`/api/v1/help/${encodeURIComponent(field)}`);
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      helpText = await response.text();
    } catch (e: any) {
      error = e.message || 'Failed to load help';
    } finally {
      loading = false;
      tooltipVisible = true;
    }
  }

  function toggleTooltip() {
    if (!tooltipVisible && trigger === 'click') {
      fetchHelp();
    } else {
      tooltipVisible = !tooltipVisible;
    }
  }

  function closeTooltip() {
    tooltipVisible = false;
  }

  // Position calculation for click trigger
  let tooltipStyle = $derived(
    tooltipVisible ? `position: absolute; ${getPositionStyle(position)}` : 'display: none;'
  );

  function getPositionStyle(pos: string): string {
    switch (pos) {
      case 'above': return 'bottom: 100%; left: 50%; transform: translateX(-50%); margin-bottom: 8px;';
      case 'below': return 'top: 100%; left: 50%; transform: translateX(-50%); margin-top: 8px;';
      case 'right': return 'left: 100%; top: 50%; transform: translateY(-50%); margin-left: 8px;';
      case 'left': return 'right: 100%; top: 50%; transform: translateY(-50%); margin-right: 8px;';
      default: return '';
    }
  }

  // Auto-fetch on hover for hover trigger
  let mouseOver = $state(false);
  
  async function handleMouseEnter() {
    if (trigger === 'hover') {
      mouseOver = true;
      await fetchHelp();
    }
  }

  function handleMouseLeave() {
    if (trigger === 'hover') {
      mouseOver = false;
      tooltipVisible = false;
    }
  }
</script>

<div 
  class="help-tooltip-wrapper relative inline-block"
  on:mouseenter={handleMouseEnter}
  on:mouseleave={handleMouseLeave}
  bind:this={$el}
>
  <button
    class="help-trigger cursor-help p-1 rounded hover:bg-[var(--color-bg-surface-2)] transition-colors"
    style="{trigger === 'click' ? '' : 'display: none;'}"
    on:click={toggleTooltip}
    aria-label="Show help for {field}"
  >
    <HelpCircle size={16} class="text-[var(--color-text-muted)]" />
  </button>

  {#if tooltipVisible}
    <div 
      class="help-tooltip-card p-4 rounded-lg shadow-xl border z-50 max-w-md"
      style={tooltipStyle}
    >
      <!-- Header -->
      <div class="flex items-start justify-between mb-3">
        <h3 class="text-sm font-semibold capitalize text-[var(--color-text)]">{field.replace(/_/g, ' ')}</h3>
        <button 
          class="help-close p-1 rounded hover:bg-[var(--color-bg-surface-2)] transition-colors"
          on:click={closeTooltip}
          aria-label="Close help"
        >
          <X size={14} />
        </button>
      </div>

      <!-- Loading state -->
      {#if loading}
        <div class="text-sm text-[var(--color-text-muted)]">Loading...</div>
      {:else if error}
        <div class="text-sm text-[var(--color-status-error)]">{error}</div>
      {:else if helpText}
        <!-- Formatted content -->
        {@html helpText.replace(/\n\n/g, '</p><p class="mb-3">').replace(/\n/g, '<br>')}
      {:else}
        <div class="text-sm text-[var(--color-text-muted)] italic">No help available for this field.</div>
      {/if}

      <!-- Footer -->
      <div class="mt-4 pt-3 border-t border-[var(--color-border-subtle)]">
        <button 
          class="btn btn-ghost btn-sm w-full"
          on:click={closeTooltip}
        >
          Close
        </button>
      </div>
    </div>
  {/if}

  <slot />
</div>

<style>
  .help-tooltip-card {
    background-color: var(--color-bg-surface-1);
    border-color: var(--color-border-subtle);
  }
  
  .help-trigger:hover ~ .help-tooltip-card,
  .help-tooltip-wrapper:focus-within .help-tooltip-card {
    /* Focus styles if needed */
  }
</style>
