import { SITE } from '@/lib/site';
import { VERSION } from '@/lib/version';

type Node = Record<string, unknown>;

const websiteId = `${SITE.url}/#website`;
const softwareId = `${SITE.url}/#software`;

/** The nodes every page carries, so a reference to them never dangles. */
function baseNodes(): Node[] {
  return [
    {
      '@type': 'WebSite',
      '@id': websiteId,
      url: `${SITE.url}/`,
      name: SITE.name,
      description: SITE.description,
      inLanguage: SITE.locale,
      about: { '@id': softwareId },
    },
    {
      '@type': 'SoftwareApplication',
      '@id': softwareId,
      name: SITE.name,
      alternateName: SITE.category,
      description: SITE.description,
      applicationCategory: 'UtilitiesApplication',
      operatingSystem: 'Windows, macOS, Linux',
      softwareVersion: VERSION,
      url: `${SITE.url}/`,
      downloadUrl: `${SITE.repository}/releases/latest`,
      codeRepository: SITE.repository,
      license: SITE.license,
      isAccessibleForFree: true,
      offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' },
    },
  ];
}

/**
 * One `@graph` per page. `<` is escaped so no string in the graph can close
 * the `<script>` it is written into.
 */
export function jsonLd(pageUrl: URL, extra: Node[] = []): string {
  const graph = {
    '@context': 'https://schema.org',
    '@graph': [
      ...baseNodes(),
      {
        '@type': 'WebPage',
        '@id': pageUrl.href,
        url: pageUrl.href,
        isPartOf: { '@id': websiteId },
        inLanguage: SITE.locale,
      },
      ...extra,
    ],
  };
  return JSON.stringify(graph).replaceAll('<', '\u003c');
}
