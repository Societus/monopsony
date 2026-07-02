import { l as ssr_context, s as sanitize_props, a as spread_props, b as slot, f as attr, m as bind_props, d as ensure_array_like, e as escape_html, k as derived } from "../../../../../chunks/index.js";
import "clsx";
import "@sveltejs/kit/internal";
import "../../../../../chunks/exports.js";
import "../../../../../chunks/utils2.js";
import "@sveltejs/kit/internal/server";
import "../../../../../chunks/root.js";
import "../../../../../chunks/state.svelte.js";
import { I as Icon } from "../../../../../chunks/Icon.js";
import { S as Save } from "../../../../../chunks/save.js";
function onDestroy(fn) {
  /** @type {SSRContext} */
  ssr_context.r.on_destroy(fn);
}
function X($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["path", { "d": "M18 6 6 18" }],
    ["path", { "d": "m6 6 12 12" }]
  ];
  Icon($$renderer, spread_props([
    { name: "x" },
    $$sanitized_props,
    {
      /**
       * @component @name X
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNMTggNiA2IDE4IiAvPgogIDxwYXRoIGQ9Im02IDYgMTIgMTIiIC8+Cjwvc3ZnPgo=) - https://lucide.dev/icons/x
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
function Info($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["circle", { "cx": "12", "cy": "12", "r": "10" }],
    ["path", { "d": "M12 16v-4" }],
    ["path", { "d": "M12 8h.01" }]
  ];
  Icon($$renderer, spread_props([
    { name: "info" },
    $$sanitized_props,
    {
      /**
       * @component @name Info
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8Y2lyY2xlIGN4PSIxMiIgY3k9IjEyIiByPSIxMCIgLz4KICA8cGF0aCBkPSJNMTIgMTZ2LTQiIC8+CiAgPHBhdGggZD0iTTEyIDhoLjAxIiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/info
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
function Triangle_alert($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    [
      "path",
      {
        "d": "m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"
      }
    ],
    ["path", { "d": "M12 9v4" }],
    ["path", { "d": "M12 17h.01" }]
  ];
  Icon($$renderer, spread_props([
    { name: "triangle-alert" },
    $$sanitized_props,
    {
      /**
       * @component @name TriangleAlert
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJtMjEuNzMgMTgtOC0xNGEyIDIgMCAwIDAtMy40OCAwbC04IDE0QTIgMiAwIDAgMCA0IDIxaDE2YTIgMiAwIDAgMCAxLjczLTMiIC8+CiAgPHBhdGggZD0iTTEyIDl2NCIgLz4KICA8cGF0aCBkPSJNMTIgMTdoLjAxIiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/triangle-alert
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
function Circle_check_big($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["path", { "d": "M21.801 10A10 10 0 1 1 17 3.335" }],
    ["path", { "d": "m9 11 3 3L22 4" }]
  ];
  Icon($$renderer, spread_props([
    { name: "circle-check-big" },
    $$sanitized_props,
    {
      /**
       * @component @name CircleCheckBig
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJNMjEuODAxIDEwQTEwIDEwIDAgMSAxIDE3IDMuMzM1IiAvPgogIDxwYXRoIGQ9Im05IDExIDMgM0wyMiA0IiAvPgo8L3N2Zz4K) - https://lucide.dev/icons/circle-check-big
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
const EMPTY_RECIPE = `# Shadow Package Recipe
# Generated by Monopsony Dashboard

[package]
name = ""
source_type = "github_release"   # github_release | git_build | direct_url
source_url = ""

# Uncomment for asset selection by OS/arch:
# [asset_selector]
# tag_pattern = ".*"
# [[asset_selector.rules]]
# os = "linux"
# arch = "amd64"
# pattern = ""

[install]
method = "extract"   # extract | build | script
target_dir = ""

[version]
detect_command = ""
detect_pattern = ""
`;
function Wand_sparkles($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    [
      "path",
      {
        "d": "m21.64 3.64-1.28-1.28a1.21 1.21 0 0 0-1.72 0L2.36 18.64a1.21 1.21 0 0 0 0 1.72l1.28 1.28a1.2 1.2 0 0 0 1.72 0L21.64 5.36a1.2 1.2 0 0 0 0-1.72"
      }
    ],
    ["path", { "d": "m14 7 3 3" }],
    ["path", { "d": "M5 6v4" }],
    ["path", { "d": "M19 14v4" }],
    ["path", { "d": "M10 2v2" }],
    ["path", { "d": "M7 8H3" }],
    ["path", { "d": "M21 16h-4" }],
    ["path", { "d": "M11 3H9" }]
  ];
  Icon($$renderer, spread_props([
    { name: "wand-sparkles" },
    $$sanitized_props,
    {
      /**
       * @component @name WandSparkles
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJtMjEuNjQgMy42NC0xLjI4LTEuMjhhMS4yMSAxLjIxIDAgMCAwLTEuNzIgMEwyLjM2IDE4LjY0YTEuMjEgMS4yMSAwIDAgMCAwIDEuNzJsMS4yOCAxLjI4YTEuMiAxLjIgMCAwIDAgMS43MiAwTDIxLjY0IDUuMzZhMS4yIDEuMiAwIDAgMCAwLTEuNzIiIC8+CiAgPHBhdGggZD0ibTE0IDcgMyAzIiAvPgogIDxwYXRoIGQ9Ik01IDZ2NCIgLz4KICA8cGF0aCBkPSJNMTkgMTR2NCIgLz4KICA8cGF0aCBkPSJNMTAgMnYyIiAvPgogIDxwYXRoIGQ9Ik03IDhIMyIgLz4KICA8cGF0aCBkPSJNMjEgMTZoLTQiIC8+CiAgPHBhdGggZD0iTTExIDNIOSIgLz4KPC9zdmc+Cg==) - https://lucide.dev/icons/wand-sparkles
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
function EstimateModal($$renderer, $$props) {
  $$renderer.component(($$renderer2) => {
    let { open = void 0, onEstimate = (result) => {
    } } = $$props;
    let binaryPath = "";
    let sourceUrl = "";
    let loading = false;
    if (open) {
      $$renderer2.push("<!--[0-->");
      $$renderer2.push(`<div class="fixed inset-0 z-50 flex items-center justify-center" style="background-color: rgba(0, 0, 0, 0.6);" role="presentation"><div class="card w-full max-w-lg p-6 space-y-4" style="background-color: var(--color-bg-surface);" role="dialog" aria-modal="true"><div class="flex items-center justify-between"><div class="flex items-center gap-2">`);
      Wand_sparkles($$renderer2, { size: 18, style: "color: var(--color-accent);" });
      $$renderer2.push(`<!----> <h2 class="text-sm font-semibold">Estimate Recipe from Binary</h2></div> <button class="btn-ghost rounded p-1" aria-label="Close">`);
      X($$renderer2, { size: 16 });
      $$renderer2.push(`<!----></button></div> <p class="text-xs" style="color: var(--color-text-muted);">Enter the path to a binary on a client system. The server will inspect it to detect
        name, version, architecture, and source type, then generate a recipe template.</p> `);
      {
        $$renderer2.push("<!--[-1-->");
      }
      $$renderer2.push(`<!--]--> `);
      {
        $$renderer2.push("<!--[0-->");
        $$renderer2.push(`<div class="space-y-3"><div class="space-y-1.5"><label class="text-xs font-medium" style="color: var(--color-text-muted);" for="binpath">Binary path <span style="color: var(--color-status-error);">*</span></label> <input id="binpath" class="input font-mono text-xs"${attr("value", binaryPath)} placeholder="/usr/local/bin/myapp"/></div> <div class="space-y-1.5"><label class="text-xs font-medium" style="color: var(--color-text-muted);" for="srcurl">Source URL <span style="color: var(--color-text-muted);">(optional)</span></label> <input id="srcurl" class="input font-mono text-xs"${attr("value", sourceUrl)} placeholder="https://github.com/owner/repo"/> <p class="text-xs" style="color: var(--color-text-muted);">Providing a GitHub URL enables asset discovery from releases.</p></div></div> <div class="flex justify-end gap-2 pt-2"><button class="btn btn-secondary">Cancel</button> <button class="btn btn-primary"${attr("disabled", loading, true)}>`);
        {
          $$renderer2.push("<!--[-1-->");
          Wand_sparkles($$renderer2, { size: 14 });
          $$renderer2.push(`<!----> Estimate`);
        }
        $$renderer2.push(`<!--]--></button></div>`);
      }
      $$renderer2.push(`<!--]--></div></div>`);
    } else {
      $$renderer2.push("<!--[-1-->");
    }
    $$renderer2.push(`<!--]-->`);
    bind_props($$props, { open });
  });
}
function Circle_x($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["circle", { "cx": "12", "cy": "12", "r": "10" }],
    ["path", { "d": "m15 9-6 6" }],
    ["path", { "d": "m9 9 6 6" }]
  ];
  Icon($$renderer, spread_props([
    { name: "circle-x" },
    $$sanitized_props,
    {
      /**
       * @component @name CircleX
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8Y2lyY2xlIGN4PSIxMiIgY3k9IjEyIiByPSIxMCIgLz4KICA8cGF0aCBkPSJtMTUgOS02IDYiIC8+CiAgPHBhdGggZD0ibTkgOSA2IDYiIC8+Cjwvc3ZnPgo=) - https://lucide.dev/icons/circle-x
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
function Terminal($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["polyline", { "points": "4 17 10 11 4 5" }],
    ["line", { "x1": "12", "x2": "20", "y1": "19", "y2": "19" }]
  ];
  Icon($$renderer, spread_props([
    { name: "terminal" },
    $$sanitized_props,
    {
      /**
       * @component @name Terminal
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cG9seWxpbmUgcG9pbnRzPSI0IDE3IDEwIDExIDQgNSIgLz4KICA8bGluZSB4MT0iMTIiIHgyPSIyMCIgeTE9IjE5IiB5Mj0iMTkiIC8+Cjwvc3ZnPgo=) - https://lucide.dev/icons/terminal
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
function AssetTester($$renderer, $$props) {
  $$renderer.component(($$renderer2) => {
    let { assets = [] } = $$props;
    let pattern = "";
    const ruleResults = derived(() => assets.map((a) => {
      return { asset: a, matches: null };
    }));
    $$renderer2.push(`<div class="space-y-3"><div class="flex items-center gap-2">`);
    Terminal($$renderer2, { size: 14, style: "color: var(--color-accent);" });
    $$renderer2.push(`<!----> <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-text-muted);">Asset Pattern Tester</span></div> `);
    if (assets.length === 0) {
      $$renderer2.push("<!--[0-->");
      $$renderer2.push(`<p class="text-xs" style="color: var(--color-text-muted);">No assets detected. Run recipe estimation with a source URL to discover available release assets.</p>`);
    } else {
      $$renderer2.push("<!--[-1-->");
      $$renderer2.push(`<div class="space-y-1.5"><label class="text-xs" style="color: var(--color-text-muted);" for="pattern">Rule pattern (regex)</label> <input id="pattern" class="input font-mono text-xs"${attr("value", pattern)} placeholder="linux.*amd64"/> `);
      {
        $$renderer2.push("<!--[-1-->");
      }
      $$renderer2.push(`<!--]--></div> <div class="space-y-1 max-h-40 overflow-y-auto"><!--[-->`);
      const each_array = ensure_array_like(ruleResults());
      for (let $$index = 0, $$length = each_array.length; $$index < $$length; $$index++) {
        let r = each_array[$$index];
        $$renderer2.push(`<div class="flex items-center gap-2 text-xs font-mono px-2 py-0.5 rounded" style="background-color: var(--color-bg-surface-2);">`);
        if (r.matches === true) {
          $$renderer2.push("<!--[0-->");
          Circle_check_big($$renderer2, { size: 12, style: "color: var(--color-status-success);" });
          $$renderer2.push(`<!----> <span style="color: var(--color-status-success);">${escape_html(r.asset)}</span>`);
        } else if (r.matches === false) {
          $$renderer2.push("<!--[1-->");
          Circle_x($$renderer2, {
            size: 12,
            style: "color: var(--color-text-muted); opacity: 0.5;"
          });
          $$renderer2.push(`<!----> <span style="color: var(--color-text-muted); opacity: 0.5;">${escape_html(r.asset)}</span>`);
        } else {
          $$renderer2.push("<!--[-1-->");
          $$renderer2.push(`<span class="w-3 h-3 rounded-full" style="border: 1px solid var(--color-border-default);"></span> <span style="color: var(--color-text-muted);">${escape_html(r.asset)}</span>`);
        }
        $$renderer2.push(`<!--]--></div>`);
      }
      $$renderer2.push(`<!--]--></div>`);
    }
    $$renderer2.push(`<!--]--></div>`);
    bind_props($$props, { assets });
  });
}
function Arrow_left($$renderer, $$props) {
  const $$sanitized_props = sanitize_props($$props);
  /**
   * @license lucide-svelte v0.460.1 - ISC
   *
   * This source code is licensed under the ISC license.
   * See the LICENSE file in the root directory of this source tree.
   */
  const iconNode = [
    ["path", { "d": "m12 19-7-7 7-7" }],
    ["path", { "d": "M19 12H5" }]
  ];
  Icon($$renderer, spread_props([
    { name: "arrow-left" },
    $$sanitized_props,
    {
      /**
       * @component @name ArrowLeft
       * @description Lucide SVG icon component, renders SVG Element with children.
       *
       * @preview ![img](data:image/svg+xml;base64,PHN2ZyAgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIgogIHdpZHRoPSIyNCIKICBoZWlnaHQ9IjI0IgogIHZpZXdCb3g9IjAgMCAyNCAyNCIKICBmaWxsPSJub25lIgogIHN0cm9rZT0iIzAwMCIgc3R5bGU9ImJhY2tncm91bmQtY29sb3I6ICNmZmY7IGJvcmRlci1yYWRpdXM6IDJweCIKICBzdHJva2Utd2lkdGg9IjIiCiAgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIgogIHN0cm9rZS1saW5lam9pbj0icm91bmQiCj4KICA8cGF0aCBkPSJtMTIgMTktNy03IDctNyIgLz4KICA8cGF0aCBkPSJNMTkgMTJINSIgLz4KPC9zdmc+Cg==) - https://lucide.dev/icons/arrow-left
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
    let recipe = EMPTY_RECIPE;
    let clientValidation = { errors: [], warnings: [] };
    let serverValidation = null;
    let saveMsg = "";
    let saveError = "";
    let estimateOpen = false;
    let detectedAssets = [];
    let lineCount = derived(() => recipe.split("\n").length);
    let lineNumbers = derived(() => Array.from({ length: lineCount() }, (_, i) => i + 1));
    onDestroy(() => {
    });
    function onEstimate(result) {
      recipe = result.recipe_toml;
      detectedAssets = result.detected.available_assets;
      saveMsg = "";
      saveError = "";
    }
    const hasErrors = derived(() => clientValidation.errors.length > 0 || serverValidation);
    const allErrors = derived(() => [
      ...clientValidation.errors,
      ...[]
    ]);
    let $$settled = true;
    let $$inner_renderer;
    function $$render_inner($$renderer3) {
      $$renderer3.push(`<div class="space-y-4"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><a href="/shadow" class="btn btn-ghost">`);
      Arrow_left($$renderer3, { size: 14 });
      $$renderer3.push(`<!----> Back</a> <h1 class="text-lg font-semibold">New Recipe</h1></div> <div class="flex items-center gap-2"><button class="btn btn-secondary">`);
      Wand_sparkles($$renderer3, { size: 14 });
      $$renderer3.push(`<!----> Estimate from Binary</button> <button class="btn btn-primary"${attr("disabled", hasErrors(), true)}>`);
      {
        $$renderer3.push("<!--[-1-->");
        Save($$renderer3, { size: 14 });
        $$renderer3.push(`<!----> Save &amp; Register`);
      }
      $$renderer3.push(`<!--]--></button></div></div> `);
      if (saveMsg) {
        $$renderer3.push("<!--[0-->");
        $$renderer3.push(`<div class="card p-3 flex items-center gap-2 border-l-4" style="border-left-color: var(--color-status-success);">`);
        Circle_check_big($$renderer3, { size: 16, style: "color: var(--color-status-success);" });
        $$renderer3.push(`<!----> <span class="text-sm">${escape_html(saveMsg)}</span></div>`);
      } else {
        $$renderer3.push("<!--[-1-->");
      }
      $$renderer3.push(`<!--]--> `);
      if (saveError) {
        $$renderer3.push("<!--[0-->");
        $$renderer3.push(`<div class="card p-3 flex items-center gap-2 border-l-4" style="border-left-color: var(--color-status-error);">`);
        Triangle_alert($$renderer3, { size: 16, style: "color: var(--color-status-error);" });
        $$renderer3.push(`<!----> <span class="text-sm" style="color: var(--color-status-error);">${escape_html(saveError)}</span></div>`);
      } else {
        $$renderer3.push("<!--[-1-->");
      }
      $$renderer3.push(`<!--]--> <div class="grid grid-cols-2 gap-4 min-h-[600px]"><div class="card flex flex-col overflow-hidden"><div class="flex items-center justify-between border-b px-4 py-2" style="border-color: var(--color-border-default);"><span class="text-xs font-medium uppercase tracking-wide" style="color: var(--color-text-muted);">Recipe TOML</span> <div class="flex items-center gap-2">`);
      if (hasErrors()) {
        $$renderer3.push("<!--[1-->");
        Circle_x($$renderer3, { size: 12, style: "color: var(--color-status-error);" });
        $$renderer3.push(`<!----> <span class="text-xs" style="color: var(--color-status-error);">${escape_html(allErrors().length)} error(s)</span>`);
      } else if (clientValidation.warnings.length > 0) {
        $$renderer3.push("<!--[2-->");
        Triangle_alert($$renderer3, { size: 12, style: "color: var(--color-status-warning);" });
        $$renderer3.push(`<!----> <span class="text-xs" style="color: var(--color-status-warning);">${escape_html(clientValidation.warnings.length)} warning(s)</span>`);
      } else if (recipe.trim()) {
        $$renderer3.push("<!--[3-->");
        Circle_check_big($$renderer3, { size: 12, style: "color: var(--color-status-success);" });
        $$renderer3.push(`<!----> <span class="text-xs" style="color: var(--color-status-success);">Valid</span>`);
      } else {
        $$renderer3.push("<!--[-1-->");
      }
      $$renderer3.push(`<!--]--></div></div> <div class="flex flex-1 overflow-hidden"><div id="line-gutter" class="overflow-hidden py-3 px-2 text-right font-mono text-xs select-none" style="background-color: var(--color-bg-base); color: var(--color-text-muted); min-width: 2.5rem;"><!--[-->`);
      const each_array = ensure_array_like(lineNumbers());
      for (let $$index = 0, $$length = each_array.length; $$index < $$length; $$index++) {
        let n = each_array[$$index];
        $$renderer3.push(`<div class="leading-6">${escape_html(n)}</div>`);
      }
      $$renderer3.push(`<!--]--></div> <textarea class="flex-1 resize-none border-0 py-3 px-3 font-mono text-xs leading-6 focus:outline-none" style="background-color: var(--color-bg-base); color: var(--color-text-primary);" spellcheck="false" autocomplete="off" autocorrect="off" autocapitalize="off">`);
      const $$body = escape_html(recipe);
      if ($$body) {
        $$renderer3.push(`${$$body}`);
      }
      $$renderer3.push(`</textarea></div></div> <div class="space-y-4 overflow-y-auto" style="max-height: 600px;"><div class="card p-4 space-y-3"><div class="flex items-center gap-2">`);
      if (hasErrors()) {
        $$renderer3.push("<!--[0-->");
        Circle_x($$renderer3, { size: 14, style: "color: var(--color-status-error);" });
        $$renderer3.push(`<!----> <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-status-error);">Validation Errors</span>`);
      } else if (clientValidation.warnings.length > 0) {
        $$renderer3.push("<!--[1-->");
        Triangle_alert($$renderer3, { size: 14, style: "color: var(--color-status-warning);" });
        $$renderer3.push(`<!----> <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-status-warning);">Warnings</span>`);
      } else {
        $$renderer3.push("<!--[-1-->");
        Circle_check_big($$renderer3, { size: 14, style: "color: var(--color-status-success);" });
        $$renderer3.push(`<!----> <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-status-success);">All Checks Passed</span>`);
      }
      $$renderer3.push(`<!--]--></div> `);
      if (allErrors().length === 0 && clientValidation.warnings.length === 0) {
        $$renderer3.push("<!--[0-->");
        $$renderer3.push(`<div class="flex items-center gap-2 text-xs" style="color: var(--color-text-muted);">`);
        Info($$renderer3, { size: 12 });
        $$renderer3.push(`<!----> Recipe structure is valid. Ready to register.</div>`);
      } else {
        $$renderer3.push("<!--[-1-->");
      }
      $$renderer3.push(`<!--]--> <!--[-->`);
      const each_array_1 = ensure_array_like(allErrors());
      for (let $$index_1 = 0, $$length = each_array_1.length; $$index_1 < $$length; $$index_1++) {
        let err = each_array_1[$$index_1];
        $$renderer3.push(`<div class="flex items-start gap-2 text-xs">`);
        Circle_x($$renderer3, {
          size: 12,
          class: "mt-0.5 shrink-0",
          style: "color: var(--color-status-error);"
        });
        $$renderer3.push(`<!----> <div><span class="font-mono font-medium" style="color: var(--color-status-error);">${escape_html(err.field)}</span> <span class="ml-1" style="color: var(--color-text-primary);">${escape_html(err.message)}</span></div></div>`);
      }
      $$renderer3.push(`<!--]--> <!--[-->`);
      const each_array_2 = ensure_array_like(clientValidation.warnings);
      for (let $$index_2 = 0, $$length = each_array_2.length; $$index_2 < $$length; $$index_2++) {
        let warn = each_array_2[$$index_2];
        $$renderer3.push(`<div class="flex items-start gap-2 text-xs">`);
        Triangle_alert($$renderer3, {
          size: 12,
          class: "mt-0.5 shrink-0",
          style: "color: var(--color-status-warning);"
        });
        $$renderer3.push(`<!----> <div><span class="font-mono" style="color: var(--color-status-warning);">${escape_html(warn.field)}</span> <span class="ml-1" style="color: var(--color-text-muted);">${escape_html(warn.message)}</span></div></div>`);
      }
      $$renderer3.push(`<!--]--></div> <div class="card p-4">`);
      AssetTester($$renderer3, {
        get assets() {
          return detectedAssets;
        },
        set assets($$value) {
          detectedAssets = $$value;
          $$settled = false;
        }
      });
      $$renderer3.push(`<!----></div> <div class="card p-4 space-y-2"><div class="flex items-center gap-2">`);
      Info($$renderer3, { size: 14, style: "color: var(--color-text-muted);" });
      $$renderer3.push(`<!----> <span class="text-xs font-semibold uppercase tracking-wide" style="color: var(--color-text-muted);">Recipe Structure</span></div> <div class="text-xs space-y-1" style="color: var(--color-text-muted);"><div><code style="color: var(--color-accent);">[package]</code> — name, source_type, source_url</div> <div><code style="color: var(--color-accent);">[install]</code> — method, target_dir</div> <div><code style="color: var(--color-accent);">[version]</code> — detect_command, detect_pattern</div> <div><code style="color: var(--color-accent);">[asset_selector]</code> — tag_pattern, rules</div> <div><code style="color: var(--color-accent);">[direct_url]</code> — version_discovery, asset_url_template</div></div></div></div></div></div> `);
      EstimateModal($$renderer3, {
        onEstimate,
        get open() {
          return estimateOpen;
        },
        set open($$value) {
          estimateOpen = $$value;
          $$settled = false;
        }
      });
      $$renderer3.push(`<!---->`);
    }
    do {
      $$settled = true;
      $$inner_renderer = $$renderer2.copy();
      $$render_inner($$inner_renderer);
    } while (!$$settled);
    $$renderer2.subsume($$inner_renderer);
  });
}
export {
  _page as default
};
