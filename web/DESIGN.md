# The website's design

What the site at `snob.dennisgr7.dev` looks like and why, settled with the
maintainer before any of it was built. `src/pages/design-system` shows every token
and component live; this file is the reasoning behind them. A change to the look
starts here.

## Who it is for

Two people read the page, in this order:

1. **Someone curious**, often arriving from a phone, who wants to know who does not
   follow them back or to keep a story. The top of the page has to hook them and get
   them to install.
2. **Someone who lives in a terminal**, who wants to see real commands, real output
   and how the tool behaves before trusting it with an account.

So the hero winks and everything below it is technical and direct: the joke is in
the name and the headline, never in the facts.

## Identity

Snob has its own look. Its author appears once, as a signature in the footer: "by
Dennis", linking to dennisgr7.dev.

- **Mark**: *snob.* set in IBM Plex Mono bold italic, with the period in the accent
  color: the nose turned up, and the last word on the matter. The favicon is the
  same "s." on a dark tile, which reads on light and dark tabs alike.
- **Accent**: magenta, `#be185d` on light and `#f472b6` on dark. It says
  "Instagram" without borrowing Instagram's gradient or logo, which the site never
  uses. It is the only brand color: the cursor, the key word of a headline, links,
  the prompt.
- **Type**: IBM Plex Mono for headlines, navigation, labels and everything that is a
  command; IBM Plex Sans for reading. Plex has a humanist, slightly editorial touch
  that suits the name and stays technical.
- **Layout**: a clean grid with the essence of a terminal: monospaced headlines, the
  navigation as `[features] [agents] [install]`, a blinking block cursor after the
  headline.

## Theme

The page follows the system (`prefers-color-scheme`). There is no toggle, so there
is no script and nothing to remember.

| Token | Light | Dark | For |
|---|---|---|---|
| `bg` | `#fafafa` | `#0b0d10` | the page |
| `surface` | `#ffffff` | `#12151a` | cards |
| `text` | `#111111` | `#e6e8ec` | text |
| `muted` | `#52525b` | `#9aa1ab` | secondary text |
| `line` | `#e4e4e7` | `#23272f` | borders, rules |
| `accent` | `#be185d` | `#f472b6` | the brand color |
| `on-accent` | `#ffffff` | `#0b0d10` | text on the accent |

Every text pair passes WCAG AA; the accent on the page background is above 6:1 in
both themes.

**Terminal blocks are dark in both themes**, as a terminal is, with a border on the
dark page. They use the palette the TUI itself uses for states: `#e6e8ec` text,
`#7ad46b` ok, `#e5c07b` warning, `#e06c75` error, `#3ec7d6` information, the dark
accent for the prompt.

The tokens are CSS variables on `:root`, swapped under the dark media query and
handed to Tailwind through `@theme inline`, so a utility such as `bg-bg` or
`text-muted` follows the system with nothing else to write.

## Scale and rhythm

- Hero headline: Plex Mono 700, fluid from 2.25 to 4.5 rem, tracking −0.03em.
- Section headline: Plex Mono 700, fluid from 1.5 to 2.25 rem.
- Body: Plex Sans 400, 1.0625 rem, line height 1.6. Small: 0.875 rem.
- Code: Plex Mono 400, 0.9375 rem.
- Spacing on Tailwind's 4 px scale. The content is at most 72 rem wide, with a 16 px
  gutter on a phone. Sections breathe 4 to 8 rem vertically.
- Radius: 6 px on controls, 10 px on cards and terminals. No shadow except under
  the terminal in the hero.

## Motion

The cursor blinks with `steps()` every 1.1 s; hover changes take 150 ms. Nothing
moves on scroll. Under `prefers-reduced-motion: reduce` nothing moves at all: the
cursor stays solid.

## The page

One page, in this order:

1. **Hero**: the headline *who doesn't follow you back?* in mono with the last word
   in the accent and the cursor after it; one sentence; the install command for the
   visitor's system with a copy button and "other ways ↓"; beside it, a terminal
   with the profile view of an account.
2. **What it does**: six cards, one command each, with a small terminal.
3. **How it behaves**: a light client on purpose, and the honest note the README
   makes: there is no official API, so this is outside Instagram's Terms, and the
   usual consequence is a verification check. The site never hides it.
4. **Scripts and agents**: the same answer seen in the TUI and as JSON down a pipe,
   with the flags and the exit codes.
5. **Install**: every platform in tabs, each with a copy button. A copy button is
   an icon, two pages that turn into a check once copied, in the terminal's dim
   text and never a state color; Homebrew and Scoop link to their own sites.
6. **Footer**: MIT, GitHub, `llms.txt`, "Not affiliated with Instagram or Meta",
   and the signature.

Every word on it comes from `src/lib/content.ts`, which also writes `llms.txt` and
the Markdown twin, so a person and an agent always read the same thing. The copy
promises nothing the README does not.

### Showing the tool

Commands and output are real text in HTML: selectable, copyable, indexed and
readable by an agent.

**Planned, not built**: a short recording of the interactive TUI to replace the
terminal in the hero, muted and looping, recorded against the test suite's invented
Instagram, never a real account. It would load nothing until it plays, play only
when the visitor has not asked for reduced motion, and otherwise keep its poster
with controls. Until it exists there is no `<video>` on the page.

### Install in the hero

The hero shows the command for the visitor's system: Scoop on Windows, Homebrew on
macOS, the install script on Linux (many Linux machines have no Homebrew), and on a
phone a line saying snob runs on your computer. Without JavaScript it shows the
macOS command. "Other ways ↓" always leads to the install section.

## Behavior and its limits

- **Two small scripts and nothing else**: the platform detection, and the copy
  buttons with the install tabs. Astro bundles both and writes their hashes into the
  Content Security Policy. Executable code is never `is:inline`, which the policy
  would not cover, and nothing carries a `style` attribute.
- **Everything works without JavaScript**: the hero shows macOS, the install
  section shows every platform one after another, and the copy buttons stay hidden
  because they could not work.
- **Accessibility**: a skip link, one `h1`, `header`, `nav`, `main` and `footer`,
  a label on every terminal, the TUI's box drawing hidden from screen readers,
  everything reachable by keyboard, nothing that needs hover.
- **Speed**: the largest paint is the headline, which is text. Only Plex Mono 700
  and Plex Sans 400 are preloaded; the CSS is inline.
