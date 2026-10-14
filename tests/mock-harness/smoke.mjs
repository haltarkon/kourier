// SPDX-License-Identifier: AGPL-3.0-only
//
// Smoke tests for harness.mjs: `node --test smoke.mjs` (Node >= 24).

import assert from 'node:assert/strict';
import { X509Certificate } from 'node:crypto';
import fs from 'node:fs';
import http2 from 'node:http2';
import { after, before, describe, test } from 'node:test';

import { startHarness } from './client.mjs';
import { openProvisioningSocket } from './provisioning-socket.mjs';

const BASE64 = /^[A-Za-z0-9+/]+={0,2}$/;

function decodeBase64(value, name) {
  assert.equal(typeof value, 'string', `${name} is a string`);
  assert.match(value, BASE64, `${name} is base64`);
  return Buffer.from(value, 'base64');
}

/** One GET over HTTP/2 that trusts only the harness CA; resolves with the status code. */
function h2Get(url, ca, requestPath) {
  return new Promise((resolve, reject) => {
    const session = http2.connect(url, { ca });
    session.on('error', reject);
    const stream = session.request({ ':method': 'GET', ':path': requestPath });
    stream.on('response', (headers) => {
      resolve(headers[':status']);
      stream.close();
      session.close();
    });
    stream.on('error', reject);
    stream.end();
  });
}

/**
 * One GET over HTTP/2 that reads the body: resolves with `{status, headers, body, broke}`
 * (`broke`: the stream was reset before the body ended).
 */
function h2Fetch(url, ca, requestPath, headers = {}) {
  return new Promise((resolve, reject) => {
    const session = http2.connect(url, { ca });
    session.on('error', reject);
    const stream = session.request({ ':method': 'GET', ':path': requestPath, ...headers });
    const chunks = [];
    let response = {};
    const done = (broke) => {
      session.close();
      resolve({ status: response[':status'], headers: response, body: Buffer.concat(chunks), broke });
    };
    stream.on('response', (got) => {
      response = got;
    });
    stream.on('data', (chunk) => chunks.push(chunk));
    stream.on('end', () => done(false));
    stream.on('error', () => done(true));
    stream.end();
  });
}

describe('mock harness', () => {
  let harness;

  before(async () => {
    harness = await startHarness();
  });

  after(async () => {
    await harness?.close();
  });

  test('handshake carries everything a libsignal-net client needs', async () => {
    const hs = harness.handshake;
    assert.equal(hs.type, 'ready');
    assert.equal(hs.protocol, 1);
    assert.equal(hs.host, 'localhost');
    assert.equal(hs.ip, '127.0.0.1');
    assert.ok(Number.isInteger(hs.port) && hs.port > 0 && hs.port < 65536, 'port');
    assert.equal(hs.httpVersion, 2);

    const ca = new X509Certificate(hs.caPem);
    assert.match(ca.subject, /CN=Signal Mock CA/);
    assert.ok(ca.ca, 'caPem is a CA certificate');
    assert.deepEqual(decodeBase64(hs.caDer, 'caDer'), ca.raw, 'caDer is the DER of caPem');

    const trustRoot = decodeBase64(hs.trustRoot, 'trustRoot');
    assert.equal(trustRoot.length, 33, 'trust root is a serialized curve25519 public key');
    assert.equal(trustRoot[0], 0x05, 'trust root has the DJB key type byte');
    for (const name of ['zkPublicParams', 'genericServerPublicParams', 'backupServerPublicParams']) {
      assert.ok(decodeBase64(hs[name], name).length > 0, `${name} is not empty`);
    }

    const base = `https://localhost:${hs.port}`;
    assert.equal(hs.urls.chat, base);
    assert.equal(hs.urls.storage, `${base}/storageService`);
    assert.deepEqual(hs.urls.cdn, { 0: base, 2: base, 3: `${base}/cdn3` });
    assert.equal(hs.paths.chatWebsocket, '/v1/websocket/');
    assert.equal(hs.paths.provisioningWebsocket, '/v1/websocket/provisioning/');

    assert.deepEqual(hs.remoteConfig['desktop.libsignalNet.grpc.AttachmentsGetUploadForm'], { enabled: true });
    assert.deepEqual(hs.remoteConfig['desktop.libsignalNet.grpc.MessagesSendMessage'], {
      enabled: true,
      value: 'ws',
    });

    assert.equal(hs.versions.mockServer, '27.0.0');
    assert.equal(hs.versions.libsignalClient, '0.100.0');
    assert.ok(fs.statSync(hs.stateDir).isDirectory(), 'state dir exists');
  });

  test('server speaks TLS + HTTP/2 under the name localhost with the handshake CA', async () => {
    const status = await h2Get(harness.handshake.urls.chat, harness.handshake.caPem, '/kourier-smoke');
    assert.equal(typeof status, 'number');
  });

  test('ping answers', async () => {
    assert.deepEqual(await harness.request('ping'), { pong: true, protocol: 1 });
  });

  test('createPrimary registers accounts with contacts and groups', async () => {
    const alice = await harness.request('createPrimary', { profileName: 'Alice' });
    assert.match(alice.aci, /^[0-9a-f-]{36}$/);
    assert.equal(alice.ref, `${alice.aci}.1`);
    assert.equal(alice.deviceId, 1);
    assert.match(alice.pni, /^PNI:[0-9a-f-]{36}$/);
    assert.match(alice.e164, /^\+\d+$/);
    assert.equal(alice.profileName, 'Alice');
    assert.equal(decodeBase64(alice.profileKey, 'profileKey').length, 32);
    assert.equal(decodeBase64(alice.identityKey, 'identityKey').length, 33);

    const phone = await harness.request('createPrimary', {
      profileName: 'Myself',
      contacts: [alice.aci],
      groups: [{ title: 'Smoke group', members: [alice.ref] }],
      linkedDevices: 1,
    });
    assert.notEqual(phone.aci, alice.aci);
    assert.deepEqual(phone.contacts, [alice.ref]);
    assert.equal(phone.secondaryDevices.length, 1);
    assert.equal(phone.groups.length, 1);
    const [group] = phone.groups;
    assert.equal(group.title, 'Smoke group');
    assert.equal(group.revision, 0);
    assert.equal(decodeBase64(group.masterKey, 'masterKey').length, 32);
    assert.equal(decodeBase64(group.id, 'group id').length, 32);

    const noNumber = await harness.request('createPrimary', { profileName: 'Nobody', hasE164: false });
    assert.equal(noNumber.e164, null);
    assert.equal(noNumber.pni, null);
  });

  test('sendText reaches another primary; waitForMessage returns text and timestamp', async () => {
    const bob = await harness.request('createPrimary', { profileName: 'Bob' });
    const carol = await harness.request('createPrimary', {
      profileName: 'Carol',
      groups: [{ title: 'Bob and Carol', members: [bob.aci] }],
    });

    const sent = await harness.request('sendText', {
      from: bob.aci,
      to: carol.aci,
      text: 'hello carol',
      timestamp: 1_700_000_000_123,
      awaitDelivery: true,
    });
    assert.deepEqual(sent, { timestamp: 1_700_000_000_123, to: carol.ref });

    const first = await harness.request('waitForMessage', { primary: carol.aci, timeout: 10_000 });
    assert.equal(first.text, 'hello carol');
    assert.equal(first.timestamp, 1_700_000_000_123);
    assert.equal(first.source, bob.ref);
    assert.equal(first.sourceAci, bob.aci);
    assert.equal(first.sourceDevice, 1);
    assert.equal(first.envelopeType, 'PreKey');
    assert.equal(first.messageType, 'PreKey');
    assert.equal(first.sealed, false);
    assert.equal(first.groupId, null);
    assert.equal(first.dataMessage.body, 'hello carol');
    assert.equal(first.remaining, 0);

    // Sealed sender, then a reply in the group; both directions now have sessions.
    const sealed = await harness.request('sendText', {
      from: bob.aci,
      to: carol.ref,
      text: 'sealed hello',
      sealed: true,
      awaitDelivery: true,
    });
    assert.ok(sealed.timestamp > 1_700_000_000_123);
    const second = await harness.request('waitForMessage', { primary: carol.aci, from: bob.aci });
    assert.equal(second.text, 'sealed hello');
    assert.equal(second.timestamp, sealed.timestamp);
    assert.equal(second.sealed, true);
    assert.equal(second.envelopeType, 'SealedSender');
    assert.equal(second.messageType, 'PreKey', 'Bob has not heard back from Carol yet');
    assert.equal(second.source, bob.ref, 'sealed sender is unsealed to the real source');

    const [group] = carol.groups;
    await harness.request('sendText', {
      from: carol.aci,
      to: bob.aci,
      text: 'in the group',
      group: group.id,
      awaitDelivery: true,
    });
    const third = await harness.request('waitForMessage', { primary: bob.aci, from: carol.ref });
    assert.equal(third.text, 'in the group');
    assert.equal(third.groupId, group.id);
    assert.equal(third.envelopeType, 'CipherText', 'Bob already had a session from the first message');
    assert.equal(third.sealed, false);
  });

  test('a message that arrives after a timed-out wait is kept for the next wait', async () => {
    const dave = await harness.request('createPrimary', { profileName: 'Dave' });
    const erin = await harness.request('createPrimary', { profileName: 'Erin' });

    await assert.rejects(harness.request('waitForMessage', { primary: erin.aci, timeout: 50 }), {
      code: 'timeout',
    });
    await harness.request('sendText', { from: dave.aci, to: erin.aci, text: 'late', awaitDelivery: true });
    const late = await harness.request('waitForMessage', { primary: erin.aci, timeout: 5_000 });
    assert.equal(late.text, 'late');
  });

  test('provisioning: waitForProvision names each socket, completeProvision answers that one', async () => {
    const phone = await harness.request('createPrimary', { profileName: 'Linking phone' });
    const first = await openProvisioningSocket(harness.handshake);
    const second = await openProvisioningSocket(harness.handshake);
    try {
      assert.notEqual(first.address, second.address);
      // Sockets are reported in the order the server handed out their addresses.
      assert.deepEqual(await harness.request('waitForProvision', { timeout: 5_000 }), { address: first.address });
      assert.deepEqual(await harness.request('waitForProvision', { timeout: 5_000 }), { address: second.address });

      // Answer the second socket; nobody links, so the wait for link + prekeys times out.
      const completion = harness.request('completeProvision', {
        url: second.linkUrl,
        primary: phone.aci,
        timeout: 1_500,
      });
      const message = second.decrypt(await second.envelope);
      assert.equal(Buffer.from(message.aciBinary).toString('hex'), phone.aci.replaceAll('-', ''));
      assert.equal(message.number, phone.e164);
      assert.equal(Buffer.from(message.profileKey).toString('base64'), phone.profileKey);
      assert.equal(Buffer.from(message.aciIdentityKeyPublic).toString('base64'), phone.identityKey);
      assert.ok(message.provisioningCode, 'provisioning code');
      assert.ok(message.pniIdentityKeyPrivate?.length > 0, 'PNI identity');
      await assert.rejects(completion, { code: 'timeout' });
      assert.ok(
        !first.requests.some((request) => request.path === '/v1/message'),
        'the other socket got no ProvisionMessage',
      );
    } finally {
      await first.close();
      await second.close();
    }

    await assert.rejects(
      harness.request('completeProvision', {
        url: 'sgnl://linkdevice?uuid=no-such-socket&pub_key=x',
        primary: phone.aci,
        timeout: 100,
      }),
      { code: 'timeout' },
    );
  });

  test('waitForDeliveries waits until a device acknowledged what was sent to it', async () => {
    const sender = await harness.request('createPrimary', { profileName: 'Sender' });
    const owner = await harness.request('createPrimary', { profileName: 'Owner', linkedDevices: 1 });
    const [offline] = owner.secondaryDevices;
    // Nothing addressed to it yet.
    assert.deepEqual(await harness.request('waitForDeliveries', { device: offline, timeout: 1_000 }), {});
    // The mock device has no socket: the envelope stays queued, the wait times out.
    await harness.request('sendText', { from: sender.aci, to: offline, text: 'queued' });
    await assert.rejects(harness.request('waitForDeliveries', { device: offline, timeout: 200 }), {
      code: 'timeout',
    });
  });

  test('unlink removes a linked device from the account', async () => {
    const phone = await harness.request('createPrimary', { profileName: 'Unlinker', linkedDevices: 1 });
    const [linked] = phone.secondaryDevices;
    assert.ok(linked.startsWith(`${phone.aci}.`) && linked !== phone.ref, 'a second device of the account');
    await assert.rejects(harness.request('unlink', { primary: phone.aci, device: phone.ref }), {
      code: 'badRequest',
    });
    assert.deepEqual(await harness.request('unlink', { primary: phone.aci, device: linked }), {});
    await assert.rejects(harness.request('unlink', { primary: phone.aci, device: linked }), { code: 'notFound' });
  });

  test('sendTexts queues texts with unique timestamps', async () => {
    const from = await harness.request('createPrimary', { profileName: 'Bulk sender' });
    const to = await harness.request('createPrimary', { profileName: 'Bulk receiver' });
    const sent = await harness.request('sendTexts', {
      from: from.aci,
      to: to.aci,
      count: 3,
      prefix: 'bulk',
      startTimestamp: 1_700_000_000_000,
    });
    assert.deepEqual(sent.timestamps, [1_700_000_000_000, 1_700_000_000_001, 1_700_000_000_002]);
    for (let i = 0; i < 3; i += 1) {
      const got = await harness.request('waitForMessage', { primary: to.aci, from: from.aci });
      assert.equal(got.text, `bulk ${i}`);
      assert.equal(got.timestamp, 1_700_000_000_000 + i);
    }

    // PNI sends are exercised against Kourier's linked device (crates/kourier-signal/tests).
    for (const args of [
      { from: from.aci, to: to.aci },
      { from: from.aci, to: to.aci, count: 0 },
      { from: from.aci, to: to.aci, count: 1, sealed: true, serviceIdKind: 'PNI' },
      { from: from.aci, to: to.aci, count: 1, serviceIdKind: 'E164' },
    ]) {
      await assert.rejects(harness.request('sendTexts', args), { code: 'badRequest' }, JSON.stringify(args));
    }
  });

  test('receipts and typing reach a primary; sync commands need a linked device', async () => {
    const frank = await harness.request('createPrimary', { profileName: 'Frank' });
    const grace = await harness.request('createPrimary', { profileName: 'Grace' });
    await harness.request('sendText', { from: frank.aci, to: grace.aci, text: 'hi', awaitDelivery: true });
    await harness.request('waitForMessage', { primary: grace.aci });

    await harness.request('sendReceipt', {
      from: grace.aci,
      to: frank.aci,
      type: 'read',
      timestamps: [1_700_000_000_001, 1_700_000_000_002],
      sealed: true,
      awaitDelivery: true,
    });
    const receipt = await harness.request('waitForReceipt', { primary: frank.aci, from: grace.aci });
    assert.equal(receipt.type, 'read');
    assert.deepEqual(receipt.timestamps, [1_700_000_000_001, 1_700_000_000_002]);
    assert.equal(receipt.sealed, true);
    assert.equal(receipt.source, grace.ref);
    // What the sealed content says about itself: the mock's primary names no group, uses the
    // default hint and adds no PNI signature.
    assert.equal(receipt.sealedGroupId, null);
    assert.equal(receipt.contentHint, 'default');
    assert.equal(receipt.pniSignature, false);

    const typed = await harness.request('sendTyping', {
      from: grace.aci,
      to: frank.aci,
      action: 'stopped',
      awaitDelivery: true,
    });
    const typing = await harness.request('waitForTyping', { primary: frank.aci, timeout: 5_000 });
    assert.equal(typing.action, 'stopped');
    assert.equal(typing.timestamp, typed.timestamp);
    assert.equal(typing.sealed, false);
    assert.equal(typing.groupId, null);

    const added = await harness.request('addDevice', { primary: frank.aci });
    assert.equal(added.aci, frank.aci);
    assert.ok(added.deviceId > 1);
    const { devices } = await harness.request('listDevices', { primary: frank.aci });
    assert.ok(devices.some((d) => d.ref === added.ref));
    await assert.rejects(harness.request('waitForEnvelope', { device: added.ref, timeout: 50 }), {
      code: 'timeout',
    });

    for (const [cmd, extra] of [
      ['sendSyncSent', { destination: grace.aci, text: 'x' }],
      ['sendMessageRequestResponse', { thread: grace.aci, type: 'accept' }],
      ['sendConfiguration', { readReceipts: true }],
    ]) {
      await assert.rejects(harness.request(cmd, { from: frank.aci, to: grace.aci, ...extra }), {
        code: 'badRequest',
      }, `${cmd} to another account`);
    }
    await assert.rejects(
      harness.request('sendReceipt', { from: grace.aci, to: frank.aci, type: 'seen', timestamps: [1] }),
      { code: 'badRequest' },
    );
    await assert.rejects(
      harness.request('sendTyping', { from: grace.aci, to: frank.aci, action: 'paused' }),
      { code: 'badRequest' },
    );
  });

  test('profiles carry avatars on CDN0; the Storage Service state gets new versions', async () => {
    const png = Buffer.from('89504e470d0a1a0a', 'hex');
    const ada = await harness.request('createPrimary', {
      profileName: 'Ada',
      avatar: png.toString('base64'),
      about: 'Out hiking',
      aboutEmoji: '🥾',
    });
    assert.match(ada.avatarPath, /^attachments\/[0-9a-f]{64}$/);
    const ca = harness.handshake.caPem;
    assert.equal(await h2Get(harness.handshake.urls.cdn['0'], ca, `/${ada.avatarPath}`), 200);
    const plain = await harness.request('createPrimary', { profileName: 'Bo' });
    assert.equal(plain.avatarPath, null);
    const stored = await harness.request('storeAttachment', { data: png.toString('base64'), contentType: 'image/png' });
    assert.equal(stored.size, png.length);
    assert.equal(decodeBase64(stored.key, 'key').length, 64);
    assert.equal(decodeBase64(stored.digest, 'digest').length, 32);
    assert.equal(await h2Get(harness.handshake.urls.cdn['0'], ca, `/attachments/${stored.cdnKey}`), 200);

    const phone = await harness.request('createPrimary', { profileName: 'Phone', contacts: [ada.ref] });
    assert.deepEqual(await harness.request('storageVersion', { primary: phone.ref }), { version: null });
    const first = await harness.request('setStorageState', {
      primary: phone.ref,
      contacts: [{ ref: ada.ref, pinned: true, archived: true }, { ref: plain.ref, blocked: true }],
      extraContacts: 3,
      account: { phoneNumberSharing: 'nobody', universalExpireTimer: 3600 },
    });
    assert.deepEqual(first, { version: 1, records: 6 }, 'account + 2 + 3 contacts');
    const second = await harness.request('setStorageState', { primary: phone.ref, recordIkm: false });
    assert.deepEqual(second, { version: 2, records: 1 });
    assert.deepEqual(await harness.request('storageVersion', { primary: phone.ref }), { version: 2 });

    await assert.rejects(
      harness.request('setStorageState', { primary: phone.ref, contacts: [{ ref: 'nobody' }] }),
      { code: 'badRequest' },
    );
    await assert.rejects(
      harness.request('setStorageState', { primary: phone.ref, account: { phoneNumberSharing: 'some' } }),
      { code: 'badRequest' },
    );
    await assert.rejects(
      harness.request('sendFetchLatest', { from: phone.ref, to: ada.ref, type: 'storageManifest' }),
      { code: 'badRequest' },
      'only to its own linked device',
    );
    const linked = await harness.request('addDevice', { primary: phone.ref });
    await assert.rejects(
      harness.request('sendFetchLatest', { from: phone.ref, to: linked.ref, type: 'everything' }),
      { code: 'badRequest' },
    );
  });

  test('groups: created with avatar, changed, texts pairwise and by sender key, storage records', async () => {
    const png = Buffer.from('89504e470d0a1a0a67726f7570', 'hex');
    const dana = await harness.request('createPrimary', { profileName: 'Dana' });
    const eli = await harness.request('createPrimary', { profileName: 'Eli' });
    const fay = await harness.request('createPrimary', { profileName: 'Fay' });
    const group = await harness.request('createGroup', {
      from: dana.ref,
      title: 'Climbing',
      members: [eli.ref],
      description: 'Weekend trips',
      timer: 3600,
      avatar: png.toString('base64'),
    });
    assert.equal(group.title, 'Climbing');
    assert.equal(group.revision, 0);
    assert.deepEqual([...group.members].sort(), [dana.aci, eli.aci].sort());
    assert.match(group.avatarKey, /^attachments\/[0-9a-f]{64}$/);
    const ca = harness.handshake.caPem;
    assert.equal(await h2Get(harness.handshake.urls.cdn['0'], ca, `/${group.avatarKey}`), 200);

    const changed = await harness.request('modifyGroup', {
      from: dana.ref,
      group: group.id,
      title: 'Climbing club',
      addMembers: [fay.ref],
      to: [eli.ref, fay.ref],
    });
    assert.equal(changed.revision, 1);
    assert.equal(changed.group.title, 'Climbing club');
    assert.equal(changed.group.members.length, 3);
    const update = await harness.request('waitForMessage', { primary: eli.aci, from: dana.aci });
    assert.equal(update.groupId, group.id);
    assert.equal(update.dataMessage.groupV2.revision, 1);
    assert.ok(update.dataMessage.groupV2.groupChange, 'the update carries the signed change');
    await harness.request('waitForMessage', { primary: fay.aci, from: dana.aci });

    const removed = await harness.request('modifyGroup', { from: dana.ref, group: group.id, removeMembers: [eli.ref] });
    assert.equal(removed.revision, 2);
    assert.deepEqual([...removed.group.members].sort(), [dana.aci, fay.aci].sort());
    await assert.rejects(
      harness.request('modifyGroup', { from: eli.ref, group: group.id, title: 'x' }),
      { code: 'badRequest' },
      'a removed member cannot change the group',
    );

    await harness.request('sendGroupText', { from: fay.ref, group: group.id, to: [dana.ref], text: 'pairwise' });
    const pairwise = await harness.request('waitForMessage', { primary: dana.aci, from: fay.aci });
    assert.equal(pairwise.text, 'pairwise');
    assert.equal(pairwise.groupId, group.id);
    for (const text of ['sender key 1', 'sender key 2']) {
      await harness.request('sendGroupText', {
        from: fay.ref,
        group: group.id,
        to: [dana.ref],
        text,
        senderKey: true,
      });
      const received = await harness.request('waitForMessage', { primary: dana.aci, from: fay.aci });
      assert.equal(received.text, text);
      assert.equal(received.groupId, group.id);
      assert.equal(received.sealed, true);
      assert.equal(received.messageType, 'SenderKey');
    }

    assert.deepEqual(await harness.request('failGroupFetch', { group: group.id }), {});
    const whileFailing = await harness.request('modifyGroup', { from: dana.ref, group: group.id, title: 'y' });
    assert.equal(whileFailing.revision, 3, 'the harness still changes a group whose fetches fail');
    await harness.request('failGroupFetch', { group: group.id, fail: false });

    const stored = await harness.request('setStorageState', {
      primary: dana.ref,
      groups: [{ id: group.id, archived: true, pinned: true }],
    });
    assert.deepEqual(stored, { version: 1, records: 2 }, 'account + group');
    assert.deepEqual(await harness.request('failStorageManifest', {}), { refused: 0 });
    assert.equal(
      (await harness.request('storageVersion', { primary: dana.ref })).version,
      1,
      'the primary still sees its manifest while linked devices are refused',
    );
    await harness.request('failStorageManifest', { fail: false });
    assert.deepEqual(await harness.request('limitProfileFetches', { retryAfter: 5 }), { refused: 0 });
    assert.deepEqual(await harness.request('limitProfileFetches', { stop: true }), { refused: 0 });
  });

  test('send controls: rate limits, token checks, stats, lost sender keys, group filter', async () => {
    const gus = await harness.request('createPrimary', { profileName: 'Gus' });
    const hal = await harness.request('createPrimary', { profileName: 'Hal' });
    const group = await harness.request('createGroup', { from: gus.ref, title: 'Stats', members: [hal.ref] });
    assert.deepEqual(await harness.request('rateLimit', { target: hal.ref, count: 2, retryAfter: 3 }), {});
    assert.deepEqual(await harness.request('rateLimit', { target: hal.ref, stop: true }), {});
    await assert.rejects(harness.request('rateLimit', { target: 'nope' }), { code: 'badRequest' });
    assert.deepEqual(await harness.request('rejectGroupSendTokens', {}), {});
    assert.deepEqual(await harness.request('rejectGroupSendTokens', { reject: false }), {});
    assert.deepEqual(await harness.request('answerMultiRecipientUnexpectedly', {}), {});
    assert.deepEqual(await harness.request('answerMultiRecipientUnexpectedly', { on: false }), {});
    const stats = await harness.request('sendStats');
    assert.equal(stats.multi, 0, 'no client sent anything');
    assert.deepEqual(stats.multiRecipients, []);
    assert.equal(stats.refusedTokens, 0);
    assert.equal(stats.unexpectedAnswers, 0);
    assert.equal(stats.groupFetches[group.id], 0);
    assert.equal(stats.uploads, 0, 'no client uploaded anything');
    assert.deepEqual(stats.certificateRequests, [], 'no client fetched a sender certificate');

    // Hal holds Gus's sender key after a sender-key text; forgetting it is counted.
    await harness.request('sendGroupText', { from: gus.ref, group: group.id, to: [hal.ref], text: 'hi', senderKey: true });
    const got = await harness.request('waitForMessage', { primary: hal.aci, group: group.id });
    assert.equal(got.text, 'hi');
    assert.equal(got.senderKeyDistribution, false, 'the distribution message came on its own');
    await assert.rejects(
      harness.request('waitForMessage', { primary: hal.aci, group: 'unknown', timeout: 20 }),
      { code: 'timeout' },
    );
    const broken = await harness.request('breakSenderKeys', { primary: hal.aci, from: gus.ref });
    assert.equal(broken.forgotten, 1);
    await assert.rejects(harness.request('waitForDecryptFailure', { primary: hal.aci, timeout: 20 }), {
      code: 'timeout',
    });
  });

  test('CDN 3 serves ranges, and breakDownloads resets a transfer part way', async () => {
    const hs = harness.handshake;
    const kim = await harness.request('createPrimary', { profileName: 'Kim' });
    const lee = await harness.request('createPrimary', { profileName: 'Lee' });
    const file = Buffer.from(Array.from({ length: 40_000 }, (_, i) => (i * 13) % 251));
    await harness.request('sendAttachments', {
      from: kim.ref,
      to: lee.aci,
      attachments: [{ data: file.toString('base64'), contentType: 'application/pdf', fileName: 'a.pdf' }],
      awaitDelivery: true,
    });
    const got = await harness.request('waitForMessage', { primary: lee.aci, from: kim.aci });
    const [pointer] = got.dataMessage.attachments;
    const blobPath = `/cdn3/attachments/${pointer.attachmentIdentifier.cdnKey}`;
    await harness.request('downloadStats', { reset: true });

    const whole = await h2Fetch(hs.urls.chat, hs.caPem, blobPath);
    assert.equal(whole.status, 200);
    assert.equal(whole.headers['accept-ranges'], 'bytes');
    const total = whole.body.length;
    assert.ok(total > file.length, 'the encrypted, padded blob');

    // The rest from a byte on, as a client asks that resumes.
    const rest = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=30000-' });
    assert.equal(rest.status, 206);
    assert.equal(rest.headers['content-range'], `bytes 30000-${total - 1}/${total}`);
    assert.deepEqual(rest.body, whole.body.subarray(30000));
    const beyond = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: `bytes=${total}-` });
    assert.equal(beyond.status, 416);
    assert.equal((await h2Fetch(hs.urls.chat, hs.caPem, `${blobPath}x`)).status, 404);

    // Two transfers are reset after 1000 body bytes, the third is whole again.
    await harness.request('breakDownloads', { after: 1000, times: 2 });
    const first = await h2Fetch(hs.urls.chat, hs.caPem, blobPath);
    assert.ok(first.broke, 'the stream was reset');
    assert.ok(first.body.length <= 1000, `${first.body.length} bytes before the reset`);
    assert.deepEqual(first.body, whole.body.subarray(0, first.body.length));
    const second = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=1000-' });
    assert.equal(second.status, 206);
    assert.ok(second.broke && second.body.length <= 1000);
    const third = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=2000-' });
    assert.ok(!third.broke);
    assert.deepEqual(third.body, whole.body.subarray(2000));

    // A CDN that knows no ranges answers with the whole blob.
    await harness.request('breakDownloads', { after: 500, times: 1, ignoreRange: true });
    const cut = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=100-' });
    assert.equal(cut.status, 200);
    assert.ok(cut.broke);
    const ignored = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=100-' });
    assert.equal(ignored.status, 200);
    assert.deepEqual(ignored.body, whole.body);
    await harness.request('breakDownloads', { stop: true });
    const after = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=100-' });
    assert.equal(after.status, 206);
    // A cut per request: after 700 bytes, then before the first byte, then whole.
    await harness.request('breakDownloads', { afters: [700, 0] });
    const part = await h2Fetch(hs.urls.chat, hs.caPem, blobPath);
    assert.ok(part.broke && part.body.length <= 700);
    const none = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=700-' });
    assert.ok(none.broke);
    assert.equal(none.body.length, 0);
    const all = await h2Fetch(hs.urls.chat, hs.caPem, blobPath, { range: 'bytes=700-' });
    assert.ok(!all.broke);
    assert.deepEqual(all.body, whole.body.subarray(700));
    await assert.rejects(harness.request('breakDownloads', { afters: [-1] }), { code: 'badRequest' });
    await harness.request('breakDownloads', { stop: true });

    const stats = await harness.request('downloadStats', {});
    assert.deepEqual(
      stats.downloads.map((d) => [d.range, d.status, d.start, d.broken]),
      [
        [null, 200, 0, false],
        ['bytes=30000-', 206, 30000, false],
        [`bytes=${total}-`, 416, total, false],
        [null, 404, 0, false],
        [null, 200, 0, true],
        ['bytes=1000-', 206, 1000, true],
        ['bytes=2000-', 206, 2000, false],
        ['bytes=100-', 200, 0, true],
        ['bytes=100-', 200, 0, false],
        ['bytes=100-', 206, 100, false],
        [null, 200, 0, true],
        ['bytes=700-', 206, 700, true],
        ['bytes=700-', 206, 700, false],
      ],
    );
    // The harness's own download still works through the gate.
    const fetched = await harness.request('fetchAttachment', { pointer });
    assert.deepEqual(decodeBase64(fetched.data, 'data'), file);
  });

  test('attachments: a phone sends files, a sticker and a long text; fetchAttachment decrypts them', async () => {
    const ivy = await harness.request('createPrimary', { profileName: 'Ivy' });
    const jon = await harness.request('createPrimary', { profileName: 'Jon' });
    const file = Buffer.from(Array.from({ length: 5000 }, (_, i) => (i * 7) % 251));
    const long = 'long text. '.repeat(400);
    const sent = await harness.request('sendAttachments', {
      from: ivy.ref,
      to: jon.aci,
      text: long,
      attachments: [{ data: file.toString('base64'), contentType: 'application/pdf', fileName: 'plan.pdf' }],
      awaitDelivery: true,
    });
    assert.equal(sent.pointers.length, 2, 'the long text and the file');
    const got = await harness.request('waitForMessage', { primary: jon.aci, from: ivy.aci });
    assert.ok(Buffer.byteLength(got.text) <= 2048 && long.startsWith(got.text), 'the body is trimmed');
    const [textPointer, filePointer] = got.dataMessage.attachments;
    assert.equal(textPointer.contentType, 'text/x-signal-plain');
    assert.equal(filePointer.fileName, 'plan.pdf');
    assert.equal(filePointer.cdnNumber, 3);
    const text = await harness.request('fetchAttachment', { pointer: textPointer });
    assert.equal(decodeBase64(text.data, 'data').toString('utf8'), long);
    const fetched = await harness.request('fetchAttachment', { pointer: filePointer });
    assert.deepEqual(decodeBase64(fetched.data, 'data'), file);
    assert.equal(fetched.size, file.length);
    assert.ok(fetched.macValid && fetched.digestValid);
    assert.equal(fetched.incrementalMacValid, null, 'no incremental MAC on this one');
    // Desktop's padding: 5000 bytes go into the 5106-byte bucket, PKCS7 rounds it up to 320
    // blocks; plus IV and MAC.
    assert.equal(fetched.blobLength, 16 + 5120 + 32);

    // A wrong digest or key is reported, not thrown.
    const forged = { ...filePointer, digest: Buffer.alloc(32, 1).toString('base64') };
    assert.equal((await harness.request('fetchAttachment', { pointer: forged })).digestValid, false);

    await harness.request('sendAttachments', {
      from: ivy.ref,
      to: jon.aci,
      sticker: true,
      attachments: [{ data: file.toString('base64'), contentType: 'image/webp' }],
      awaitDelivery: true,
    });
    const sticker = await harness.request('waitForMessage', { primary: jon.aci, from: ivy.aci });
    assert.equal(sticker.dataMessage.attachments.length, 0);
    assert.equal(sticker.dataMessage.sticker.data.contentType, 'image/webp');

    // A hostile sender's pointer: the blob is stored as usual, the pointer lies about it.
    const lied = await harness.request('sendAttachments', {
      from: ivy.ref,
      to: jon.aci,
      attachments: [
        { data: file.toString('base64'), forge: { digest: true } },
        { data: file.toString('base64'), forge: { size: 7, cdnNumber: 77, cdnKey: '../x' } },
      ],
      awaitDelivery: true,
    });
    const honest = await harness.request('fetchAttachment', { pointer: filePointer });
    assert.ok(honest.digestValid);
    const [wrongDigest, wrongPlace] = (await harness.request('waitForMessage', { primary: jon.aci, from: ivy.aci }))
      .dataMessage.attachments;
    assert.equal(wrongDigest.digest, lied.pointers[0].digest);
    const lie = await harness.request('fetchAttachment', { pointer: wrongDigest });
    assert.ok(lie.macValid && !lie.digestValid, 'the stored blob is intact, the digest is not its own');
    assert.equal(lie.size, file.length);
    assert.deepEqual(
      [wrongPlace.size, wrongPlace.cdnNumber, wrongPlace.attachmentIdentifier.cdnKey],
      [7, 77, '../x'],
    );
    await assert.rejects(
      harness.request('sendAttachments', {
        from: ivy.ref,
        to: jon.aci,
        attachments: [{ data: file.toString('base64'), forge: 'digest' }],
      }),
      { code: 'badRequest' },
    );

    await assert.rejects(harness.request('sendAttachments', { from: ivy.ref, to: jon.aci }), { code: 'badRequest' });
    await assert.rejects(
      harness.request('sendAttachments', { from: ivy.ref, to: jon.aci, sticker: true, text: 'no file' }),
      { code: 'badRequest' },
    );
    await assert.rejects(harness.request('fetchAttachment', { pointer: { cdnKey: '../x' } }), { code: 'badRequest' });
    await assert.rejects(
      harness.request('fetchAttachment', { pointer: { ...filePointer, attachmentIdentifier: { cdnKey: 'missing' } } }),
      { code: 'notFound' },
    );
  });

  test('robustness controls: upgrade gate and server clock, challenges, keys, number change', async () => {
    const hs = harness.handshake;
    /** One websocket upgrade (extended CONNECT); resolves with its response headers. */
    const upgrade = (authorization) =>
      new Promise((resolve, reject) => {
        const session = http2.connect(hs.urls.chat, { ca: hs.caPem });
        session.on('error', reject);
        session.once('remoteSettings', () => {
          const headers = {
            ':method': 'CONNECT',
            ':protocol': 'websocket',
            ':scheme': 'https',
            ':path': hs.paths.chatWebsocket,
            'sec-websocket-version': '13',
          };
          if (authorization !== undefined) {
            headers.authorization = authorization;
          }
          const stream = session.request(headers, { endStream: false });
          stream.on('error', () => {});
          stream.once('response', (response) => {
            resolve(response);
            stream.close();
            session.destroy();
          });
        });
      });
    const basic = (username, password) => `Basic ${Buffer.from(`${username}:${password}`).toString('base64')}`;

    // Every upgrade states the server's clock; `setServerTime` moves it.
    const plain = await upgrade();
    assert.equal(plain[':status'], 200, 'an unauthenticated socket');
    assert.ok(Math.abs(Number(plain['x-signal-timestamp']) - Date.now()) < 5_000, 'the true time');
    assert.deepEqual(await harness.request('setServerTime', { skewMs: 25 * 3_600_000 }), {});
    const skewed = await upgrade();
    const ahead = Number(skewed['x-signal-timestamp']) - Date.now();
    assert.ok(Math.abs(ahead - 25 * 3_600_000) < 5_000, `25 h ahead (${ahead})`);
    await assert.rejects(harness.request('setServerTime', { skewMs: 'later' }), { code: 'badRequest' });
    assert.deepEqual(await harness.request('setServerTime', { skewMs: 0 }), {});

    // Credentials the server does not know are refused at the upgrade, as the real server does.
    const owner = await harness.request('createPrimary', { profileName: 'Gatekeeper', linkedDevices: 1 });
    const [scripted] = owner.secondaryDevices;
    const ghost = `${owner.aci}.9`;
    assert.equal((await upgrade(basic(ghost, 'secret')))[':status'], 403, 'no such device');
    assert.equal((await upgrade('Bearer nope'))[':status'], 401, 'not Basic credentials');
    assert.deepEqual(await harness.request('connectionStats', { device: ghost }), {
      connects: 1,
      open: false,
      keepalives: 0,
    });
    // A scripted device has no credentials to refuse or to connect with, and no socket.
    await assert.rejects(harness.request('failAuth', { device: scripted }), { code: 'notFound' });
    await assert.rejects(harness.request('secondSocket', { device: scripted }), { code: 'notFound' });
    assert.deepEqual(await harness.request('closeSocket', { device: scripted, code: 4401 }), { closed: false });
    await assert.rejects(harness.request('closeSocket', { device: scripted, code: 7 }), { code: 'badRequest' });
    assert.deepEqual(await harness.request('failKeepalive', {}), {});
    assert.deepEqual(await harness.request('failKeepalive', { on: false }), {});
    // A device that is not linked yet is named by its future credentials: the upgrade is
    // refused as asked, as often as asked, and then left to the gate again.
    const unborn = `${owner.aci}.7`;
    assert.deepEqual(await harness.request('failAuth', { username: unborn, status: 500, count: 1 }), {});
    assert.equal((await upgrade(basic(unborn, 'secret')))[':status'], 500, 'refused as asked');
    assert.equal((await upgrade(basic(unborn, 'secret')))[':status'], 403, 'once: then no such device');
    assert.deepEqual(await harness.request('failAuth', { username: unborn, status: 503 }), {});
    assert.deepEqual(await harness.request('failAuth', { username: unborn, stop: true }), {});
    assert.equal((await upgrade(basic(unborn, 'secret')))[':status'], 403, 'stopped');
    await assert.rejects(harness.request('failAuth', { username: owner.aci }), { code: 'badRequest' });
    // The answer to a link can be held back, and is not by default.
    assert.deepEqual(await harness.request('delayLink', { ms: 5 }), {});
    assert.deepEqual(await harness.request('delayLink', {}), {});
    await assert.rejects(harness.request('delayLink', { ms: -1 }), { code: 'badRequest' });
    assert.deepEqual(await harness.request('setLatency', { ms: 5 }), {});
    assert.deepEqual(await harness.request('setLatency', {}), {});
    await assert.rejects(harness.request('setLatency', { ms: -1 }), { code: 'badRequest' });

    // Challenges: set, listed with their token, lifted.
    const target = await harness.request('createPrimary', { profileName: 'Challenged' });
    assert.deepEqual(await harness.request('challengeAnswers'), { answers: [], open: {} });
    assert.deepEqual(await harness.request('challenge', { target: target.ref, token: 'tok', accept: 'yes' }), {});
    assert.deepEqual((await harness.request('challengeAnswers')).open, { [target.aci]: { token: 'tok', refused: 0 } });
    assert.deepEqual(await harness.request('challenge', { target: target.ref, stop: true }), {});
    assert.deepEqual((await harness.request('challengeAnswers')).open, {});
    await assert.rejects(harness.request('challenge', { target: 'nope' }), { code: 'badRequest' });

    // Keys: counts per identity.
    const keys = await harness.request('keyCounts', { device: scripted });
    for (const kind of ['aci', 'pni']) {
      assert.ok(Number.isInteger(keys[kind].count) && Number.isInteger(keys[kind].pqCount), kind);
      assert.ok(Number.isInteger(keys[kind].signedPreKeyId), `${kind} signed prekey`);
      assert.ok(Number.isInteger(keys[kind].lastResortKeyId), `${kind} last-resort key`);
      assert.ok(Array.isArray(keys[kind].uploads));
    }
    // Only keys a client uploaded can be used up; a scripted device's are made on demand.
    await assert.rejects(harness.request('consumeKeys', { device: scripted, kind: 'PNI' }), { code: 'badRequest' });
    await assert.rejects(harness.request('consumeKeys', { device: scripted, kind: 'XYZ' }), { code: 'badRequest' });

    // A number change gives the account a new PNI and number, for every device.
    const changed = await harness.request('changeNumber', { primary: owner.ref });
    assert.equal(changed.aci, owner.aci);
    assert.equal(changed.oldPni, owner.pni);
    assert.notEqual(changed.pni, owner.pni);
    assert.notEqual(changed.e164, owner.e164);
    assert.equal(decodeBase64(changed.pniIdentityKey, 'pniIdentityKey').length, 33);
    const { devices } = await harness.request('listDevices', { primary: owner.aci });
    assert.deepEqual(
      devices.map((device) => device.pni),
      [changed.pni, changed.pni],
    );

    // Unlinking closes nothing when the device has no socket, and removes it all the same.
    assert.deepEqual(await harness.request('unlink', { primary: owner.aci, device: scripted, closeSocket: false }), {});
    assert.equal((await harness.request('listDevices', { primary: owner.aci })).devices.length, 1);
  });

  test('sendData builds messages, reactions and edits; waitForEdit returns edits; sendSync reaches a linked device', async () => {
    const ivy = await harness.request('createPrimary', { profileName: 'Ivy' });
    const jack = await harness.request('createPrimary', { profileName: 'Jack' });

    // A text from parts, with its own timestamp.
    const sent = await harness.request('sendData', {
      from: ivy.aci,
      to: jack.aci,
      text: 'first draft',
      timestamp: 1_700_000_100_000,
      awaitDelivery: true,
    });
    assert.equal(sent.timestamp, 1_700_000_100_000);
    assert.equal(sent.to, jack.ref);
    const got = await harness.request('waitForMessage', { primary: jack.aci, from: ivy.aci });
    assert.equal(got.text, 'first draft');
    assert.equal(got.timestamp, sent.timestamp);
    assert.equal(got.kind, 'message');
    assert.equal(got.sealed, false);

    // A sealed reaction to it: targets are whatever the sender says (the hostile case too).
    await harness.request('sendData', {
      from: jack.aci,
      to: ivy.aci,
      sealed: true,
      reaction: { emoji: '👍', targetAuthor: ivy.aci, targetTimestamp: sent.timestamp },
      awaitDelivery: true,
    });
    const reaction = await harness.request('waitForMessage', { primary: ivy.aci, kind: 'reaction' });
    assert.equal(reaction.kind, 'reaction');
    assert.equal(reaction.sealed, true);
    assert.equal(reaction.dataMessage.reaction.emoji, '👍');
    assert.equal(Number(reaction.dataMessage.reaction.targetSentTimestamp), sent.timestamp);

    // An edit is an EditMessage around the new revision: waitForEdit returns it, with the
    // revision's own timestamp and the message it names.
    await assert.rejects(harness.request('waitForEdit', { primary: jack.aci, timeout: 50 }), { code: 'timeout' });
    const edited = await harness.request('sendData', {
      from: ivy.aci,
      to: jack.aci,
      text: 'second draft',
      edit: { targetTimestamp: sent.timestamp },
      awaitDelivery: true,
    });
    const edit = await harness.request('waitForEdit', { primary: jack.aci, from: ivy.aci });
    assert.equal(edit.source, ivy.ref);
    assert.equal(edit.targetTimestamp, sent.timestamp);
    assert.equal(edit.timestamp, edited.timestamp);
    assert.equal(edit.text, 'second draft');
    assert.equal(edit.groupId, null);
    assert.equal(edit.remaining, 0);
    await assert.rejects(harness.request('waitForEdit', { primary: jack.aci, from: ivy.aci, timeout: 50 }), {
      code: 'timeout',
    });
    await assert.rejects(harness.request('waitForEdit', { primary: 'not-a-ref' }), { code: 'badRequest' });
    await assert.rejects(harness.request('sendData', { from: ivy.aci, to: [], text: 'x' }), { code: 'badRequest' });

    // Sync messages other than transcripts go from a primary to one of its linked devices.
    const linked = await harness.request('addDevice', { primary: ivy.aci });
    const names = {
      clientUuid: Buffer.alloc(16, 7).toString('base64'),
      digest: [...Buffer.alloc(32, 9)],
    };
    const syncs = [
      {
        deleteForMe: {
          messages: [{ conversation: { aci: jack.aci }, author: jack.aci, timestamp: 11 }],
          conversations: [
            {
              conversation: { aci: jack.aci },
              isFullDelete: false,
              mostRecent: [{ author: ivy.aci, timestamp: 12 }],
              mostRecentNonExpiring: [{ author: jack.aci, timestamp: 10 }],
            },
          ],
          localOnly: [{ conversation: { e164: '+15550100' } }],
          attachments: [{ conversation: { aci: jack.aci }, author: jack.aci, timestamp: 11, ...names }],
        },
      },
      { viewOnceOpen: { sender: jack.aci, timestamp: 13 } },
      { viewed: [{ sender: jack.aci, timestamp: 14 }] },
      { read: [{ sender: jack.aci, timestamp: 15 }] },
    ];
    for (const [i, content] of syncs.entries()) {
      const synced = await harness.request('sendSync', {
        from: ivy.aci,
        to: linked.ref,
        timestamp: 1_700_000_200_000 + i,
        ...content,
      });
      // Built, encrypted for the linked device and queued for it (nobody drives a scripted
      // device, so nothing reads it here; the Rust suites read it with a linked Kourier).
      assert.equal(synced.timestamp, 1_700_000_200_000 + i, Object.keys(content)[0]);
      assert.equal(synced.to, linked.ref);
    }
    await assert.rejects(harness.request('sendSync', { from: ivy.aci, to: jack.aci, read: [] }), {
      code: 'badRequest',
    });
    await assert.rejects(harness.request('sendSync', { from: ivy.aci, to: linked.ref }), { code: 'badRequest' });
    await assert.rejects(
      harness.request('sendSync', {
        from: ivy.aci,
        to: linked.ref,
        deleteForMe: { conversations: [{ conversation: { name: 'nobody' } }] },
      }),
      { code: 'badRequest' },
    );
    await assert.rejects(
      harness.request('sendSync', {
        from: ivy.aci,
        to: linked.ref,
        deleteForMe: {
          attachments: [{ conversation: { aci: jack.aci }, author: jack.aci, timestamp: 1, digest: 5 }],
        },
      }),
      { code: 'badRequest' },
    );
    // The phone's answer to a request for lost files: each entry form; the uploaded pointers
    // come back in entry order (long text last) and decrypt to what was uploaded.
    const backfill = (spec) =>
      harness.request('sendSync', {
        from: ivy.aci,
        to: linked.ref,
        attachmentBackfillResponse: { conversation: { aci: jack.aci }, author: jack.aci, timestamp: 21, ...spec },
      });
    const uuid = Buffer.alloc(16, 3).toString('base64');
    const answered = await backfill({
      attachments: [
        { status: 'pending' },
        { data: Buffer.from('again').toString('base64'), contentType: 'image/png', clientUuid: uuid },
        { status: 'terminalError' },
        { empty: true },
        { data: Buffer.from('bare').toString('base64'), forge: { digest: 'none' } },
      ],
      longText: { data: Buffer.from('the rest').toString('base64'), contentType: 'text/x-signal-plain' },
    });
    assert.equal(answered.pointers.length, 3);
    assert.equal(answered.pointers[0].clientUuid, uuid);
    assert.equal(answered.pointers[1].digest ?? null, null, 'no digest');
    assert.equal(
      Buffer.from((await harness.request('fetchAttachment', { pointer: answered.pointers[0] })).data, 'base64').toString(),
      'again',
    );
    const again = await backfill({ attachments: [{ pointer: answered.pointers[0] }] });
    assert.deepEqual(again.pointers, [answered.pointers[0]]);
    for (const spec of [{ error: 'messageNotFound' }, { error: 7 }, { empty: true }]) {
      assert.deepEqual(Object.keys(await backfill(spec)).sort(), ['pointers', 'timestamp', 'to']);
    }
    for (const spec of [
      { attachments: [{ status: 'later' }] },
      { attachments: [{}] },
      { error: 'messageNotFound', attachments: [] },
      {},
    ]) {
      await assert.rejects(backfill(spec), { code: 'badRequest' }, JSON.stringify(spec));
    }
    await harness.request('unlink', { primary: ivy.aci, device: linked.ref, closeSocket: false });

    // The multi-recipient challenge of the websocket route is set and lifted.
    assert.deepEqual(await harness.request('challengeMultiRecipient', { retryAfter: 7 }), { refused: 0 });
    assert.deepEqual(await harness.request('challengeMultiRecipient', { stop: true }), { refused: 0 });
  });

  test('errors are reported per request', async () => {
    await assert.rejects(harness.request('noSuchCommand'), { code: 'unknownCommand' });
    await assert.rejects(harness.request('createPrimary', {}), { code: 'badRequest' });
    await assert.rejects(harness.request('createPrimary', { profileName: 'Bad group', groups: [{}] }), {
      code: 'badRequest',
    });
    await assert.rejects(
      harness.request('createPrimary', {
        profileName: 'Unknown member',
        groups: [{ title: 'g', members: ['00000000-0000-4000-8000-000000000000'] }],
      }),
      { code: 'notFound' },
    );
    // setTimeout would fire at once for a longer timeout.
    await assert.rejects(harness.request('waitForProvision', { timeout: 2 ** 31 }), { code: 'badRequest' });
    await assert.rejects(harness.request('waitForMessage', { primary: 'not-a-ref' }), { code: 'badRequest' });
    await assert.rejects(
      harness.request('waitForMessage', { primary: '00000000-0000-4000-8000-000000000000' }),
      { code: 'notFound' },
    );
    await assert.rejects(harness.request('waitForProvision', { timeout: 20 }), { code: 'timeout' });
    const frank = await harness.request('createPrimary', { profileName: 'Frank' });
    await assert.rejects(
      harness.request('completeProvision', { url: 'sgnl://linkdevice?pub_key=x', primary: frank.aci }),
      { code: 'badRequest' },
    );
    await assert.rejects(harness.request('completeProvision', { url: 'sgnl://linkdevice', primary: 7 }), {
      code: 'badRequest',
    });
    await assert.rejects(harness.request('sendText', { from: frank.aci, to: frank.aci, text: 'me' }), {
      code: 'badRequest',
    });

    const garbage = await harness.sendRaw('{not json', null);
    assert.equal(garbage.ok, false);
    assert.equal(garbage.error.code, 'badRequest');

    // The harness keeps serving after errors.
    assert.deepEqual(await harness.request('ping'), { pong: true, protocol: 1 });
  });
});

test('shutdown answers, exits 0 and removes its state dir', async () => {
  const harness = await startHarness();
  const { stateDir } = harness.handshake;
  assert.ok(fs.existsSync(stateDir));
  assert.deepEqual(await harness.request('shutdown'), {});
  const { code } = await harness.exited;
  assert.equal(code, 0);
  assert.equal(fs.existsSync(stateDir), false);
});

test('closing stdin stops the harness', async () => {
  const harness = await startHarness();
  const { code } = await harness.close();
  assert.equal(code, 0);
});
