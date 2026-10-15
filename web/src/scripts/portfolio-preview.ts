/**
 * The live preview on the author's portfolio (dennisgr7.dev), which shows this
 * page in an iframe on its Snob CLI entry. The portfolio keeps its static
 * screenshot until the framed page says it has rendered, with this one message;
 * the type is the portfolio's protocol, named after the first site to use it.
 * The portfolio also says which theme it shows, light or dark: in the URL
 * (`?preview-theme=`) so the first paint matches, and in a message when the
 * visitor switches it. Inside an iframe prefers-color-scheme is the system's,
 * so without this the preview ignored the portfolio's theme toggle.
 *
 * Inert everywhere else: on its own tab, in any other frame, and when the
 * referrer is not exactly that origin. `frame-ancestors` in `public/_headers`
 * is what lets the portfolio frame the page at all; the two name the same
 * origin and change together.
 */
const PORTFOLIO = 'https://dennisgr7.dev';

function framedByThePortfolio(): boolean {
  if (window.parent === window || !document.referrer) return false;
  try {
    return new URL(document.referrer).origin === PORTFOLIO;
  } catch {
    return false;
  }
}

type Theme = 'light' | 'dark';

const isTheme = (value: unknown): value is Theme => value === 'light' || value === 'dark';

function applyTheme(theme: unknown) {
  if (isTheme(theme)) document.documentElement.dataset.theme = theme;
}

if (framedByThePortfolio()) {
  applyTheme(new URLSearchParams(location.search).get('preview-theme'));
  window.addEventListener('message', (event) => {
    if (event.origin !== PORTFOLIO || event.source !== window.parent) return;
    if (event.data?.type === 'chesey:preview-theme') applyTheme(event.data.theme);
  });

  const ready = () => window.parent.postMessage({ type: 'chesey:preview-ready' }, PORTFOLIO);
  if (document.readyState === 'complete') ready();
  else window.addEventListener('load', ready, { once: true });
}
