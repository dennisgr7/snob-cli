/**
 * The live preview on the author's portfolio (dennisgr7.dev), which shows this
 * page in an iframe on its Snob CLI entry. The portfolio keeps its static
 * screenshot until the framed page says it has rendered, with this one message;
 * the type is the portfolio's protocol, named after the first site to use it.
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

if (framedByThePortfolio()) {
  const ready = () => window.parent.postMessage({ type: 'chesey:preview-ready' }, PORTFOLIO);
  if (document.readyState === 'complete') ready();
  else window.addEventListener('load', ready, { once: true });
}
