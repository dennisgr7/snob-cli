/**
 * The page's one polite live region (BaseLayout), so a screen reader hears
 * "Copied" without a region per button.
 */
export function announce(message: string): void {
  const region = document.getElementById('announcer');
  if (!region) return;
  // Emptied first, so the same message twice is announced twice.
  region.textContent = '';
  requestAnimationFrame(() => {
    region.textContent = message;
  });
}
