/**
 * What a terminal block shows, as data rather than markup, so the same lines
 * render as HTML on the page and as a code block in the Markdown twin.
 */

/** The TUI's own state colors, plus the prompt and dimmed text. */
export type Tone = 'plain' | 'dim' | 'ok' | 'warn' | 'error' | 'info' | 'prompt';

/** A run of text in one tone; a bare string is `plain`. */
export type Segment = string | { text: string; tone: Tone };

export type TerminalLine =
  /** A command typed at the prompt. The `$` is drawn, never part of the text. */
  | { command: string }
  /**
   * What the program printed. `art` marks box drawing and other decoration a
   * screen reader should skip.
   */
  | { output: string | Segment[]; art?: boolean };

/** The text of a line as a terminal would print it, prompt included. */
export function plainLine(line: TerminalLine): string {
  if ('command' in line) return `$ ${line.command}`;
  const segments = typeof line.output === 'string' ? [line.output] : line.output;
  return segments.map((segment) => (typeof segment === 'string' ? segment : segment.text)).join('');
}
