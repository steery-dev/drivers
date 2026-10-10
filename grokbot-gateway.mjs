#!/usr/bin/env node
import { spawn } from 'node:child_process';

function credentials() {
  const token = process.env.GROK_ACCESS_TOKEN;
  const version = process.env.GROK_CLIENT_VERSION;
  if (!token || !version) throw new Error('Set GROK_ACCESS_TOKEN and GROK_CLIENT_VERSION driver settings.');
  return { token, version };
}

async function jsonRequest(url, headers, body) {
  const response = await fetch(url, { method: 'POST', redirect: 'error', headers, body: JSON.stringify(body), signal: AbortSignal.timeout(30000) });
  if (!response.ok) throw new Error(`Grok Bot HTTP ${response.status} at ${new URL(url).pathname}`);
  return response.json();
}

try {
  const { token, version } = await credentials();
  const args = process.argv.slice(2);
  if (args[0] === 'api') {
    if (args.length !== 3 || !/^[A-Za-z][A-Za-z0-9]*$/.test(args[1])) throw new Error('Usage: grokbot-gateway.mjs api METHOD JSON');
    const payload = JSON.parse(args[2]);
    const sandbox = await jsonRequest('https://api2.cursor.sh/aiserver.v1.GrokBotService/EnsureSandBox', {
      'content-type': 'application/json', 'connect-protocol-version': '1', authorization: `Bearer ${token}`,
      'x-cursor-client-type': 'sand', 'x-cursor-client-version': version, 'x-sand-box-namespace': 'prod',
    }, {});
    const endpoint = new URL(sandbox.gatewayUrl);
    if (endpoint.protocol !== 'https:' || endpoint.username || endpoint.password) throw new Error('Invalid Grok Bot gateway URL.');
    const headers = { 'content-type': 'application/json', authorization: `Bearer ${sandbox.gatewayToken}`,
      ...(sandbox.gatewayHeaders ?? sandbox.gateway_headers ?? {}),
      'x-anyrun-network-token': sandbox.anyrunNetworkToken ?? sandbox.anyrun_network_token ?? sandbox.networkToken,
    };
    const result = await jsonRequest(endpoint.toString().replace(/\/$/, '') + '/api/' + args[1], headers, payload);
    process.stdout.write(JSON.stringify(result) + '\n');
  } else {
    const child = spawn('gbot', args, { stdio: 'inherit', env: { ...process.env, CURSOR_ACCESS_TOKEN: token, SAND_CLIENT_VERSION: version } });
    child.on('error', error => { process.stderr.write(error.message + '\n'); process.exitCode = 1; });
    child.on('exit', (code, signal) => { process.exitCode = code ?? (signal ? 1 : 0); });
  }
} catch (error) {
  process.stderr.write(error.message + '\n');
  process.exitCode = 1;
}
