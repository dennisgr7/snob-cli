import {
  AGENTS,
  BEHAVIOR,
  FEATURES,
  FEATURES_SECTION,
  HERO,
  HERO_SAMPLE,
  INSTALL,
  INSTALL_SECTION,
  type InstallBlock,
  type Sample,
} from '@/lib/content';
import { SITE } from '@/lib/site';
import { plainLine } from '@/lib/terminal';

/**
 * The home page as Markdown, for agents: the same words, from the same data,
 * in the same order. Lines marked as decoration on the page are left out here
 * too.
 */

const block = (lines: Sample): string =>
  ['```', ...lines.filter((line) => !('art' in line && line.art)).map(plainLine), '```'].join('\n');

const heading = (title: string, accent: string): string => `${title} ${accent}`;

export function homeToMarkdown(site: URL | string): string {
  const url = new URL('/', site).href;

  const features = FEATURES.map(
    (feature) => `### snob ${feature.name}\n\n${feature.description}\n\n${block(feature.sample)}`,
  ).join('\n\n');

  const install = INSTALL.map((method) => {
    const link = 'link' in method && method.link ? ` [${method.link.label}](${method.link.href})` : '';
    const blocks = method.blocks
      .map((b: InstallBlock) => `${b.label ? `${b.label}:\n\n` : ''}${block(b.commands.map((command) => ({ command })))}`)
      .join('\n\n');
    return `### ${method.name}\n\n${method.note}${link}\n\n${blocks}`;
  }).join('\n\n');

  const flags = AGENTS.flags.map((item) => `- \`${item.flag}\`: ${item.means}`).join('\n');
  const exits = AGENTS.exitCodes.map((item) => `- \`${item.code}\`: ${item.means}`).join('\n');

  return `# ${SITE.name}

## ${HERO.title} ${HERO.accent}

${HERO.lead}

${block(HERO_SAMPLE)}

## ${heading(FEATURES_SECTION.title, FEATURES_SECTION.accent)}

${FEATURES_SECTION.lead}

${features}

## ${heading(BEHAVIOR.title, BEHAVIOR.accent)}

${BEHAVIOR.lead}

> ${BEHAVIOR.honest}

${block(BEHAVIOR.checks)}

## ${heading(AGENTS.title, AGENTS.accent)}

${AGENTS.lead}

On a terminal, the view (its frame left out):

${block(AGENTS.human)}

Down a pipe:

${block(AGENTS.machine)}

### Flags for scripts

${flags}

### Exit codes

${exits}

## ${heading(INSTALL_SECTION.title, INSTALL_SECTION.accent)}

${INSTALL_SECTION.lead}

${install}

---

Source: ${SITE.repository} · License: MIT · Not affiliated with Instagram or Meta.

HTML version: ${url}
`;
}
