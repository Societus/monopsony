import "clsx";
import { S as Shield } from "../../../chunks/shield.js";
function _page($$renderer) {
  $$renderer.push(`<div class="space-y-6"><h1 class="text-xl font-semibold">Security</h1> <div class="card p-12 text-center">`);
  Shield($$renderer, {
    size: 32,
    class: "mx-auto mb-3",
    style: "color: var(--color-text-muted);"
  });
  $$renderer.push(`<!----> <p class="text-sm" style="color: var(--color-text-muted);">Security dashboard coming in Phase 4</p> <p class="mt-1 text-xs" style="color: var(--color-text-muted);">TLS certificates, scan results, CVE alerts, and quarantine management</p></div></div>`);
}
export {
  _page as default
};
