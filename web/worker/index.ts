/**
 * The site's one piece of Worker code, and it serves the hero's recording
 * only: `run_worker_first` in wrangler.jsonc sends the videos under `/video/`
 * here, and everything else, their poster included, is served from `dist/`
 * with no code at all.
 *
 * **Why it exists: Safari plays a video only from byte ranges.** It asks for
 * `bytes=0-1` first and gives up on anything but a 206, and Workers static
 * assets answer a `Range` with 200 and the whole file (measured on the live
 * site, November 2026, cached or not). So the file is read whole from the
 * assets and the range is cut here.
 *
 * The files are a few hundred kilobytes, so reading one whole costs nothing
 * worth streaming around. `_headers` is not applied to what a Worker answers
 * (Cloudflare's docs, "Headers"), so the headers a video needs are set here:
 * its type, `nosniff`, the assets' own caching and ETag.
 */

/** The assets binding, as much of it as is used: no generated types needed. */
interface Env {
  ASSETS: { fetch(request: Request): Promise<Response> };
}

/** `bytes=start-end`, `bytes=start-` or `bytes=-suffix`: one range, as browsers send. */
const RANGE = /^bytes=(\d*)-(\d*)$/;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method !== 'GET' && request.method !== 'HEAD') {
      return new Response(null, { status: 405, headers: { Allow: 'GET, HEAD' } });
    }
    // The whole file, whatever the browser asked: no range, no condition.
    const asset = await env.ASSETS.fetch(new Request(request.url));
    if (!asset.ok) return asset;

    const headers = new Headers({
      'Content-Type': asset.headers.get('Content-Type') ?? 'application/octet-stream',
      'Accept-Ranges': 'bytes',
      'Cache-Control': 'public, max-age=0, must-revalidate',
      'X-Content-Type-Options': 'nosniff',
    });
    const etag = asset.headers.get('ETag');
    if (etag) {
      headers.set('ETag', etag);
      if (request.headers.get('If-None-Match') === etag) return new Response(null, { status: 304, headers });
    }

    const body = await asset.arrayBuffer();
    const size = body.byteLength;
    const range = RANGE.exec(request.headers.get('Range') ?? '');
    const [, first = '', last = ''] = range ?? [];
    if (!range || (first === '' && last === '')) return new Response(body, { headers });

    const start = first === '' ? Math.max(0, size - Number(last)) : Number(first);
    const end = first !== '' && last !== '' ? Math.min(Number(last), size - 1) : size - 1;
    if (start >= size || start > end) {
      headers.set('Content-Range', `bytes */${size}`);
      return new Response(null, { status: 416, headers });
    }
    headers.set('Content-Range', `bytes ${start}-${end}/${size}`);
    return new Response(body.slice(start, end + 1), { status: 206, headers });
  },
};
