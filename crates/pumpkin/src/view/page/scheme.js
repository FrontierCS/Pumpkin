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
  const WORDS = { system: "the browser's setting", light: "light", dark: "dark" };
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
    // One button, whose icon is the scheme chosen (the style shows it by `data-scheme`); a press
    // takes the next of the three.
    const choice = /** @type {HTMLButtonElement} */ (document.getElementById("scheme"));
    const name = () => {
      const now = root.dataset.scheme ?? "system";
      const next = SCHEMES[(SCHEMES.indexOf(now) + 1) % SCHEMES.length];
      choice.title = "The page's colours: " + WORDS[now] + ". Press for " + WORDS[next] + ".";
      choice.setAttribute("aria-label", choice.title);
    };
    name();
    choice.addEventListener("click", () => {
      const chosen = SCHEMES[(SCHEMES.indexOf(root.dataset.scheme ?? "system") + 1) % SCHEMES.length];
      root.dataset.scheme = chosen;
      name();
      try {
        localStorage.setItem("pumpkin.scheme", chosen);
      } catch {
        // Not kept: the address still holds it.
      }
      const address = new URL(location.href);
      address.searchParams.set("scheme", chosen);
      history.replaceState(null, "", address);
    });
  });
}
