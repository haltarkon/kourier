// SPDX-License-Identifier: AGPL-3.0-only
//
// Minimal JavaScript driver for harness.mjs, used by the node tests. It speaks the same
// JSON-lines protocol as crates/kourier-signal/tests/support/harness.rs.

import { spawn } from 'node:child_process';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** Spawns `harness.mjs` and resolves once its handshake line has arrived. */
export async function startHarness({ args = [], env = {}, startupTimeoutMs = 30_000 } = {}) {
  const child = spawn(process.execPath, [path.join(HERE, 'harness.mjs'), ...args], {
    cwd: HERE,
    env: { ...process.env, ...env },
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  const stderr = [];
  readline.createInterface({ input: child.stderr }).on('line', (line) => {
    stderr.push(line);
    if (process.env.HARNESS_VERBOSE) {
      process.stderr.write(`${line}\n`);
    }
  });

  const pending = new Map();
  const events = [];
  let nextId = 1;
  let handshakeResolve;
  let handshakeReject;
  const handshake = new Promise((resolve, reject) => {
    handshakeResolve = resolve;
    handshakeReject = reject;
  });

  const exited = new Promise((resolve) => {
    child.on('exit', (code, signal) => {
      const error = new Error(`harness exited (code ${code}, signal ${signal})\n${stderr.slice(-20).join('\n')}`);
      handshakeReject(error);
      for (const { reject } of pending.values()) {
        reject(error);
      }
      pending.clear();
      resolve({ code, signal });
    });
  });

  readline.createInterface({ input: child.stdout }).on('line', (line) => {
    const message = JSON.parse(line);
    switch (message.type) {
      case 'ready':
        handshakeResolve(message);
        break;
      case 'fatal':
        handshakeReject(new Error(`harness fatal: ${message.error.message}`));
        break;
      case 'event':
        events.push(message);
        break;
      case 'response': {
        const entry = pending.get(message.id);
        if (entry) {
          pending.delete(message.id);
          entry.resolve(message);
        }
        break;
      }
      default:
        throw new Error(`unexpected harness line: ${line}`);
    }
  });

  let timer;
  const ready = await Promise.race([
    handshake,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('no handshake from harness')), startupTimeoutMs);
    }),
  ]).finally(() => clearTimeout(timer));

  /** Sends one raw line; resolves with the full response object for `id`. */
  function sendRaw(line, id) {
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      child.stdin.write(`${line}\n`);
    });
  }

  /** Sends a command; resolves with `result`, rejects with an Error carrying `code`. */
  async function request(cmd, args = {}) {
    const id = nextId;
    nextId += 1;
    const response = await sendRaw(JSON.stringify({ id, cmd, args }), id);
    if (!response.ok) {
      const error = new Error(`${cmd}: ${response.error.code}: ${response.error.message}`);
      error.code = response.error.code;
      throw error;
    }
    return response.result;
  }

  return {
    child,
    handshake: ready,
    events,
    stderr,
    exited,
    request,
    sendRaw,
    async close() {
      if (child.exitCode !== null || child.signalCode !== null) {
        return exited;
      }
      child.stdin.end();
      const killer = setTimeout(() => child.kill('SIGKILL'), 5_000);
      try {
        return await exited;
      } finally {
        clearTimeout(killer);
      }
    },
  };
}
