<script lang="ts">
  import { testAssetPattern } from '$lib/utils/recipe';
  import CheckCircle from 'lucide-svelte/icons/check-circle';
  import XCircle from 'lucide-svelte/icons/x-circle';
  import Terminal from 'lucide-svelte/icons/terminal';

  let {
    assets = $bindable([] as string[]),
  } = $props();

  let pattern = $state('');
  let tagPattern = $state('');

  // Derive patterns from the test fields
  const ruleResults = $derived(
    assets.map(a => {
      if (!pattern) return { asset: a, matches: null as boolean | null };
      try {
        const re = new RegExp(pattern);
        return { asset: a, matches: re.test(a) };
      } catch {
        return { asset: a, matches: null };
      }
    })
  );

  const matchCount = $derived(ruleResults.filter(r => r.matches === true).length);
</script>

<div class="space-y-3">
  <div class="flex items-center gap-2">
    <Terminal size={14} style="color: var(--color-accent);" />
    <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-text-muted);">
      Asset Pattern Tester
    </span>
  </div>

  {#if assets.length === 0}
    <p class="text-xs" style="color: var(--color-text-muted);">
      No assets detected. Run recipe estimation with a source URL to discover available release assets.
    </p>
  {:else}
    <div class="space-y-1.5">
      <label class="text-xs" style="color: var(--color-text-muted);" for="pattern">
        Rule pattern (regex)
      </label>
      <input
        id="pattern"
        class="input font-mono text-xs"
        bind:value={pattern}
        placeholder="linux.*amd64"
      />
      {#if pattern}
        <p class="text-xs" style="color: var(--color-text-muted);">
          {matchCount} / {assets.length} assets match
        </p>
      {/if}
    </div>

    <div class="space-y-1 max-h-40 overflow-y-auto">
      {#each ruleResults as r}
        <div class="flex items-center gap-2 text-xs font-mono px-2 py-0.5 rounded" style="background-color: var(--color-bg-surface-2);">
          {#if r.matches === true}
            <CheckCircle size={12} style="color: var(--color-status-success);" />
            <span style="color: var(--color-status-success);">{r.asset}</span>
          {:else if r.matches === false}
            <XCircle size={12} style="color: var(--color-text-muted); opacity: 0.5;" />
            <span style="color: var(--color-text-muted); opacity: 0.5;">{r.asset}</span>
          {:else}
            <span class="w-3 h-3 rounded-full" style="border: 1px solid var(--color-border-default);"></span>
            <span style="color: var(--color-text-muted);">{r.asset}</span>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>
