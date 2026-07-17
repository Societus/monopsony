<script lang="ts">
  import HelpCircle from 'lucide-svelte/icons/help-circle';

  // The help key, e.g. "field:package.name", "section:install", "gui:cache.purge"
  let { helpKey, label }: { helpKey: string; label?: string } = $props();

  let open = $state(false);
  let data = $state<{ title: string; body: string } | null>(null);
  let loading = $state(false);
  let error = $state('');
  let container: HTMLDivElement;

  async function load() {
    if (data || loading) return;
    loading = true;
    error = '';
    try {
      const res = await fetch(`/api/v1/help/${encodeURIComponent(helpKey)}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      data = await res.json();
    } catch (e: any) {
      error = e.message || 'Failed to load help';
    } finally {
      loading = false;
    }
  }

  function onEnter() {
    if (!open) {
      open = true;
      load();
    }
  }

  function onLeave() {
    open = false;
  }

  function onToggle(e: MouseEvent) {
    e.stopPropagation();
    open = !open;
    if (open) load();
  }

  // Close on Escape
  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') open = false;
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div
  class="help-icon-wrapper relative inline-flex"
  bind:this={container}
  onmouseenter={onEnter}
  onmouseleave={onLeave}
  role="button"
  tabindex="0"
  aria-label="Show help: {label || helpKey}"
>
  <button
    type="button"
    class="help-trigger p-0.5 rounded hover:bg-[var(--color-bg-surface-2)] transition-colors cursor-help"
    onclick={onToggle}
    aria-expanded={open}
  >
    <HelpCircle size={14} class="text-[var(--color-text-muted)]" />
  </button>

  {#if open}
    <div
      class="help-popup absolute z-50 bottom-full left-1/2 -translate-x-1/2 mb-2 w-80 max-w-[90vw]"
      role="tooltip"
    >
      <div
        class="card p-3 shadow-xl border rounded-lg"
        style="background-color: var(--color-bg-surface-1); border-color: var(--color-border-subtle);"
      >
        {#if loading}
          <div class="text-xs" style="color: var(--color-text-muted);">Loading...</div>
        {:else if error}
          <div class="text-xs" style="color: var(--color-status-error);">{error}</div>
        {:else if data}
          <div class="text-xs font-semibold mb-1.5" style="color: var(--color-text);">
            {data.title}
          </div>
          <p class="text-xs leading-relaxed" style="color: var(--color-text-muted);">
            {data.body}
          </p>
        {:else}
          <div class="text-xs italic" style="color: var(--color-text-muted);">
            No help available.
          </div>
        {/if}
      </div>
      <!-- Arrow -->
      <div
        class="absolute left-1/2 -translate-x-1/2 -bottom-1 w-2 h-2 rotate-45 border-r border-b"
        style="background-color: var(--color-bg-surface-1); border-color: var(--color-border-subtle);"
      ></div>
    </div>
  {/if}
</div>

<style>
  .help-icon-wrapper {
    vertical-align: middle;
  }
  .help-trigger:focus-visible {
    outline: 2px solid var(--color-accent);
    outline-offset: 1px;
  }
</style>
