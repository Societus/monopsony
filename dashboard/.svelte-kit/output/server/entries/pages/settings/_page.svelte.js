import { f as attr, e as escape_html, k as derived } from "../../../chunks/index.js";
import "@sveltejs/kit/internal";
import "../../../chunks/exports.js";
import "../../../chunks/utils2.js";
import "@sveltejs/kit/internal/server";
import "../../../chunks/root.js";
import "../../../chunks/state.svelte.js";
import { R as Refresh_cw } from "../../../chunks/refresh-cw.js";
import { S as Save } from "../../../chunks/save.js";
function _page($$renderer, $$props) {
  $$renderer.component(($$renderer2) => {
    let editedToml = "";
    let loading = true;
    let saving = false;
    const isDirty = derived(() => editedToml !== "");
    $$renderer2.push(`<div class="space-y-6"><div class="flex items-center justify-between"><h1 class="text-xl font-semibold">Settings</h1> <div class="flex gap-2"><button class="btn btn-ghost"${attr("disabled", loading, true)}>`);
    Refresh_cw($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> Reset</button> <button class="btn btn-primary"${attr("disabled", !isDirty() || saving, true)}>`);
    Save($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> ${escape_html("Save")}</button></div></div> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--> `);
    {
      $$renderer2.push("<!--[0-->");
      $$renderer2.push(`<div class="text-sm" style="color: var(--color-text-muted);">Loading config...</div>`);
    }
    $$renderer2.push(`<!--]--></div>`);
  });
}
export {
  _page as default
};
