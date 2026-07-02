import "clsx";
import "@sveltejs/kit/internal";
import "../../../chunks/exports.js";
import "../../../chunks/utils2.js";
import "@sveltejs/kit/internal/server";
import "../../../chunks/root.js";
import "../../../chunks/state.svelte.js";
import { R as Refresh_cw } from "../../../chunks/refresh-cw.js";
import { P as Plus } from "../../../chunks/plus.js";
import { s as sanitize_props, a as spread_props, b as slot } from "../../../chunks/index.js";
import { I as Icon } from "../../../chunks/Icon.js";
function Search($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["circle", { "cx": "11", "cy": "11", "r": "8" }],
    ["path", { "d": "m21 21-4.3-4.3" }]
  ];
  Icon($$renderer, spread_props([
    { name: "search" },
    $$sanitized_props,
    {
      /**
       * @component @name Search
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8Y2lyY2xlIGN4PSIxMSIgY3k9IjExIiByPSI4IiAvPgogIDxwYXRoIGQ9Im0yMSAyMS00LjMtNC4zIiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/search
       * @see https://lucide.dev/guide/packages/lucide-svelte - Documentation
       *
       * @param {Object} props - Lucide icons props and any valid SVG attribute
       * @returns {FunctionalComponent} Svelte component
       *
       */
      iconNode,
      children: ($$renderer2) => {
        $$renderer2.push(`<!--[-->`);
        slot($$renderer2, $$props, "default", {});
        $$renderer2.push(`<!--]-->`);
      },
      $$slots: { default: true }
    }
  ]));
}
function _page($$renderer, $$props) {
  $$renderer.component(($$renderer2) => {
    $$renderer2.push(`<div class="space-y-6"><div class="flex items-center justify-between"><h1 class="text-xl font-semibold">Shadow Packages</h1> <div class="flex gap-2"><button class="btn btn-ghost">`);
    Refresh_cw($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> Refresh</button> <button class="btn btn-primary">`);
    Plus($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> New Recipe</button></div></div> `);
    {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]--> <div class="card overflow-hidden">`);
    {
      $$renderer2.push("<!--[0-->");
      $$renderer2.push(`<div class="p-8 text-center text-sm" style="color: var(--color-text-muted);">Loading...</div>`);
    }
    $$renderer2.push(`<!--]--></div> <div class="card p-5"><div class="flex items-center justify-between"><div class="flex items-center gap-2">`);
    Search($$renderer2, { size: 16, style: "color: var(--color-accent);" });
    $$renderer2.push(`<!----> <div><h2 class="text-sm font-semibold">Recipe Editor</h2> <p class="text-xs mt-0.5" style="color: var(--color-text-muted);">Interactive TOML editor with live validation and binary estimation</p></div></div> <a href="/shadow/recipes/new" class="btn btn-primary">`);
    Plus($$renderer2, { size: 14 });
    $$renderer2.push(`<!----> New Recipe</a></div></div></div>`);
  });
}
export {
  _page as default
};
