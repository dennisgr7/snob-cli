import { readFileSync } from 'node:fs';
import { join } from 'node:path';

/**
 * The version of the release the site describes, read at build time from the
 * workspace manifest, so nobody keeps a second copy by hand. Workers Builds
 * clones the whole repository, so the manifest is there. Production deploys
 * from `main`, which only takes a version as it is released, so there it names
 * files on the releases page (once the tag's build has published them). A
 * preview build of another branch may carry a version not released yet, and
 * its .deb lines can then name a file that does not exist.
 *
 * From the working directory rather than this module's URL: the build bundles
 * this file somewhere else, and every tool here runs from `web/`.
 */
function workspaceVersion(): string {
  const manifest = readFileSync(join(process.cwd(), '..', 'Cargo.toml'), 'utf8');
  const table = manifest.split(/^\[workspace\.package\]\s*$/m)[1]?.split(/^\[/m)[0] ?? '';
  const version = /^version\s*=\s*"([^"]+)"/m.exec(table)?.[1];
  if (!version) throw new Error('No version in [workspace.package] of Cargo.toml');
  return version;
}

export const VERSION = workspaceVersion();
