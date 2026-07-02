import "clsx";
import "@sveltejs/kit/internal";
import "../../../chunks/exports.js";
import "../../../chunks/utils2.js";
import "@sveltejs/kit/internal/server";
import "../../../chunks/root.js";
import "../../../chunks/state.svelte.js";
import { B as Boxes } from "../../../chunks/boxes.js";
function _page($$renderer, $$props) {
  $$renderer.component(($$renderer2) => {
    $$renderer2.push(`<div class="flex min-h-screen items-center justify-center" style="background-color: var(--color-bg-base);"><div class="w-full max-w-sm space-y-6"><div class="text-center"><div class="inline-flex items-center gap-2.5">`);
    Boxes($$renderer2, { size: 28, color: "var(--color-accent)" });
    $$renderer2.push(`<!----> <span class="text-lg font-semibold">Monopsony</span></div> <p class="mt-2 text-xs" style="color: var(--color-text-muted);">Dashboard Login</p></div> `);
    {
      $$renderer2.push("<!--[0-->");
      $$renderer2.push(`<div class="text-center text-sm" style="color: var(--color-text-muted);">Checking access...</div>`);
    }
    $$renderer2.push(`<!--]--></div></div>`);
  });
}
export {
  _page as default
};
