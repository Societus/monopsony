<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { goto } from '$app/navigation';
  import { api } from '$lib/api/client';
  import type { EstimateResponse, RecipeValidationResult } from '$lib/api/types';
  import { quickValidateToml, EMPTY_RECIPE, type ClientValidationResult } from '$lib/utils/recipe';
  import EstimateModal from '$lib/components/EstimateModal.svelte';
  import AssetTester from '$lib/components/AssetTester.svelte';
  import Wand2 from 'lucide-svelte/icons/wand-2';
  import Save from 'lucide-svelte/icons/save';
  import ArrowLeft from 'lucide-svelte/icons/arrow-left';
  import CheckCircle from 'lucide-svelte/icons/check-circle';
  import AlertTriangle from 'lucide-svelte/icons/alert-triangle';
  import XCircle from 'lucide-svelte/icons/x-circle';
  import Info from 'lucide-svelte/icons/info';
  import Loader from 'lucide-svelte/icons/loader-circle';
  import HelpTooltip from '$lib/components/HelpTooltip.svelte';

  let recipe = $state(EMPTY_RECIPE);
  let clientValidation = $state<ClientValidationResult>({ errors: [], warnings: [] });
  let serverValidation = $state<RecipeValidationResult | null>(null);
  let validating = $state(false);
  let saving = $state(false);
  let saveMsg = $state('');
  let saveError = $state('');
  let estimateOpen = $state(false);
  let detectedAssets = $state<string[]>([]);
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;
  let textareaEl: HTMLTextAreaElement | null = null;

  // Line numbers for the editor
  let lineCount = $derived(recipe.split('\n').length);
  let lineNumbers = $derived(Array.from({ length: lineCount }, (_, i) => i + 1));

  // Run client-side validation immediately on every keystroke
  $effect(() => {
    clientValidation = quickValidateToml(recipe);
  });

  // Debounced server validation
  function scheduleServerValidation() {
    if (debounceTimer) clearTimeout(debounceTimer);
    debounceTimer = setTimeout(async () => {
      if (!recipe.trim()) {
        serverValidation = null;
        return;
      }
      validating = true;
      try {
        serverValidation = await api.shadow.recipe.validate(recipe);
      } catch {
        // Silently ignore — client validation still runs
        serverValidation = null;
      } finally {
        validating = false;
      }
    }, 500);
  }

  // Trigger server validation when recipe changes
  $effect(() => {
    // Access recipe to track it
    void recipe;
    scheduleServerValidation();
  });

  onDestroy(() => {
    if (debounceTimer) clearTimeout(debounceTimer);
  });

  function syncScroll() {
    // Sync line numbers with textarea scroll
    if (textareaEl) {
      const gutter = document.getElementById('line-gutter');
      if (gutter) {
        gutter.scrollTop = textareaEl.scrollTop;
      }
    }
  }

  function onEstimate(result: EstimateResponse) {
    recipe = result.recipe_toml;
    detectedAssets = result.detected.available_assets;
    saveMsg = '';
    saveError = '';
  }

  async function saveAndRegister() {
    // Final validation check
    if (clientValidation.errors.length > 0) {
      saveError = 'Fix validation errors before saving';
      return;
    }

    saving = true;
    saveMsg = '';
    saveError = '';

    try {
      // Parse recipe to extract package info for registration
      const pkgName = recipe.match(/^name\s*=\s*["'](.+?)["']/m)?.[1] || '';
      const sourceType = recipe.match(/^source_type\s*=\s*["'](.+?)["']/m)?.[1] || 'direct_url';
      const sourceUrl = recipe.match(/^source_url\s*=\s*["'](.+?)["']/m)?.[1] || '';

      if (!pkgName) {
        saveError = 'Package name is required';
        saving = false;
        return;
      }

      const result = await api.shadow.register({
        name: pkgName,
        source_type: sourceType,
        source_url: sourceUrl,
      });

      if (result.success) {
        saveMsg = `Package "${pkgName}" registered successfully`;
        await goto('/shadow');
      } else {
        saveError = 'Registration failed';
      }
    } catch (e: any) {
      saveError = e.message;
    } finally {
      saving = false;
    }
  }

  const hasErrors = $derived(
    clientValidation.errors.length > 0 ||
    (serverValidation && !serverValidation.valid && serverValidation.errors.length > 0)
  );

  const allErrors = $derived([
    ...clientValidation.errors,
    ...(serverValidation?.errors || []),
  ]);

  const allWarnings = $derived([
    ...clientValidation.warnings,
    ...(serverValidation && !serverValidation.valid ? [] : []),
  ]);
</script>

<div class="space-y-4">
  <!-- Header bar -->
  <div class="flex items-center justify-between">
    <div class="flex items-center gap-3">
      <a href="/shadow" class="btn btn-ghost">
        <ArrowLeft size={14} />
        Back
      </a>
      <h1 class="text-lg font-semibold">New Recipe</h1>
    </div>
    <div class="flex items-center gap-2">
      <button class="btn btn-secondary" onclick={() => (estimateOpen = true)}>
        <Wand2 size={14} />
        Estimate from Binary
      </button>
      <button class="btn btn-primary" onclick={saveAndRegister} disabled={saving || hasErrors}>
        {#if saving}
          <Loader size={14} class="animate-spin" />
          Registering...
        {:else}
          <Save size={14} />
          Save & Register
        {/if}
      </button>
    </div>
  </div>

  {#if saveMsg}
    <div class="card p-3 flex items-center gap-2 border-l-4" style="border-left-color: var(--color-status-success);">
      <CheckCircle size={16} style="color: var(--color-status-success);" />
      <span class="text-sm">{saveMsg}</span>
    </div>
  {/if}

  {#if saveError}
    <div class="card p-3 flex items-center gap-2 border-l-4" style="border-left-color: var(--color-status-error);">
      <AlertTriangle size={16} style="color: var(--color-status-error);" />
      <span class="text-sm" style="color: var(--color-status-error);">{saveError}</span>
    </div>
  {/if}

  <!-- Split pane: editor | validation -->
  <div class="grid grid-cols-2 gap-4 min-h-[600px]">
    <!-- Left: TOML editor -->
    <div class="card flex flex-col overflow-hidden">
      <div class="flex items-center justify-between border-b px-4 py-2" style="border-color: var(--color-border-default);">
        <span class="text-xs font-medium uppercase tracking-wide" style="color: var(--color-text-muted);">
          Recipe TOML
        </span>
        <div class="flex items-center gap-2">
          {#if validating}
            <Loader size={12} class="animate-spin" style="color: var(--color-text-muted);" />
            <span class="text-xs" style="color: var(--color-text-muted);">Validating...</span>
          {:else if hasErrors}
            <XCircle size={12} style="color: var(--color-status-error);" />
            <span class="text-xs" style="color: var(--color-status-error);">{allErrors.length} error(s)</span>
          {:else if clientValidation.warnings.length > 0}
            <AlertTriangle size={12} style="color: var(--color-status-warning);" />
            <span class="text-xs" style="color: var(--color-status-warning);">{clientValidation.warnings.length} warning(s)</span>
          {:else if recipe.trim()}
            <CheckCircle size={12} style="color: var(--color-status-success);" />
            <span class="text-xs" style="color: var(--color-status-success);">Valid</span>
          {/if}
        </div>
      </div>

      <!-- Code editor with line numbers -->
      <div class="flex flex-1 overflow-hidden">
        <!-- Line number gutter -->
        <div
          id="line-gutter"
          class="overflow-hidden py-3 px-2 text-right font-mono text-xs select-none"
          style="background-color: var(--color-bg-base); color: var(--color-text-muted); min-width: 2.5rem;"
        >
          {#each lineNumbers as n}
            <div class="leading-6">{n}</div>
          {/each}
        </div>

        <!-- Textarea -->
        <textarea
          bind:this={textareaEl}
          bind:value={recipe}
          oninput={syncScroll}
          class="flex-1 resize-none border-0 py-3 px-3 font-mono text-xs leading-6 focus:outline-none"
          style="background-color: var(--color-bg-base); color: var(--color-text-primary);"
          spellcheck="false"
          autocomplete="off"
          autocapitalize="off"
        ></textarea>
      </div>
    </div>

    <!-- Right: Validation + Assets panel -->
    <div class="space-y-4 overflow-y-auto" style="max-height: 600px;">
      <!-- Validation results -->
      <div class="card p-4 space-y-3">
        <div class="flex items-center gap-2">
          {#if hasErrors}
            <XCircle size={14} style="color: var(--color-status-error);" />
            <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-status-error);">
              Validation Errors
            </span>
          {:else if clientValidation.warnings.length > 0}
            <AlertTriangle size={14} style="color: var(--color-status-warning);" />
            <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-status-warning);">
              Warnings
            </span>
          {:else}
            <CheckCircle size={14} style="color: var(--color-status-success);" />
            <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-status-success);">
              All Checks Passed
            </span>
          {/if}
        </div>

        {#if allErrors.length === 0 && clientValidation.warnings.length === 0}
          <div class="flex items-center gap-2 text-xs" style="color: var(--color-text-muted);">
            <Info size={12} />
            Recipe structure is valid. Ready to register.
          </div>
        {/if}

        {#each allErrors as err}
          <div class="flex items-start gap-2 text-xs">
            <XCircle size={12} class="mt-0.5 shrink-0" style="color: var(--color-status-error);" />
            <div>
              <span class="font-mono font-medium" style="color: var(--color-status-error);">{err.field}</span>
              <span class="ml-1" style="color: var(--color-text-primary);">{err.message}</span>
            </div>
          </div>
        {/each}

        {#each clientValidation.warnings as warn}
          <div class="flex items-start gap-2 text-xs">
            <AlertTriangle size={12} class="mt-0.5 shrink-0" style="color: var(--color-status-warning);" />
            <div>
              <span class="font-mono" style="color: var(--color-status-warning);">{warn.field}</span>
              <span class="ml-1" style="color: var(--color-text-muted);">{warn.message}</span>
            </div>
          </div>
        {/each}
      </div>

      <!-- Asset pattern tester -->
      <div class="card p-4">
        <AssetTester bind:assets={detectedAssets} />
      </div>

      <!-- Recipe reference -->
      <div class="card p-4 space-y-2">
        <div class="flex items-center gap-2">
          <Info size={14} style="color: var(--color-text-muted);" />
          <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-text-muted);">
            Recipe Structure
          </span>
        </div>
        <div class="text-xs space-y-1" style="color: var(--color-text-muted);">
          <div class="flex items-center gap-1">
            <code style="color: var(--color-accent);">[package]</code>
            <HelpTooltip helpKey="section:package" label="[package] section" />
            <span class="ml-1">— name, source_type, source_url</span>
          </div>
          <div class="flex items-center gap-1">
            <code style="color: var(--color-accent);">[install]</code>
            <HelpTooltip helpKey="section:install" label="[install] section" />
            <span class="ml-1">— method, target_dir</span>
          </div>
          <div class="flex items-center gap-1">
            <code style="color: var(--color-accent);">[version]</code>
            <HelpTooltip helpKey="section:version" label="[version] section" />
            <span class="ml-1">— detect_command, detect_pattern</span>
          </div>
          <div class="flex items-center gap-1">
            <code style="color: var(--color-accent);">[asset_selector]</code>
            <HelpTooltip helpKey="section:asset_selector" label="[asset_selector] section" />
            <span class="ml-1">— tag_pattern, rules</span>
          </div>
          <div class="flex items-center gap-1">
            <code style="color: var(--color-accent);">[direct_url]</code>
            <HelpTooltip helpKey="section:direct_url" label="[direct_url] section" />
            <span class="ml-1">— version_discovery, asset_url_template</span>
          </div>
        </div>
      </div>
    </div>
  </div>
</div>

<EstimateModal bind:open={estimateOpen} onEstimate={onEstimate} />
