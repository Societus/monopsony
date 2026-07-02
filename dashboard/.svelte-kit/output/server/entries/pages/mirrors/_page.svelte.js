import { f as attr, e as escape_html } from "../../../chunks/index.js";
import "@sveltejs/kit/internal";
import "../../../chunks/exports.js";
import "../../../chunks/utils2.js";
import "@sveltejs/kit/internal/server";
import "../../../chunks/root.js";
import "../../../chunks/state.svelte.js";
import { R as Refresh_cw } from "../../../chunks/refresh-cw.js";
import { P as Plus } from "../../../chunks/plus.js";
import { S as Save } from "../../../chunks/save.js";
function _page($$renderer, $$props) {
  $$renderer.component(($$renderer2) => {
    let shadowEnabled = false;
    let storageQuota = false;
    let bandwidthQuota = false;
    let trustMode = "open";
    let savingFeatures = false;
    $$renderer2.push(`<div class="space-y-6"><div class="flex items-center justify-between"><h1 class="text-xl font-semibold">Mirrors</h1> <div class="flex gap-2"><button class="btn btn-ghost">`);
    Refresh_cw($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> Refresh</button> <button class="btn btn-primary">`);
    Plus($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> Add Mirror</button></div></div> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--> <div class="card overflow-hidden">`);
    {
      $$renderer2.push("<!--[0-->");
      $$renderer2.push(`<div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>`);
    }
    $$renderer2.push(`<!--]--></div> <div class="card p-5 space-y-4"><div class="flex items-center justify-between"><h2 class="text-sm font-semibold">Feature Toggles</h2> <button class="btn btn-primary"${attr("disabled", savingFeatures, true)}>`);
    Save($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> ${escape_html("Save")}</button></div> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--> <div class="grid grid-cols-2 gap-4"><label class="flex items-center gap-3 rounded p-3" style="background-color: var(--color-bg-surface-2);"><input type="checkbox"${attr("checked", shadowEnabled, true)} class="w-4 h-4 accent-[var(--color-accent)]"/> <div><div class="text-sm font-medium">Shadow Packages</div> <div class="text-xs" style="color: var(--color-text-muted);">Enable shadow package distribution</div></div></label> <div class="rounded p-3" style="background-color: var(--color-bg-surface-2);"><div class="text-sm font-medium mb-1">Client Trust Mode</div> `);
    $$renderer2.select({ class: "input text-xs", value: trustMode }, ($$renderer3) => {
      $$renderer3.option({ value: "open" }, ($$renderer4) => {
        $$renderer4.push(`Open (no auth)`);
      });
      $$renderer3.option({ value: "token" }, ($$renderer4) => {
        $$renderer4.push(`Token`);
      });
      $$renderer3.option({ value: "jwt" }, ($$renderer4) => {
        $$renderer4.push(`JWT`);
      });
      $$renderer3.option({ value: "mtls" }, ($$renderer4) => {
        $$renderer4.push(`mTLS`);
      });
    });
    $$renderer2.push(`</div> <div class="rounded p-3" style="background-color: var(--color-bg-surface-2);"><label class="flex items-center gap-2 mb-2"><input type="checkbox"${attr("checked", storageQuota, true)} class="w-4 h-4 accent-[var(--color-accent)]"/> <span class="text-sm font-medium">Storage Quota</span></label> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--></div> <div class="rounded p-3" style="background-color: var(--color-bg-surface-2);"><label class="flex items-center gap-2 mb-2"><input type="checkbox"${attr("checked", bandwidthQuota, true)} class="w-4 h-4 accent-[var(--color-accent)]"/> <span class="text-sm font-medium">Bandwidth Quota</span></label> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--></div></div></div></div> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]-->`);
  });
}
export {
  _page as default
};
