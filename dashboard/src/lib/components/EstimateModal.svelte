<script lang="ts">
  import { api } from '$lib/api/client';
  import type { EstimateResponse } from '$lib/api/types';
  import Wand2 from 'lucide-svelte/icons/wand-2';
  import X from 'lucide-svelte/icons/x';
  import Loader from 'lucide-svelte/icons/loader-circle';
  import AlertTriangle from 'lucide-svelte/icons/alert-triangle';

  let {
    open = $bindable(),
    onEstimate = (result: EstimateResponse) => {},
  } = $props();

  let binaryPath = $state('');
  let sourceUrl = $state('');
  let loading = $state(false);
  let error = $state('');
  let result = $state<EstimateResponse | null>(null);

  function reset() {
    binaryPath = '';
    sourceUrl = '';
    loading = false;
    error = '';
    result = null;
  }

  function close() {
    reset();
    open = false;
  }

  async function estimate() {
    if (!binaryPath.trim()) {
      error = 'Binary path is required';
      return;
    }

    loading = true;
    error = '';

    try {
      result = await api.shadow.recipe.estimate({
        binary_path: binaryPath,
        source_url: sourceUrl || undefined,
      });
    } catch (e: any) {
      error = e.message;
    } finally {
      loading = false;
    }
  }

  function apply() {
    if (result) {
      onEstimate(result);
      close();
    }
  }
</script>

{#if open}
  <!-- Backdrop -->
  <div
    class="fixed inset-0 z-50 flex items-center justify-center"
    style="background-color: rgba(0, 0, 0, 0.6);"
    onclick={close}
    onkeydown={(e) => e.key === 'Escape' && close()}
    role="presentation"
  >
    <!-- Modal -->
    <div
      class="card w-full max-w-lg p-6 space-y-4"
      style="background-color: var(--color-bg-surface);"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
      role="dialog"
      aria-modal="true"
      tabindex="-1"
    >
      <div class="flex items-center justify-between">
        <div class="flex items-center gap-2">
          <Wand2 size={18} style="color: var(--color-accent);" />
          <h2 class="text-sm font-semibold">Estimate Recipe from Binary</h2>
        </div>
        <button class="btn-ghost rounded p-1" onclick={close} aria-label="Close">
          <X size={16} />
        </button>
      </div>

      <p class="text-xs" style="color: var(--color-text-muted);">
        Enter the path to a binary on a client system. The server will inspect it to detect
        name, version, architecture, and source type, then generate a recipe template.
      </p>

      {#if error}
        <div class="rounded px-3 py-2 text-sm" style="background-color: color-mix(in srgb, var(--color-status-error) 15%, transparent); color: var(--color-status-error);">
          {error}
        </div>
      {/if}

      {#if !result}
        <!-- Input form -->
        <div class="space-y-3">
          <div class="space-y-1.5">
            <label class="text-xs font-medium" style="color: var(--color-text-muted);" for="binpath">
              Binary path <span style="color: var(--color-status-error);">*</span>
            </label>
            <input
              id="binpath"
              class="input font-mono text-xs"
              bind:value={binaryPath}
              placeholder="/usr/local/bin/myapp"
            />
          </div>

          <div class="space-y-1.5">
            <label class="text-xs font-medium" style="color: var(--color-text-muted);" for="srcurl">
              Source URL <span style="color: var(--color-text-muted);">(optional)</span>
            </label>
            <input
              id="srcurl"
              class="input font-mono text-xs"
              bind:value={sourceUrl}
              placeholder="https://github.com/owner/repo"
            />
            <p class="text-xs" style="color: var(--color-text-muted);">
              Providing a GitHub URL enables asset discovery from releases.
            </p>
          </div>
        </div>

        <div class="flex justify-end gap-2 pt-2">
          <button class="btn btn-secondary" onclick={close}>Cancel</button>
          <button class="btn btn-primary" onclick={estimate} disabled={loading}>
            {#if loading}
              <Loader size={14} class="animate-spin" />
              Estimating...
            {:else}
              <Wand2 size={14} />
              Estimate
            {/if}
          </button>
        </div>
      {:else}
        <!-- Results -->
        <div class="space-y-3">
          <div class="rounded p-3 space-y-2" style="background-color: var(--color-bg-surface-2);">
            <div class="grid grid-cols-2 gap-2 text-xs">
              <div>
                <span style="color: var(--color-text-muted);">Name:</span>
                <span class="ml-2 font-medium">{result.detected.name}</span>
              </div>
              <div>
                <span style="color: var(--color-text-muted);">Version:</span>
                <span class="ml-2 font-medium">{result.detected.version || '—'}</span>
              </div>
              <div>
                <span style="color: var(--color-text-muted);">Arch:</span>
                <span class="ml-2 font-medium">{result.detected.arch}</span>
              </div>
              <div>
                <span style="color: var(--color-text-muted);">Source:</span>
                <span class="ml-2 font-medium">{result.detected.source_type}</span>
              </div>
            </div>

            {#if result.detected.available_assets.length > 0}
              <div>
                <span class="text-xs" style="color: var(--color-text-muted);">Available assets:</span>
                <div class="mt-1 space-y-0.5">
                  {#each result.detected.available_assets.slice(0, 6) as asset}
                    <div class="text-xs font-mono pl-3" style="color: var(--color-text-primary);">• {asset}</div>
                  {/each}
                  {#if result.detected.available_assets.length > 6}
                    <div class="text-xs pl-3" style="color: var(--color-text-muted);">
                      +{result.detected.available_assets.length - 6} more
                    </div>
                  {/if}
                </div>
              </div>
            {/if}
          </div>

          {#if result.warnings.length > 0}
            <div class="space-y-1">
              {#each result.warnings as w}
                <div class="flex items-start gap-1.5 text-xs" style="color: var(--color-status-warning);">
                  <AlertTriangle size={12} class="mt-0.5 shrink-0" />
                  <span>{w}</span>
                </div>
              {/each}
            </div>
          {/if}

          <!-- Generated template preview -->
          <div>
            <span class="text-xs font-medium" style="color: var(--color-text-muted);">Generated recipe:</span>
            <pre class="mt-1 max-h-40 overflow-auto rounded p-3 text-xs font-mono" style="background-color: var(--color-bg-base); color: var(--color-text-primary);">{result.recipe_toml}</pre>
          </div>
        </div>

        <div class="flex justify-end gap-2 pt-2">
          <button class="btn btn-secondary" onclick={reset}>Try Again</button>
          <button class="btn btn-primary" onclick={apply}>
            <Wand2 size={14} />
            Load into Editor
          </button>
        </div>
      {/if}
    </div>
  </div>
{/if}
