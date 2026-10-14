#!/usr/bin/env node
// SPDX-License-Identifier: AGPL-3.0-only
//
// Kourier's driver for @signalapp/mock-server (docs/architecture.md §10.3).
//
// Starts a mock Signal server on a random loopback port, prints one handshake line with
// everything a libsignal-net client needs to reach it, then serves JSON-lines commands on
// stdin and answers each with a JSON line on stdout. The wire protocol is documented in
// README.md; stdout carries protocol lines only, logs go to stderr (`DEBUG=mock:*` for the
// mock server's own logs).
//
// Mirrors how Signal Desktop drives the same server: SD/ts/test-mock/bootstrap.node.ts.

import assert from 'node:assert';
import { createRequire } from 'node:module';
import fs from 'node:fs/promises';
import http2 from 'node:http2';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { parseArgs } from 'node:util';

const require = createRequire(import.meta.url);
const mock = require('@signalapp/mock-server');
const { Server, loadCertificates, EnvelopeType, ServiceIdKind, StorageState } = mock;
const nodeCrypto = require('node:crypto');
const SignalClient = require('@signalapp/libsignal-client');
const zkgroup = require('@signalapp/libsignal-client/zkgroup');
const Proto = mock.Proto;
// The websocket router of the mock: patched below so profile answers can carry an avatar and
// "about" fields, which the mock does not serve itself.
const { Router } = require('@signalapp/mock-server/src/server/ws/router.js');
// One websocket of the mock: patched below so a device can remove itself from its account
// (`DELETE /v1/devices/<id>`), which the mock has no route for.
const { Connection: MockConnection } = require('@signalapp/mock-server/src/server/ws/connection.js');

/** Version of the JSON-lines protocol; bump on incompatible changes. */
const PROTOCOL_VERSION = 1;

/** Default timeout of every `waitFor*` style command, in milliseconds. */
const DEFAULT_WAIT_MS = 30_000;

/** Default timeout of `completeProvision`: the client has to link and upload keys. */
const DEFAULT_PROVISION_MS = 60_000;

/** Longest `timeout` argument: `setTimeout` fires at once for anything above 2^31 - 1 ms. */
const MAX_TIMEOUT_MS = 2 ** 31 - 1;

/** Most texts one `sendTexts` queues. */
const MAX_SEND_TEXTS = 10_000;

/** How often `HarnessServer.send` delivers an envelope again that a socket lost unacked. */
const MAX_REDELIVERIES = 50;

/** Pause before such a redelivery, in milliseconds. */
const REDELIVERY_DELAY_MS = 50;

/** The certificate's SANs are `localhost`, `127.0.0.1` and `::1`; clients use the name. */
const TLS_HOST = 'localhost';

/** Marks a decrypted Content that arrived in a sealed-sender envelope (see `#trackPrimary`). */
const SEALED_SENDER = Symbol('kourier.sealedSender');
/** The group id (base64) a sealed envelope's content named, or null (see `#trackPrimary`). */
const SEALED_GROUP = Symbol('kourier.sealedGroup');
/** The content hint of a sealed envelope's content, by name (see `#trackPrimary`). */
const SEALED_HINT = Symbol('kourier.sealedHint');
/** `UnidentifiedSenderMessageContent.ContentHint` names by value. */
const CONTENT_HINT_NAMES = ['default', 'resendable', 'implicit'];

/** The device id of a primary (the mock's `PRIMARY_DEVICE_ID`). */
const PRIMARY_DEVICE_ID = 1;

/** `ReceiptMessage.Type` by name. */
const RECEIPT_TYPES = { delivery: 0, read: 1, viewed: 2 };
/** `TypingMessage.Action` by name. */
const TYPING_ACTIONS = { started: 0, stopped: 1 };
/** `SyncMessage.CallEvent.Type`, `.Direction` and `.Event` by name (`sendSync {callEvent}`). */
const CALL_EVENT_TYPES = { audio: 1, video: 2, group: 3, adHoc: 4 };
const CALL_EVENT_DIRECTIONS = { incoming: 1, outgoing: 2 };
const CALL_EVENT_EVENTS = { accepted: 1, notAccepted: 2, delete: 3, observed: 4 };
/** `SyncMessage.AttachmentBackfillResponse.AttachmentData.Status` and `.Error` by name. */
const BACKFILL_STATUS = { pending: 0, terminalError: 1 };
const BACKFILL_ERROR = { messageNotFound: 0 };
/** `CallMessage.Hangup.Type` by name (`sendCall {hangup}`). */
const HANGUP_TYPES = { normal: 0, accepted: 1, declined: 2, busy: 3, needPermission: 4 };
/** `SyncMessage.MessageRequestResponse.Type` by name. */
const REQUEST_RESPONSES = { accept: 1, delete: 2, block: 3, blockAndDelete: 4, spam: 5, blockAndSpam: 6 };

/**
 * Desktop's mock remote config, copied from SD/ts/test-mock/bootstrap.node.ts:186-226
 * (Signal-Desktop at the commit the research reports cite). It is served by `/v2/config`:
 * 1:1 and sync sends stay on the websocket (`value: 'ws'`), the attachment upload form and
 * multi-recipient sends use gRPC, which is the only transport the mock offers for them.
 */
const DEFAULT_REMOTE_CONFIG = [
  ['desktop.internalUser', { enabled: true }],
  ['desktop.senderKey.retry', { enabled: true }],
  ['global.backups.mediaTierFallbackCdnNumber', { enabled: true, value: '3' }],
  ['global.groupsv2.groupSizeHardLimit', { enabled: true, value: '64' }],
  ['global.groupsv2.maxGroupSize', { enabled: true, value: '32' }],
  ['global.pinnedChatLimit', { enabled: true, value: '4' }],
  ['desktop.libsignalNet.grpc.AccountsAnonymousLookupUsernameHash', { enabled: true }],
  ['desktop.libsignalNet.grpc.AccountsAnonymousLookupUsernameLink.2', { enabled: true }],
  ['desktop.libsignalNet.grpc.MessagesAnonymousSendMultiRecipientMessage.2', { enabled: true }],
  ['desktop.libsignalNet.grpc.AttachmentsGetUploadForm', { enabled: true }],
  ['desktop.libsignalNet.grpc.BackupsAnonymousGetUploadForm', { enabled: true }],
  [
    'desktop.libsignalNet.grpc.AccountsAnonymousCheckAccountExistence.2',
    { enabled: true, value: 'ws' },
  ],
  ['desktop.libsignalNet.grpc.KeyTransparencyQueryServiceSearchV2', { enabled: true, value: 'ws' }],
  [
    'desktop.libsignalNet.grpc.MessagesAnonymousSendSingleRecipientMessage',
    { enabled: true, value: 'ws' },
  ],
  ['desktop.libsignalNet.grpc.MessagesSendMessage', { enabled: true, value: 'ws' }],
];

/** `Member.Role` by name. */
const MEMBER_ROLE = { default: 1, administrator: 2 };
/** `AccessControl.AccessRequired`: what Desktop creates groups with. */
const ACCESS_MEMBER = 3;
const ACCESS_UNSATISFIABLE = 4;

/** `UnidentifiedSenderMessage.Message.ContentHint` by name (libsignal's `ContentHint`). */
const CONTENT_HINTS = { default: 0, resendable: 1, implicit: 2 };

/** `ManifestRecord.Identifier.Type.CONTACT`. */
const STORAGE_CONTACT = 1;
/** `AccountRecord.PhoneNumberSharingMode` by name. */
const NUMBER_SHARING = { everybody: 1, nobody: 2 };
/** `SyncMessage.FetchLatest.Type` by name. */
const FETCH_LATEST = { localProfile: 1, storageManifest: 2, subscriptionStatus: 3 };
/** Most synthetic contacts one `setStorageState` adds. */
const MAX_EXTRA_CONTACTS = 5000;
/** Desktop's padded lengths of profile fields (`SD/ts/Crypto.node.ts` `PaddedLengths`). */
const PADDED_NAME = [53, 257];
const PADDED_ABOUT = [128, 254, 512];
const PADDED_EMOJI = [32];

// ---------------------------------------------------------------------------------------------
// Profiles: the mock serves name and keys only; avatar and "about" come from here
// ---------------------------------------------------------------------------------------------

/** Extra profile fields by ACI (`avatar` CDN0 path, encrypted `about`/`aboutEmoji`). */
const profileExtras = new Map();

/**
 * `limitProfileFetches`: every profile request answers 429 with this `Retry-After` (seconds)
 * while set (`null`: served); `refused` counts the answers.
 */
const profileLimit = { retryAfter: null, refused: 0 };

{
  const register = Router.prototype.register;
  Router.prototype.register = function registerWithProfileExtras(method, pattern, handler) {
    if (method !== 'GET' || !pattern.startsWith('/v1/profile/:serviceId')) {
      return register.call(this, method, pattern, handler);
    }
    const withExtras = async (params, ...rest) => {
      if (profileLimit.retryAfter !== null) {
        // The real server's answer to profile fetches over its rate limit.
        profileLimit.refused += 1;
        return [429, { error: 'rate limited' }, { 'retry-after': String(profileLimit.retryAfter) }];
      }
      const response = await handler(params, ...rest);
      const extras = profileExtras.get(params.serviceId);
      if (response[0] === 200 && extras && response[1] && typeof response[1] === 'object') {
        return [response[0], { ...response[1], ...extras }, ...response.slice(2)];
      }
      return response;
    };
    return register.call(this, method, pattern, withExtras);
  };
}

// ---------------------------------------------------------------------------------------------
// Sends: per-recipient rate limits, group send token checks, counters
// ---------------------------------------------------------------------------------------------

/**
 * What the harness does to client sends (`rateLimit`, `rejectGroupSendTokens`) and what it saw
 * (`sendStats`). The mock serves 1:1 sends on the websocket router and multi-recipient sends
 * only over gRPC; both are wrapped here before the server registers its routes.
 */
const sends = {
  /** Rate-limited recipients: service id -> `{left, retryAfter}` (`left` < 0 = until stopped). */
  rateLimited: new Map(),
  /** Answer every multi-recipient send with `FailedUnidentifiedAuthorization`. */
  rejectTokens: false,
  /** 1:1 sends (`PUT /v1/messages/:serviceId`) by destination service id, rate-limited ones included. */
  single: new Map(),
  /**
   * The `urgent` flag of every 1:1 send by destination service id, in order: `{timestamp,
   * urgent}` (the request's own, which the mock server does not keep).
   */
  urgency: new Map(),
  /** Multi-recipient sends served (after the token check). */
  multi: 0,
  /**
   * Every multi-recipient send a client asked for, served or not, on either route (gRPC, and
   * the websocket route the mock does not serve).
   */
  multiRequests: 0,
  /**
   * Every `GET /v1/certificate/delivery` (a client fetching its sender certificate), by path
   * (with its query: the kind without the number asks `?includeE164=false`), in order.
   */
  certificateRequests: [],
  /** Recipients (service ids) of every multi-recipient send, in order. */
  multiRecipients: [],
  /** Multi-recipient sends refused for their group send token. */
  refusedTokens: 0,
  /** Answer every multi-recipient send with a frame the client's typed API has no case for. */
  unexpected: false,
  /** Multi-recipient sends answered that way. */
  unexpectedAnswers: 0,
  /**
   * Recipients whose 1:1 sends are refused with a challenge (`challenge`): service id ->
   * `{token, accept, retryAfter, refused}`.
   */
  challenges: new Map(),
  /** Every `PUT /v1/challenge` a client sent: `{type, token, captcha, accepted, status}`. */
  challengeAnswers: [],
  /** Answers to refuse with 413 before one is looked at (`challenge {busy}`). */
  busyAnswers: 0,
  /**
   * The challenge every multi-recipient send over the websocket route meets
   * (`challengeMultiRecipient`): `{token, retryAfter, refused}`, or null.
   */
  multiChallenge: null,
  /** `GET /v1/keepalive` requests served. */
  keepalives: 0,
  /** Answer keepalives with 500 (`failKeepalive`). */
  failKeepalives: false,
  /** How long the answer to `PUT /v1/devices/link` is held back after the link (`delayLink`), ms. */
  linkDelayMs: 0,
  /** Every `PUT /v1/devices/link`: `{arrived, answered, status}` (ms since the epoch). */
  linkRequests: [],
  /**
   * History transfer (`GET /v1/devices/transfer_archive`): every long-poll a client sent,
   * `{at, timeout (the seconds it asked for, or null), status (204, 200, the status of an
   * injected failure, or 'silent')}`.
   */
  transferPolls: [],
  /** Never answer the long-poll (`holdTransferArchive {silent}`): the client's own timeout ends it. */
  transferSilent: false,
  /** Answer this many long-polls with 500 first (`holdTransferArchive {fail}`). */
  transferFailures: 0,
  /** The status of those answers (`holdTransferArchive {status}`). */
  transferFailureStatus: 500,
  /**
   * Answer every long-poll with 204 at once, without holding it (`holdTransferArchive
   * {immediate}`): a server, or a proxy in front of it, that does not hold the request.
   */
  transferImmediate: false,
};

{
  const register = Router.prototype.register;
  Router.prototype.register = function registerWithRateLimits(method, pattern, handler) {
    if (method !== 'PUT' || pattern !== '/v1/messages/:serviceId') {
      return register.call(this, method, pattern, handler);
    }
    const limited = async (params, ...rest) => {
      const target = params.serviceId;
      sends.single.set(target, (sends.single.get(target) ?? 0) + 1);
      try {
        const list = JSON.parse(Buffer.from(rest[0] ?? []).toString());
        const flags = sends.urgency.get(target) ?? [];
        flags.push({ timestamp: Number(list?.timestamp ?? 0), urgent: list?.urgent === true });
        sends.urgency.set(target, flags);
      } catch {
        // A body the mock refuses too: nothing to record.
      }
      const challenge = sends.challenges.get(target);
      if (challenge) {
        // The real server's answer to a send it wants a person to vouch for.
        challenge.refused += 1;
        return [
          428,
          { token: challenge.token, options: ['captcha'] },
          { 'retry-after': String(challenge.retryAfter) },
        ];
      }
      const limit = sends.rateLimited.get(target);
      if (limit && limit.left !== 0) {
        if (limit.left > 0) {
          limit.left -= 1;
        }
        // The real server's answer to a send over its rate limit.
        return [429, { error: 'rate limited' }, { 'retry-after': String(limit.retryAfter) }];
      }
      return handler(params, ...rest);
    };
    return register.call(this, method, pattern, limited);
  };
}

// ---------------------------------------------------------------------------------------------
// Robustness (S15): challenge answers, keepalives, key uploads, a device removing itself
// ---------------------------------------------------------------------------------------------

{
  const register = Router.prototype.register;
  Router.prototype.register = function registerWithRobustness(method, pattern, handler) {
    if (method === 'PUT' && pattern === '/v1/challenge') {
      // The mock answers every challenge answer alike; here an answer has to name a challenge
      // that is open and (when the test set one) bring the expected captcha, and lifts it.
      const answered = async (params, body, ...rest) => {
        const original = await handler(params, body, ...rest);
        if (original[0] !== 200) {
          return original;
        }
        let answer;
        try {
          answer = JSON.parse(Buffer.from(body ?? []).toString());
        } catch {
          return [422, { error: 'Malformed challenge answer' }];
        }
        const record = {
          type: String(answer?.type ?? ''),
          token: String(answer?.token ?? ''),
          captcha: String(answer?.captcha ?? ''),
          accepted: false,
          status: 200,
        };
        sends.challengeAnswers.push(record);
        const refuse = (status, json, headers = {}) => {
          record.status = status;
          return [status, json, headers];
        };
        if (sends.busyAnswers > 0) {
          sends.busyAnswers -= 1;
          return refuse(413, { error: 'Too many attempts' }, { 'retry-after': '2' });
        }
        const open = [...sends.challenges.entries()].find(([, challenge]) => challenge.token === record.token);
        if (record.type !== 'captcha' || !open) {
          return refuse(400, { error: 'No such challenge' });
        }
        const [target, challenge] = open;
        if (challenge.accept !== undefined && challenge.accept !== record.captcha) {
          // What the real server answers to a wrong solution is not documented; the harness
          // asks again with the same token.
          return refuse(428, { token: challenge.token, options: ['captcha'] });
        }
        sends.challenges.delete(target);
        record.accepted = true;
        return [200, { ok: true }];
      };
      return register.call(this, method, pattern, answered);
    }
    if (method === 'PUT' && pattern === '/v1/devices/link') {
      // `delayLink`: the device is linked, and the client hears of it only later. What a
      // client does that is cancelled or logged out in between decides whether a device
      // nobody drives stays on the account.
      const held = async (...args) => {
        const record = { arrived: Date.now(), answered: null, status: null };
        sends.linkRequests.push(record);
        const answer = await handler(...args);
        if (sends.linkDelayMs > 0) {
          await new Promise((resolve) => setTimeout(resolve, sends.linkDelayMs));
        }
        record.answered = Date.now();
        record.status = answer?.[0] ?? null;
        return answer;
      };
      return register.call(this, method, pattern, held);
    }
    if (method === 'GET' && pattern === '/v1/devices/transfer_archive') {
      // The real server holds this request for the `timeout` (seconds) the client asks for
      // and answers 204 when the primary has provided nothing by then; the mock waits for
      // ever. Here: 204 after the timeout, and the faults of `holdTransferArchive`.
      const polled = async (params, body, headers, query) => {
        const asked = Number(query?.timeout);
        const record = { at: Date.now(), timeout: Number.isFinite(asked) ? asked : null, status: null };
        sends.transferPolls.push(record);
        if (sends.transferSilent) {
          record.status = 'silent';
          return new Promise(() => {});
        }
        if (sends.transferFailures > 0) {
          sends.transferFailures -= 1;
          record.status = sends.transferFailureStatus;
          return [sends.transferFailureStatus, { error: 'injected failure (holdTransferArchive)' }];
        }
        if (sends.transferImmediate) {
          record.status = 204;
          return [204, {}];
        }
        const waitMs = Math.min(Number.isFinite(asked) && asked > 0 ? asked * 1000 : 30_000, 300_000);
        let timer;
        const nothing = new Promise((resolve) => {
          timer = setTimeout(() => resolve(null), waitMs);
        });
        const answer = await Promise.race([handler(params, body, headers, query), nothing]);
        clearTimeout(timer);
        if (answer === null) {
          record.status = 204;
          return [204, {}];
        }
        record.status = answer[0];
        return answer;
      };
      return register.call(this, method, pattern, polled);
    }
    if (method === 'GET' && pattern === '/v1/keepalive') {
      const counted = async (...args) => {
        sends.keepalives += 1;
        if (sends.failKeepalives) {
          return [500, { error: 'injected keepalive failure (failKeepalive)' }];
        }
        return handler(...args);
      };
      return register.call(this, method, pattern, counted);
    }
    if (method === 'PUT' && pattern === '/v2/keys') {
      // The real server takes an upload without one-time keys (a rotation of the signed prekey
      // and the last-resort key); the mock's schema insists on a `preKeys` array.
      const lenient = async (params, rawBody, ...rest) => {
        let body = rawBody;
        try {
          const json = JSON.parse(Buffer.from(rawBody ?? []).toString());
          if (json && typeof json === 'object' && !Array.isArray(json.preKeys)) {
            body = Buffer.from(JSON.stringify({ ...json, preKeys: [] }));
          }
        } catch {
          // Left to the mock's own error handling.
        }
        return handler(params, body, ...rest);
      };
      return register.call(this, method, pattern, lenient);
    }
    return register.call(this, method, pattern, handler);
  };
}

{
  const handleRequest = MockConnection.prototype.handleRequest;
  // `DELETE /v1/devices/<id>` (Desktop's `unlink`): the primary may remove any linked device, a
  // linked device only itself. The device's socket is closed after the answer, as the real
  // server closes the connections of a device that no longer exists.
  MockConnection.prototype.handleRequest = async function handleRequestWithDeviceRemoval(request) {
    const match = request.verb === 'DELETE' ? /^\/v1\/devices\/(\d+)$/.exec(request.path ?? '') : null;
    if (!match) {
      return handleRequest.call(this, request);
    }
    const respond = (status, json) => ({
      id: request.id,
      status,
      message: null,
      headers: ['Content-Type:application/json'],
      body: json === undefined ? null : Buffer.from(JSON.stringify(json)),
    });
    const requester = this.device;
    if (!requester) {
      return respond(401, { error: 'Not authenticated' });
    }
    const deviceId = Number(match[1]);
    const allowed =
      deviceId !== PRIMARY_DEVICE_ID &&
      (requester.deviceId === PRIMARY_DEVICE_ID || requester.deviceId === deviceId);
    if (!allowed) {
      return respond(403, { error: 'Not allowed to remove this device' });
    }
    const device = await activeServer.getDeviceByServiceId(requester.aci, deviceId);
    if (!device) {
      return respond(404, { error: 'No such device' });
    }
    await activeServer.removeDevice(device, { closeSocket: false });
    setImmediate(() => activeServer.closeSocket(device, 4401));
    return respond(204);
  };
}

{
  const handleRequest = MockConnection.prototype.handleRequest;
  // `PUT /v1/messages/multi_recipient` over a websocket (the route a client uses while the
  // remote config does not move multi-recipient sends to gRPC): the mock has no such route
  // and answers 404. With `challengeMultiRecipient` it answers as the real server answers a
  // send it wants a person to vouch for: 428 with the token, the options and Retry-After.
  MockConnection.prototype.handleRequest = async function handleRequestWithMultiChallenge(request) {
    const challenge = sends.multiChallenge;
    const multi = request.verb === 'PUT' && /^\/v1\/messages\/multi_recipient(\?|$)/.test(request.path ?? '');
    if (multi) {
      sends.multiRequests += 1;
    }
    if (!multi || !challenge) {
      return handleRequest.call(this, request);
    }
    challenge.refused += 1;
    return {
      id: request.id,
      status: 428,
      message: null,
      headers: ['Content-Type:application/json', `Retry-After:${challenge.retryAfter}`],
      body: Buffer.from(JSON.stringify({ token: challenge.token, options: ['captcha'] })),
    };
  };
}

{
  const handleRequest = MockConnection.prototype.handleRequest;
  // The sender certificate a client seals its sends with: a group send must fetch it once,
  // not once per member (`sendStats.certificateRequests`).
  MockConnection.prototype.handleRequest = async function handleRequestCountingCertificates(request) {
    if (request.verb === 'GET' && /^\/v1\/certificate\/delivery(\?|$)/.test(request.path ?? '')) {
      sends.certificateRequests.push(request.path);
    }
    return handleRequest.call(this, request);
  };
}

/**
 * A round trip added to every request a client makes (`setLatency`): its answer is held back
 * `ms` (0 = none). Websocket requests and the gRPC multi-recipient route; the server's own
 * pushes (envelopes) are not delayed.
 */
const latency = { ms: 0 };

/** Waits the configured round trip, if any. */
function roundTrip() {
  return latency.ms > 0 ? new Promise((resolve) => setTimeout(resolve, latency.ms)) : undefined;
}

{
  const handleRequest = MockConnection.prototype.handleRequest;
  MockConnection.prototype.handleRequest = async function handleRequestWithLatency(request) {
    await roundTrip();
    return handleRequest.call(this, request);
  };
}

/**
 * The clock the server states when it delivers an envelope: `skewMs` is added to the true
 * time (`setServerTime`).
 */
const deliveryClock = { skewMs: 0 };

{
  // The real server states its clock on every envelope it delivers (`X-Signal-Timestamp` of
  // the `PUT /api/v1/message` request). Clients measure how long an envelope waited on the
  // server as that time minus the envelope's `serverTimestamp`: the age of a call's ring
  // (Desktop's `messageAgeSec`). The mock sends the request without any header.
  MockConnection.prototype.sendMessage = async function sendMessageWithDeliveryTime(message) {
    const response =
      message === 'empty'
        ? await this.send('PUT', '/api/v1/queue/empty', {})
        : await this.send('PUT', '/api/v1/message', {
            body: message,
            headers: [`X-Signal-Timestamp:${Date.now() + deliveryClock.skewMs}`],
          });
    assert.strictEqual(response.status, 200, `WebSocket send error ${response.status} ${response.message}`);
  };
}

/** The mock's gRPC route of multi-recipient sends (it serves no websocket route for them). */
const MULTI_RECIPIENT_ROUTE = '/org.signal.chat.messages.MessagesAnonymous/SendMultiRecipientMessage';

{
  const microrouter = require('microrouter');
  const { buffer: readBody } = require('micro');
  const grpcServices = require('@signalapp/mock-server/protos/compiled.js').$services;
  const SealedSenderMultiRecipientMessage = require('@signalapp/libsignal-client/dist/SealedSenderMultiRecipientMessage').default;
  const post = microrouter.post;
  microrouter.post = function postWithTokenChecks(pattern, handler) {
    if (pattern !== MULTI_RECIPIENT_ROUTE) {
      return post.call(this, pattern, handler);
    }
    const definition = grpcServices[MULTI_RECIPIENT_ROUTE.slice(1)];
    /** Answers `res` with one gRPC message (status OK). */
    const respond = (res, message) => {
      const body = Buffer.from(definition.Response.encode(message));
      const header = Buffer.alloc(5);
      header.writeUint32BE(body.length, 1);
      res.writeHead(200, { 'content-type': 'application/grpc' });
      res.addTrailers({ 'grpc-status': '0' });
      res.end(Buffer.concat([header, body]));
      return undefined;
    };
    const checked = async (req, res) => {
      // micro caches the body, so the mock's handler reads it again.
      const raw = await readBody(req, { limit: '16mb' });
      await roundTrip();
      sends.multiRequests += 1;
      if (sends.unexpected) {
        // A response without its `response` case, as a server version the client does not know
        // might answer.
        sends.unexpectedAnswers += 1;
        return respond(res, {});
      }
      let accepted = false;
      try {
        const request = definition.Request.decode(raw.subarray(5));
        const payload = Buffer.from(request.message?.payload ?? []);
        const recipients = Object.keys(new SealedSenderMultiRecipientMessage(payload).recipientsByServiceIdString());
        const token = new zkgroup.GroupSendFullToken(Buffer.from(request.groupSendToken ?? []));
        const keyPair = zkgroup.GroupSendDerivedKeyPair.forExpiration(token.getExpiration(), activeServer.zkSecret);
        // The mock does not check the token itself; the real server does.
        token.verify(recipients.map((id) => SignalClient.ServiceId.parseFromServiceIdString(id)), keyPair);
        accepted = !sends.rejectTokens;
        if (accepted) {
          sends.multi += 1;
          sends.multiRecipients.push(...recipients);
        }
      } catch (error) {
        log('multi-recipient send refused', error instanceof Error ? error.message : error);
      }
      if (accepted) {
        return handler(req, res);
      }
      sends.refusedTokens += 1;
      return respond(res, { response: { failedUnidentifiedAuthorization: {} } });
    };
    return post.call(this, pattern, checked);
  };
}

/** The running server (the token check needs its zkgroup secret). */
let activeServer;

/** Desktop's attachment padding (`SD/ts/util/logPadSize.std.ts`). */
function logPadSize(size) {
  return Math.max(541, Math.floor(1.05 ** Math.ceil(Math.log(size) / Math.log(1.05))));
}

/** AES-256-GCM as Desktop's `encryptProfile`: 12-byte IV ‖ ciphertext ‖ 16-byte tag. */
function encryptProfileValue(key, plaintext) {
  const iv = nodeCrypto.randomBytes(12);
  const cipher = nodeCrypto.createCipheriv('aes-256-gcm', key, iv);
  const body = Buffer.concat([cipher.update(plaintext), cipher.final()]);
  return Buffer.concat([iv, body, cipher.getAuthTag()]);
}

/** A profile field padded with zeros to the first of `lengths` that fits, then encrypted. */
function encryptPadded(key, text, lengths) {
  const bytes = Buffer.from(text, 'utf8');
  const length = lengths.find((n) => bytes.length <= n);
  if (length === undefined) {
    throw badRequest(`profile field longer than ${lengths[lengths.length - 1]} bytes`);
  }
  const padded = Buffer.alloc(length);
  bytes.copy(padded);
  return encryptProfileValue(key, padded);
}

/**
 * Desktop's `encryptAttachmentV2` (`SD/ts/AttachmentCrypto.node.ts`): 64 random key bytes,
 * `IV ‖ AES-256-CBC(plaintext ‖ zeros to logPadSize) ‖ HMAC-SHA256`, digest = SHA-256 of it all.
 */
function encryptAttachment(plaintext) {
  const padded = Buffer.alloc(Math.max(plaintext.length, logPadSize(plaintext.length)));
  plaintext.copy(padded);
  const aesKey = nodeCrypto.randomBytes(32);
  const macKey = nodeCrypto.randomBytes(32);
  const iv = nodeCrypto.randomBytes(16);
  const cipher = nodeCrypto.createCipheriv('aes-256-cbc', aesKey, iv);
  const body = Buffer.concat([iv, cipher.update(padded), cipher.final()]);
  const mac = nodeCrypto.createHmac('sha256', macKey).update(body).digest();
  const blob = Buffer.concat([body, mac]);
  return {
    blob,
    key: Buffer.concat([aesKey, macKey]),
    digest: nodeCrypto.createHash('sha256').update(blob).digest(),
    size: plaintext.length,
  };
}

/** Bytes of a base64 string or a byte array from a JSON pointer (`jsonSafe` writes base64). */
function pointerBytes(value, name) {
  if (typeof value === 'string') {
    return Buffer.from(value, 'base64');
  }
  if (Array.isArray(value)) {
    return Buffer.from(value);
  }
  throw badRequest(`pointer "${name}" missing`);
}

/**
 * A wire pointer back from its JSON (`jsonSafe`, as `sendAttachments`, `waitForMessage` and
 * `sendSync {attachmentBackfillResponse}` print it): base64 back to bytes, numbers back to
 * `BigInt` where the proto has a u64.
 */
function pointerFromJson(value, name) {
  if (typeof value !== 'object' || value === null) {
    throw badRequest(`"${name}" must be a pointer object`);
  }
  const bytes = (field) => (value[field] == null ? null : pointerBytes(value[field], `${name}.${field}`));
  const identifier = value.attachmentIdentifier ?? {};
  return {
    attachmentIdentifier:
      typeof identifier.cdnKey === 'string'
        ? { cdnKey: identifier.cdnKey }
        : identifier.cdnId != null
          ? { cdnId: BigInt(identifier.cdnId) }
          : null,
    cdnNumber: value.cdnNumber ?? null,
    contentType: value.contentType ?? null,
    key: bytes('key'),
    size: value.size ?? null,
    digest: bytes('digest'),
    fileName: value.fileName ?? null,
    flags: value.flags ?? null,
    width: value.width ?? null,
    height: value.height ?? null,
    caption: value.caption ?? null,
    blurHash: value.blurHash ?? null,
    uploadTimestamp: value.uploadTimestamp == null ? null : BigInt(value.uploadTimestamp),
    clientUuid: bytes('clientUuid'),
    thumbnail: null,
    incrementalMac: bytes('incrementalMac'),
    chunkSize: value.chunkSize ?? null,
    audioDurationSeconds: value.audioDurationSeconds ?? null,
  };
}

/** Desktop's long-text limit: a longer body goes out trimmed with a text attachment. */
const MAX_INLINE_BODY_BYTES = 2048;

/** `trimBody`: the longest prefix of at most 2048 UTF-8 bytes on a character boundary. */
function trimBody(text) {
  let out = '';
  for (const ch of text) {
    if (Buffer.byteLength(out + ch) > MAX_INLINE_BODY_BYTES) {
      break;
    }
    out += ch;
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Output: stdout is the protocol channel, everything else goes to stderr.
// ---------------------------------------------------------------------------------------------

// A stray console.log from a dependency must not corrupt the protocol stream.
console.log = console.error;
console.info = console.error;
console.debug = console.error;

function emit(line) {
  process.stdout.write(`${JSON.stringify(line)}\n`);
}

// The driving process closed its end of stdout (EPIPE): nobody can hear us any more. Exit at
// once; reporting the error would only fail again.
process.stdout.on('error', () => {
  process.exit(0);
});

function log(...args) {
  console.error('[harness]', ...args);
}

function emitEvent(event, data = {}) {
  emit({ type: 'event', event, data });
}

// ---------------------------------------------------------------------------------------------
// Errors and argument validation
// ---------------------------------------------------------------------------------------------

/** An error with a stable `code` that is reported to the client. */
class HarnessError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'HarnessError';
    this.code = code;
  }
}

const badRequest = (message) => new HarnessError('badRequest', message);
const notFound = (message) => new HarnessError('notFound', message);

function errorToWire(error) {
  if (error instanceof HarnessError) {
    return { code: error.code, message: error.message };
  }
  const message = error instanceof Error ? error.message : String(error);
  const stack = error instanceof Error ? error.stack : undefined;
  return { code: 'failed', message, ...(stack ? { stack } : {}) };
}

function reqString(args, name) {
  const value = args[name];
  if (typeof value !== 'string' || value.length === 0) {
    throw badRequest(`"${name}" must be a non-empty string`);
  }
  return value;
}

function optString(args, name) {
  const value = args[name];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== 'string') {
    throw badRequest(`"${name}" must be a string`);
  }
  return value;
}

function optBool(args, name, fallback) {
  const value = args[name];
  if (value === undefined || value === null) {
    return fallback;
  }
  if (typeof value !== 'boolean') {
    throw badRequest(`"${name}" must be a boolean`);
  }
  return value;
}

function optInt(args, name, fallback, { min = 0, max = Number.MAX_SAFE_INTEGER } = {}) {
  const value = args[name];
  if (value === undefined || value === null) {
    return fallback;
  }
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw badRequest(`"${name}" must be an integer in ${min}..=${max}`);
  }
  return value;
}

function optArray(args, name) {
  const value = args[name];
  if (value === undefined || value === null) {
    return [];
  }
  if (!Array.isArray(value)) {
    throw badRequest(`"${name}" must be an array`);
  }
  return value;
}

function timeoutArg(args, fallback = DEFAULT_WAIT_MS) {
  return optInt(args, 'timeout', fallback, { min: 1, max: MAX_TIMEOUT_MS });
}

/** `serviceIdKind?`: `"ACI"` (default) or `"PNI"`. */
function serviceIdKindArg(args) {
  const kind = optString(args, 'serviceIdKind') ?? ServiceIdKind.ACI;
  if (kind !== ServiceIdKind.ACI && kind !== ServiceIdKind.PNI) {
    throw badRequest('"serviceIdKind" must be "ACI" or "PNI"');
  }
  return kind;
}

/** Resolves `promise` or rejects with a `timeout` error after `ms` milliseconds. */
function withTimeout(promise, ms, what) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new HarnessError('timeout', `${what}: timed out after ${ms} ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

// ---------------------------------------------------------------------------------------------
// JSON conversion of mock-server objects
// ---------------------------------------------------------------------------------------------

const b64 = (bytes) => Buffer.from(bytes).toString('base64');

function toNumber(value) {
  if (value === null || value === undefined) {
    return null;
  }
  return typeof value === 'bigint' ? Number(value) : value;
}

/**
 * Converts a decoded protobuf (or any mock-server value) into plain JSON: bytes become base64,
 * safe bigints numbers (others decimal strings), null and undefined fields are dropped.
 */
function jsonSafe(value, depth = 0) {
  if (depth > 32) {
    return '<too deep>';
  }
  if (value === null || value === undefined) {
    return undefined;
  }
  if (typeof value === 'bigint') {
    return value <= BigInt(Number.MAX_SAFE_INTEGER) && value >= BigInt(Number.MIN_SAFE_INTEGER)
      ? Number(value)
      : value.toString();
  }
  if (value instanceof Uint8Array) {
    return b64(value);
  }
  if (Array.isArray(value)) {
    return value.map((item) => jsonSafe(item, depth + 1) ?? null);
  }
  if (typeof value === 'object') {
    const out = {};
    for (const [key, item] of Object.entries(value)) {
      const converted = jsonSafe(item, depth + 1);
      if (converted !== undefined) {
        out[key] = converted;
      }
    }
    return out;
  }
  if (typeof value === 'function' || typeof value === 'symbol') {
    return undefined;
  }
  return value;
}

/** `<aci>.<deviceId>`: how devices are named on the wire. */
function deviceRef(device) {
  return `${device.aci}.${device.deviceId}`;
}

function describeDevice(device) {
  return {
    ref: deviceRef(device),
    aci: device.aci,
    pni: device.pni ?? null,
    deviceId: device.deviceId,
    e164: device.number ?? null,
  };
}

const REF_RE = /^([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:\.([1-9][0-9]*))?$/;

function parseRef(ref, name) {
  if (typeof ref !== 'string') {
    throw badRequest(`"${name}" must be a device ref "<aci>" or "<aci>.<deviceId>"`);
  }
  const match = REF_RE.exec(ref.toLowerCase());
  if (!match) {
    throw badRequest(`"${name}": malformed device ref ${JSON.stringify(ref)}`);
  }
  return { aci: match[1], deviceId: match[2] === undefined ? undefined : Number(match[2]) };
}

/** Whether `device` matches a ref filter: a bare ACI matches every device of the account. */
function refMatches(filter, device) {
  if (device.aci !== filter.aci) {
    return false;
  }
  return filter.deviceId === undefined || filter.deviceId === device.deviceId;
}

/** The 16 bytes of an ACI (`<uuid>` string), as `*Binary` proto fields carry it. */
function aciBytes(aci) {
  return Buffer.from(aci.replace(/-/g, ''), 'hex');
}

/** A Content with a SyncMessage of these fields and Desktop-style random padding. */
function syncContent(fields) {
  return {
    content: {
      syncMessage: {
        content: null,
        stickerPackOperation: null,
        read: null,
        viewed: null,
        padding: Buffer.alloc(1 + Math.floor(Math.random() * 512)),
        ...fields,
      },
    },
    pniSignatureMessage: null,
    senderKeyDistributionMessage: null,
  };
}

/**
 * `envelope` (encoded) with the server timestamp `serverTimestamp`: the mock's primaries copy
 * the client timestamp into it, a real server stamps the time it took the message.
 */
function restamp(envelope, serverTimestamp) {
  const decoded = Proto.Envelope.decode(envelope);
  return Buffer.from(Proto.Envelope.encode({ ...decoded, serverTimestamp: BigInt(serverTimestamp) }));
}

/** What a decoded DataMessage is: a `reaction`, a `delete`, an `adminDelete`, a `timer`
 * update (the EXPIRATION_TIMER_UPDATE flag), or a `message`. */
function dataKind(dataMessage) {
  if (dataMessage?.reaction) {
    return 'reaction';
  }
  if (dataMessage?.delete) {
    return 'delete';
  }
  if (dataMessage?.adminDelete) {
    return 'adminDelete';
  }
  if ((dataMessage?.flags ?? 0) & 2) {
    return 'timer';
  }
  return 'message';
}

/** What a decoded SyncMessage carries: `read`, `viewed`, or the name of its content field. */
function syncKind(sync) {
  if (sync?.read?.length) {
    return 'read';
  }
  if (sync?.viewed?.length) {
    return 'viewed';
  }
  const content = sync?.content ?? {};
  return Object.keys(content).find((key) => content[key] !== null && content[key] !== undefined) ?? 'empty';
}

// ---------------------------------------------------------------------------------------------
// Mailbox: a buffered queue with predicate waits and timeouts
// ---------------------------------------------------------------------------------------------

/**
 * Holds items until a `take` claims them. Unlike the mock server's own queues, a timed out
 * `take` never swallows an item that arrives later.
 */
class Mailbox {
  #items = [];
  #waiters = [];

  constructor(name) {
    this.name = name;
  }

  get size() {
    return this.#items.length;
  }

  push(item) {
    const index = this.#waiters.findIndex((waiter) => waiter.predicate(item));
    if (index !== -1) {
      const [waiter] = this.#waiters.splice(index, 1);
      waiter.resolve(item);
      return;
    }
    this.#items.push(item);
  }

  /** Removes every buffered item that matches `predicate`. */
  remove(predicate) {
    this.#items = this.#items.filter((item) => !predicate(item));
  }

  take(predicate, timeoutMs, what) {
    const index = this.#items.findIndex(predicate);
    if (index !== -1) {
      return Promise.resolve(this.#items.splice(index, 1)[0]);
    }
    return new Promise((resolve, reject) => {
      const waiter = {
        predicate,
        resolve: (item) => {
          clearTimeout(timer);
          resolve(item);
        },
      };
      const timer = setTimeout(() => {
        const at = this.#waiters.indexOf(waiter);
        if (at !== -1) {
          this.#waiters.splice(at, 1);
        }
        reject(new HarnessError('timeout', `${what}: timed out after ${timeoutMs} ms`));
      }, timeoutMs);
      this.#waiters.push(waiter);
    });
  }
}

// ---------------------------------------------------------------------------------------------
// Server subclass: learns which provisioning address each pending provision belongs to
// ---------------------------------------------------------------------------------------------

/**
 * `Server.waitForProvision()` hands out pending provisions in the order the provisioning
 * sockets asked for them, without saying which socket (address) each belongs to. Completing
 * the wrong one fails the mock's `uuid` check, so this subclass records the addresses in the
 * same order. `getProvisioningResponse` pushes onto the mock's queue synchronously, before its
 * first `await`, so both FIFOs stay aligned; an entry that is aborted before it is handed out
 * leaves both.
 */
class HarnessServer extends Server {
  #openedAddresses = [];
  /** Envelopes handed to `send` and not yet acknowledged by their device, by device. */
  #inFlight = new Map();
  /** `{device, resolve}` of `waitForDeliveries` callers. */
  #idleWaiters = [];
  /** Set when the harness shuts down: lost envelopes are not delivered again. */
  stopping = false;
  /** Groups (base64 public params) whose Storage Service requests fail (`failGroupFetch`). */
  failingGroups = new Set();

  /**
   * Every group route of the mock's Storage Service looks its group up here: a failing group
   * throws, which the mock's HTTP layer answers with 500. The harness's own group changes use
   * `getGroupUnchecked`.
   */
  async getGroup(publicParams) {
    const key = Buffer.from(publicParams).toString('base64');
    this.groupFetches.set(key, (this.groupFetches.get(key) ?? 0) + 1);
    if (this.failingGroups.has(key)) {
      throw new Error('injected group fetch failure (failGroupFetch)');
    }
    return super.getGroup(publicParams);
  }

  /** Linked devices' manifest requests fail with 500 (`failStorageManifest`). */
  failingManifest = false;
  /** Manifest requests of linked devices refused that way. */
  refusedManifests = 0;

  /**
   * The Storage Service manifest routes look the manifest up here: while `failingManifest`,
   * a linked device's request throws (500); the primary's own writes and the harness's
   * `storageVersion` (device 1) still see it.
   */
  getStorageManifest(device) {
    if (this.failingManifest && device.deviceId !== 1) {
      this.refusedManifests += 1;
      throw new Error('injected manifest failure (failStorageManifest)');
    }
    return super.getStorageManifest(device);
  }

  /** Storage Service group requests by group (base64 public params), for `sendStats`. */
  groupFetches = new Map();

  /** Uploads clients started on CDN 3 (TUS creation requests), for `sendStats`. */
  cdn3Uploads = 0;

  /**
   * `breakDownloads`: `{afters, after, left, ignoreRange}` while downloads from CDN 3 are
   * broken (`afters`: the cut of each next request in turn; then `after` for `left` more
   * requests, `left` < 0: until stopped), else null.
   */
  downloadFault = null;
  /** Every attachment download asked of CDN 3: `{key, range, status, start, sent, broken}`. */
  downloads = [];

  /**
   * Serves `GET /cdn3/attachments/<key>` itself. The mock answers these with the whole file
   * and knows no `Range` header; a real CDN serves ranges (`206` with `Content-Range`), which
   * is what a client that resumes a download relies on. With `breakDownloads` the transfer is
   * reset after a number of body bytes, as a connection that dropped.
   */
  installCdnGate(cdn3Path) {
    const server = this.https;
    const listeners = server.listeners('request');
    server.removeAllListeners('request');
    server.on('request', (req, res) => {
      let pathname = '';
      try {
        pathname = new URL(req.url ?? '/', 'http://localhost').pathname;
      } catch {
        // Left to the mock.
      }
      const match = /^\/cdn3\/attachments\/([A-Za-z0-9_\-.=]+)$/.exec(pathname);
      if (req.method === 'GET' && match && match[1] !== '.' && match[1] !== '..') {
        this.#serveAttachment(req, res, cdn3Path, match[1]).catch((error) => {
          log('serving an attachment failed', error);
          res.stream?.close(http2.constants.NGHTTP2_INTERNAL_ERROR);
        });
        return;
      }
      for (const listener of listeners) {
        listener.call(server, req, res);
      }
    });
  }

  async #serveAttachment(req, res, cdn3Path, key) {
    const range = typeof req.headers.range === 'string' ? req.headers.range : null;
    const entry = { key, range, status: 200, start: 0, sent: 0, broken: false };
    this.downloads.push(entry);
    let blob;
    try {
      blob = await fs.readFile(path.join(cdn3Path, 'attachments', key));
    } catch (error) {
      entry.status = error?.code === 'ENOENT' ? 404 : 500;
      res.writeHead(entry.status);
      res.end();
      return;
    }
    const fault = this.downloadFault;
    const headers = { 'accept-ranges': 'bytes' };
    const asked = /^bytes=(\d+)-$/.exec(range ?? '');
    if (asked && !fault?.ignoreRange) {
      entry.start = Number(asked[1]);
      if (entry.start >= blob.length) {
        entry.status = 416;
        res.writeHead(416, { 'content-range': `bytes */${blob.length}` });
        res.end();
        return;
      }
      entry.status = 206;
      headers['content-range'] = `bytes ${entry.start}-${blob.length - 1}/${blob.length}`;
    }
    const body = blob.subarray(entry.start);
    headers['content-length'] = body.length;
    let cut = null;
    if (fault && fault.afters.length > 0) {
      cut = fault.afters.shift();
    } else if (fault && fault.left !== 0) {
      if (fault.left > 0) {
        fault.left -= 1;
      }
      cut = fault.after;
    }
    if (cut !== null && cut >= body.length) {
      // Nothing left to cut off: the rest goes out whole.
      cut = null;
    }
    res.writeHead(entry.status, headers);
    if (cut === null) {
      entry.sent = body.length;
      res.end(body);
      return;
    }
    entry.broken = true;
    entry.sent = cut;
    const reset = () => res.stream?.close(http2.constants.NGHTTP2_INTERNAL_ERROR);
    if (cut === 0) {
      reset();
      return;
    }
    // The first part goes out whole, then the stream is reset: the client sees a transfer
    // that broke off after `cut` bytes.
    res.write(body.subarray(0, cut), reset);
  }

  /**
   * Called with `{target, timestamp, error}` when a harness primary could not decrypt a client
   * envelope; returns whether the failure was handled (a real client does not fail the sender's
   * request, it answers with a retry request).
   */
  onDecryptFailure = () => false;

  /** The group without the injected failure. */
  getGroupUnchecked(publicParams) {
    return super.getGroup(publicParams);
  }

  // -- robustness (S15) ------------------------------------------------------------------------

  /** Added to the server clock the websocket upgrade states (`setServerTime`), in ms. */
  timeSkewMs = 0;
  /** Forced upgrade answers (`failAuth`): username -> `{status, left}` (`left` < 0 = until stopped). */
  forcedAuth = new Map();
  /** Authenticated websocket upgrades asked for, by username (refused ones included). */
  connects = new Map();
  /** What each client key upload carried, by `"<device ref>|<kind>"`. */
  keyUploads = new Map();
  /** Called with a device the server removed (the harness tells its primary). */
  onDeviceRemoved = () => {};

  /**
   * Puts a gate in front of the mock's websocket upgrade, which answers every upgrade with 200
   * and closes the socket of unknown credentials afterwards (code 3000). The real server
   * refuses the upgrade itself, and clients tell from that status that they were unlinked:
   * 403 for a device the account no longer has, 401 for a wrong password. The gate also states
   * the server's clock in `x-signal-timestamp`, as the real server does on every upgrade
   * (clients measure their clock skew against it).
   */
  installUpgradeGate() {
    const server = this.https;
    const listeners = server.listeners('connect');
    server.removeAllListeners('connect');
    server.on('connect', (req, res) => {
      const timestamp = { 'x-signal-timestamp': String(Date.now() + this.timeSkewMs) };
      let pathname = '';
      try {
        pathname = new URL(req.url ?? '/', 'http://localhost').pathname;
      } catch {
        // Left to the mock.
      }
      if (req.method === 'CONNECT' && pathname === '/v1/websocket/') {
        const status = this.#upgradeStatus(req.headers.authorization);
        if (status !== 200) {
          log(`websocket upgrade refused with ${status}`);
          res.writeHead(status, timestamp);
          res.end();
          return;
        }
      }
      this.setWebsocketUpgradeResponseHeaders(timestamp);
      for (const listener of listeners) {
        listener.call(server, req, res);
      }
    });
  }

  /** The status of an upgrade with this `Authorization` header (none: an unauthenticated socket). */
  #upgradeStatus(authorization) {
    if (typeof authorization !== 'string') {
      return 200;
    }
    const match = /^Basic\s+(\S+)$/i.exec(authorization);
    if (!match) {
      return 401;
    }
    const decoded = Buffer.from(match[1], 'base64').toString();
    const colon = decoded.indexOf(':');
    const username = colon === -1 ? decoded : decoded.slice(0, colon);
    const password = colon === -1 ? '' : decoded.slice(colon + 1);
    this.connects.set(username, (this.connects.get(username) ?? 0) + 1);
    const forced = this.forcedAuth.get(username);
    if (forced && forced.left !== 0) {
      if (forced.left > 0) {
        forced.left -= 1;
      }
      return forced.status;
    }
    const entry = this.devicesByAuth.get(username);
    if (!entry) {
      return 403;
    }
    return entry.password === password ? 200 : 401;
  }

  /** The username (`<aci>.<deviceId>`) and password of `device`, or undefined once it is removed. */
  credentials(device) {
    for (const [username, entry] of this.devicesByAuth) {
      if (entry.device === device) {
        return { username, password: entry.password };
      }
    }
    return undefined;
  }

  /** Whether `device` has an open websocket. */
  isConnected(device) {
    return this.webSockets.has(device);
  }

  /** Closes `device`'s websocket with `code`; false when it has none. */
  closeSocket(device, code) {
    const socket = this.webSockets.get(device);
    if (!socket) {
      return false;
    }
    socket.close(code);
    return true;
  }

  /**
   * Removes a linked device from its account: its credentials stop working, its primary
   * forgets it, and (unless `closeSocket` is false) its websocket is closed with 4401, as the
   * real server invalidates the connections of a removed device.
   */
  async removeDevice(device, { closeSocket = true } = {}) {
    await this.removeDeviceByServiceId(device.aci, device.deviceId);
    this.onDeviceRemoved(device);
    if (closeSocket) {
      this.closeSocket(device, 4401);
    }
  }

  /**
   * A client's key upload. The real server replaces only the kinds of key an upload brings; the
   * mock drops the one-time keys of an upload that has none (a rotation of the signed prekey
   * and the last-resort key), so those are carried over here.
   */
  async updateDeviceKeys(device, serviceIdKind, keys) {
    const existing = device.keys.get(serviceIdKind);
    const key = `${deviceRef(device)}|${serviceIdKind}`;
    const uploads = this.keyUploads.get(key) ?? [];
    uploads.push({
      preKeys: keys.preKeys?.length ?? 0,
      pqPreKeys: keys.kyberPreKeys?.length ?? 0,
      signedPreKey: keys.signedPreKey?.keyId ?? null,
      pqLastResortPreKey: keys.lastResortKey?.keyId ?? null,
    });
    this.keyUploads.set(key, uploads);
    // The mock keeps its provisioning entry of the identity whose keys arrived first while
    // linking, and then fails every later one-time key upload of that identity ("Duplicate
    // service id kind", after it stored the keys). The real server takes uploads at any time.
    const provisioning = `${device.getServiceIdByKind(serviceIdKind)}.${device.getCheckedRegistrationId(serviceIdKind)}`;
    if (this.provisionResultQueueByKey.get(provisioning)?.seenServiceIdKinds.has(serviceIdKind)) {
      this.provisionResultQueueByKey.delete(provisioning);
    }
    return super.updateDeviceKeys(device, serviceIdKind, {
      ...keys,
      preKeys: keys.preKeys?.length ? keys.preKeys : existing?.preKeys,
      kyberPreKeys: keys.kyberPreKeys?.length ? keys.kyberPreKeys : existing?.kyberPreKeys,
    });
  }

  /**
   * Every envelope to a device goes through here: primaries' sends (`config.send` is bound to
   * this method) and client sends routed by the mock. The mock resolves once the device's
   * socket answered 200, and re-queues the envelope if the socket closes first; counting the
   * pending calls tells when a device has acknowledged everything addressed to it.
   *
   * The mock loses an envelope it delivers from its offline queue when that socket dies (or
   * answers non-200) before the ack: only its live path re-queues. The real server keeps every
   * unacknowledged envelope, so this delivers it again (queued for the next connection) — the
   * crash-safety tests kill clients mid-delivery and rely on that.
   */
  async send(target, message) {
    this.#inFlight.set(target, (this.#inFlight.get(target) ?? 0) + 1);
    try {
      for (let attempt = 1; ; attempt += 1) {
        try {
          return await super.send(target, message);
        } catch (error) {
          if (this.stopping || attempt >= MAX_REDELIVERIES) {
            throw error;
          }
          log(`delivery to ${deviceRef(target)} lost unacknowledged; queueing it again`);
          await new Promise((resolve) => setTimeout(resolve, REDELIVERY_DELAY_MS));
        }
      }
    } finally {
      const left = (this.#inFlight.get(target) ?? 1) - 1;
      if (left === 0) {
        this.#inFlight.delete(target);
        const waiting = this.#idleWaiters.filter((w) => w.device === target);
        this.#idleWaiters = this.#idleWaiters.filter((w) => w.device !== target);
        for (const waiter of waiting) {
          waiter.resolve();
        }
      } else {
        this.#inFlight.set(target, left);
      }
    }
  }

  /** Resolves once nothing addressed to `device` is in flight or queued. */
  waitForDeliveries(device) {
    if (!this.#inFlight.has(device)) {
      return Promise.resolve();
    }
    return new Promise((resolve) => this.#idleWaiters.push({ device, resolve }));
  }

  /** Called with the address when a provisioning attempt ends, successfully or not. */
  onProvisionSettled = () => {};

  /** Called with `{target, source, envelopeType, timestamp}` for every client envelope to a
   * scripted secondary device (`linkedDevices`, `addDevice`), which the mock drops. */
  onScriptedEnvelope = () => {};

  async handleMessage(source, serviceIdKind, envelopeType, target, encrypted, timestamp) {
    if (target.deviceId !== PRIMARY_DEVICE_ID && !target.isProvisioned) {
      this.onScriptedEnvelope({ target, source, envelopeType, timestamp: toNumber(timestamp) });
    }
    try {
      return await super.handleMessage(source, serviceIdKind, envelopeType, target, encrypted, timestamp);
    } catch (error) {
      if (target.deviceId === PRIMARY_DEVICE_ID && this.onDecryptFailure({ target, timestamp: toNumber(timestamp), error })) {
        return undefined;
      }
      throw error;
    }
  }

  async getProvisioningResponse(id, abortSignal) {
    const entry = { id };
    this.#openedAddresses.push(entry);
    try {
      return await super.getProvisioningResponse(id, abortSignal);
    } finally {
      const index = this.#openedAddresses.indexOf(entry);
      if (index !== -1) {
        this.#openedAddresses.splice(index, 1);
      }
      this.onProvisionSettled(id);
    }
  }

  /** The address of the pending provision `waitForProvision()` returned last. */
  takeOpenedAddress() {
    return this.#openedAddresses.shift()?.id;
  }
}

// ---------------------------------------------------------------------------------------------
// Harness state and commands
// ---------------------------------------------------------------------------------------------

/** Per scripted primary device ("phone"): the mock's PrimaryDevice plus buffered inboxes. */
class PrimaryState {
  constructor(primary) {
    this.primary = primary;
    this.contacts = [];
    this.groups = [];
    this.inbox = {
      message: new Mailbox('message'),
      receipt: new Mailbox('receipt'),
      editMessage: new Mailbox('editMessage'),
      story: new Mailbox('story'),
      syncMessage: new Mailbox('syncMessage'),
      decryptionError: new Mailbox('decryptionError'),
      typing: new Mailbox('typing'),
      distribution: new Mailbox('distribution'),
      decryptFailure: new Mailbox('decryptFailure'),
    };
    /** The inner envelope of the last failed decrypt (`#trackPrimary`). */
    this.lastFailure = undefined;
    /** Devices this primary already has a session with (see `Harness#ensureSession`). */
    this.sessions = new Set();
    /** Devices whose PNI this primary already has a session with. */
    this.pniSessions = new Set();
  }

  get device() {
    return this.primary.device;
  }

  describe() {
    const { primary, device } = this;
    return {
      ...describeDevice(device),
      profileName: primary.profileName,
      profileKey: b64(primary.profileKey.serialize()),
      identityKey: b64(primary.getPublicKey(ServiceIdKind.ACI).serialize()),
      secondaryDevices: primary.secondaryDevices.map(deviceRef),
      contacts: this.contacts.map((contact) => deviceRef(contact.device)),
      groups: this.groups.map(describeGroup),
      avatarPath: profileExtras.get(device.aci)?.avatar ?? null,
    };
  }
}

function describeGroup(group) {
  const members = (group.state.members ?? [])
    .map((member) => {
      try {
        return member.userId ? group.decryptServiceId(member.userId) : undefined;
      } catch {
        return undefined;
      }
    })
    .filter((aci) => aci !== undefined);
  return {
    id: group.id,
    title: group.title,
    masterKey: b64(group.masterKey),
    revision: group.revision,
    members,
    avatarKey: group.state.avatarUrl || null,
  };
}

/** A `GroupAttributeBlob` with one field, encrypted under the group's secret params. */
function groupBlob(secretParams, content) {
  const cipher = new zkgroup.ClientZkGroupCipher(secretParams);
  return Buffer.from(cipher.encryptBlob(Buffer.from(Proto.GroupAttributeBlob.encode({ content }))));
}

/** The fields of a `Member` the mock's server state keeps, with encrypted id and profile key. */
function encryptedMember(secretParams, primary, role, joinedAtVersion) {
  const cipher = new zkgroup.ClientZkGroupCipher(secretParams);
  const aci = SignalClient.Aci.parseFromServiceIdString(primary.device.aci);
  return {
    role,
    userId: Buffer.from(cipher.encryptServiceId(aci).serialize()),
    profileKey: Buffer.from(cipher.encryptProfileKey(primary.profileKey, aci).serialize()),
    presentation: null,
    joinedAtVersion,
    labelEmoji: null,
    labelString: null,
  };
}

class Harness {
  /** @type {Map<string, PrimaryState>} by ACI */
  #primaries = new Map();
  /** @type {Map<string, object>} groups by id (base64 group identifier) */
  #groups = new Map();
  /** Pending provisions `{address, pending}` by provisioning socket. */
  #provisions = new Mailbox('provision');
  /** Addresses of opened provisioning sockets, for `waitForProvision`. */
  #openedProvisions = new Mailbox('openedProvision');
  /** Client envelopes to scripted secondary devices, for `waitForEnvelope`. */
  #scriptedEnvelopes = new Mailbox('scriptedEnvelope');
  #shuttingDown = false;
  /** Sender-key distribution ids already sent, `"<sender aci>|<device ref>|<group id>"`. */
  #distributions = new Map();

  constructor({ server, handshake, stateDir, ownsStateDir }) {
    this.server = server;
    this.handshake = handshake;
    this.stateDir = stateDir;
    this.ownsStateDir = ownsStateDir;
    server.onScriptedEnvelope = (entry) => this.#scriptedEnvelopes.push(entry);
    server.onDecryptFailure = (failure) => this.#onDecryptFailure(failure);
    server.onDeviceRemoved = (device) => {
      const state = this.#primaries.get(device.aci);
      if (state) {
        // The primary's list; a device it never linked itself (`addDevice`) is not on it.
        state.primary.unlink(device).catch(() => {});
        state.sessions.delete(device);
        state.pniSessions.delete(device);
      }
    };
    server.onProvisionSettled = (address) => {
      // The socket closed or the provision completed: neither may be handed out again.
      this.#provisions.remove((item) => item.address === address);
      this.#openedProvisions.remove((item) => item.address === address);
    };
    this.#pumpProvisions();
  }

  // -- plumbing -------------------------------------------------------------------------------

  #pumpProvisions() {
    const loop = async () => {
      while (!this.#shuttingDown) {
        let pending;
        try {
          pending = await this.server.waitForProvision();
        } catch {
          // The mock's queue gives up after its API timeout; just wait again.
          continue;
        }
        const address = this.server.takeOpenedAddress();
        if (address === undefined) {
          log('pending provision without a known address; ignoring it');
          continue;
        }
        this.#provisions.push({ address, pending });
        this.#openedProvisions.push({ address });
        emitEvent('provisionOpened', { address });
      }
    };
    loop().catch((error) => log('provision pump failed', error));
  }

  #pumpInbox(state, mailbox, next) {
    const loop = async () => {
      while (!this.#shuttingDown) {
        let entry;
        try {
          entry = await next();
        } catch (error) {
          // The primary's queues have no timeout; keep pumping whatever went wrong.
          log(`inbox pump ${mailbox.name}`, error);
          continue;
        }
        if (entry?.source) {
          // Receiving anything from a device means the primary now has a session with it.
          state.sessions.add(entry.source);
        }
        mailbox.push(entry);
      }
    };
    loop().catch((error) => log(`inbox pump ${mailbox.name} failed`, error));
  }

  #trackPrimary(primary) {
    const state = new PrimaryState(primary);
    const { inbox } = state;
    // The mock reports a sealed envelope by its inner type (`decrypt` recurses with the
    // unsealed type), so tag the decoded Content of every outer sealed-sender decrypt; the
    // inboxes keep that object, and `waitFor*` reads the tag back.
    // The mock ignores typing messages; the outermost decrypt (a sealed one recurses) buffers
    // them, after the sealed tag is set.
    const decrypt = primary.decrypt.bind(primary);
    let depth = 0;
    primary.decrypt = async (source, serviceIdKind, envelopeType, encrypted) => {
      depth += 1;
      let result;
      try {
        result = await decrypt(source, serviceIdKind, envelopeType, encrypted);
      } catch (error) {
        // The innermost failure names the sender (a sealed envelope recurses with it).
        if (source !== undefined && state.lastFailure === undefined) {
          state.lastFailure = { source, envelopeType, encrypted: Buffer.from(encrypted) };
        }
        throw error;
      } finally {
        depth -= 1;
      }
      if (envelopeType === EnvelopeType.SealedSender && result?.content) {
        result.content[SEALED_SENDER] = true;
        // The group the sealed content names (`UnidentifiedSenderMessageContent.groupId`): a
        // client files a decryption failure under it. Unsealing the outer layer again is
        // stateless.
        try {
          const usmc = await SignalClient.sealedSenderDecryptToUsmc(
            encrypted,
            primary.identity.get(serviceIdKind),
          );
          const groupId = usmc.groupId();
          result.content[SEALED_GROUP] = groupId ? Buffer.from(groupId).toString('base64') : null;
          result.content[SEALED_HINT] = CONTENT_HINT_NAMES[usmc.contentHint()] ?? String(usmc.contentHint());
        } catch (error) {
          log('reading the sealed group id failed', error instanceof Error ? error.message : error);
        }
      }
      const typing = result?.content?.content?.typingMessage;
      if (depth === 0 && typing) {
        inbox.typing.push({ source: result.unsealedSource, typing, content: result.content });
      }
      // A Content that is a sender-key distribution message and nothing else: the mock
      // processes it and reports nothing.
      const skdm = result?.content?.senderKeyDistributionMessage;
      if (depth === 0 && skdm != null && skdm.length > 0 && result.content.content == null) {
        inbox.distribution.push({ source: result.unsealedSource, content: result.content });
      }
      return result;
    };
    this.#pumpInbox(state, inbox.message, () => primary.waitForMessage());
    this.#pumpInbox(state, inbox.receipt, () => primary.waitForReceipt());
    this.#pumpInbox(state, inbox.editMessage, () => primary.waitForEditMessage());
    this.#pumpInbox(state, inbox.story, () => primary.waitForStory());
    this.#pumpInbox(state, inbox.syncMessage, () => primary.waitForSyncMessage());
    this.#pumpInbox(state, inbox.decryptionError, () => primary.waitForDecryptionError());
    this.#primaries.set(primary.device.aci, state);
    return state;
  }

  /**
   * A harness primary could not decrypt `timestamp` from a client: as a real client does, it
   * records the failure (`waitForDecryptFailure`) and sends the sender's device a retry request
   * (a `DecryptionErrorMessage` for the original). Returns false when the failure did not come
   * from a decrypt (the error then fails the client's request as before).
   */
  #onDecryptFailure({ target, timestamp, error }) {
    const state = this.#primaries.get(target.aci);
    const failure = state?.lastFailure;
    if (!state || !failure) {
      return false;
    }
    state.lastFailure = undefined;
    const { source, envelopeType, encrypted } = failure;
    const types = {
      [EnvelopeType.SenderKey]: SignalClient.CiphertextMessageType.SenderKey,
      [EnvelopeType.CipherText]: SignalClient.CiphertextMessageType.Whisper,
      [EnvelopeType.PreKey]: SignalClient.CiphertextMessageType.PreKey,
      [EnvelopeType.Plaintext]: SignalClient.CiphertextMessageType.Plaintext,
    };
    const type = types[envelopeType] ?? SignalClient.CiphertextMessageType.SenderKey;
    log(`${deviceRef(target)} could not decrypt ${timestamp} from ${deviceRef(source)}:`, error?.message ?? error);
    state.inbox.decryptFailure.push({ source, timestamp, envelopeType });
    emitEvent('decryptFailed', { primary: deviceRef(target), from: deviceRef(source), timestamp, envelopeType });
    // Off the delivery's call stack: the primary's lock is free again once this returns.
    setImmediate(() => {
      this.#sendRetryRequestFor(state, source, timestamp, type, encrypted).catch((e) =>
        log('retry request after a decrypt failure failed', e),
      );
    });
    return true;
  }

  /** Sends the device `target` a retry request from `state`'s primary (see `sendRetryRequest`). */
  async #sendRetryRequestFor(state, target, timestamp, type, original) {
    const bytes = type === SignalClient.CiphertextMessageType.SenderKey ? Buffer.alloc(0) : original;
    const dem = SignalClient.DecryptionErrorMessage.forOriginal(bytes, type, timestamp, target.deviceId);
    const content = {
      content: { decryptionErrorMessage: dem.serialize() },
      pniSignatureMessage: null,
      senderKeyDistributionMessage: null,
    };
    const now = Date.now();
    await this.ensureSession(state, target);
    const envelope = await state.primary.encryptContent(target, content, { timestamp: now });
    this.#reportFailure(this.deliver(state, target, envelope), target, now);
  }

  async resolveDevice(ref, name) {
    const { aci, deviceId = 1 } = parseRef(ref, name);
    const device = await this.server.getDeviceByServiceId(aci, deviceId);
    if (!device) {
      throw notFound(`"${name}": no device ${aci}.${deviceId}`);
    }
    return device;
  }

  resolvePrimary(ref, name) {
    const { aci, deviceId } = parseRef(ref, name);
    const state = this.#primaries.get(aci);
    if (!state || (deviceId !== undefined && deviceId !== state.device.deviceId)) {
      throw notFound(`"${name}": ${ref} is not a primary created by this harness`);
    }
    return state;
  }

  resolveGroup(id) {
    const group = this.#groups.get(id);
    if (!group) {
      throw notFound(`unknown group ${id}`);
    }
    return group;
  }

  /**
   * Gives `state`'s primary a session with `target` from one of the target's uploaded
   * one-time prekeys, as Desktop's bootstrap does after linking. Skipped when the primary
   * already has one (seeded before, or it received a message from that device).
   */
  async ensureSession(state, target, kind = ServiceIdKind.ACI) {
    const sessions = kind === ServiceIdKind.PNI ? state.pniSessions : state.sessions;
    if (sessions.has(target)) {
      return;
    }
    const key = await target.popSingleUseKey(kind);
    await state.primary.addSingleUseKey(target, key, kind);
    sessions.add(target);
  }

  /**
   * Delivers an encrypted envelope. A harness primary is fed directly (the mock only routes
   * client sends to primaries); any other device gets it through the server, which resolves
   * once the device's socket acknowledged it (and waits for the device to connect).
   */
  deliver(sender, target, envelope) {
    const targetPrimary = this.#primaries.get(target.aci);
    if (targetPrimary && targetPrimary.device === target) {
      return targetPrimary.primary.receive(sender.device, envelope);
    }
    return this.server.send(target, envelope);
  }

  /** Logs and reports (`deliveryFailed`) a delivery nobody waits for. */
  #reportFailure(delivery, target, timestamp) {
    delivery.catch((error) => {
      log(`delivery ${timestamp} to ${deviceRef(target)} failed`, error);
      emitEvent('deliveryFailed', { to: deviceRef(target), timestamp, ...errorToWire(error) });
    });
  }

  /**
   * Encrypts `content` (a mock-server Content object) from primary `sender` to `target` and
   * delivers it; waits for the delivery only with `awaitDelivery`.
   */
  /**
   * Encrypts `plaintext` like Desktop (`encryptAttachmentV2`), stores it on the mock's CDN 3
   * (TUS store) or CDN 0 (`spec.cdn`) and returns its wire pointer (`sendAttachments`, the
   * pointers of `sendSync {attachmentBackfillResponse}`). `spec`: `contentType?`, `fileName?`,
   * `width?`, `height?`, `flags?`, `caption?`, `blurHash?`, `durationSeconds?`, `cdn?` (3),
   * `clientUuid?` (base64 of 16 bytes, as Android puts into a backfilled pointer; none by
   * default), `uploadTimestamp?` (ms; now), `forge?` (`{digest? (true: a wrong digest;
   * "none": no digest at all), size?, cdnNumber?, cdnKey?}`: what a hostile sender puts into
   * the pointer in place of the truth; the blob is stored as usual).
   */
  async #pointerOf(plaintext, spec) {
    const encrypted = encryptAttachment(plaintext);
    const cdn = spec.cdn ?? 3;
    let cdnKey;
    if (cdn === 3) {
      cdnKey = nodeCrypto.randomUUID();
      await this.server.storeAttachmentOnCdn(3, cdnKey, encrypted.blob);
    } else if (cdn === 0) {
      cdnKey = await this.server.storeAttachment(encrypted.blob);
    } else {
      throw badRequest(`unsupported cdn ${cdn}`);
    }
    const forge = spec.forge ?? {};
    if (typeof forge !== 'object' || forge === null) {
      throw badRequest('"forge" must be an object');
    }
    let { digest } = encrypted;
    if (forge.digest === true) {
      digest = Buffer.from(digest);
      digest[0] ^= 1;
    } else if (forge.digest === 'none') {
      digest = null;
    }
    return {
      attachmentIdentifier: { cdnKey: typeof forge.cdnKey === 'string' ? forge.cdnKey : cdnKey },
      cdnNumber: Number.isInteger(forge.cdnNumber) ? forge.cdnNumber : cdn,
      contentType: spec.contentType ?? 'application/octet-stream',
      key: encrypted.key,
      size: Number.isInteger(forge.size) ? forge.size : encrypted.size,
      digest,
      fileName: spec.fileName ?? null,
      flags: spec.flags ?? null,
      width: spec.width ?? null,
      height: spec.height ?? null,
      caption: spec.caption ?? null,
      blurHash: spec.blurHash ?? null,
      uploadTimestamp: BigInt(Number.isSafeInteger(spec.uploadTimestamp) ? spec.uploadTimestamp : Date.now()),
      clientUuid: spec.clientUuid == null ? null : pointerBytes(spec.clientUuid, 'clientUuid'),
      thumbnail: null,
      incrementalMac: null,
      chunkSize: null,
      audioDurationSeconds: spec.durationSeconds ?? null,
    };
  }

  async #sendContent(sender, target, content, { timestamp, sealed, awaitDelivery, timeout, serverTimestamp }) {
    await this.ensureSession(sender, target);
    let envelope = await sender.primary.encryptContent(target, content, { timestamp, sealed });
    if (serverTimestamp !== undefined) {
      envelope = restamp(envelope, serverTimestamp);
    }
    const delivery = this.deliver(sender, target, envelope);
    if (awaitDelivery) {
      await withTimeout(delivery, timeout, 'waiting for delivery');
    } else {
      this.#reportFailure(delivery, target, timestamp);
    }
    // `serverTimestamp`: what the envelope was stamped with, when the caller chose it. A
    // client that takes the server's time for a message shows this one, not `timestamp`.
    return { timestamp, to: deviceRef(target), ...(serverTimestamp === undefined ? {} : { serverTimestamp }) };
  }

  /** `from` (a primary) and `to` (one of its linked devices) for sync messages. */
  async #syncPair(args) {
    const sender = this.resolvePrimary(args.from, 'from');
    const target = await this.resolveDevice(args.to, 'to');
    if (target.aci !== sender.device.aci || target === sender.device) {
      throw badRequest('sync messages go from a primary to one of its linked devices');
    }
    return { sender, target };
  }

  /**
   * The id of `sender`'s sender-key chain for `target` in `group`, sending the distribution
   * message first when the device does not have the chain yet (as Desktop does before a
   * sender-key message).
   */
  async #distributionFor(sender, target, group, timestamp) {
    const key = `${sender.device.aci}|${deviceRef(target)}|${group.id}`;
    let distributionId = this.#distributions.get(key);
    if (distributionId === undefined) {
      distributionId = nodeCrypto.randomUUID();
      const senderKeys = sender.primary.senderKeys.get(ServiceIdKind.ACI);
      // The mock names its own chain by the target's address (`PrimaryDevice.sendSenderKey`);
      // `encrypt` with a distribution id does the same.
      const skdm = await SignalClient.SenderKeyDistributionMessage.create(target.address, distributionId, senderKeys);
      const skdmContent = {
        content: null,
        pniSignatureMessage: null,
        senderKeyDistributionMessage: Buffer.from(skdm.serialize()),
      };
      const skdmEnvelope = await sender.primary.encryptContent(target, skdmContent, { timestamp: Date.now() });
      const delivery = this.deliver(sender, target, skdmEnvelope);
      if (this.#primaries.get(target.aci)?.device === target) {
        // A harness primary handles envelopes concurrently: let it store the key first. A
        // client device gets both over one socket, in order.
        await delivery;
      } else {
        this.#reportFailure(delivery, target, timestamp);
      }
      this.#distributions.set(key, distributionId);
    }
    return distributionId;
  }

  /**
   * A DataMessage from `sender` built from the parts in `args` (see `sendData`), in `group`
   * when given. Nothing is checked against what an honest client would send: a target author
   * or timestamp may name anyone's message.
   */
  #dataMessage(sender, args, timestamp, group) {
    const aciOf = (value, name) => aciBytes(parseRef(value, name).aci);
    const part = (name) => {
      const value = args[name];
      if (value === undefined || value === null) {
        return undefined;
      }
      if (typeof value !== 'object') {
        throw badRequest(`"${name}" must be an object`);
      }
      return value;
    };
    const stamp = (value, name) => {
      if (!Number.isSafeInteger(value) || value < 0) {
        throw badRequest(`"${name}" must be a timestamp`);
      }
      return BigInt(value);
    };
    const message = {
      ...mock.EMPTY_DATA_MESSAGE,
      timestamp: BigInt(optInt(args, 'dataTimestamp', timestamp, { min: 1 })),
      body: optString(args, 'text') ?? null,
      groupV2: group ? group.toContext() : null,
      profileKey: optBool(args, 'withProfileKey', false) ? sender.primary.profileKey.serialize() : null,
      expireTimer: optInt(args, 'expireTimer', undefined, { max: 2 ** 32 - 1 }) ?? null,
      expireTimerVersion: optInt(args, 'expireTimerVersion', undefined, { max: 2 ** 32 - 1 }) ?? null,
      flags: optInt(args, 'flags', undefined, { max: 2 ** 32 - 1 }) ?? null,
      isViewOnce: optBool(args, 'viewOnce', false) ? true : null,
      requiredProtocolVersion: optInt(args, 'requiredProtocolVersion', undefined) ?? null,
    };
    const reaction = part('reaction');
    if (reaction) {
      message.reaction = {
        emoji: typeof reaction.emoji === 'string' ? reaction.emoji : null,
        remove: reaction.remove === true,
        targetAuthorAci: null,
        targetAuthorAciBinary: reaction.targetAuthor == null ? null : aciOf(reaction.targetAuthor, 'reaction.targetAuthor'),
        targetSentTimestamp: stamp(reaction.targetTimestamp, 'reaction.targetTimestamp'),
      };
    }
    const del = part('delete');
    if (del) {
      message.delete = { targetSentTimestamp: stamp(del.targetTimestamp, 'delete.targetTimestamp') };
    }
    const adminDelete = part('adminDelete');
    if (adminDelete) {
      message.adminDelete = {
        targetAuthorAciBinary: aciOf(adminDelete.targetAuthor, 'adminDelete.targetAuthor'),
        targetSentTimestamp: stamp(adminDelete.targetTimestamp, 'adminDelete.targetTimestamp'),
      };
    }
    const quote = part('quote');
    if (quote) {
      const attachments = Array.isArray(quote.attachments) ? quote.attachments : [];
      message.quote = {
        id: stamp(quote.id, 'quote.id'),
        authorAci: null,
        authorAciBinary: aciOf(quote.author, 'quote.author'),
        text: typeof quote.text === 'string' ? quote.text : null,
        attachments: attachments.map((attachment) => ({
          contentType: attachment.contentType ?? null,
          fileName: attachment.fileName ?? null,
          thumbnail: null,
        })),
        bodyRanges: null,
        type: 0,
      };
    }
    const groupCallUpdate = part('groupCallUpdate');
    if (groupCallUpdate) {
      // `{eraId?}`: somebody started or joined a group call (no era id: Android's update
      // after a declined ring).
      message.groupCallUpdate = { eraId: typeof groupCallUpdate.eraId === 'string' ? groupCallUpdate.eraId : null };
    }
    const unsupported = optString(args, 'unsupported');
    const someone = aciBytes(sender.device.aci);
    switch (unsupported) {
      case undefined:
        break;
      case 'contact':
        message.contact = [{ name: null, number: null, email: null, address: null, avatar: null, organization: 'Example' }];
        break;
      case 'payment':
        message.payment = { Item: { activation: { type: 0 } } };
        break;
      case 'giftBadge':
        message.giftBadge = { receiptCredentialPresentation: nodeCrypto.randomBytes(16) };
        break;
      case 'poll':
        message.pollCreate = { question: 'Lunch?', allowMultiple: false, options: ['Yes', 'No'] };
        break;
      case 'pollEnd':
        message.pollTerminate = { targetSentTimestamp: BigInt(timestamp - 1) };
        break;
      case 'storyReply':
        message.storyContext = { authorAci: null, authorAciBinary: someone, sentTimestamp: BigInt(timestamp - 1) };
        break;
      case 'groupCall':
        message.groupCallUpdate = { eraId: 'era' };
        break;
      case 'pin':
        message.pinMessage = {
          targetAuthorAciBinary: someone,
          targetSentTimestamp: BigInt(timestamp - 1),
          pinDuration: { pinDurationForever: true },
        };
        break;
      default:
        throw badRequest(`unknown "unsupported" kind ${unsupported}`);
    }
    return message;
  }

  get isShuttingDown() {
    return this.#shuttingDown;
  }

  async shutdown(code = 0) {
    if (this.#shuttingDown) {
      return;
    }
    this.#shuttingDown = true;
    this.server.stopping = true;
    // Open client connections keep the http2 server from closing; do not wait for them.
    await Promise.race([
      this.server.close().catch(() => {}),
      new Promise((resolve) => setTimeout(resolve, 500)),
    ]);
    if (this.ownsStateDir) {
      await fs.rm(this.stateDir, { recursive: true, force: true }).catch(() => {});
    }
    process.exit(code);
  }

  // -- commands -------------------------------------------------------------------------------

  /**
   * Command table: `name -> async (args) => result`. A new command is one entry here plus a
   * line in README.md (and a typed wrapper in crates/kourier-signal/tests/support/harness.rs).
   */
  commands = {
    /** Liveness check. */
    ping: async () => ({ pong: true, protocol: PROTOCOL_VERSION }),

    /**
     * Registers a scripted account with a primary device ("phone").
     * Args: `profileName`, `contacts?` (refs of earlier primaries; served as contact sync),
     * `groups?` (`[{title, members?}]`, created by this primary with itself as a member),
     * `hasE164?` (default true), `linkedDevices?` (extra mock secondary devices, default 0).
     */
    createPrimary: async (args) => {
      const profileName = reqString(args, 'profileName');
      const avatar = optString(args, 'avatar');
      const about = optString(args, 'about');
      const aboutEmoji = optString(args, 'aboutEmoji');
      const contacts = optArray(args, 'contacts').map((ref, i) => this.resolvePrimary(ref, `contacts[${i}]`));
      const hasE164 = optBool(args, 'hasE164', true);
      const linkedDevices = optInt(args, 'linkedDevices', 0);
      // Validate everything before registering anything: a bad request leaves no account.
      const groupSpecs = optArray(args, 'groups').map((spec, i) => {
        if (typeof spec !== 'object' || spec === null || Array.isArray(spec)) {
          throw badRequest(`"groups[${i}]" must be an object`);
        }
        return {
          title: reqString(spec, 'title'),
          members: optArray(spec, 'members').map((ref, j) =>
            this.resolvePrimary(ref, `groups[${i}].members[${j}]`),
          ),
        };
      });

      const primary = await this.server.createPrimaryDevice({
        profileName,
        contacts: contacts.map((contact) => contact.primary),
        hasE164,
      });
      const state = this.#trackPrimary(primary);
      state.contacts = contacts;

      // Real clients pad the name (`given \0 family`, Desktop `PaddedLengths`); the mock does not.
      const profileKey = Buffer.from(primary.profileKey.serialize());
      primary.device.profileName = encryptPadded(profileKey, profileName, PADDED_NAME);
      const extras = {};
      if (avatar !== undefined) {
        // Profile avatars are CDN0 blobs encrypted with the profile key.
        const blob = encryptProfileValue(profileKey, Buffer.from(avatar, 'base64'));
        extras.avatar = `attachments/${await this.server.storeAttachment(blob)}`;
      }
      if (about !== undefined) {
        extras.about = encryptPadded(profileKey, about, PADDED_ABOUT).toString('base64');
      }
      if (aboutEmoji !== undefined) {
        extras.aboutEmoji = encryptPadded(profileKey, aboutEmoji, PADDED_EMOJI).toString('base64');
      }
      profileExtras.set(primary.device.aci, extras);

      for (let i = 0; i < linkedDevices; i += 1) {
        await this.server.createSecondaryDevice(primary);
      }

      for (const { title, members } of groupSpecs) {
        const group = await primary.createGroup({
          title,
          members: [primary, ...members.map((member) => member.primary)],
        });
        this.#groups.set(group.id, group);
        state.groups.push(group);
      }

      return state.describe();
    },

    /**
     * Waits until a client opens a provisioning socket. Result: `{address}`, the `uuid` the
     * client's `sgnl://linkdevice` URL must carry.
     */
    waitForProvision: async (args) => {
      const timeout = timeoutArg(args);
      const { address } = await this.#openedProvisions.take(() => true, timeout, 'waitForProvision');
      return { address };
    },

    /**
     * Links a new secondary device to `primary`: answers the provisioning socket whose address
     * is the `uuid` in `url` with a ProvisionMessage, then waits until the client has linked
     * and uploaded one-time prekeys for ACI and PNI (the mock's notion of "provisioned").
     * Afterwards the primary gets a session with the new device, as in Desktop's bootstrap.
     * Args: `url` (sgnl://linkdevice?uuid=..&pub_key=..), `primary`, `timeout?`.
     */
    completeProvision: async (args) => {
      const url = reqString(args, 'url');
      const state = this.resolvePrimary(args.primary, 'primary');
      const timeout = timeoutArg(args, DEFAULT_PROVISION_MS);

      let parsed;
      try {
        parsed = new URL(url);
      } catch {
        throw badRequest(`"url" is not a URL: ${url}`);
      }
      const address = parsed.searchParams.get('uuid');
      if (!address || !parsed.searchParams.get('pub_key')) {
        throw badRequest('"url" must carry uuid and pub_key query parameters');
      }

      const deadline = Date.now() + timeout;
      const { pending } = await this.#provisions.take(
        (item) => item.address === address,
        timeout,
        `completeProvision: provisioning socket ${address}`,
      );
      // The phone's side of a history transfer: the key goes into the provisioning message
      // only when the linking URL offers `backup5` AND the phone's user chose to transfer
      // (`setEphemeralBackupKey`), as the Android phone does.
      const capabilities = (parsed.searchParams.get('capabilities') ?? '').split(',').filter(Boolean);
      const transferKey = state.transferKey;
      state.primary.ephemeralBackupKey =
        transferKey && (state.transferKeyForced || capabilities.includes('backup5')) ? transferKey : undefined;
      const sentAt = Date.now();
      const device = await withTimeout(
        pending.complete({ provisionURL: url, primaryDevice: state.primary }),
        Math.max(1, deadline - Date.now()),
        'completeProvision: waiting for link and prekey upload',
      );
      await this.ensureSession(state, device);
      // From the provisioning message leaving the phone to the link request reaching the
      // server and to its answer: what the phone waits for before it uploads an archive.
      const link = sends.linkRequests.findLast((r) => r.arrived >= sentAt && r.status === 200);
      return {
        ...describeDevice(device),
        capabilities,
        sentTransferKey: state.primary.ephemeralBackupKey !== undefined,
        linkRequestAfterMs: link ? link.arrived - sentAt : null,
        linkedAfterMs: link?.answered != null ? link.answered - sentAt : null,
      };
    },

    /**
     * The phone's user chooses "Transfer message history": `primary` puts an ephemeral
     * backup key into the provisioning message of every later `completeProvision` whose URL
     * offers `backup5`. Args: `primary`, `key?` (base64 of 32 bytes; random when absent),
     * `clear?` (true: "Don't transfer" again), `force?` (true: the key is sent also when the
     * URL does not offer `backup5`, which no real phone does: for a client that must ignore
     * a key it did not ask for). Result: `{key}` (base64, or null).
     */
    setEphemeralBackupKey: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      state.transferKeyForced = optBool(args, 'force', false);
      if (optBool(args, 'clear', false)) {
        state.transferKey = undefined;
        return { key: null };
      }
      const given = optString(args, 'key');
      const key = given === undefined ? nodeCrypto.randomBytes(32) : Buffer.from(given, 'base64');
      if (key.length !== 32) {
        throw badRequest('"key" must be 32 bytes');
      }
      state.transferKey = key;
      return { key: key.toString('base64') };
    },

    /**
     * Puts the file at `path` on CDN 3 as a transfer archive, as the phone's upload would.
     * Args: `path` (a local file). Result: `{cdn: 3, key, size}`: the place to hand to
     * `provideTransferArchive`. Downloads of it are served with ranges and obey
     * `breakDownloads` like any CDN 3 attachment (`downloadStats`).
     */
    storeTransferArchive: async (args) => {
      const data = await fs.readFile(reqString(args, 'path'));
      const key = nodeCrypto.randomBytes(16).toString('hex');
      await this.server.storeAttachmentOnCdn(3, key, data);
      return { cdn: 3, key, size: data.length };
    },

    /**
     * The phone answers the new device's long-poll (`GET /v1/devices/transfer_archive`):
     * with the archive's place (`cdn`, `key`), or with `error` (`"RELINK_REQUESTED"` or
     * `"CONTINUE_WITHOUT_UPLOAD"`; any other string is passed on as it is, for a client's
     * handling of an answer it does not know). Args: `device`, and `cdn` + `key`, or
     * `error`. Result: `{}`.
     */
    provideTransferArchive: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const error = optString(args, 'error');
      const archive =
        error !== undefined ? { error } : { cdn: optInt(args, 'cdn', 3), key: reqString(args, 'key') };
      await this.server.provideTransferArchive(device, archive);
      return {};
    },

    /**
     * Faults of the transfer long-poll: `silent` (true: no long-poll is answered at all, so a
     * client's own request timeout ends each one; false: answered again; one that was left
     * unanswered stays so), `fail` (answer the next n long-polls with `status`, 500 unless
     * given), `immediate` (true: every long-poll is answered 204 at once instead of being
     * held for its timeout). Result: `{}`.
     */
    holdTransferArchive: async (args) => {
      sends.transferSilent = optBool(args, 'silent', false);
      sends.transferFailures = optInt(args, 'fail', 0, { min: 0, max: 1000 });
      sends.transferFailureStatus = optInt(args, 'status', 500, { min: 400, max: 599 });
      sends.transferImmediate = optBool(args, 'immediate', false);
      return {};
    },

    /**
     * The transfer long-polls clients sent so far: `{polls: [{at, timeout, status}]}`
     * (`status`: 204, 200, 500 or `"silent"`; null while one is held). With `reset?` the
     * list starts anew.
     */
    transferStats: async (args) => {
      const polls = sends.transferPolls.map((entry) => ({ ...entry }));
      if (optBool(args, 'reset', false)) {
        sends.transferPolls = [];
      }
      return { polls };
    },

    /**
     * Sends a text DataMessage from a primary to one device.
     * Args: `from` (primary), `to` (device), `text`, `sealed?` (default false), `timestamp?`
     * (default now), `withProfileKey?`, `group?` (group id, sets the GroupContextV2),
     * `serviceIdKind?` (`"ACI"` default, or `"PNI"`: addressed to the device's PNI, a PreKey
     * message from a session built on its PNI prekeys; never sealed), `awaitDelivery?` (default
     * false: return once queued; true: wait until the target acknowledged it, up to
     * `timeout?`). Result: `{timestamp, to}`.
     */
    sendText: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      if (typeof args.text !== 'string') {
        throw badRequest('"text" must be a string');
      }
      if (target === sender.device) {
        throw badRequest('a primary cannot send to itself');
      }
      const sealed = optBool(args, 'sealed', false);
      const serviceIdKind = serviceIdKindArg(args);
      if (sealed && serviceIdKind === ServiceIdKind.PNI) {
        throw badRequest('sealed sender to a PNI is not allowed');
      }
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const withProfileKey = optBool(args, 'withProfileKey', false);
      const groupId = optString(args, 'group');
      const group = groupId === undefined ? undefined : this.resolveGroup(groupId);
      const awaitDelivery = optBool(args, 'awaitDelivery', false);
      const timeout = timeoutArg(args);

      await this.ensureSession(sender, target, serviceIdKind);
      const envelope = await sender.primary.encryptText(target, args.text, {
        timestamp,
        sealed,
        withProfileKey,
        group,
        serviceIdKind,
      });
      const delivery = this.deliver(sender, target, envelope);
      if (awaitDelivery) {
        await withTimeout(delivery, timeout, 'sendText: waiting for delivery');
      } else {
        this.#reportFailure(delivery, target, timestamp);
      }
      return { timestamp, to: deviceRef(target) };
    },

    /**
     * Queues `count` texts `"<prefix> <i>"` (i from 0) from a primary to one device, with the
     * timestamps `startTimestamp + i` (unique, so each is its own message), without waiting for
     * delivery: the bulk load of the catch-up and throughput tests. Encryption runs in order on
     * one session. Args: `from`, `to`, `count` (1..=10000), `prefix?` ("message"), `sealed?`,
     * `serviceIdKind?`, `startTimestamp?` (default now). Result: `{timestamps, to}`.
     */
    sendTexts: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      if (target === sender.device) {
        throw badRequest('a primary cannot send to itself');
      }
      const count = optInt(args, 'count', undefined, { min: 1, max: MAX_SEND_TEXTS });
      if (count === undefined) {
        throw badRequest('"count" is required');
      }
      const prefix = optString(args, 'prefix') ?? 'message';
      const sealed = optBool(args, 'sealed', false);
      const serviceIdKind = serviceIdKindArg(args);
      if (sealed && serviceIdKind === ServiceIdKind.PNI) {
        throw badRequest('sealed sender to a PNI is not allowed');
      }
      const start = optInt(args, 'startTimestamp', Date.now(), { min: 1 });

      await this.ensureSession(sender, target, serviceIdKind);
      const timestamps = [];
      for (let i = 0; i < count; i += 1) {
        const timestamp = start + i;
        const envelope = await sender.primary.encryptText(target, `${prefix} ${i}`, {
          timestamp,
          sealed,
          serviceIdKind,
        });
        this.#reportFailure(this.deliver(sender, target, envelope), target, timestamp);
        timestamps.push(timestamp);
      }
      return { timestamps, to: deviceRef(target) };
    },

    /**
     * Waits for the next DataMessage a primary decrypted, optionally only one `from` a device
     * (a bare ACI matches any device of that account). Args: `primary` (alias `device`),
     * `from?`, `group?` (only messages in that group), `kind?` (only a `message`, `reaction`,
     * `delete`, `adminDelete` or `timer` update; the others stay buffered), `timeout?`. Result:
     * `{source, sourceAci, kind,
     * sourceDevice, text, timestamp, envelopeType, messageType, sealed, serviceIdKind, groupId,
     * sealedGroupId, senderKeyDistribution, dataMessage, remaining}`: `envelopeType` is the wire
     * type (`SealedSender` when sealed), `messageType` the type of the (unsealed) ciphertext,
     * `sealedGroupId` the group the sealed content names (null when unsealed or none),
     * `remaining` the number of messages still buffered for the primary.
     */
    waitForMessage: async (args) => {
      const state = this.resolvePrimary(args.primary ?? args.device, 'primary');
      const timeout = timeoutArg(args);
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const groupFilter = optString(args, 'group');
      const groupOf = (dataMessage) => {
        const masterKey = dataMessage?.groupV2?.masterKey;
        return masterKey
          ? [...this.#groups.values()].find((g) => g.masterKey.equals(Buffer.from(masterKey)))
          : undefined;
      };
      const kindFilter = optString(args, 'kind');
      const entry = await state.inbox.message.take(
        (item) =>
          (from === undefined || refMatches(from, item.source)) &&
          (groupFilter === undefined || groupOf(item.dataMessage)?.id === groupFilter) &&
          (kindFilter === undefined || dataKind(item.dataMessage) === kindFilter),
        timeout,
        'waitForMessage',
      );
      const { source, envelopeType: messageType, serviceIdKind, body, dataMessage } = entry;
      const sealed = entry.content?.[SEALED_SENDER] === true;
      const group = groupOf(dataMessage);
      const skdm = entry.content?.senderKeyDistributionMessage;
      return {
        source: deviceRef(source),
        sourceAci: source.aci,
        sourceDevice: source.deviceId,
        // The mock reports a DataMessage without a body (reaction, attachment only) as ''.
        text: body,
        timestamp: toNumber(dataMessage.timestamp),
        envelopeType: sealed ? EnvelopeType.SealedSender : messageType,
        messageType,
        kind: dataKind(dataMessage),
        sealed,
        serviceIdKind,
        groupId: group?.id ?? null,
        sealedGroupId: entry.content?.[SEALED_GROUP] ?? null,
        senderKeyDistribution: skdm != null && skdm.length > 0,
        dataMessage: jsonSafe(dataMessage) ?? {},
        remaining: state.inbox.message.size,
      };
    },

    /**
     * Waits until `primary` has answered the CONTACTS, BLOCKED and CONFIGURATION sync
     * requests of `device` (the mock's `waitForSync`). With `reset?` (default true) the sync
     * state is cleared afterwards, so a later sync can be awaited again.
     */
    waitForSync: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const device = await this.resolveDevice(args.device, 'device');
      const timeout = timeoutArg(args);
      const reset = optBool(args, 'reset', true);
      await withTimeout(state.primary.waitForSync(device), timeout, 'waitForSync');
      if (reset) {
        state.primary.resetSyncState(device);
      }
      return {};
    },

    /**
     * Unlinks `device` from `primary`'s account, as the phone's "unlink device" does: the
     * primary forgets it and the server deletes it, so its credentials stop working (a new
     * websocket upgrade answers 403) and its open socket is closed with 4401 (`closeSocket?`,
     * default true; false leaves it open, as the mock itself would).
     */
    unlink: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const device = await this.resolveDevice(args.device, 'device');
      if (device.aci !== state.device.aci || device === state.device) {
        throw badRequest(`${deviceRef(device)} is not a linked device of ${deviceRef(state.device)}`);
      }
      const closeSocket = optBool(args, 'closeSocket', true);
      await this.server.removeDevice(device, { closeSocket });
      return {};
    },

    /**
     * Opens a second websocket with `device`'s own credentials, as another client using them
     * would, holds it for `holdMs?` (default 0) and closes it. The server closes the device's
     * first socket with 4409 (connected elsewhere). Result: `{status, tookOver}` (`tookOver`:
     * a socket was open and has been replaced).
     */
    secondSocket: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const holdMs = optInt(args, 'holdMs', 0, { max: 60_000 });
      const credentials = this.server.credentials(device);
      if (!credentials) {
        throw notFound(`${deviceRef(device)} has no credentials (it is a scripted device)`);
      }
      const wasConnected = this.server.isConnected(device);
      const before = this.server.webSockets.get(device);
      const session = http2.connect(this.handshake.urls.chat, { ca: this.handshake.caPem });
      const failed = new Promise((_, reject) => {
        session.once('error', reject);
      });
      failed.catch(() => {});
      let stream;
      try {
        // Extended CONNECT is only allowed after the server's SETTINGS enabled it.
        await Promise.race([new Promise((resolve) => session.once('remoteSettings', resolve)), failed]);
        const basic = Buffer.from(`${credentials.username}:${credentials.password}`).toString('base64');
        stream = session.request(
          {
            ':method': 'CONNECT',
            ':protocol': 'websocket',
            ':scheme': 'https',
            ':path': this.handshake.paths.chatWebsocket,
            'sec-websocket-version': '13',
            authorization: `Basic ${basic}`,
          },
          { endStream: false },
        );
        stream.on('error', () => {});
        // What the server pushes at this socket is never answered: it stays queued for the
        // device's next real connection.
        stream.on('data', () => {});
        const status = await Promise.race([
          new Promise((resolve) => stream.once('response', (headers) => resolve(headers[':status']))),
          failed,
        ]);
        if (status === 200) {
          // The mock registers the socket a moment after it answered the upgrade.
          await withTimeout(
            (async () => {
              while (this.server.webSockets.get(device) === before) {
                await new Promise((resolve) => setTimeout(resolve, 5));
              }
            })(),
            5_000,
            'secondSocket',
          );
          await new Promise((resolve) => setTimeout(resolve, holdMs));
        }
        return { status, tookOver: status === 200 && wasConnected };
      } finally {
        stream?.close();
        session.destroy();
      }
    },

    /**
     * Closes `device`'s websocket from the server's side with `code?` (default 1001, going
     * away; 4401 = connection invalidated, 4409 = connected elsewhere). Result: `{closed}`
     * (false: it had none).
     */
    closeSocket: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const code = optInt(args, 'code', 1001, { min: 1000, max: 4999 });
      return { closed: this.server.closeSocket(device, code) };
    },

    /**
     * Refuses the next `count?` websocket upgrades (default until `stop: true`) with `device`'s
     * credentials with HTTP `status?` (401, the default, 403, or any 4xx or 5xx), though the
     * device is still linked: what a client sees whose credentials the server no longer accepts
     * (or, with a 5xx, a server that cannot serve it now). `username` (`"<aci>.<deviceId>"`)
     * instead of `device` names a device that is not linked yet, so that the first upgrade
     * after its link is refused already. Result: `{}`.
     */
    failAuth: async (args) => {
      let username;
      if (args.username !== undefined && args.username !== null) {
        const { aci, deviceId } = parseRef(reqString(args, 'username'), 'username');
        if (deviceId === undefined) {
          throw badRequest('"username" must name a device: "<aci>.<deviceId>"');
        }
        username = `${aci}.${deviceId}`;
      } else {
        const device = await this.resolveDevice(args.device, 'device');
        const credentials = this.server.credentials(device);
        if (!credentials) {
          throw notFound(`${deviceRef(device)} has no credentials (it is a scripted device)`);
        }
        username = credentials.username;
      }
      if (optBool(args, 'stop', false)) {
        this.server.forcedAuth.delete(username);
        return {};
      }
      const status = optInt(args, 'status', 401, { min: 400, max: 599 });
      const count = optInt(args, 'count', -1, { min: -1 });
      this.server.forcedAuth.set(username, { status, left: count });
      return {};
    },

    /**
     * Adds a round trip of `ms` (0, the default, removes it) to every request a client makes
     * from now on: websocket requests and the gRPC multi-recipient send. Result: `{}`.
     */
    setLatency: async (args) => {
      latency.ms = optInt(args, 'ms', 0, { min: 0, max: 10_000 });
      return {};
    },

    /**
     * Holds the answer to every `PUT /v1/devices/link` back for `ms` (0, the default, answers
     * at once again) after the server linked the device: the device is on the account
     * (`listDevices`) while its client still waits to hear so. Result: `{}`.
     */
    delayLink: async (args) => {
      sends.linkDelayMs = optInt(args, 'ms', 0, { min: 0, max: 60_000 });
      return {};
    },

    /**
     * Moves the clock the server states on every websocket upgrade and on every envelope it
     * delivers (`x-signal-timestamp`) by `skewMs` (may be negative; 0 = the true time).
     * Result: `{}`.
     */
    setServerTime: async (args) => {
      const skew = args.skewMs;
      if (!Number.isSafeInteger(skew)) {
        throw badRequest('"skewMs" must be an integer (ms)');
      }
      this.server.timeSkewMs = skew;
      deliveryClock.skewMs = skew;
      return {};
    },

    /**
     * Answers `GET /v1/keepalive` with 500 (`on?`, default true) or serves it again. Result: `{}`.
     */
    failKeepalive: async (args) => {
      sends.failKeepalives = optBool(args, 'on', true);
      return {};
    },

    /**
     * How `device` (a ref; it may have been unlinked since) has been connecting: `{connects,
     * open}`: authenticated websocket upgrades asked for with its name (refused ones and
     * `secondSocket` included) and whether it has a socket now; `keepalives`: every keepalive
     * request the server got (from any socket).
     */
    connectionStats: async (args) => {
      // By name, not by lookup: a device that was unlinked still has a history.
      const { aci, deviceId = PRIMARY_DEVICE_ID } = parseRef(reqString(args, 'device'), 'device');
      const open = [...this.server.webSockets.keys()].some(
        (device) => device.aci === aci && device.deviceId === deviceId,
      );
      return {
        connects: this.server.connects.get(`${aci}.${deviceId}`) ?? 0,
        open,
        keepalives: sends.keepalives,
      };
    },

    /**
     * The one-time prekeys the server holds for `device`, per identity, and its key uploads:
     * `{aci, pni}`, each `{count, pqCount, signedPreKeyId, lastResortKeyId, uploads}` (null
     * when the device has no keys of that identity); `uploads` lists what each client upload
     * carried: `{preKeys, pqPreKeys, signedPreKey (id or null), pqLastResortPreKey (id or null)}`.
     */
    keyCounts: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const describe = async (kind) => {
        const keys = device.keys.get(kind);
        if (!keys) {
          return null;
        }
        return {
          count: await device.getPreKeyCount(kind),
          pqCount: await device.getKyberPreKeyCount(kind),
          signedPreKeyId: keys.signedPreKey?.keyId ?? null,
          lastResortKeyId: keys.lastResortKey?.keyId ?? null,
          uploads: [...(this.server.keyUploads.get(`${deviceRef(device)}|${kind}`) ?? [])],
        };
      };
      return { aci: await describe(ServiceIdKind.ACI), pni: await describe(ServiceIdKind.PNI) };
    },

    /**
     * Uses up `device`'s one-time prekeys on the server until `leave?` (default 0) of each
     * kind are left, for `kind?` (`ACI`, the default, or `PNI`), as senders fetching them
     * would. Only for a device a client linked (it uploaded its keys; the mock makes those of
     * primaries and scripted devices on demand). Result: `{count, pqCount}` left.
     */
    consumeKeys: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const kind = serviceIdKindArg({ serviceIdKind: args.kind });
      const leave = optInt(args, 'leave', 0);
      const keys = device.keys.get(kind);
      if (!keys) {
        throw notFound(`${deviceRef(device)} has no ${kind} keys`);
      }
      if (keys.preKeyIterator || keys.kyberPreKeyIterator) {
        // A primary or a scripted device: the mock makes its one-time keys on demand.
        throw badRequest(`${deviceRef(device)} has no uploaded one-time keys to use up`);
      }
      keys.preKeys.splice(leave);
      keys.kyberPreKeys.splice(leave);
      return {
        count: await device.getPreKeyCount(kind),
        pqCount: await device.getKyberPreKeyCount(kind),
      };
    },

    /**
     * `primary` changes its phone number: the account gets a new PNI, PNI identity key and
     * registration id, the server new PNI keys for every device, and every linked device a
     * `SyncMessage.pniChangeNumber` with its new PNI keys (the envelope carries `updatedPni`;
     * queued for a device that is offline, `awaitDelivery?` waits for all of them, `timeout?`).
     * Result: the primary `{ref, aci, pni, deviceId, e164}` after the change, `oldPni`,
     * `pniIdentityKey` (base64) and `pniRegistrationId`.
     */
    changeNumber: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const oldPni = state.device.pni;
      const devices = [state.device, ...state.primary.secondaryDevices];
      // The sync message to each linked device needs a session with it.
      for (const device of state.primary.secondaryDevices) {
        await this.ensureSession(state, device);
      }
      const prepared = await state.primary.prepareChangeNumber();
      // Delivery waits for each device's socket (an offline device gets it at its next
      // connect), so it is not waited for unless asked.
      const delivery = state.primary.sendChangeNumber(prepared);
      if (optBool(args, 'awaitDelivery', false)) {
        await withTimeout(delivery, timeoutArg(args), 'changeNumber');
      } else {
        delivery.catch((error) => log('number change not delivered', error));
      }
      // Sessions to the old number are sessions to an identity that is gone.
      for (const other of this.#primaries.values()) {
        for (const device of devices) {
          other.pniSessions.delete(device);
        }
      }
      return {
        ...describeDevice(state.device),
        oldPni,
        pniIdentityKey: b64(state.primary.getPublicKey(ServiceIdKind.PNI).serialize()),
        pniRegistrationId: state.device.pniRegistrationId ?? null,
      };
    },

    /**
     * Refuses client 1:1 sends to `target` (a ref) with a challenge: `PUT /v1/messages/<target>`
     * answers 428 `{token, options: ["captcha"]}` (`token?`, default `"challenge-token"`;
     * `Retry-After: <retryAfter?>` seconds, default 60) until a client answers it with
     * `PUT /v1/challenge {type: "captcha", token, captcha}`. `accept?`: the only captcha that
     * is accepted (default: any); a wrong one is answered 428 again. `busy?`: the next that
     * many answers get 413 with `Retry-After: 2` first. `stop: true` lifts the challenge
     * without an answer. Result: `{}`.
     */
    challenge: async (args) => {
      const { aci } = parseRef(reqString(args, 'target'), 'target');
      if (optBool(args, 'stop', false)) {
        sends.challenges.delete(aci);
        return {};
      }
      sends.challenges.set(aci, {
        token: optString(args, 'token') ?? 'challenge-token',
        accept: optString(args, 'accept'),
        retryAfter: optInt(args, 'retryAfter', 60),
        refused: 0,
      });
      sends.busyAnswers = optInt(args, 'busy', 0);
      return {};
    },

    /**
     * Answers every multi-recipient send that arrives over the websocket route
     * (`PUT /v1/messages/multi_recipient`) with 428, a challenge `token?`
     * (`multi-challenge-token`), the option `captcha` and `Retry-After: <retryAfter?>` (60);
     * `stop: true` leaves these sends to the mock again (it has no such route and fails them). A client
     * sends there only while the remote config does not move these sends to gRPC
     * (`setRemoteConfig`). Result: `{refused}` (sends answered that way so far).
     */
    challengeMultiRecipient: async (args) => {
      const refused = sends.multiChallenge?.refused ?? 0;
      if (optBool(args, 'stop', false)) {
        sends.multiChallenge = null;
        return { refused };
      }
      sends.multiChallenge = {
        token: optString(args, 'token') ?? 'multi-challenge-token',
        retryAfter: optInt(args, 'retryAfter', 60, { min: 0 }),
        refused,
      };
      return { refused };
    },

    /**
     * What clients answered: `{answers: [{type, token, captcha, accepted, status}], open:
     * {<service id>: {token, refused}}}` (`refused`: sends answered 428 for that challenge so
     * far).
     */
    challengeAnswers: async () => ({
      answers: sends.challengeAnswers.map((answer) => ({ ...answer })),
      open: Object.fromEntries(
        [...sends.challenges].map(([target, challenge]) => [
          target,
          { token: challenge.token, refused: challenge.refused },
        ]),
      ),
    }),

    /**
     * Waits until `device` has acknowledged every envelope addressed to it (nothing in flight,
     * nothing queued for its next connection). Args: `device`, `timeout?`. Result: `{}`.
     */
    waitForDeliveries: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const timeout = timeoutArg(args);
      await withTimeout(this.server.waitForDeliveries(device), timeout, 'waitForDeliveries');
      return {};
    },

    /**
     * Lists the devices the server has registered for `primary`'s account (the primary first),
     * e.g. to prove that a refused link registered nothing. Args: `primary`.
     * Result: `{devices: [{ref, aci, pni, deviceId, e164}]}`.
     */
    listDevices: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const devices = await this.server.getAllDevicesByServiceId(state.device.aci);
      return { devices: devices.map(describeDevice) };
    },

    /**
     * Registers one more scripted device on `primary`'s account (a linked device nobody
     * drives: clients must send to it, and `waitForEnvelope` sees what they send).
     * Args: `primary`. Result: the device `{ref, aci, pni, deviceId, e164}`.
     */
    addDevice: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const device = await this.server.createSecondaryDevice(state.primary);
      return describeDevice(device);
    },

    /**
     * Waits for the next client envelope addressed to a scripted secondary device (`device`),
     * optionally `from` a device (sealed envelopes have no source). Result:
     * `{to, source, envelopeType, timestamp, sealed}`.
     */
    waitForEnvelope: async (args) => {
      const device = await this.resolveDevice(args.device, 'device');
      const timeout = timeoutArg(args);
      const entry = await this.#scriptedEnvelopes.take(
        (item) => item.target === device,
        timeout,
        'waitForEnvelope',
      );
      return {
        to: deviceRef(entry.target),
        source: entry.source ? deviceRef(entry.source) : null,
        envelopeType: entry.envelopeType,
        timestamp: entry.timestamp,
        sealed: entry.envelopeType === EnvelopeType.SealedSender,
      };
    },

    /**
     * Sends a ReceiptMessage from a primary to one device. Args: `from`, `to`, `type`
     * (`delivery`, `read`, `viewed`), `timestamps` (of the messages), `timestamp?` (now),
     * `sealed?`, `awaitDelivery?`, `timeout?`. Result: `{timestamp, to}`.
     */
    sendReceipt: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      const type = RECEIPT_TYPES[reqString(args, 'type')];
      if (type === undefined) {
        throw badRequest('"type" must be delivery, read or viewed');
      }
      const timestamps = optArray(args, 'timestamps');
      if (timestamps.length === 0 || !timestamps.every((t) => Number.isSafeInteger(t) && t > 0)) {
        throw badRequest('"timestamps" must be positive integers');
      }
      const content = {
        content: { receiptMessage: { type, timestamp: timestamps.map((t) => BigInt(t)) } },
        pniSignatureMessage: null,
        senderKeyDistributionMessage: null,
      };
      return this.#sendContent(sender, target, content, {
        timestamp: optInt(args, 'timestamp', Date.now(), { min: 1 }),
        sealed: optBool(args, 'sealed', false),
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Sends a TypingMessage from a primary to one device, 1:1 or in `group?` (a group id).
     * Args: `from`, `to`, `action` (`started`, `stopped`), `timestamp?` (now), `sealed?`,
     * `group?`, `awaitDelivery?`, `timeout?`. Result: `{timestamp, to}`.
     */
    sendTyping: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      const action = TYPING_ACTIONS[reqString(args, 'action')];
      if (action === undefined) {
        throw badRequest('"action" must be started or stopped');
      }
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const content = {
        content: {
          typingMessage: {
            timestamp: BigInt(timestamp),
            action,
            groupId: args.group == null
              ? null
              : Buffer.from(this.resolveGroup(reqString(args, 'group')).publicParams.getGroupIdentifier().serialize()),
          },
        },
        pniSignatureMessage: null,
        senderKeyDistributionMessage: null,
      };
      return this.#sendContent(sender, target, content, {
        timestamp,
        sealed: optBool(args, 'sealed', false),
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Sends a sync transcript (SyncMessage.Sent) of a 1:1 text from a primary to one of its
     * linked devices: the phone "sent" `text` to `destination` (an ACI). Args: `from`, `to`,
     * `destination`, `text`, `timestamp?` (now), `withProfileKey?` (true: what real phones
     * send in accepted chats), `expireTimer?` (seconds), `unidentified?` (false),
     * `destinationKind?` (`ACI`; `PNI`: the phone wrote to the contact's number, so the
     * transcript names their PNI), `awaitDelivery?`, `timeout?`. Result: `{timestamp, to}`.
     */
    sendSyncSent: async (args) => {
      const { sender, target } = await this.#syncPair(args);
      const destination = parseRef(args.destination, 'destination').aci;
      if (typeof args.text !== 'string') {
        throw badRequest('"text" must be a string');
      }
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      let destinationBinary = aciBytes(destination);
      if (serviceIdKindArg({ serviceIdKind: args.destinationKind }) === ServiceIdKind.PNI) {
        const pni = this.resolvePrimary(args.destination, 'destination').device.pniRawUuid;
        if (!pni) {
          throw badRequest('"destination" has no PNI');
        }
        // A PNI's binary form: one byte of kind, then the UUID.
        destinationBinary = Buffer.concat([Buffer.from([1]), Buffer.from(pni)]);
      }
      const expireTimer = optInt(args, 'expireTimer', 0, { max: 2 ** 32 - 1 });
      const dataMessage = {
        ...mock.EMPTY_DATA_MESSAGE,
        body: args.text,
        timestamp: BigInt(timestamp),
        profileKey: optBool(args, 'withProfileKey', true) ? sender.primary.profileKey.serialize() : null,
        expireTimer: expireTimer > 0 ? expireTimer : null,
      };
      const content = syncContent({
        content: {
          sent: {
            destinationServiceIdBinary: destinationBinary,
            timestamp: BigInt(timestamp),
            message: dataMessage,
            unidentifiedStatus: [
              {
                destinationServiceIdBinary: destinationBinary,
                unidentified: optBool(args, 'unidentified', false),
                destinationServiceId: null,
                destinationPniIdentityKey: null,
              },
            ],
            destinationE164: null,
            isRecipientUpdate: null,
            expirationStartTimestamp: expireTimer > 0 ? BigInt(timestamp) : null,
            storyMessage: null,
            storyMessageRecipients: null,
            editMessage: null,
            destinationServiceId: null,
          },
        },
      });
      return this.#sendContent(sender, target, content, {
        timestamp,
        sealed: false,
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Sends SyncMessage.MessageRequestResponse for the 1:1 chat with `thread` (an ACI) from a
     * primary to one of its linked devices. Args: `from`, `to`, `thread`, `type` (`accept`,
     * `delete`, `block`, `blockAndDelete`, `spam`, `blockAndSpam`), `awaitDelivery?`,
     * `timeout?`. Result: `{timestamp, to}`.
     */
    sendMessageRequestResponse: async (args) => {
      const { sender, target } = await this.#syncPair(args);
      const thread = parseRef(args.thread, 'thread').aci;
      const type = REQUEST_RESPONSES[reqString(args, 'type')];
      if (type === undefined) {
        throw badRequest('"type" must be a MessageRequestResponse type name');
      }
      const content = syncContent({
        content: {
          messageRequestResponse: { threadAciBinary: aciBytes(thread), type, threadAci: null, groupId: null },
        },
      });
      return this.#sendContent(sender, target, content, {
        timestamp: Date.now(),
        sealed: false,
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Sends SyncMessage.Configuration from a primary to one of its linked devices. Args:
     * `from`, `to`, `readReceipts?`, `typingIndicators?`, `linkPreviews?`,
     * `unidentifiedDeliveryIndicators?` (absent ones are not sent), `awaitDelivery?`,
     * `timeout?`. Result: `{timestamp, to}`.
     */
    sendConfiguration: async (args) => {
      const { sender, target } = await this.#syncPair(args);
      const content = syncContent({
        content: {
          configuration: {
            readReceipts: optBool(args, 'readReceipts', null),
            typingIndicators: optBool(args, 'typingIndicators', null),
            linkPreviews: optBool(args, 'linkPreviews', null),
            unidentifiedDeliveryIndicators: optBool(args, 'unidentifiedDeliveryIndicators', null),
            provisioningVersion: null,
          },
        },
      });
      return this.#sendContent(sender, target, content, {
        timestamp: Date.now(),
        sealed: false,
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Waits for the next ReceiptMessage a primary decrypted, optionally `from` a device.
     * Result: `{source, type (delivery|read|viewed), timestamps, sealed, sealedGroupId,
     * contentHint, pniSignature, remaining}`: `sealedGroupId` = the group id (base64) the sealed
     * content names, `contentHint` its hint (both null when unsealed or none); `pniSignature` =
     * the Content carried a `PniSignatureMessage` beside the receipt.
     */
    waitForReceipt: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const entry = await state.inbox.receipt.take(
        (item) => from === undefined || refMatches(from, item.source),
        timeoutArg(args),
        'waitForReceipt',
      );
      const names = Object.fromEntries(Object.entries(RECEIPT_TYPES).map(([name, value]) => [value, name]));
      return {
        source: deviceRef(entry.source),
        type: names[entry.receiptMessage.type ?? 0] ?? String(entry.receiptMessage.type),
        timestamps: (entry.receiptMessage.timestamp ?? []).map(toNumber),
        sealed: entry.content?.[SEALED_SENDER] === true,
        sealedGroupId: entry.content?.[SEALED_GROUP] ?? null,
        contentHint: entry.content?.[SEALED_HINT] ?? null,
        pniSignature: entry.content?.pniSignatureMessage != null,
        remaining: state.inbox.receipt.size,
      };
    },

    /**
     * Waits for the next TypingMessage a primary decrypted, optionally `from` a device.
     * Result: `{source, action (started|stopped), timestamp, groupId, sealed}`.
     */
    waitForTyping: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const entry = await state.inbox.typing.take(
        (item) => from === undefined || refMatches(from, item.source),
        timeoutArg(args),
        'waitForTyping',
      );
      return {
        source: deviceRef(entry.source),
        action: (entry.typing.action ?? 0) === TYPING_ACTIONS.stopped ? 'stopped' : 'started',
        timestamp: toNumber(entry.typing.timestamp),
        groupId: entry.typing.groupId ? b64(entry.typing.groupId) : null,
        sealed: entry.content?.[SEALED_SENDER] === true,
      };
    },

    /**
     * Waits for the next Content a primary decrypted that carried a sender-key distribution
     * message and nothing else (what a group send's 1:1 distribution looks like), optionally
     * `from` a device. Args: `primary`, `from?`, `timeout?`. Result: `{source, sealed,
     * sealedGroupId, contentHint}`: the group and the content hint (`default`, `resendable`,
     * `implicit`) its sealed content names, null when it was not sealed.
     */
    waitForDistribution: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const entry = await state.inbox.distribution.take(
        (item) => from === undefined || refMatches(from, item.source),
        timeoutArg(args),
        'waitForDistribution',
      );
      return {
        source: deviceRef(entry.source),
        sealed: entry.content?.[SEALED_SENDER] === true,
        sealedGroupId: entry.content?.[SEALED_GROUP] ?? null,
        contentHint: entry.content?.[SEALED_HINT] ?? null,
      };
    },

    /**
     * Waits for the next retry request (`DecryptionErrorMessage`) a primary decrypted,
     * optionally only one `from` a device. Args: `primary`, `from?`, `timeout?`. Result:
     * `{source, timestamp, senderDevice, hasRatchetKey}` (`timestamp` and `senderDevice` name
     * the original message the sender could not read).
     */
    waitForDecryptionError: async (args) => {
      const state = this.resolvePrimary(args.primary ?? args.device, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const entry = await state.inbox.decryptionError.take(
        (item) => from === undefined || refMatches(from, item.source),
        timeoutArg(args),
        'waitForDecryptionError',
      );
      return {
        source: deviceRef(entry.source),
        timestamp: toNumber(entry.timestamp),
        senderDevice: entry.senderDevice,
        hasRatchetKey: entry.ratchetKey != null,
      };
    },

    /**
     * Waits for the next sync message (not a sync request) a primary got from one of its
     * linked devices, optionally `from` a device and of a `kind` (`sent`, `read`, `viewed`, or
     * any SyncMessage content field name). Result: `{source, kind, syncMessage}`.
     */
    waitForSyncMessage: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const kind = optString(args, 'kind');
      const entry = await state.inbox.syncMessage.take(
        (item) =>
          (from === undefined || refMatches(from, item.source)) &&
          (kind === undefined || syncKind(item.syncMessage) === kind),
        timeoutArg(args),
        'waitForSyncMessage',
      );
      return {
        source: deviceRef(entry.source),
        kind: syncKind(entry.syncMessage),
        syncMessage: jsonSafe(entry.syncMessage) ?? {},
      };
    },

    /**
     * Replaces the primary's Storage Service state (a new manifest version, as the phone writes
     * it). Args: `primary`; `contacts?` `[{ref, profileKey? (true), givenName? (their profile
     * name), familyName?, systemGivenName?, systemFamilyName?, whitelisted? (true), blocked?,
     * archived?, markedUnread?, mutedUntil? (ms), pinned?, identityKey? (true), byPni? (the
     * record names the contact's PNI and number only: no ACI, profile key, identity key or
     * profile name unless asked for), withPni? (the record carries the PNI beside the ACI:
     * the phone joined the two)}]` (refs of
     * primaries); `groups?` `[{id, blocked?, whitelisted? (true), archived?, markedUnread?,
     * mutedUntil? (ms), pinned?}]` (`GroupV2Record`s of harness groups); `extraContacts?`
     * (synthetic contacts "Contact <n>" with random ACIs);
     * `account?` `{readReceipts?, typingIndicators?, linkPreviews?, sealedSenderIndicators?,
     * phoneNumberSharing? (everybody|nobody), universalExpireTimer? (s), noteToSelfArchived?,
     * noteToSelfMarkedUnread?}`; `recordIkm?` (true; false = legacy record keys). Result:
     * `{version, records}`.
     */
    setStorageState: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const { primary } = state;
      const specs = optArray(args, 'contacts').map((spec, i) => {
        if (typeof spec !== 'object' || spec === null || Array.isArray(spec)) {
          throw badRequest(`"contacts[${i}]" must be an object`);
        }
        return { spec, contact: this.resolvePrimary(reqString(spec, 'ref'), `contacts[${i}].ref`) };
      });
      const groupSpecs = optArray(args, 'groups').map((spec, i) => {
        if (typeof spec !== 'object' || spec === null || Array.isArray(spec)) {
          throw badRequest(`"groups[${i}]" must be an object`);
        }
        return { spec, group: this.resolveGroup(reqString(spec, 'id')) };
      });
      const extra = optInt(args, 'extraContacts', 0, { max: MAX_EXTRA_CONTACTS });
      const account = args.account ?? {};
      if (typeof account !== 'object' || Array.isArray(account)) {
        throw badRequest('"account" must be an object');
      }
      const sharing = optString(account, 'phoneNumberSharing') ?? 'everybody';
      if (!Object.hasOwn(NUMBER_SHARING, sharing)) {
        throw badRequest('"account.phoneNumberSharing" must be everybody or nobody');
      }
      // The previous state decrypts under the record keys it was written with.
      const previous = await primary.getStorageState();
      if (optBool(args, 'recordIkm', true) === false) {
        primary.storageRecordIkm = undefined;
      }

      let next = StorageState.getEmpty().updateAccount({
        profileKey: Buffer.from(primary.profileKey.serialize()),
        givenName: primary.profileName,
        readReceipts: optBool(account, 'readReceipts', true),
        typingIndicators: optBool(account, 'typingIndicators', true),
        linkPreviews: optBool(account, 'linkPreviews', false),
        sealedSenderIndicators: optBool(account, 'sealedSenderIndicators', false),
        phoneNumberSharingMode: NUMBER_SHARING[sharing],
        universalExpireTimer: optInt(account, 'universalExpireTimer', 0),
        noteToSelfArchived: optBool(account, 'noteToSelfArchived', false),
        noteToSelfMarkedUnread: optBool(account, 'noteToSelfMarkedUnread', false),
      });
      for (const { spec, contact } of specs) {
        const theirs = contact.primary;
        // A contact the phone knows by number only has a PNI and no ACI, no profile and no
        // identity key; once it learned the account, the record carries both ids.
        const byPni = optBool(spec, 'byPni', false);
        const record = {
          profileKey:
            optBool(spec, 'profileKey', !byPni) ? Buffer.from(theirs.profileKey.serialize()) : null,
          identityKey: optBool(spec, 'identityKey', !byPni)
            ? Buffer.from(theirs.getPublicKey(ServiceIdKind.ACI).serialize())
            : null,
          givenName: optString(spec, 'givenName') ?? (byPni ? null : theirs.profileName),
          familyName: optString(spec, 'familyName') ?? null,
          systemGivenName: optString(spec, 'systemGivenName') ?? null,
          systemFamilyName: optString(spec, 'systemFamilyName') ?? null,
          whitelisted: optBool(spec, 'whitelisted', true),
          blocked: optBool(spec, 'blocked', false),
          archived: optBool(spec, 'archived', false),
          markedUnread: optBool(spec, 'markedUnread', false),
          mutedUntilTimestamp: BigInt(optInt(spec, 'mutedUntil', 0)),
        };
        if (optBool(spec, 'withPni', false)) {
          record.pniBinary = theirs.device.pniRawUuid ?? null;
        }
        next = next.addContact(theirs, record, byPni ? ServiceIdKind.PNI : ServiceIdKind.ACI);
        if (optBool(spec, 'pinned', false)) {
          next = next.pin(theirs);
        }
      }
      for (const { spec, group } of groupSpecs) {
        next = next.addGroup(group, {
          blocked: optBool(spec, 'blocked', false),
          whitelisted: optBool(spec, 'whitelisted', true),
          archived: optBool(spec, 'archived', false),
          markedUnread: optBool(spec, 'markedUnread', false),
          mutedUntilTimestamp: BigInt(optInt(spec, 'mutedUntil', 0)),
        });
        if (optBool(spec, 'pinned', false)) {
          next = next.pinGroup(group);
        }
      }
      for (let i = 0; i < extra; i += 1) {
        next = next.addRecord({
          type: STORAGE_CONTACT,
          record: {
            contact: {
              aciBinary: nodeCrypto.randomBytes(16),
              givenName: `Contact ${i + 1}`,
              whitelisted: false,
            },
          },
        });
      }
      const written = await primary.setStorageState(next, previous);
      return { version: toNumber(written.version), records: written.items.length };
    },

    /**
     * Encrypts `data` (base64) like Desktop's `encryptAttachmentV2` (with its padding) and puts
     * it on CDN0, as a phone's upload would. Args: `data`, `contentType?`. Result: the
     * `AttachmentPointer` fields `{cdnKey, cdnNumber, key, digest, size, contentType}` (bytes as
     * base64).
     */
    storeAttachment: async (args) => {
      const plaintext = Buffer.from(reqString(args, 'data'), 'base64');
      const contentType = optString(args, 'contentType') ?? 'application/octet-stream';
      const padded = Buffer.alloc(Math.max(plaintext.length, logPadSize(plaintext.length)));
      plaintext.copy(padded);
      const aesKey = nodeCrypto.randomBytes(32);
      const macKey = nodeCrypto.randomBytes(32);
      const iv = nodeCrypto.randomBytes(16);
      const cipher = nodeCrypto.createCipheriv('aes-256-cbc', aesKey, iv);
      const body = Buffer.concat([iv, cipher.update(padded), cipher.final()]);
      const mac = nodeCrypto.createHmac('sha256', macKey).update(body).digest();
      const blob = Buffer.concat([body, mac]);
      const digest = nodeCrypto.createHash('sha256').update(blob).digest();
      const cdnKey = await this.server.storeAttachment(blob);
      return {
        cdnKey,
        cdnNumber: 0,
        key: Buffer.concat([aesKey, macKey]).toString('base64'),
        digest: digest.toString('base64'),
        size: plaintext.length,
        contentType,
      };
    },

    /**
     * Sends a DataMessage with attachments from a primary to one device, the way a phone sends
     * them: each file encrypted like Desktop (`encryptAttachmentV2`) and stored on the mock's
     * CDN 3 (TUS store; `cdn: 0` uses the legacy `/attachments/` store). Args: `from`, `to`,
     * `attachments` `[{data (base64), contentType?, fileName?, width?, height?, flags? (1 voice,
     * 8 GIF), caption?, blurHash?, durationSeconds?, cdn? (3), forge? ({digest? (true: a wrong
     * digest), size?, cdnNumber?, cdnKey?}: what a hostile sender puts into the pointer in
     * place of the truth; the blob is stored as usual)}]`, `text?` (the body; longer than
     * 2048 bytes goes out trimmed with a `text/x-signal-plain` attachment first, like Desktop),
     * `sticker?` (the first attachment becomes the message's sticker), `sealed?`,
     * `withProfileKey?` (true), `timestamp?`, `editTarget?` (a sent timestamp: the message goes
     * out as an EditMessage for it, the edit of a message with attachments), `awaitDelivery?`,
     * `timeout?`. Result:
     * `{timestamp, to, pointers}` (the pointers as sent, `jsonSafe`).
     */
    sendAttachments: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      if (target === sender.device) {
        throw badRequest('a primary cannot send to itself');
      }
      const specs = optArray(args, 'attachments');
      const text = optString(args, 'text');
      if (specs.length === 0 && text === undefined) {
        throw badRequest('"attachments" or "text" is required');
      }
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const pointerOf = (plaintext, spec) => this.#pointerOf(plaintext, spec);
      const pointers = [];
      let body = text ?? null;
      if (text !== undefined && Buffer.byteLength(text) > MAX_INLINE_BODY_BYTES) {
        pointers.push(await pointerOf(Buffer.from(text, 'utf8'), { contentType: 'text/x-signal-plain' }));
        body = trimBody(text);
      }
      for (const [i, spec] of specs.entries()) {
        if (typeof spec !== 'object' || spec === null || typeof spec.data !== 'string') {
          throw badRequest(`attachments[${i}].data must be base64`);
        }
        pointers.push(await pointerOf(Buffer.from(spec.data, 'base64'), spec));
      }
      let sticker = null;
      if (optBool(args, 'sticker', false)) {
        const index = pointers.findIndex((p) => p.contentType !== 'text/x-signal-plain');
        if (index < 0) {
          throw badRequest('a sticker needs an attachment');
        }
        const [data] = pointers.splice(index, 1);
        sticker = {
          packId: nodeCrypto.randomBytes(16),
          packKey: nodeCrypto.randomBytes(32),
          stickerId: 0,
          data,
          emoji: null,
        };
      }
      const dataMessage = {
        ...mock.EMPTY_DATA_MESSAGE,
        body,
        attachments: pointers,
        sticker,
        timestamp: BigInt(timestamp),
        profileKey: optBool(args, 'withProfileKey', true) ? sender.primary.profileKey.serialize() : null,
      };
      const editTarget = optInt(args, 'editTarget', undefined, { min: 1 });
      const content = {
        content:
          editTarget === undefined
            ? { dataMessage }
            : { editMessage: { targetSentTimestamp: BigInt(editTarget), dataMessage } },
        pniSignatureMessage: null,
        senderKeyDistributionMessage: null,
      };
      const sent = await this.#sendContent(sender, target, content, {
        timestamp,
        sealed: optBool(args, 'sealed', false),
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
      return { ...sent, pointers: jsonSafe([...pointers, ...(sticker ? [sticker.data] : [])]) };
    },

    /**
     * Downloads and decrypts an attachment a client uploaded, the way a phone would: the blob
     * from the mock's CDN (3: the TUS store, 0: the legacy store), its MAC, digest and (when the
     * pointer has one) incremental MAC checked with libsignal-client, the padding stripped to
     * `size`. Args: `pointer` (a pointer from `waitForMessage`'s `dataMessage.attachments`).
     * Result: `{data (base64), size, blobLength, macValid, digestValid, incrementalMacValid
     * (null without one), chunkSize}`.
     */
    fetchAttachment: async (args) => {
      const pointer = args.pointer;
      if (typeof pointer !== 'object' || pointer === null) {
        throw badRequest('"pointer" must be an object');
      }
      const cdnKey = pointer.attachmentIdentifier?.cdnKey ?? pointer.cdnKey;
      if (typeof cdnKey !== 'string' || !/^[A-Za-z0-9_\-.=]+$/.test(cdnKey)) {
        throw badRequest('pointer has no usable cdnKey');
      }
      const cdn = toNumber(pointer.cdnNumber ?? 0);
      let blob;
      if (cdn === 3) {
        blob = await fs.readFile(path.join(this.stateDir, 'cdn3', 'attachments', cdnKey)).catch((error) => {
          if (error?.code === 'ENOENT') {
            return null;
          }
          throw error;
        });
      } else if (cdn === 0) {
        blob = await this.server.fetchAttachment(cdnKey);
      }
      if (!blob) {
        throw new HarnessError('notFound', `no blob on CDN ${cdn}`);
      }
      blob = Buffer.from(blob);
      const key = pointerBytes(pointer.key, 'key');
      const digest = pointerBytes(pointer.digest, 'digest');
      const size = toNumber(pointer.size);
      if (key.length !== 64 || blob.length < 64) {
        throw badRequest('bad key or blob length');
      }
      const macKey = key.subarray(32);
      const body = blob.subarray(0, blob.length - 32);
      const mac = nodeCrypto.createHmac('sha256', macKey).update(body).digest();
      const macValid = mac.equals(blob.subarray(blob.length - 32));
      const digestValid = nodeCrypto.createHash('sha256').update(blob).digest().equals(digest);
      const decipher = nodeCrypto.createDecipheriv('aes-256-cbc', key.subarray(0, 32), body.subarray(0, 16));
      const padded = Buffer.concat([decipher.update(body.subarray(16)), decipher.final()]);
      let incrementalMacValid = null;
      const chunkSize = pointer.chunkSize == null ? null : toNumber(pointer.chunkSize);
      if (pointer.incrementalMac != null) {
        const incremental = require('@signalapp/libsignal-client/dist/incremental_mac.js');
        const expected = pointerBytes(pointer.incrementalMac, 'incrementalMac');
        const digester = new incremental.DigestingPassThrough(macKey, incremental.everyNthByte(chunkSize));
        digester.resume();
        await new Promise((resolve, reject) => {
          digester.on('end', resolve);
          digester.on('error', reject);
          digester.end(blob);
        });
        incrementalMacValid = Buffer.from(digester.getFinalDigest()).equals(expected);
      }
      return {
        data: padded.subarray(0, size).toString('base64'),
        size,
        blobLength: blob.length,
        macValid,
        digestValid,
        incrementalMacValid,
        chunkSize,
      };
    },

    /**
     * The version of the primary's Storage Service manifest on the server. Args: `primary`.
     * Result: `{version}` (`null` without a manifest; a linked device must never create one).
     */
    storageVersion: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const manifest = this.server.getStorageManifest(state.device);
      return { version: toNumber(manifest?.version) };
    },

    /**
     * The primary tells its linked device to fetch something again (`SyncMessage.FetchLatest`).
     * Args: `from`, `to` (its linked device), `type` (`storageManifest`, `localProfile`,
     * `subscriptionStatus`), `awaitDelivery?`, `timeout?`. Result: `{timestamp, to}`.
     */
    sendFetchLatest: async (args) => {
      const { sender, target } = await this.#syncPair(args);
      const type = reqString(args, 'type');
      if (!Object.hasOwn(FETCH_LATEST, type)) {
        throw badRequest(`unknown "type" ${type}`);
      }
      const content = syncContent({ content: { fetchLatest: { type: FETCH_LATEST[type] } } });
      return this.#sendContent(sender, target, content, {
        timestamp: Date.now(),
        sealed: false,
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Creates a group on the mock's Storage Service as `from`'s phone would (Desktop's group
     * state: title, description, timer and avatar as encrypted `GroupAttributeBlob`s, members
     * with encrypted ids and profile keys, member-level access). `from` is an administrator.
     * Args: `from`, `title`, `members?` (refs of other primaries), `admins?` (refs among them),
     * `description?`, `timer?` (seconds), `avatar?` (image bytes, base64; stored on CDN0
     * encrypted with the group's blob key), `announcementsOnly?`. Result: the group
     * (`{id, title, masterKey, revision, members, avatarKey}`).
     */
    createGroup: async (args) => {
      const creator = this.resolvePrimary(args.from, 'from');
      const title = reqString(args, 'title');
      const members = optArray(args, 'members').map((ref, i) => this.resolvePrimary(ref, `members[${i}]`));
      const admins = new Set(optArray(args, 'admins').map((ref, i) => parseRef(ref, `admins[${i}]`).aci));
      const description = optString(args, 'description');
      const timer = optInt(args, 'timer', 0, { max: 2 ** 32 - 1 });
      const avatar = optString(args, 'avatar');
      const announcementsOnly = optBool(args, 'announcementsOnly', false);

      const secretParams = zkgroup.GroupSecretParams.generate();
      const all = [creator, ...members.filter((m) => m !== creator)];
      let avatarUrl = null;
      if (avatar !== undefined) {
        const blob = groupBlob(secretParams, { avatar: Buffer.from(avatar, 'base64') });
        avatarUrl = `attachments/${await this.server.storeAttachment(blob)}`;
      }
      const state = {
        publicKey: Buffer.from(secretParams.getPublicParams().serialize()),
        version: 0,
        title: groupBlob(secretParams, { title }),
        description: description === undefined ? null : groupBlob(secretParams, { descriptionText: description }),
        avatarUrl,
        disappearingMessagesTimer: timer > 0 ? groupBlob(secretParams, { disappearingMessagesDuration: timer }) : null,
        accessControl: {
          attributes: ACCESS_MEMBER,
          members: ACCESS_MEMBER,
          addFromInviteLink: ACCESS_UNSATISFIABLE,
          memberLabel: ACCESS_MEMBER,
        },
        // The mock's server group takes members as profile-key presentations (it decrypts the
        // id and the profile key from them, as the real server verifies them).
        members: await Promise.all(
          all.map(async (member) => ({
            role:
              member === creator || admins.has(member.device.aci)
                ? MEMBER_ROLE.administrator
                : MEMBER_ROLE.default,
            presentation: Buffer.from((await member.primary.getProfileKeyPresentation(secretParams)).serialize()),
            userId: null,
            profileKey: null,
            joinedAtVersion: null,
            labelEmoji: null,
            labelString: null,
          })),
        ),
        membersPendingProfileKey: null,
        membersPendingAdminApproval: null,
        inviteLinkPassword: null,
        announcementsOnly: announcementsOnly || null,
        membersBanned: null,
        terminated: null,
      };
      const serverGroup = await this.server.createGroup(state);
      const group = new mock.Group({ secretParams, groupState: serverGroup.state });
      this.#groups.set(group.id, group);
      creator.groups.push(group);
      return describeGroup(group);
    },

    /**
     * Changes a group as member `from` (one signed change, one new revision), then sends the
     * update (`DataMessage.groupV2` with the new revision and the signed change) to `to?`
     * (device refs; `from`'s own linked devices get it as a sync transcript). The mock's own
     * modify path has no member adds or avatar changes, so the change is built here.
     * Args: `from`, `group`, `title?`, `description?`, `timer?` (seconds, 0 = off), `avatar?`
     * (base64 image; `""` removes it), `addMembers?`, `removeMembers?` (refs), `admins?` (refs
     * among the added), `announcementsOnly?`, `to?`, `timestamp?` (now). Result:
     * `{revision, timestamp, group}`.
     */
    modifyGroup: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const group = this.resolveGroup(reqString(args, 'group'));
      const add = optArray(args, 'addMembers').map((ref, i) => this.resolvePrimary(ref, `addMembers[${i}]`));
      const remove = optArray(args, 'removeMembers').map((ref, i) => parseRef(ref, `removeMembers[${i}]`).aci);
      const admins = new Set(optArray(args, 'admins').map((ref, i) => parseRef(ref, `admins[${i}]`).aci));
      const targets = await Promise.all(optArray(args, 'to').map((ref, i) => this.resolveDevice(ref, `to[${i}]`)));
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const title = optString(args, 'title');
      const description = optString(args, 'description');
      const avatar = optString(args, 'avatar');
      const timer = optInt(args, 'timer', undefined, { max: 2 ** 32 - 1 });
      const announcementsOnly = optBool(args, 'announcementsOnly', undefined);

      const serverGroup = await this.server.getGroupUnchecked(group.publicParams.serialize());
      if (!serverGroup) {
        throw notFound(`group ${group.id} is not on the server`);
      }
      const { secretParams } = group;
      const current = serverGroup.state;
      const version = (current.version ?? 0) + 1;
      const userIdOf = (aci) => Buffer.from(group.encryptServiceId(aci));
      if (!(current.members ?? []).some((m) => m.userId && Buffer.from(m.userId).equals(userIdOf(sender.device.aci)))) {
        throw badRequest('"from" is not a member of the group');
      }
      const actions = {
        ...mock.EMPTY_GROUP_ACTIONS,
        sourceUserId: userIdOf(sender.device.aci),
        groupId: Buffer.from(group.publicParams.getGroupIdentifier().serialize()),
        version,
      };
      const next = { ...current, version };
      let members = [...(current.members ?? [])];
      if (add.length > 0) {
        const added = add.map((member) =>
          encryptedMember(
            secretParams,
            member.primary,
            admins.has(member.device.aci) ? MEMBER_ROLE.administrator : MEMBER_ROLE.default,
            version,
          ),
        );
        for (const member of added) {
          if (members.some((m) => m.userId && Buffer.from(m.userId).equals(member.userId))) {
            throw badRequest('a member to add is already in the group');
          }
        }
        members = [...members, ...added];
        actions.addMembers = added.map((member) => ({ added: member, joinFromInviteLink: false }));
      }
      if (remove.length > 0) {
        const ids = remove.map(userIdOf);
        for (const id of ids) {
          if (!members.some((m) => m.userId && Buffer.from(m.userId).equals(id))) {
            throw badRequest('a member to remove is not in the group');
          }
        }
        members = members.filter((m) => !ids.some((id) => m.userId && Buffer.from(m.userId).equals(id)));
        actions.deleteMembers = ids.map((deletedUserId) => ({ deletedUserId }));
      }
      next.members = members;
      if (title !== undefined) {
        next.title = groupBlob(secretParams, { title });
        actions.modifyTitle = { title: next.title };
      }
      if (description !== undefined) {
        next.description = groupBlob(secretParams, { descriptionText: description });
        actions.modifyDescription = { description: next.description };
      }
      if (timer !== undefined) {
        next.disappearingMessagesTimer = groupBlob(secretParams, { disappearingMessagesDuration: timer });
        actions.modifyDisappearingMessageTimer = { timer: next.disappearingMessagesTimer };
      }
      if (avatar !== undefined) {
        let url = '';
        if (avatar.length > 0) {
          const blob = groupBlob(secretParams, { avatar: Buffer.from(avatar, 'base64') });
          url = `attachments/${await this.server.storeAttachment(blob)}`;
        }
        next.avatarUrl = url || null;
        actions.modifyAvatar = { avatar: url };
      }
      if (announcementsOnly !== undefined) {
        next.announcementsOnly = announcementsOnly;
        actions.modifyAnnouncementsOnly = { announcementsOnly };
      }
      const encodedActions = Buffer.from(Proto.GroupChange.Actions.encode(actions));
      const changeEpoch = 1;
      const serverSignature = Buffer.from(serverGroup.zkSecret.sign(encodedActions).serialize());
      // The log keeps the change without its signature, as the mock's own modify does.
      serverGroup.changes.groupChanges.push({
        groupChange: { actions: encodedActions, changeEpoch, serverSignature: null },
        groupState: next,
      });
      const updated = new mock.Group({ secretParams, groupState: next });
      this.#groups.set(updated.id, updated);
      for (const state of this.#primaries.values()) {
        state.groups = state.groups.map((g) => (g.id === updated.id ? updated : g));
      }

      const groupV2 = {
        masterKey: updated.masterKey,
        revision: version,
        groupChange: Buffer.from(Proto.GroupChange.encode({ actions: encodedActions, changeEpoch, serverSignature })),
      };
      for (const target of targets) {
        const dataMessage = { ...mock.EMPTY_DATA_MESSAGE, groupV2, timestamp: BigInt(timestamp) };
        const content =
          target.aci === sender.device.aci
            ? syncContent({
                content: {
                  sent: {
                    destinationServiceIdBinary: null,
                    timestamp: BigInt(timestamp),
                    message: dataMessage,
                    unidentifiedStatus: [],
                    destinationE164: null,
                    isRecipientUpdate: null,
                    expirationStartTimestamp: null,
                    storyMessage: null,
                    storyMessageRecipients: null,
                    editMessage: null,
                    destinationServiceId: null,
                  },
                },
              })
            : { content: { dataMessage }, pniSignatureMessage: null, senderKeyDistributionMessage: null };
        await this.#sendContent(sender, target, content, {
          timestamp,
          sealed: false,
          awaitDelivery: false,
          timeout: DEFAULT_WAIT_MS,
        });
      }
      return { revision: version, timestamp, group: describeGroup(updated) };
    },

    /**
     * Sends a group text from member `from` to devices `to` (refs): pairwise, or with
     * `senderKey?` as Desktop sends groups (a sender-key distribution message to each device
     * once, then the text as a sealed sender-key message). Args: `from`, `group`, `to`, `text`,
     * `timestamp?` (now), `senderKey?` (false), `sealed?` (pairwise only, false). Result:
     * `{timestamp, to}` (`to` the refs).
     */
    sendGroupText: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const group = this.resolveGroup(reqString(args, 'group'));
      const targets = await Promise.all(optArray(args, 'to').map((ref, i) => this.resolveDevice(ref, `to[${i}]`)));
      if (typeof args.text !== 'string') {
        throw badRequest('"text" must be a string');
      }
      if (targets.length === 0) {
        throw badRequest('"to" names no device');
      }
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const senderKey = optBool(args, 'senderKey', false);
      const sealed = optBool(args, 'sealed', false);
      for (const target of targets) {
        await this.ensureSession(sender, target);
        if (!senderKey) {
          const envelope = await sender.primary.encryptText(target, args.text, { timestamp, sealed, group });
          this.#reportFailure(this.deliver(sender, target, envelope), target, timestamp);
          continue;
        }
        const distributionId = await this.#distributionFor(sender, target, group, timestamp);
        const content = {
          content: {
            dataMessage: {
              ...mock.EMPTY_DATA_MESSAGE,
              groupV2: group.toContext(),
              body: args.text,
              timestamp: BigInt(timestamp),
            },
          },
          pniSignatureMessage: null,
          senderKeyDistributionMessage: null,
        };
        const envelope = await sender.primary.encryptContent(target, content, {
          timestamp,
          sealed: true,
          distributionId,
          group,
        });
        this.#reportFailure(this.deliver(sender, target, envelope), target, timestamp);
      }
      return { timestamp, to: targets.map(deviceRef) };
    },

    /**
     * Sends a DataMessage built from parts, or (with `edit`) an EditMessage around it: what the
     * phone and contacts need to edit, delete, react, quote and change timers, and what a
     * hostile sender needs to try the same on other people's messages (targets are not
     * checked). Args: `from` (primary), `to` (a device ref, or an array of refs for a group),
     * `group?` (group id: sets the GroupContextV2), `senderKey?` (group only: as a sealed
     * sender-key message, false), `sealed?` (pairwise, false), `timestamp?` (now),
     * `dataTimestamp?` (the DataMessage's own timestamp when it should differ),
     * `serverTimestamp?` (the envelope's server timestamp; default the time now, as a real
     * server stamps it, whatever `timestamp` claims), `text?`,
     * `withProfileKey?` (false), `expireTimer?`, `expireTimerVersion?`, `flags?` (number: 1 end
     * session, 2 timer update, 4 profile key update), `viewOnce?`, `requiredProtocolVersion?`,
     * `reaction?` `{emoji, remove?, targetAuthor (ACI), targetTimestamp}`,
     * `delete?` `{targetTimestamp}`, `adminDelete?` `{targetAuthor, targetTimestamp}`,
     * `quote?` `{id, author (ACI), text?, attachments? [{contentType?, fileName?}]}`,
     * `groupCallUpdate?` `{eraId?}` (a group-call update; with `sync` it is the phone's own),
     * `unsupported?` (content Kourier does not show: `contact`, `payment`, `giftBadge`, `poll`,
     * `pollEnd`, `storyReply`, `groupCall`, `pin`), `edit?` `{targetTimestamp}`;
     * `sync?` `{destination? (ACI of the 1:1 chat; none in a group), unidentified?,
     * expirationStartTimestamp?, isRecipientUpdate?}`: sends it as the phone's sync transcript
     * (SyncMessage.Sent with `message` or `editMessage`) to its linked device `to` instead;
     * `awaitDelivery?`, `timeout?`. Result: `{timestamp, serverTimestamp, to}` (`to` a ref, or
     * the refs; `serverTimestamp` is what the envelope was stamped with).
     */
    sendData: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const many = Array.isArray(args.to);
      const targets = await Promise.all(
        (many ? args.to : [args.to]).map((ref, i) => this.resolveDevice(ref, many ? `to[${i}]` : 'to')),
      );
      if (targets.length === 0) {
        throw badRequest('"to" names no device');
      }
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const groupId = optString(args, 'group');
      const group = groupId === undefined ? undefined : this.resolveGroup(groupId);
      const dataMessage = this.#dataMessage(sender, args, timestamp, group);
      const edit = args.edit ?? undefined;
      if (edit !== undefined && (typeof edit !== 'object' || !Number.isSafeInteger(edit.targetTimestamp))) {
        throw badRequest('"edit" must be {targetTimestamp}');
      }
      const editMessage =
        edit === undefined ? null : { targetSentTimestamp: BigInt(edit.targetTimestamp), dataMessage };
      const awaitDelivery = optBool(args, 'awaitDelivery', false);
      const timeout = timeoutArg(args);
      // Like a real server, the envelope is stamped with the time it was taken, whatever the
      // sender claims as its own timestamp.
      const serverTimestamp = optInt(args, 'serverTimestamp', Date.now(), { min: 1 });
      const sync = args.sync ?? undefined;
      if (sync !== undefined) {
        if (typeof sync !== 'object' || many) {
          throw badRequest('"sync" must be an object and "to" one linked device');
        }
        const pair = await this.#syncPair(args);
        const destination = sync.destination == null ? null : aciBytes(parseRef(sync.destination, 'sync.destination').aci);
        const start = sync.expirationStartTimestamp;
        const content = syncContent({
          content: {
            sent: {
              destinationServiceIdBinary: destination,
              timestamp: BigInt(timestamp),
              message: editMessage ? null : dataMessage,
              editMessage,
              unidentifiedStatus:
                destination === null
                  ? null
                  : [
                      {
                        destinationServiceIdBinary: destination,
                        unidentified: sync.unidentified === true,
                        destinationServiceId: null,
                        destinationPniIdentityKey: null,
                      },
                    ],
              destinationE164: null,
              isRecipientUpdate: sync.isRecipientUpdate === true ? true : null,
              expirationStartTimestamp: Number.isSafeInteger(start) ? BigInt(start) : null,
              storyMessage: null,
              storyMessageRecipients: null,
              destinationServiceId: null,
            },
          },
        });
        return this.#sendContent(pair.sender, pair.target, content, {
          timestamp,
          sealed: false,
          awaitDelivery,
          timeout,
          serverTimestamp,
        });
      }
      const content = {
        content: editMessage ? { editMessage } : { dataMessage },
        pniSignatureMessage: null,
        senderKeyDistributionMessage: null,
      };
      const senderKey = optBool(args, 'senderKey', false);
      if (senderKey && !group) {
        throw badRequest('"senderKey" needs a "group"');
      }
      const sealed = optBool(args, 'sealed', false);
      for (const target of targets) {
        if (target === sender.device) {
          throw badRequest('a primary cannot send to itself');
        }
        await this.ensureSession(sender, target);
        const options = senderKey
          ? {
              timestamp,
              sealed: true,
              distributionId: await this.#distributionFor(sender, target, group, timestamp),
              group,
            }
          : { timestamp, sealed, group };
        const envelope = restamp(await sender.primary.encryptContent(target, content, options), serverTimestamp);
        const delivery = this.deliver(sender, target, envelope);
        if (awaitDelivery) {
          await withTimeout(delivery, timeout, 'sendData: waiting for delivery');
        } else {
          this.#reportFailure(delivery, target, timestamp);
        }
      }
      return { timestamp, serverTimestamp, to: many ? targets.map(deviceRef) : deviceRef(targets[0]) };
    },

    /**
     * Sends call signalling (a `CallMessage`) from a primary to a device, as the caller's or
     * the ringer's phone does. Args: `from` (primary), `to` (device), `timestamp?` (now),
     * `sealed?` (false), and any of `offer` `{id, video?}`, `hangup` `{id, type? (`normal`,
     * `accepted`, `declined`, `busy`, `needPermission`, or the wire number), deviceId?}`,
     * `answer` `{id}`, `busy` `{id}`, `iceUpdate` `{id}`, `ring` `{group (a group id) or
     * groupId (base64 bytes of any group id), ringId, cancelled?}` (RingRTC's
     * `ring_intention` in `opaque.data`, with a `group_call_message` beside it as real
     * clients send), `opaque` (base64: the raw `opaque.data`, in place of `ring`),
     * `destinationDeviceId?`; ids are numbers or decimal strings (a call id is a uint64, a
     * ring id an int64). `ageMs?` (0): how long the envelope waited on the server: its
     * `serverTimestamp` is the server's clock now minus that (`serverTimestamp?` states it
     * outright); `awaitDelivery?`, `timeout?`. Result: `{timestamp, serverTimestamp, to}`
     * (`timestamp` is the sender's own, `serverTimestamp` the envelope's: the two are read
     * from the clock one after the other and can differ by a millisecond).
     */
    sendCall: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const callId = (value, name) => {
        try {
          return BigInt.asUintN(64, BigInt(value));
        } catch {
          throw badRequest(`"${name}" must be a number or a decimal string`);
        }
      };
      const part = (name) => {
        const value = args[name];
        if (value == null) {
          return undefined;
        }
        if (typeof value !== 'object') {
          throw badRequest(`"${name}" must be an object`);
        }
        return value;
      };
      const offer = part('offer');
      const hangup = part('hangup');
      const answer = part('answer');
      const busy = part('busy');
      const iceUpdate = part('iceUpdate');
      const ring = part('ring');
      let hangupType = null;
      if (hangup && hangup.type != null) {
        hangupType = Number.isSafeInteger(hangup.type) ? hangup.type : HANGUP_TYPES[hangup.type];
        if (hangupType === undefined) {
          throw badRequest(`"hangup.type" must be one of ${Object.keys(HANGUP_TYPES).join(', ')} or a number`);
        }
      }
      let opaqueData = null;
      if (ring) {
        const groupId =
          typeof ring.groupId === 'string'
            ? Buffer.from(ring.groupId, 'base64')
            : Buffer.from(this.resolveGroup(reqString(ring, 'group')).publicParams.getGroupIdentifier().serialize());
        let ringId;
        try {
          ringId = BigInt.asIntN(64, BigInt(ring.ringId));
        } catch {
          throw badRequest('"ring.ringId" must be a number or a decimal string');
        }
        opaqueData = Buffer.from(
          mock.SignalingProto.CallMessage.encode({
            ringIntention: {
              type: ring.cancelled === true ? 1 : 0,
              groupId,
              ringId,
            },
            groupCallMessage: { groupId },
          }),
        );
      } else if (typeof args.opaque === 'string') {
        opaqueData = Buffer.from(args.opaque, 'base64');
      }
      if (!offer && !hangup && !answer && !busy && !iceUpdate && opaqueData === null) {
        throw badRequest('one of "offer", "hangup", "answer", "busy", "iceUpdate", "ring" or "opaque" is required');
      }
      const content = {
        content: {
          callMessage: {
            offer: offer
              ? { id: callId(offer.id, 'offer.id'), type: offer.video === true ? 1 : 0, opaque: null }
              : null,
            answer: answer ? { id: callId(answer.id, 'answer.id'), opaque: null } : null,
            iceUpdate: iceUpdate ? [{ id: callId(iceUpdate.id, 'iceUpdate.id'), opaque: null }] : null,
            busy: busy ? { id: callId(busy.id, 'busy.id') } : null,
            hangup: hangup
              ? {
                  id: callId(hangup.id, 'hangup.id'),
                  type: hangupType,
                  deviceId: optInt(hangup, 'deviceId', undefined, { max: 2 ** 32 - 1 }) ?? null,
                }
              : null,
            destinationDeviceId: optInt(args, 'destinationDeviceId', undefined, { max: 2 ** 32 - 1 }) ?? null,
            opaque: opaqueData === null ? null : { urgency: null, data: opaqueData },
          },
        },
        pniSignatureMessage: null,
        senderKeyDistributionMessage: null,
      };
      // The envelope is stamped on the server's clock, the one its delivery is stamped on.
      const serverNow = Date.now() + deliveryClock.skewMs;
      const serverTimestamp = optInt(
        args,
        'serverTimestamp',
        serverNow - optInt(args, 'ageMs', 0, { min: 0 }),
        { min: 1 },
      );
      return this.#sendContent(sender, target, content, {
        timestamp,
        sealed: optBool(args, 'sealed', false),
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
        serverTimestamp,
      });
    },

    /**
     * Sends a sync message other than a transcript from a primary to one of its linked devices.
     * Args: `from`, `to`, `timestamp?` (now), and one of
     * `deleteForMe` `{messages? [{conversation, author (ACI), timestamp}], conversations?
     * [{conversation, isFullDelete? (true), mostRecent? [{author, timestamp}],
     * mostRecentNonExpiring? [{author, timestamp}]}], localOnly? [{conversation}], attachments?
     * [{conversation, author, timestamp, clientUuid?, digest?, plaintextHash?}]}` where
     * `conversation` is `{aci}`, `{group}` (a group id) or `{e164}` and the three names of an
     * attachment are base64 or byte arrays, as a pointer from `waitForMessage` has them;
     * `viewOnceOpen` `{sender (ACI), timestamp}`; `viewed` or `read`
     * `[{sender, timestamp}]`; `callEvent` `{conversation ({aci}, {group} or {raw}: base64
     * bytes), callId (a number, or a decimal string for ids above 2^53), timestamp?, type
     * (`audio`, `video`, `group`, `adHoc`, or the wire number), direction (`incoming`,
     * `outgoing`), event (`accepted`, `notAccepted`, `delete`, `observed`)}`: what the phone
     * syncs about a call; `attachmentBackfillResponse` `{conversation, author (ACI), timestamp
     * (the message's sent time), error? ("messageNotFound" or a number) | attachments? [Entry],
     * longText? Entry | empty? (true: neither)}`, the phone's answer to a request for lost
     * files, where an Entry is `{status: "pending" | "terminalError" | <number>}`, `{data
     * (base64), ...}` (uploaded now, the spec of `#pointerOf`: `clientUuid?`, `forge?`, ...),
     * `{pointer}` (a JSON pointer sent as it is) or `{empty: true}` (an AttachmentData with
     * neither field); `awaitDelivery?`, `timeout?`. Result: `{timestamp, to}`, and for a
     * backfill answer `pointers` too (the pointers it put in, in entry order, long text last).
     */
    sendSync: async (args) => {
      const { sender, target } = await this.#syncPair(args);
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const stamp = (value, name) => {
        if (!Number.isSafeInteger(value) || value < 0) {
          throw badRequest(`"${name}" must be a timestamp`);
        }
        return BigInt(value);
      };
      const thread = (conversation, name) => {
        if (typeof conversation !== 'object' || conversation === null) {
          throw badRequest(`"${name}" must be {aci}, {group} or {e164}`);
        }
        if (typeof conversation.aci === 'string') {
          return { identifier: { threadServiceIdBinary: aciBytes(parseRef(conversation.aci, `${name}.aci`).aci) } };
        }
        if (typeof conversation.group === 'string') {
          const group = this.resolveGroup(conversation.group);
          return {
            identifier: { threadGroupId: Buffer.from(group.publicParams.getGroupIdentifier().serialize()) },
          };
        }
        if (typeof conversation.e164 === 'string') {
          return { identifier: { threadE164: conversation.e164 } };
        }
        throw badRequest(`"${name}" must be {aci}, {group} or {e164}`);
      };
      const addressed = (item, name) => ({
        author: { authorServiceIdBinary: aciBytes(parseRef(item.author, `${name}.author`).aci) },
        sentTimestamp: stamp(item.timestamp, `${name}.timestamp`),
      });
      const seen = (list, name) =>
        optArray({ [name]: list }, name).map((item, i) => ({
          senderAci: null,
          senderAciBinary: aciBytes(parseRef(item.sender, `${name}[${i}].sender`).aci),
          timestamp: stamp(item.timestamp, `${name}[${i}].timestamp`),
        }));
      let fields;
      if (args.deleteForMe != null) {
        const spec = args.deleteForMe;
        // One MessageDeletes per conversation, like Desktop's `getDeleteForMeSyncMessage`.
        const byConversation = new Map();
        for (const [i, item] of optArray(spec, 'messages').entries()) {
          const key = JSON.stringify(item.conversation ?? null);
          if (!byConversation.has(key)) {
            byConversation.set(key, {
              conversation: thread(item.conversation, `deleteForMe.messages[${i}].conversation`),
              messages: [],
            });
          }
          byConversation.get(key).messages.push(addressed(item, `deleteForMe.messages[${i}]`));
        }
        fields = {
          content: {
            deleteForMe: {
              messageDeletes: [...byConversation.values()],
              conversationDeletes: optArray(spec, 'conversations').map((item, i) => ({
                conversation: thread(item.conversation, `deleteForMe.conversations[${i}].conversation`),
                mostRecentMessages: optArray(item, 'mostRecent').map((m, j) =>
                  addressed(m, `deleteForMe.conversations[${i}].mostRecent[${j}]`),
                ),
                isFullDelete: item.isFullDelete !== false,
                mostRecentNonExpiringMessages: optArray(item, 'mostRecentNonExpiring').map((m, j) =>
                  addressed(m, `deleteForMe.conversations[${i}].mostRecentNonExpiring[${j}]`),
                ),
              })),
              localOnlyConversationDeletes: optArray(spec, 'localOnly').map((item, i) => ({
                conversation: thread(item.conversation, `deleteForMe.localOnly[${i}].conversation`),
              })),
              attachmentDeletes: optArray(spec, 'attachments').map((item, i) => ({
                conversation: thread(item.conversation, `deleteForMe.attachments[${i}].conversation`),
                targetMessage: addressed(item, `deleteForMe.attachments[${i}]`),
                clientUuid: item.clientUuid == null ? null : pointerBytes(item.clientUuid, 'clientUuid'),
                fallbackDigest: item.digest == null ? null : pointerBytes(item.digest, 'digest'),
                fallbackPlaintextHash:
                  item.plaintextHash == null ? null : pointerBytes(item.plaintextHash, 'plaintextHash'),
              })),
            },
          },
        };
      } else if (args.viewOnceOpen != null) {
        const open = args.viewOnceOpen;
        fields = {
          content: {
            viewOnceOpen: {
              senderAci: null,
              senderAciBinary: aciBytes(parseRef(open.sender, 'viewOnceOpen.sender').aci),
              timestamp: stamp(open.timestamp, 'viewOnceOpen.timestamp'),
            },
          },
        };
      } else if (args.viewed != null) {
        fields = { viewed: seen(args.viewed, 'viewed') };
      } else if (args.read != null) {
        fields = { read: seen(args.read, 'read') };
      } else if (args.callEvent != null) {
        const spec = args.callEvent;
        const pick = (table, value, name) => {
          if (value == null) {
            return null;
          }
          if (Number.isSafeInteger(value)) {
            return value;
          }
          if (!(value in table)) {
            throw badRequest(`"callEvent.${name}" must be one of ${Object.keys(table).join(', ')} or a number`);
          }
          return table[value];
        };
        let conversationId = null;
        if (typeof spec.conversation?.aci === 'string') {
          conversationId = aciBytes(parseRef(spec.conversation.aci, 'callEvent.conversation.aci').aci);
        } else if (typeof spec.conversation?.group === 'string') {
          conversationId = Buffer.from(
            this.resolveGroup(spec.conversation.group).publicParams.getGroupIdentifier().serialize(),
          );
        } else if (typeof spec.conversation?.raw === 'string') {
          conversationId = Buffer.from(spec.conversation.raw, 'base64');
        }
        fields = {
          content: {
            callEvent: {
              conversationId,
              callId: spec.callId == null ? null : BigInt.asUintN(64, BigInt(spec.callId)),
              timestamp: spec.timestamp == null ? null : stamp(spec.timestamp, 'callEvent.timestamp'),
              type: pick(CALL_EVENT_TYPES, spec.type, 'type'),
              direction: pick(CALL_EVENT_DIRECTIONS, spec.direction, 'direction'),
              event: pick(CALL_EVENT_EVENTS, spec.event, 'event'),
            },
          },
        };
      } else if (args.attachmentBackfillResponse != null) {
        const spec = args.attachmentBackfillResponse;
        const name = 'attachmentBackfillResponse';
        const pick = (table, value, what) => {
          if (Number.isSafeInteger(value)) {
            return value;
          }
          if (!(value in table)) {
            throw badRequest(`"${what}" must be one of ${Object.keys(table).join(', ')} or a number`);
          }
          return table[value];
        };
        const entry = async (item, what) => {
          if (typeof item !== 'object' || item === null) {
            throw badRequest(`"${what}" must be an object`);
          }
          if (item.empty === true) {
            return { data: null };
          }
          if (item.status != null) {
            return { data: { status: pick(BACKFILL_STATUS, item.status, `${what}.status`) } };
          }
          if (item.pointer != null) {
            const pointer = pointerFromJson(item.pointer, `${what}.pointer`);
            uploaded.push(pointer);
            return { data: { attachment: pointer } };
          }
          if (typeof item.data === 'string') {
            const pointer = await this.#pointerOf(Buffer.from(item.data, 'base64'), item);
            uploaded.push(pointer);
            return { data: { attachment: pointer } };
          }
          throw badRequest(`"${what}" needs one of status, pointer, data or empty`);
        };
        if (typeof spec.author !== 'string') {
          throw badRequest(`"${name}.author" must be an ACI`);
        }
        const hasList = spec.attachments != null || spec.longText != null;
        if (spec.error != null && hasList) {
          throw badRequest(`"${name}" takes "error" or "attachments"/"longText", not both`);
        }
        const uploaded = [];
        let data = null;
        if (spec.error != null) {
          data = { error: pick(BACKFILL_ERROR, spec.error, `${name}.error`) };
        } else if (hasList) {
          const attachments = [];
          for (const [i, item] of optArray(spec, 'attachments').entries()) {
            attachments.push(await entry(item, `${name}.attachments[${i}]`));
          }
          const longText = spec.longText == null ? null : await entry(spec.longText, `${name}.longText`);
          data = { attachments: { attachments, longText } };
        } else if (spec.empty !== true) {
          throw badRequest(`"${name}" needs "error", "attachments", "longText" or "empty"`);
        }
        fields = {
          content: {
            attachmentBackfillResponse: {
              targetMessage: addressed(spec, name),
              targetConversation: thread(spec.conversation, `${name}.conversation`),
              data,
            },
          },
        };
        const sent = await this.#sendContent(sender, target, syncContent(fields), {
          timestamp,
          sealed: false,
          awaitDelivery: optBool(args, 'awaitDelivery', false),
          timeout: timeoutArg(args),
        });
        return { ...sent, pointers: jsonSafe(uploaded) };
      } else {
        throw badRequest(
          'one of "deleteForMe", "viewOnceOpen", "viewed", "read", "callEvent" or "attachmentBackfillResponse" is required',
        );
      }
      return this.#sendContent(sender, target, syncContent(fields), {
        timestamp,
        sealed: false,
        awaitDelivery: optBool(args, 'awaitDelivery', false),
        timeout: timeoutArg(args),
      });
    },

    /**
     * Waits for the next EditMessage a primary decrypted, optionally only one `from` a device
     * or in a `group`. Args: `primary`, `from?`, `group?`, `timeout?`. Result: `{source,
     * targetTimestamp, timestamp, text, sealed, messageType, groupId, sealedGroupId,
     * dataMessage, remaining}` (`timestamp` is the new revision's own).
     */
    waitForEdit: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const groupFilter = optString(args, 'group');
      const groupOf = (dataMessage) => {
        const masterKey = dataMessage?.groupV2?.masterKey;
        return masterKey
          ? [...this.#groups.values()].find((g) => g.masterKey.equals(Buffer.from(masterKey)))
          : undefined;
      };
      const entry = await state.inbox.editMessage.take(
        (item) =>
          (from === undefined || refMatches(from, item.source)) &&
          (groupFilter === undefined || groupOf(item.editMessage?.dataMessage)?.id === groupFilter),
        timeoutArg(args),
        'waitForEdit',
      );
      const dataMessage = entry.editMessage?.dataMessage ?? {};
      return {
        source: deviceRef(entry.source),
        targetTimestamp: toNumber(entry.editMessage?.targetSentTimestamp),
        timestamp: toNumber(dataMessage.timestamp),
        text: dataMessage.body ?? '',
        sealed: entry.content?.[SEALED_SENDER] === true,
        messageType: entry.envelopeType,
        groupId: groupOf(dataMessage)?.id ?? null,
        sealedGroupId: entry.content?.[SEALED_GROUP] ?? null,
        dataMessage: jsonSafe(dataMessage) ?? {},
        remaining: state.inbox.editMessage.size,
      };
    },

    /**
     * Sends `from` a message `to` a device that the device cannot decrypt (§6.6 retry): a sealed
     * sender-key message whose distribution message is never sent, so `group_decrypt` on the
     * recipient has no state. The content hint is chosen (`hint`: `default`, `resendable`,
     * `implicit`); `groupId` is omitted, so the recipient treats the failure as 1:1. Args:
     * `from`, `to` (a device ref), `text?`, `timestamp?` (now), `hint?` (`resendable`). Result:
     * `{timestamp, to}`.
     */
    sendUndecryptable: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      const timestamp = optInt(args, 'timestamp', Date.now(), { min: 1 });
      const text = optString(args, 'text') ?? 'undecryptable';
      const hintName = optString(args, 'hint') ?? 'resendable';
      if (!Object.hasOwn(CONTENT_HINTS, hintName)) {
        throw badRequest('"hint" must be default, resendable or implicit');
      }
      // Sealing needs the recipient's identity, which the sender learns from a session.
      await this.ensureSession(sender, target);
      const senderKeys = sender.primary.senderKeys.get(ServiceIdKind.ACI);
      const identity = sender.primary.identity.get(ServiceIdKind.ACI);
      // Seed the sender's own chain for a fresh distribution id; the distribution message that
      // would let the recipient build the matching state is deliberately never sent.
      const distributionId = nodeCrypto.randomUUID();
      await SignalClient.SenderKeyDistributionMessage.create(target.address, distributionId, senderKeys);
      const padded = Buffer.concat([Buffer.from(text, 'utf8'), Buffer.from([0x80])]);
      const ciphertext = await SignalClient.groupEncrypt(target.address, distributionId, senderKeys, padded);
      const usmc = SignalClient.UnidentifiedSenderMessageContent.new(
        ciphertext,
        sender.primary.senderCertificate,
        CONTENT_HINTS[hintName],
        null,
      );
      const content = await SignalClient.sealedSenderEncrypt(usmc, target.address, identity);
      const envelope = Buffer.from(
        Proto.Envelope.encode({
          type: Proto.Envelope.Type.UNIDENTIFIED_SENDER,
          sourceServiceIdBinary: null,
          sourceDeviceId: null,
          destinationServiceIdBinary: target.getServiceIdBinaryByKind(ServiceIdKind.ACI),
          serverTimestamp: BigInt(timestamp),
          clientTimestamp: BigInt(timestamp),
          content,
        }),
      );
      this.#reportFailure(this.deliver(sender, target, envelope), target, timestamp);
      return { timestamp, to: deviceRef(target) };
    },

    /**
     * Sends `from` a retry request (a `DecryptionErrorMessage` in a `PlaintextContent`) `to` a
     * device, about a message the recipient sent at `timestamp` from device `senderDevice` (the
     * recipient's device id). The request names no ratchet key (its original is a sender-key
     * message), so the recipient does not archive its session but still resends from the Message
     * Send Log. Args: `from`, `to`, `timestamp`, `senderDevice`. Result: `{timestamp, to}`.
     */
    sendRetryRequest: async (args) => {
      const sender = this.resolvePrimary(args.from, 'from');
      const target = await this.resolveDevice(args.to, 'to');
      const timestamp = optInt(args, 'timestamp', undefined, { min: 1 });
      if (timestamp === undefined) {
        throw badRequest('"timestamp" is required');
      }
      const senderDevice = optInt(args, 'senderDevice', undefined, { min: 1, max: 2 ** 31 - 1 });
      if (senderDevice === undefined) {
        throw badRequest('"senderDevice" is required (the recipient device the message came from)');
      }
      const dem = SignalClient.DecryptionErrorMessage.forOriginal(
        Buffer.alloc(0),
        SignalClient.CiphertextMessageType.SenderKey,
        timestamp,
        senderDevice,
      );
      // An ordinary encrypted Content carrying the decryption-error message; the recipient maps
      // it to a resend (`on_retry_request`). Not a sealed PLAINTEXT_CONTENT, which Kourier only
      // accepts when it carries the message alone.
      const content = {
        content: { decryptionErrorMessage: dem.serialize() },
        pniSignatureMessage: null,
        senderKeyDistributionMessage: null,
      };
      const now = Date.now();
      await this.ensureSession(sender, target);
      const envelope = await sender.primary.encryptContent(target, content, { timestamp: now });
      this.#reportFailure(this.deliver(sender, target, envelope), target, now);
      return { timestamp, to: deviceRef(target) };
    },

    /**
     * Makes every Storage Service request for `group` fail with 500 (`fail?` true) or work
     * again (`fail: false`): a client's group fetch fails while everything else works.
     * `modifyGroup` still changes the group (the mock's own primary paths fail). Args: `group`,
     * `fail?`. Result: `{}`.
     */
    failGroupFetch: async (args) => {
      const group = this.resolveGroup(reqString(args, 'group'));
      const key = Buffer.from(group.publicParams.serialize()).toString('base64');
      if (optBool(args, 'fail', true)) {
        this.server.failingGroups.add(key);
      } else {
        this.server.failingGroups.delete(key);
      }
      return {};
    },

    /**
     * Makes every linked device's Storage Service manifest request fail with 500 (`fail?`
     * true) or work again (`fail: false`): the device's Storage Service sync fails and retries
     * while everything else works. Result: `{refused}` (requests refused so far).
     */
    failStorageManifest: async (args) => {
      this.server.failingManifest = optBool(args, 'fail', true);
      return { refused: this.server.refusedManifests };
    },

    /**
     * Rate-limits every profile fetch: `GET /v1/profile/...` answers 429 with `Retry-After:
     * <retryAfter?>` (seconds, default 60) until `stop: true`. Result: `{refused}` (profile
     * requests refused so far).
     */
    limitProfileFetches: async (args) => {
      profileLimit.retryAfter = optBool(args, 'stop', false)
        ? null
        : optInt(args, 'retryAfter', 60, { min: 0 });
      return { refused: profileLimit.refused };
    },

    /** Sets one remote-config entry served by `/v2/config`. Args: `key`, `enabled?`, `value?`. */
    setRemoteConfig: async (args) => {
      const key = reqString(args, 'key');
      const enabled = optBool(args, 'enabled', true);
      const value = optString(args, 'value');
      this.server.setRemoteConfig(key, value === undefined ? { enabled } : { enabled, value });
      return {};
    },

    /**
     * Rate-limits client 1:1 sends to `target` (a ref): `PUT /v1/messages/<target>` answers 429
     * with `Retry-After: <retryAfter?>` (seconds, default 1) for the next `count?` sends (default
     * until `stop: true`). Multi-recipient sends are not limited per recipient (the real server
     * does not either). Result: `{}`.
     */
    rateLimit: async (args) => {
      const { aci } = parseRef(reqString(args, 'target'), 'target');
      if (optBool(args, 'stop', false)) {
        sends.rateLimited.delete(aci);
        return {};
      }
      const count = optInt(args, 'count', -1, { min: -1 });
      const retryAfter = optInt(args, 'retryAfter', 1, { min: 0 });
      sends.rateLimited.set(aci, { left: count, retryAfter });
      return {};
    },

    /**
     * Makes every multi-recipient send fail as with an invalid group send token (`reject`,
     * default true), as the server answers an expired or forged token; `reject: false` checks
     * the tokens again (they are always verified). Result: `{}`.
     */
    rejectGroupSendTokens: async (args) => {
      sends.rejectTokens = optBool(args, 'reject', true);
      return {};
    },

    /**
     * Answers every multi-recipient send (`on`, default true) with a gRPC response that has no
     * `response` case, which the client's typed API reports as unexpected; `on: false` serves
     * them again. Result: `{}`.
     */
    answerMultiRecipientUnexpectedly: async (args) => {
      sends.unexpected = optBool(args, 'on', true);
      return {};
    },

    /**
     * Breaks attachment downloads from CDN 3: a `GET /cdn3/attachments/<key>` answer is
     * reset after a number of body bytes, as a connection that dropped (0: before the first
     * byte). `afters?` lists that number for each of the next requests in turn; or `after`
     * (0) for each of the next `times?` (1; -1: until stopped). An answer no longer than its
     * cut is left alone. With `ignoreRange?` the server also answers range requests with the
     * whole blob (200), as a CDN that knows no ranges. `stop: true` ends it. Result: `{}`.
     */
    breakDownloads: async (args) => {
      if (optBool(args, 'stop', false)) {
        this.server.downloadFault = null;
        return {};
      }
      const afters = optArray(args, 'afters');
      if (afters.some((n) => !Number.isSafeInteger(n) || n < 0)) {
        throw badRequest('"afters" must be non-negative integers');
      }
      this.server.downloadFault = {
        afters: [...afters],
        after: optInt(args, 'after', 0),
        left: afters.length > 0 ? 0 : optInt(args, 'times', 1, { min: -1 }),
        ignoreRange: optBool(args, 'ignoreRange', false),
      };
      return {};
    },

    /**
     * The attachment downloads asked of CDN 3 so far, in order: `{downloads: [{key, range
     * (the request's Range header or null), status, start (first byte served), sent (body
     * bytes written), broken}]}`. With `reset?` the list starts anew.
     */
    downloadStats: async (args) => {
      const downloads = this.server.downloads.map((entry) => ({ ...entry }));
      if (optBool(args, 'reset', false)) {
        this.server.downloads = [];
      }
      return { downloads };
    },

    /**
     * What clients sent: `{single: {<service id>: n}, multi, multiRecipients, refusedTokens,
     * unexpectedAnswers, groupFetches: {<group id>: n}, uploads, sendRequests}` (`groupFetches`
     * counts every Storage Service group request, `uploads` the attachment uploads clients
     * started on CDN 3). `sendRequests` is every request a client made to send a message to
     * anybody, answered with success or not: the 1:1 route (`PUT /v1/messages/<service id>`:
     * messages, receipts, typing, call signalling, sync messages to its own account) plus
     * every multi-recipient send on either route. A test that says "the client sent nothing"
     * compares it before and after.
     */
    sendStats: async () => {
      const groupFetches = {};
      for (const group of this.#groups.values()) {
        const key = Buffer.from(group.publicParams.serialize()).toString('base64');
        groupFetches[group.id] = this.server.groupFetches.get(key) ?? 0;
      }
      return {
        single: Object.fromEntries(sends.single),
        urgency: Object.fromEntries(sends.urgency),
        multi: sends.multi,
        multiRecipients: [...sends.multiRecipients],
        refusedTokens: sends.refusedTokens,
        unexpectedAnswers: sends.unexpectedAnswers,
        groupFetches,
        uploads: this.server.cdn3Uploads,
        sendRequests: [...sends.single.values()].reduce((sum, n) => sum + n, 0) + sends.multiRequests,
        // `PUT /v1/devices/link` requests served: one per device a client linked (or tried).
        linkRequests: sends.linkRequests.length,
        certificateRequests: [...sends.certificateRequests],
      };
    },

    /**
     * Forgets the sender keys `primary` holds from `from` (a ref; its devices), as a lost
     * database would, so the next sender-key message it gets from them fails to decrypt and it
     * asks for a resend. Its own chains stay. Result: `{forgotten}`.
     */
    breakSenderKeys: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const { aci } = parseRef(reqString(args, 'from'), 'from');
      const store = state.primary.senderKeys.get(ServiceIdKind.ACI);
      let forgotten = 0;
      // The mock's store keys are `<sender address>.<distribution id>` (`aci.device.uuid`).
      for (const key of [...store.keys.keys()]) {
        if (key.startsWith(`${aci}.`)) {
          store.keys.delete(key);
          forgotten += 1;
        }
      }
      return { forgotten };
    },

    /**
     * Waits until `primary` could not decrypt an envelope (from `from?`). Result: `{source,
     * timestamp, envelopeType}` (the inner type).
     */
    waitForDecryptFailure: async (args) => {
      const state = this.resolvePrimary(args.primary, 'primary');
      const from = args.from === undefined || args.from === null ? undefined : parseRef(args.from, 'from');
      const entry = await state.inbox.decryptFailure.take(
        (item) => from === undefined || refMatches(from, item.source),
        timeoutArg(args),
        'waitForDecryptFailure',
      );
      return { source: deviceRef(entry.source), timestamp: entry.timestamp, envelopeType: entry.envelopeType };
    },

    /** Answers, then stops the server and exits with status 0. */
    shutdown: async () => {
      setImmediate(() => {
        this.shutdown(0).catch(() => process.exit(0));
      });
      return {};
    },
  };

  async dispatch(line) {
    if (line.trim().length === 0) {
      return;
    }
    let request;
    try {
      request = JSON.parse(line);
    } catch (error) {
      emit({ type: 'response', id: null, ok: false, error: { code: 'badRequest', message: `invalid JSON: ${error.message}` } });
      return;
    }
    const id = request?.id ?? null;
    try {
      if (typeof request !== 'object' || request === null || Array.isArray(request)) {
        throw badRequest('a request must be an object {id, cmd, args}');
      }
      const { cmd } = request;
      const args = request.args ?? {};
      if (typeof args !== 'object' || Array.isArray(args)) {
        throw badRequest('"args" must be an object');
      }
      const handler = typeof cmd === 'string' && Object.hasOwn(this.commands, cmd) ? this.commands[cmd] : undefined;
      if (!handler) {
        throw new HarnessError('unknownCommand', `unknown command ${JSON.stringify(cmd)}`);
      }
      const result = await handler(args);
      emit({ type: 'response', id, ok: true, result: result ?? {} });
    } catch (error) {
      if (!(error instanceof HarnessError)) {
        log(`command ${request?.cmd} failed`, error);
      }
      emit({ type: 'response', id, ok: false, error: errorToWire(error) });
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------

/** The installed version of a package, resolved the way `from` resolves it. */
function packageVersion(name, from = require) {
  try {
    return from(`${name}/package.json`).version ?? null;
  } catch {
    return null;
  }
}

function pemToDer(pem) {
  const body = pem
    .replace(/-----BEGIN CERTIFICATE-----/, '')
    .replace(/-----END CERTIFICATE-----[\s\S]*/, '')
    .replace(/\s+/g, '');
  return Buffer.from(body, 'base64');
}

async function main() {
  const { values } = parseArgs({
    options: {
      host: { type: 'string', default: '127.0.0.1' },
      'state-dir': { type: 'string' },
      'api-timeout': { type: 'string' },
    },
    strict: true,
  });

  const ownsStateDir = values['state-dir'] === undefined;
  const stateDir = ownsStateDir
    ? await fs.mkdtemp(path.join(os.tmpdir(), 'kourier-mock-'))
    : path.resolve(values['state-dir']);
  const cdn3Path = path.join(stateDir, 'cdn3');
  await fs.mkdir(cdn3Path, { recursive: true });

  const config = { cdn3Path };
  if (values['api-timeout'] !== undefined) {
    const apiTimeout = Number(values['api-timeout']);
    if (!Number.isSafeInteger(apiTimeout) || apiTimeout <= 0) {
      throw new Error('--api-timeout must be a positive integer (ms)');
    }
    config.timeout = apiTimeout;
  }

  const server = new HarnessServer(config);
  activeServer = server;
  await server.listen(0, values.host);
  server.installUpgradeGate();
  server.installCdnGate(cdn3Path);
  // Counts the uploads clients start (the mock's TUS server has no hook for it); the compat
  // request listener the mock installed keeps handling the stream.
  server.https.on('stream', (_stream, headers) => {
    if (headers[':method'] === 'POST' && String(headers[':path'] ?? '').startsWith('/cdn3/attachments/')) {
      server.cdn3Uploads += 1;
    }
  });
  const { port } = server.address();
  for (const [key, value] of DEFAULT_REMOTE_CONFIG) {
    server.setRemoteConfig(key, value);
  }

  const certs = await loadCertificates();
  const url = `https://${TLS_HOST}:${port}`;
  const handshake = {
    type: 'ready',
    protocol: PROTOCOL_VERSION,
    pid: process.pid,
    host: TLS_HOST,
    ip: values.host,
    port,
    httpVersion: 2,
    caPem: certs.certificateAuthority,
    caDer: pemToDer(certs.certificateAuthority).toString('base64'),
    trustRoot: certs.serverTrustRoots[0],
    zkPublicParams: certs.serverPublicParams,
    genericServerPublicParams: certs.genericServerPublicParams,
    backupServerPublicParams: certs.backupServerPublicParams,
    urls: {
      chat: url,
      storage: `${url}/storageService`,
      cdn: { 0: url, 2: url, 3: `${url}/cdn3` },
      resources: `${url}/updates2`,
      sfu: `${url}/callingService`,
    },
    paths: {
      chatWebsocket: '/v1/websocket/',
      provisioningWebsocket: '/v1/websocket/provisioning/',
    },
    remoteConfig: Object.fromEntries(DEFAULT_REMOTE_CONFIG),
    stateDir,
    versions: {
      mockServer: packageVersion('@signalapp/mock-server'),
      libsignalClient: packageVersion(
        '@signalapp/libsignal-client',
        createRequire(require.resolve('@signalapp/mock-server')),
      ),
      node: process.version,
    },
  };

  const harness = new Harness({ server, handshake, stateDir, ownsStateDir });

  process.on('unhandledRejection', (reason) => {
    // The mock fires some sends and pumps without awaiting them; report, keep serving.
    log('unhandled rejection', reason);
    emitEvent('unhandledRejection', errorToWire(reason));
  });
  let fatal = false;
  process.on('uncaughtException', (error) => {
    if (fatal) {
      process.exit(70);
    }
    fatal = true;
    log('uncaught exception', error);
    emit({ type: 'fatal', error: errorToWire(error) });
    harness.shutdown(70).catch(() => process.exit(70));
  });
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => {
      // A second signal (or one during shutdown) exits at once.
      if (harness.isShuttingDown) {
        process.exit(0);
      }
      harness.shutdown(0).catch(() => process.exit(0));
    });
  }

  emit(handshake);
  log(`mock server listening on ${values.host}:${port}`);

  const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  input.on('line', (line) => {
    harness.dispatch(line).catch((error) => log('dispatch failed', error));
  });
  input.on('close', () => {
    // The driving process went away (or closed stdin): stop.
    harness.shutdown(0).catch(() => process.exit(0));
  });
}

main().catch((error) => {
  log('startup failed', error);
  emit({ type: 'fatal', error: errorToWire(error) });
  process.exit(1);
});
