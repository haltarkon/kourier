// SPDX-License-Identifier: AGPL-3.0-only
//
// Test-only stand-in for the provisioning half of a linking client, used by smoke.mjs to
// exercise `waitForProvision` and `completeProvision` without Kourier's Signal code.
//
// The mock serves websockets only over HTTP/2 extended CONNECT (RFC 8441), as libsignal-net
// speaks them, so this opens one by hand and frames RFC 6455 messages itself: client frames
// are masked, server frames are not, messages are never fragmented by the mock.
// The ProvisionEnvelope is decrypted as Desktop does (SD/ts/textsecure/ProvisioningCipher.node.ts).

import crypto from 'node:crypto';
import http2 from 'node:http2';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Proto } = require('@signalapp/mock-server');

const OPCODE_BINARY = 0x2;
const OPCODE_CLOSE = 0x8;
const OPCODE_PING = 0x9;
const OPCODE_PONG = 0xa;

/** One RFC 6455 frame from the client: FIN set, masked. */
function encodeFrame(opcode, payload) {
  const length = payload.length;
  let header;
  if (length < 126) {
    header = Buffer.from([0x80 | opcode, 0x80 | length]);
  } else if (length < 0x10000) {
    header = Buffer.alloc(4);
    header.writeUInt8(0x80 | opcode, 0);
    header.writeUInt8(0x80 | 126, 1);
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header.writeUInt8(0x80 | opcode, 0);
    header.writeUInt8(0x80 | 127, 1);
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  const mask = crypto.randomBytes(4);
  const masked = Buffer.alloc(length);
  for (let i = 0; i < length; i += 1) {
    masked[i] = payload[i] ^ mask[i % 4];
  }
  return Buffer.concat([header, mask, masked]);
}

/** Splits complete frames off `buffer`; returns `{frames, rest}`. */
function decodeFrames(buffer) {
  const frames = [];
  let offset = 0;
  for (;;) {
    if (buffer.length - offset < 2) {
      break;
    }
    const opcode = buffer[offset] & 0x0f;
    const masked = (buffer[offset + 1] & 0x80) !== 0;
    let length = buffer[offset + 1] & 0x7f;
    let headerLength = 2;
    if (length === 126) {
      if (buffer.length - offset < 4) {
        break;
      }
      length = buffer.readUInt16BE(offset + 2);
      headerLength = 4;
    } else if (length === 127) {
      if (buffer.length - offset < 10) {
        break;
      }
      length = Number(buffer.readBigUInt64BE(offset + 2));
      headerLength = 10;
    }
    const maskLength = masked ? 4 : 0;
    const total = headerLength + maskLength + length;
    if (buffer.length - offset < total) {
      break;
    }
    const payload = Buffer.from(buffer.subarray(offset + headerLength + maskLength, offset + total));
    if (masked) {
      const mask = buffer.subarray(offset + headerLength, offset + headerLength + 4);
      for (let i = 0; i < payload.length; i += 1) {
        payload[i] ^= mask[i % 4];
      }
    }
    frames.push({ opcode, payload });
    offset += total;
  }
  return { frames, rest: buffer.subarray(offset) };
}

/** A fresh X25519 key pair; `publicKey` is libsignal's 33-byte serialization (0x05 prefix). */
function generateKeyPair() {
  const { privateKey, publicKey } = crypto.generateKeyPairSync('x25519');
  const raw = Buffer.from(publicKey.export({ format: 'jwk' }).x, 'base64url');
  return { privateKey, publicKey: Buffer.concat([Buffer.from([0x05]), raw]) };
}

function x25519PublicKey(serialized) {
  if (serialized.length !== 33 || serialized[0] !== 0x05) {
    throw new Error('expected a 33-byte DJB public key');
  }
  return crypto.createPublicKey({
    key: { kty: 'OKP', crv: 'X25519', x: Buffer.from(serialized.subarray(1)).toString('base64url') },
    format: 'jwk',
  });
}

/**
 * Decrypts a ProvisionEnvelope for `privateKey`: version 1, HMAC-SHA256, AES-256-CBC with keys
 * from HKDF-SHA256(ECDH, salt 32 x 0, "TextSecure Provisioning Message"). Returns the decoded
 * ProvisionMessage.
 */
export function decryptProvisionEnvelope(envelopeBytes, privateKey) {
  const envelope = Proto.ProvisionEnvelope.decode(envelopeBytes);
  const shared = crypto.diffieHellman({ privateKey, publicKey: x25519PublicKey(Buffer.from(envelope.publicKey)) });
  const keys = Buffer.from(
    crypto.hkdfSync('sha256', shared, Buffer.alloc(32), Buffer.from('TextSecure Provisioning Message'), 64),
  );
  const body = Buffer.from(envelope.body);
  if (body[0] !== 1) {
    throw new Error(`unknown provisioning version ${body[0]}`);
  }
  const signed = body.subarray(0, body.length - 32);
  const mac = crypto.createHmac('sha256', keys.subarray(32)).update(signed).digest();
  if (!crypto.timingSafeEqual(mac, body.subarray(body.length - 32))) {
    throw new Error('provisioning MAC mismatch');
  }
  const decipher = crypto.createDecipheriv('aes-256-cbc', keys.subarray(0, 32), signed.subarray(1, 17));
  const plaintext = Buffer.concat([decipher.update(signed.subarray(17)), decipher.final()]);
  return Proto.ProvisionMessage.decode(plaintext);
}

/**
 * Opens `/v1/websocket/provisioning/` on the harness and answers the server's requests with
 * 200. Resolves once the server sent our provisioning address. The result carries:
 * `address`, `linkUrl` (the `sgnl://linkdevice` URL a client would show as QR code),
 * `envelope` (a promise of the raw ProvisionEnvelope bytes), `decrypt(bytes)` and `close()`.
 */
export async function openProvisioningSocket(handshake, { timeoutMs = 10_000 } = {}) {
  const session = http2.connect(handshake.urls.chat, { ca: handshake.caPem });
  const failed = new Promise((_, reject) => {
    session.once('error', reject);
  });
  failed.catch(() => {});

  // Extended CONNECT is only allowed after the server's SETTINGS enabled it.
  await Promise.race([
    new Promise((resolve) => {
      session.once('remoteSettings', resolve);
    }),
    failed,
  ]);

  const stream = session.request(
    {
      ':method': 'CONNECT',
      ':protocol': 'websocket',
      ':scheme': 'https',
      ':path': handshake.paths.provisioningWebsocket,
      'sec-websocket-version': '13',
    },
    { endStream: false },
  );

  const keyPair = generateKeyPair();
  const requests = [];
  const address = Promise.withResolvers();
  const envelope = Promise.withResolvers();
  envelope.promise.catch(() => {});
  // Settling an already settled promise is a no-op, so this only fails what is still open.
  const fail = (error) => {
    address.reject(error);
    envelope.reject(error);
  };

  const send = (opcode, payload) => {
    if (!stream.destroyed) {
      stream.write(encodeFrame(opcode, payload));
    }
  };

  const onMessage = (payload) => {
    const message = Proto.WebSocketMessage.decode(payload);
    const { request } = message;
    if (message.type !== Proto.WebSocketMessage.Type.REQUEST || !request) {
      return;
    }
    requests.push({ verb: request.verb, path: request.path });
    if (request.verb === 'PUT' && request.path === '/v1/address') {
      address.resolve(Proto.ProvisioningAddress.decode(request.body).address);
    } else if (request.verb === 'PUT' && request.path === '/v1/message') {
      envelope.resolve(Buffer.from(request.body));
    }
    send(
      OPCODE_BINARY,
      Buffer.from(
        Proto.WebSocketMessage.encode({
          type: Proto.WebSocketMessage.Type.RESPONSE,
          request: null,
          response: { id: request.id, status: 200, message: 'OK', headers: [], body: null },
        }),
      ),
    );
  };

  let pending = Buffer.alloc(0);
  stream.on('response', (headers) => {
    if (headers[':status'] !== 200) {
      fail(new Error(`provisioning websocket: status ${headers[':status']}`));
    }
  });
  stream.on('data', (chunk) => {
    const { frames, rest } = decodeFrames(Buffer.concat([pending, chunk]));
    pending = Buffer.from(rest);
    for (const { opcode, payload } of frames) {
      if (opcode === OPCODE_BINARY) {
        onMessage(payload);
      } else if (opcode === OPCODE_PING) {
        send(OPCODE_PONG, payload);
      } else if (opcode === OPCODE_CLOSE) {
        stream.end();
      }
    }
  });
  stream.on('error', fail);
  stream.on('close', () => fail(new Error('provisioning websocket closed')));
  failed.catch(fail);

  let timer;
  const ourAddress = await Promise.race([
    address.promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('no provisioning address from the server')), timeoutMs);
    }),
  ]).finally(() => clearTimeout(timer));

  const query = new URLSearchParams({ uuid: ourAddress, pub_key: keyPair.publicKey.toString('base64') });
  return {
    address: ourAddress,
    linkUrl: `sgnl://linkdevice?${query}&capabilities=`,
    envelope: envelope.promise,
    requests,
    decrypt: (bytes) => decryptProvisionEnvelope(bytes, keyPair.privateKey),
    /** Closes the websocket (the server then drops this provisioning attempt). */
    async close() {
      if (!stream.destroyed) {
        send(OPCODE_CLOSE, Buffer.from([0x03, 0xe8]));
        stream.end();
      }
      await new Promise((resolve) => {
        if (session.closed || session.destroyed) {
          resolve();
          return;
        }
        session.close(resolve);
        setTimeout(() => {
          session.destroy();
          resolve();
        }, 1_000).unref();
      });
    },
  };
}
