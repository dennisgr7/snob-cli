/** Every page the site serves, plus one that does not exist. */
export const PAGES = ['/', '/design-system/'] as const;
export const MISSING = '/this-page-does-not-exist/';
export const ALL_ROUTES = [...PAGES, MISSING] as const;
