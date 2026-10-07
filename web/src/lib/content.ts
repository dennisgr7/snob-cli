import { SITE } from './site.ts';
import type { TerminalLine } from './terminal.ts';
import { VERSION } from './version.ts';

/**
 * Every word on the page. The HTML, llms.txt and the Markdown twin all read it
 * from here, so a person and an agent read the same thing. Nothing here may
 * promise what the README does not: the commands and their wording come from
 * it and from `--help`.
 */

/** The systems the hero can tell apart. */
export type Platform = 'windows' | 'macos' | 'linux' | 'mobile';

/** Commands run one after the other, copied together. */
export interface InstallBlock {
  /** Which system, when a method has one block per system. */
  label?: string;
  commands: readonly string[];
}

/** A word of a line that links somewhere, such as a package manager's site. */
export interface TextLink {
  text: string;
  href: string;
}

/**
 * The line cut around the first `link.text` in it, so the word can be drawn
 * as a link in place; undefined when there is no link or the word is absent.
 */
export function splitAround(line: string, link: TextLink | undefined): [string, string] | undefined {
  if (!link) return undefined;
  const at = line.indexOf(link.text);
  return at < 0 ? undefined : [line.slice(0, at), line.slice(at + link.text.length)];
}

export interface InstallMethod {
  id: string;
  /** The tab's name. */
  name: string;
  /** Who it is for, one line under the tab. */
  note: string;
  /** The package manager, linked where its name appears in the note and the hero. */
  tool?: TextLink;
  /** Alternatives: each block is complete on its own. */
  blocks: readonly InstallBlock[];
  /** A page the method needs, such as the releases page for the .deb. */
  link?: { label: string; href: string };
}

const RELEASES = `${SITE.repository}/releases/latest`;

export const INSTALL = [
  {
    id: 'homebrew',
    name: 'macOS · Linux',
    note: 'With Homebrew.',
    tool: { text: 'Homebrew', href: 'https://brew.sh/' },
    blocks: [{ commands: [`brew tap dennisgr7/snob ${SITE.repository}`, 'brew install snob'] }],
  },
  {
    id: 'scoop',
    name: 'Windows',
    note: 'With Scoop, on Windows 11.',
    tool: { text: 'Scoop', href: 'https://scoop.sh/' },
    blocks: [{ commands: [`scoop bucket add snob ${SITE.repository}`, 'scoop install snob'] }],
  },
  {
    id: 'deb',
    name: 'Debian · Ubuntu',
    note: 'The .deb for your architecture, downloaded from the releases page.',
    blocks: [
      { label: 'x86_64', commands: [`sudo apt install ./snob-v${VERSION}-x86_64-unknown-linux-musl.deb`] },
      { label: 'ARM64', commands: [`sudo apt install ./snob-v${VERSION}-aarch64-unknown-linux-musl.deb`] },
    ],
    link: { label: 'Releases', href: RELEASES },
  },
  {
    id: 'script',
    name: 'Script',
    note: 'Without a package manager.',
    blocks: [
      {
        label: 'macOS · Linux',
        commands: ['curl -fsSL https://raw.githubusercontent.com/dennisgr7/snob-cli/main/packaging/install.sh | sh'],
      },
      {
        label: 'Windows 11 · PowerShell',
        commands: ['irm https://raw.githubusercontent.com/dennisgr7/snob-cli/main/packaging/install.ps1 | iex'],
      },
    ],
  },
  {
    id: 'source',
    name: 'From source',
    note: 'With Rust and a C compiler.',
    blocks: [{ commands: [`cargo install --locked --git ${SITE.repository} snob-cli`] }],
  },
] as const satisfies readonly InstallMethod[];

type InstallId = (typeof INSTALL)[number]['id'];

const method = (id: InstallId): InstallMethod => INSTALL.find((m) => m.id === id)!;

/**
 * What the hero offers each system. Linux gets the script rather than
 * Homebrew, which many Linux machines do not have. A phone gets no command.
 */
export const HERO_INSTALL: Record<
  Exclude<Platform, 'mobile'>,
  { label: string; commands: readonly string[]; tool?: TextLink | undefined }
> = {
  macos: { label: 'macOS · Homebrew', commands: method('homebrew').blocks[0]!.commands, tool: method('homebrew').tool },
  windows: { label: 'Windows 11 · Scoop', commands: method('scoop').blocks[0]!.commands, tool: method('scoop').tool },
  linux: { label: 'Linux · install script', commands: method('script').blocks[0]!.commands },
};

/** The install tab selected first for each system, matching the hero. */
export const HERO_TAB: Record<Platform, InstallId> = {
  macos: 'homebrew',
  windows: 'scoop',
  linux: 'script',
  mobile: 'homebrew',
};

/** README, under "Install", word for word. Windows 10 is not supported. */
export const SUPPORT =
  'Builds exist for Windows 11 and Linux on x86_64 and ARM64, and macOS on Apple Silicon. Everything that talks to Instagram needs a Chromium-based browser installed; no window is ever shown.';

export const ON_A_PHONE = 'Snob runs on your computer: Windows 11, macOS and Linux.';

export interface PlatformBuild {
  name: string;
  /** The architectures built for it; `arm` marks the native ARM build. */
  arches: readonly { name: string; arm?: boolean }[];
}

/** SUPPORT drawn as a table: every system, every build. */
export const PLATFORMS: readonly PlatformBuild[] = [
  { name: 'Windows 11', arches: [{ name: 'x86_64' }, { name: 'ARM64', arm: true }] },
  { name: 'macOS', arches: [{ name: 'Apple Silicon', arm: true }] },
  { name: 'Linux', arches: [{ name: 'x86_64' }, { name: 'ARM64', arm: true }] },
];

/** The ARM builds, said once: each is native, none runs under emulation. */
export const ARM = {
  label: 'ARM ready',
  text: 'A native ARM64 build on every system, nothing emulated: Apple Silicon Macs, Windows 11 on ARM laptops, and ARM64 Linux, Raspberry Pi included.',
} as const;

/**
 * The invitation to star the repository. No page can star it for the
 * visitor: that takes their GitHub session, so the link opens the repository
 * and its own Star button does the rest.
 */
export const STAR = {
  label: 'Star',
  line: 'Useful to you? A star on GitHub helps other people find it.',
  link: 'Star it on GitHub',
} as const;

/** Lines of terminal that show a command and what it printed. */
export type Sample = readonly TerminalLine[];

export const HERO = {
  /** The headline; `accent` closes it in the brand color. */
  title: "who doesn't follow you",
  accent: 'back?',
  lead: 'Snob knows. An Instagram client for the terminal: download reels or stories, who unfollowed you, who never followed you back, ... Also ready for your AI agent.',
} as const;

/**
 * A sketch of the full-screen profile view, shown in the hero until the
 * recording of the real one exists.
 */
export const HERO_SAMPLE: Sample = [
  { command: 'snob profile someone' },
  { output: [{ text: '@someone', tone: 'prompt' }, '  Someone Example  ', { text: '(verified)', tone: 'dim' }] },
  { output: '  photographer, mostly coffee' },
  { output: '' },
  { output: 'Followers:    1234' },
  { output: 'Following:    56' },
  { output: 'Posts:        78' },
  { output: '' },
  { output: [{ text: 'You do not follow them, they follow you', tone: 'info' }] },
  { output: 'Highlights:   2' },
  { output: [{ text: 'Stories up:   1', tone: 'ok' }] },
];

export interface Feature {
  /** The command, as typed after `snob`. */
  name: string;
  description: string;
  sample: Sample;
}

/**
 * The six cards. Output is what snob prints, with invented accounts; a line
 * starting with `#` is a comment, never output snob does not print.
 */
export const FEATURES: readonly Feature[] = [
  {
    name: 'unfollowers',
    description: 'You follow them; they do not follow you back.',
    sample: [
      { command: 'snob unfollowers' },
      { output: [{ text: '3 accounts you follow that do not follow you back - 3 of 87 - 9 requests', tone: 'dim' }] },
    ],
  },
  {
    name: 'profile',
    description: "An account's page, the way Instagram shows it.",
    sample: [{ command: 'snob profile someone' }, { output: 'Followers:    1234' }, { output: 'Following:    56' }],
  },
  {
    name: 'stories · highlights',
    description: 'See them and save them. A story is never marked as seen.',
    sample: [
      { command: 'snob stories someone -d all' },
      { output: [{ text: '# every story it has up, saved', tone: 'dim' }] },
    ],
  },
  {
    name: 'post · reel',
    description: 'See or download a post or reel, including every photo and video in it.',
    sample: [
      { command: 'snob reel <link> -d all' },
      { output: [{ text: '# every photo and video in it, saved', tone: 'dim' }] },
    ],
  },
  {
    name: 'watch',
    description: 'Keeps watching on a schedule and reports what changed.',
    sample: [
      { command: 'snob watch once' },
      { output: 'Changes for @me since Aug 14 at 09:12' },
      { output: [{ text: '  followers gained: 1', tone: 'ok' }] },
      { output: [{ text: '  followers lost: 1', tone: 'error' }] },
    ],
  },
  {
    name: 'follow · unfollow',
    description: 'The only two writes. One account at a time, after asking.',
    sample: [
      { command: 'snob unfollow someone' },
      { output: 'Unfollow @someone? [y/N] y' },
      { output: [{ text: 'No longer following @someone.', tone: 'ok' }] },
    ],
  },
];

export const FEATURES_SECTION = {
  title: 'Everything you check by hand,',
  accent: 'in one command.',
  lead: 'On a terminal most commands open a full-screen view you move through with the arrow keys; snob --help lists everything.',
} as const;

export const BEHAVIOR = {
  title: 'A light client,',
  accent: 'on purpose.',
  lead: "It signs in as you and sends its requests from a Chrome, Edge, Brave or Chromium already on your machine, at a human pace. Your password never goes through snob, and it never reads your own browser's cookies.",
  /** README, "Staying a light client", word for word. */
  honest:
    "There is no official API for most of this, so snob uses the same web API instagram.com uses, signed in as you. That is outside Instagram's Terms of Use, and the usual consequence is that Instagram asks the account to verify itself. snob keeps to modest volumes, sends only the web app's own requests, and stops at the first sign of push-back. Use it from the connection you normally browse from.",
  checks: [
    { output: [{ text: '✓ ', tone: 'ok' }, '12 accounts a page, 1 to 3 seconds apart'] },
    { output: [{ text: '✓ ', tone: 'ok' }, 'a rest of 5 to 15 minutes every 40 pages'] },
    { output: [{ text: '✓ ', tone: 'ok' }, 'two writes, ever: follow and unfollow, after asking'] },
    { output: [{ text: '✓ ', tone: 'ok' }, 'stories are never marked as seen'] },
    { output: [{ text: '✓ ', tone: 'ok' }, 'the session is stored on your machine, in the system keyring when there is one'] },
    { output: [{ text: '! ', tone: 'warn' }, 'the first push-back stops it, and it cools down'] },
  ] as Sample,
} as const;

export const AGENTS = {
  title: 'Same answer,',
  accent: 'for people and programs.',
  lead: 'Every command can print instead of opening a view, in a stable form a program can read. Down a pipe that is the default, and the default format is JSON.',
  /** The full-screen view, drawn without side borders so it lines up in any font. */
  human: [
    { output: [{ text: '╭ unfollowers of @me — 3 accounts', tone: 'dim' }], art: true },
    { output: [{ text: '> ', tone: 'prompt' }, 'anna.example    Anna Example'] },
    { output: '  bob_builds      Bob' },
    { output: ['  camille.k       Camille     ', { text: 'verified', tone: 'dim' }] },
    { output: [{ text: '╰ ↑↓ move · enter open profile · / filter · q quit', tone: 'dim' }], art: true },
  ] as Sample,
  machine: [
    { command: "snob unfollowers | jq '.[0]'" },
    { output: '{' },
    { output: [{ text: '  "pk"', tone: 'info' }, ': 1234567890,'] },
    { output: [{ text: '  "username"', tone: 'info' }, ': "anna.example",'] },
    { output: [{ text: '  "full_name"', tone: 'info' }, ': "Anna Example",'] },
    { output: [{ text: '  "is_private"', tone: 'info' }, ': false,'] },
    { output: [{ text: '  "is_verified"', tone: 'info' }, ': false'] },
    { output: '}' },
  ] as Sample,
  flags: [
    { flag: '--format json|ndjson|csv|xlsx|md|table', means: 'a list or a document, in the form you need' },
    { flag: '-o <file>', means: 'write it to a file' },
    { flag: '--json', means: 'a status object, for commands such as whoami' },
    { flag: '--no-interactive', means: 'always print, never open a view' },
    { flag: '-y', means: 'answer the one confirmation in advance' },
    { flag: 'watch --webhook <url>', means: 'post each report as JSON' },
  ],
  exitCodes: [
    { code: '0', means: 'it worked' },
    { code: '1', means: 'it failed' },
    { code: '2', means: 'the command line could not be parsed' },
    { code: '3', means: 'log in again' },
    { code: '4', means: 'the account needs verifying' },
    { code: '5', means: 'rate limited or cooling down' },
    { code: '130', means: 'stopped' },
  ],
} as const;

export const INSTALL_SECTION = { title: 'Install', accent: 'it.', lead: SUPPORT } as const;
