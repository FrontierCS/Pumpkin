// The page's colour scheme, as the root element's `data-scheme` names it: `system`, the
// browser's setting, or `light` or `dark`, which overrides it. The address's `scheme` wins over the
// choice this browser kept; with neither it is `system`. The style picks each scheme's colours, so
// `system` follows the browser's setting as it changes.
//
// A classic script in the head, which the browser runs before it parses the body, so that the
// first paint is in the scheme chosen, as Starlight sets its scheme before it draws
// (https://github.com/withastro/starlight/blob/5ca12c29226d2d75d904061ca986346579dde1b1/packages/starlight/src/components/ThemeProvider.astro#L1-L11);
// the page's policy refuses an inline script, and a module runs only once the page is parsed.
{
  const SCHEMES = ["system", "light", "dark"];
  /** @type {Record<string, string>} */
  const WORDS = { system: "System", light: "Light", dark: "Dark" };
  const root = document.documentElement;
  // Storage can be refused, as in a private window: the choice is then not kept.
  let kept = "";
  try {
    kept = localStorage.getItem("pumpkin.scheme") ?? "";
  } catch {
    // Nothing kept.
  }
  const asked = new URLSearchParams(location.search).get("scheme") ?? "";
  root.dataset.scheme = SCHEMES.includes(asked) ? asked : SCHEMES.includes(kept) ? kept : "system";
  document.addEventListener("DOMContentLoaded", () => {
    const choice = /** @type {HTMLSelectElement} */ (document.getElementById("scheme"));
    for (const scheme of SCHEMES) choice.add(new Option(WORDS[scheme], scheme));
    choice.value = root.dataset.scheme ?? "system";
    choice.addEventListener("change", () => {
      root.dataset.scheme = choice.value;
      try {
        localStorage.setItem("pumpkin.scheme", choice.value);
      } catch {
        // Not kept: the address still holds it.
      }
      const address = new URL(location.href);
      address.searchParams.set("scheme", choice.value);
      history.replaceState(null, "", address);
    });
  });
}
