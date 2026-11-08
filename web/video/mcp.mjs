// @ts-check
/**
 * Asks `snob mcp` what the chat in the hero's video shows, as an agent's
 * client would: the account's unfollowers, then an unfollow, answered yes in
 * the form the server asks it in. `record.sh` runs it against the invented
 * world, with `snob` on the PATH, and it writes what came back into the
 * directory it is given:
 *
 * - `mcp-unfollowers.json`: the `snob_list` result's structured content;
 * - `mcp-question.json`: the parameters of the server's `elicitation/create`;
 * - `mcp-unfollow.json`: the `snob_unfollow` result's structured content.
 *
 * Usage: node web/video/mcp.mjs <captures directory>
 */
import { spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { createInterface } from 'node:readline';

const out = process.argv[2];
if (!out) throw new Error('usage: node mcp.mjs <captures directory>');

const server = spawn('snob', ['mcp', '--allow-writes'], { stdio: ['pipe', 'pipe', 'inherit'] });
const lines = createInterface({ input: server.stdout })[Symbol.asyncIterator]();

/** @param {object} message */
const send = (message) => server.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);

/** The next message the server sends that is a request or an answer. */
async function next() {
  for (;;) {
    const { value, done } = await lines.next();
    if (done) throw new Error('snob mcp stopped answering');
    const message = JSON.parse(value);
    if ('id' in message) return message;
  }
}

/** @param {string} name @param {unknown} content */
const write = (name, content) =>
  writeFileSync(path.join(out, name), `${JSON.stringify(content, null, 2)}\n`);

send({
  id: 1,
  method: 'initialize',
  params: {
    protocolVersion: '2025-06-18',
    capabilities: { elicitation: {} },
    clientInfo: { name: 'snob-website-video', version: '1' },
  },
});
await next();
send({ method: 'notifications/initialized' });

send({ id: 2, method: 'tools/call', params: { name: 'snob_list', arguments: { list: 'unfollowers' } } });
const list = await next();
if (list.result?.isError) throw new Error(JSON.stringify(list.result));
write('mcp-unfollowers.json', list.result.structuredContent);

send({ id: 3, method: 'tools/call', params: { name: 'snob_unfollow', arguments: { account: 'dex.drums' } } });
const question = await next();
if (question.method !== 'elicitation/create') throw new Error(`no question: ${JSON.stringify(question)}`);
write('mcp-question.json', question.params);
send({ id: question.id, result: { action: 'accept', content: { confirm: true } } });
const unfollow = await next();
if (unfollow.result?.isError) throw new Error(JSON.stringify(unfollow.result));
write('mcp-unfollow.json', unfollow.result.structuredContent);

server.stdin.end();
