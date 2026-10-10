# Signal mock-server harness

Drives [`@signalapp/mock-server`](https://www.npmjs.com/package/@signalapp/mock-server) 27.0.0
(Signal's own test server, AGPL, the one Signal Desktop's `ts/test-mock` uses) for Kourier's
Signal integration tests, without the Desktop checkout (`docs/architecture.md` §10.3).
`harness.mjs` starts a mock server on a random loopback port, prints one handshake line with
everything a libsignal-net client needs, then takes JSON-lines commands on stdin. It never
talks to a real Signal server.

| File | What |
|---|---|
| `harness.mjs` | the harness (Node ≥ 24) |
| `client.mjs` | JavaScript driver used by the node tests |
| `provisioning-socket.mjs` | test-only provisioning socket (HTTP/2 extended CONNECT websocket + ProvisionEnvelope decryption) |
| `smoke.mjs` | node tests of the harness |
| `../../crates/kourier-signal/tests/support/harness.rs` | Rust driver for the integration tests |

## Setup

Node 24 is installed with nvm; the nvm default alias stays 22, so call it by path:

```bash
cd tests/mock-harness
PATH=~/.nvm/versions/node/v24.21.0/bin:$PATH npm ci     # node_modules/ is git-ignored
```

`npm ci` installs exactly `package-lock.json` (mock-server 27.0.0 with
`@signalapp/libsignal-client` 0.100.0 and its prebuilt linux-x64 binaries).

## Run

```bash
# node tests of the harness itself
~/.nvm/versions/node/v24.21.0/bin/node --test smoke.mjs

# Rust driver smoke test (from the workspace root)
cargo test -p kourier-signal --test harness_smoke

# the harness by hand: type commands, e.g. {"id":1,"cmd":"createPrimary","args":{"profileName":"A"}}
~/.nvm/versions/node/v24.21.0/bin/node harness.mjs
```

The Rust tests look for Node in `$KOURIER_NODE`, then `~/.nvm/versions/node/v24.21.0/bin/node`,
then `node` on `PATH` (if it is ≥ 24). Without Node ≥ 24 or without `node_modules` they print
`skipping: mock harness unavailable: ...` and pass; set `KOURIER_REQUIRE_MOCK_HARNESS=1` to make
that a failure. `KOURIER_HARNESS_VERBOSE=1` forwards the harness's stderr; `DEBUG=mock:*` turns
on the mock server's own logs (they go to stderr too).

Harness options: `--host <ip>` (default `127.0.0.1`), `--state-dir <dir>` (keep the server's
files; default is a temporary directory removed on exit), `--api-timeout <ms>` (the mock's
internal queue timeout, default 60000).

## Protocol

stdout carries only protocol lines, one JSON object each; logs go to stderr. Every line has a
`type`:

| `type` | When | Fields |
|---|---|---|
| `ready` | first line, once | the handshake below |
| `response` | one per request | `id` (echoed, `null` for unparseable input), `ok`, `result` or `error{code, message}` |
| `event` | any time | `event`, `data` (`provisionOpened{address}`, `deliveryFailed{to, timestamp, code, message}`, `unhandledRejection`) |
| `fatal` | before exiting on a crash | `error{code, message}` |

Requests are `{"id": <number>, "cmd": "<name>", "args": {...}}`. Commands run concurrently;
responses come back in completion order, matched by `id`. Error codes: `badRequest`,
`notFound`, `timeout`, `unknownCommand`, `failed` (anything thrown by the mock). Closing stdin,
`SIGINT`/`SIGTERM` or `shutdown` stop the server and exit 0.

### Handshake

| Field | What |
|---|---|
| `protocol` | protocol version (1) |
| `host`, `ip`, `port` | connect to `ip:port`, TLS server name `host` (`localhost`) |
| `httpVersion` | `2`: websockets are served only over HTTP/2 extended CONNECT (RFC 8441); REST also accepts HTTP/1.1 |
| `caPem`, `caDer` | the CA of the server certificate (PEM, and DER in base64 for `RootCertificates::FromDer`) |
| `trustRoot` | sealed-sender trust root, 33-byte public key, base64 |
| `zkPublicParams` | zkgroup `ServerPublicParams`, base64 |
| `genericServerPublicParams`, `backupServerPublicParams` | the other zk params, base64 |
| `urls` | `chat`, `storage` (`/storageService`), `cdn` {`0`, `2`, `3` (`/cdn3`)}, `resources`, `sfu` |
| `paths` | `chatWebsocket` (`/v1/websocket/`), `provisioningWebsocket` (`/v1/websocket/provisioning/`) |
| `remoteConfig` | what `/v2/config` serves: Desktop's mock config (`SD/ts/test-mock/bootstrap.node.ts`), 1:1 and sync sends on the websocket, upload form and multi-recipient send on gRPC |
| `stateDir`, `pid`, `versions` | diagnostics |

### Devices

A device is named by a ref `"<aci>.<deviceId>"`; a bare `"<aci>"` means device 1 where one
device is needed and any device of the account in `from` filters. Primaries ("phones") are
scripted by the harness; everything they receive is decrypted and buffered per primary, so a
`waitFor*` that times out never loses a message that arrives later.

### Commands

| Command | Args | Result |
|---|---|---|
| `ping` | | `{pong, protocol}` |
| `createPrimary` | `profileName`, `contacts?` (refs of earlier primaries, served as contact sync), `groups?` (`[{title, members?}]`; all args are checked before anything is registered), `hasE164?` (true), `linkedDevices?` (0), `avatar?` (image bytes, base64: the profile avatar, encrypted with the profile key on CDN0), `about?`, `aboutEmoji?` | `{ref, aci, pni, deviceId, e164, profileName, profileKey, identityKey, secondaryDevices, contacts, groups[{id, title, masterKey, revision, members, avatarKey}], avatarPath}` |
| `waitForProvision` | `timeout?` | `{address}` of the next provisioning socket a client opened (the `uuid` of its link URL) |
| `completeProvision` | `url` (`sgnl://linkdevice?uuid=..&pub_key=..`), `primary`, `timeout?` (60000) | the linked device `{ref, aci, pni, deviceId, e164}`, once it linked and uploaded one-time prekeys; the primary then has a session with it |
| `sendText` | `from` (primary), `to` (device), `text`, `sealed?`, `timestamp?`, `withProfileKey?`, `group?` (group id), `serviceIdKind?` (`ACI`, or `PNI`: to the device's PNI, a PreKey message; never sealed), `awaitDelivery?` (false), `timeout?` | `{timestamp, to}` |
| `sendTexts` | `from`, `to`, `count` (1..=10000), `prefix?` ("message"), `sealed?`, `serviceIdKind?`, `startTimestamp?` (now) | `{timestamps, to}`: texts `"<prefix> <i>"` at `startTimestamp + i`, queued without waiting (failures as `deliveryFailed` events) |
| `waitForMessage` | `primary` (alias `device`), `from?`, `group?` (a group id: only messages in that group), `kind?` (`message`, `reaction`, `delete`, `adminDelete`, `timer`: only that kind, the others stay buffered), `timeout?` | `{source, sourceAci, sourceDevice, text, timestamp, kind, envelopeType, messageType, sealed, serviceIdKind, groupId, sealedGroupId, senderKeyDistribution, dataMessage, remaining}`; `text` is `""` for a DataMessage without a body; `sealedGroupId` = the group id (base64) the sealed content names (null when unsealed or none); `senderKeyDistribution` = the Content also carried a sender-key distribution message |
| `waitForSync` | `primary`, `device`, `timeout?`, `reset?` (true) | `{}` once the primary answered the device's CONTACTS, BLOCKED and CONFIGURATION requests |
| `waitForDeliveries` | `device`, `timeout?` | `{}` once the device has acknowledged everything addressed to it (nothing in flight, nothing queued for its next connection) |
| `listDevices` | `primary` | `{devices: [{ref, aci, pni, deviceId, e164}]}`: every device the server registered for the primary's account, primary first |
| `unlink` | `primary`, `device`, `closeSocket?` (true) | `{}`; the device is removed from the account, as the phone's "unlink device" does: its credentials stop working (a new websocket upgrade answers 403) and its open socket is closed with 4401 (`closeSocket: false` leaves it open, as the mock itself would) |
| `secondSocket` | `device`, `holdMs?` (0) | `{status, tookOver}`; opens a second websocket with the device's own credentials, as another client using them would, and closes it again: the server closes the device's first socket with 4409 (connected elsewhere) |
| `closeSocket` | `device`, `code?` (1001) | `{closed}`; closes the device's websocket from the server's side (1001 going away, 4401 connection invalidated, 4409 connected elsewhere) |
| `failAuth` | `device` (or `username` `"<aci>.<deviceId>"` for a device that is not linked yet), `status?` (401; any 4xx or 5xx), `count?` (until stopped), `stop?` | `{}`; websocket upgrades with the device's credentials answer HTTP `status` although the device is still linked |
| `delayLink` | `ms?` (0 = answer at once again) | `{}`; the answer to `PUT /v1/devices/link` is held back that long after the server linked the device, so the device is on the account (`listDevices`) while its client still waits to hear so |
| `setServerTime` | `skewMs` | `{}`; moves the clock the server states on every websocket upgrade and on every envelope it delivers (`x-signal-timestamp`) by that much; 0 = the true time |
| `failKeepalive` | `on?` (true) | `{}`; `GET /v1/keepalive` answers 500 |
| `connectionStats` | `device` (a ref; the device may be unlinked by now) | `{connects, open, keepalives}`: authenticated upgrades asked for under its name (refused ones and `secondSocket` included), whether it has a socket now, keepalive requests served (all sockets) |
| `keyCounts` | `device` | `{aci, pni}`, each `{count, pqCount, signedPreKeyId, lastResortKeyId, uploads}`; `uploads` lists what each key upload of the device carried, the link first: `{preKeys, pqPreKeys, signedPreKey, pqLastResortPreKey}` (counts, and key ids or null) |
| `consumeKeys` | `device` (a linked client, not a scripted device), `kind?` (`ACI`), `leave?` (0) | `{count, pqCount}`; uses up the device's one-time prekeys on the server, as senders fetching them would |
| `changeNumber` | `primary`, `awaitDelivery?` (false), `timeout?` | the primary `{ref, aci, pni, deviceId, e164}` after the change, `oldPni`, `pniIdentityKey`, `pniRegistrationId`; the account gets a new number, PNI and PNI identity, and every linked device a `SyncMessage.pniChangeNumber` (envelope `updatedPni`) |
| `challenge` | `target` (ref), `token?` (`"challenge-token"`), `accept?` (the only captcha accepted; default any), `retryAfter?` (s, 60), `busy?` (answers refused with 413 first), `stop?` | `{}`; client 1:1 sends to `target` answer 428 `{token, options: ["captcha"]}` until a client answers `PUT /v1/challenge {type: "captcha", token, captcha}`; a wrong captcha is answered 428 again, an unknown token 400 |
| `challengeMultiRecipient` | `token?` (`"multi-challenge-token"`), `retryAfter?` (s, 60), `stop?` | `{refused}`; every multi-recipient send that arrives over the websocket route (`PUT /v1/messages/multi_recipient`, used while the remote config does not move these sends to gRPC) answers 428 `{token, options: ["captcha"]}` with `Retry-After`; `stop` leaves them to the mock again (it has no such route and fails them) |
| `challengeAnswers` | | `{answers: [{type, token, captcha, accepted, status}], open: {<service id>: {token, refused}}}`: every challenge answer clients sent, and the challenges still standing with the sends refused so far |
| `addDevice` | `primary` | the new scripted device `{ref, aci, pni, deviceId, e164}` of the primary's account (nobody drives it; see `waitForEnvelope`) |
| `waitForEnvelope` | `device` (a scripted secondary), `timeout?` | `{to, source, envelopeType, timestamp, sealed}` of the next client envelope addressed to it (`source` is `null` when sealed) |
| `sendReceipt` | `from` (primary), `to` (device), `type` (`delivery`, `read`, `viewed`), `timestamps`, `timestamp?`, `sealed?`, `awaitDelivery?`, `timeout?` | `{timestamp, to}` |
| `sendTyping` | `from`, `to`, `action` (`started`, `stopped`), `timestamp?`, `sealed?`, `group?` (group id: a group typing message), `awaitDelivery?`, `timeout?` | `{timestamp, to}` |
| `sendSyncSent` | `from` (primary), `to` (its linked device), `destination` (ACI), `text`, `timestamp?`, `withProfileKey?` (true), `expireTimer?`, `unidentified?`, `destinationKind?` (`ACI`; `PNI`: the phone wrote to the contact's number, the transcript names their PNI), `awaitDelivery?`, `timeout?` | `{timestamp, to}`: the phone's transcript of a 1:1 text it sent |
| `sendMessageRequestResponse` | `from`, `to` (its linked device), `thread` (ACI), `type` (`accept`, `delete`, `block`, `blockAndDelete`, `spam`, `blockAndSpam`), `awaitDelivery?`, `timeout?` | `{timestamp, to}` |
| `sendConfiguration` | `from`, `to` (its linked device), `readReceipts?`, `typingIndicators?`, `linkPreviews?`, `unidentifiedDeliveryIndicators?`, `awaitDelivery?`, `timeout?` | `{timestamp, to}` |
| `waitForReceipt` | `primary`, `from?`, `timeout?` | `{source, type, timestamps, sealed, sealedGroupId, contentHint, pniSignature, remaining}`; `sealedGroupId` and `contentHint` as in `waitForDistribution` (null when unsealed), `pniSignature` = the Content carried a `PniSignatureMessage` beside the receipt |
| `waitForTyping` | `primary`, `from?`, `timeout?` | `{source, action, timestamp, groupId, sealed}` (the mock ignores typing; the harness buffers it) |
| `waitForDistribution` | `primary`, `from?`, `timeout?` | `{source, sealed, sealedGroupId, contentHint}`: the next Content that carried a sender-key distribution message and nothing else (a group send's 1:1 distribution; the mock processes it silently, the harness buffers it); `contentHint` = the sealed content's hint (`default`, `resendable`, `implicit`; null when unsealed) |
| `waitForSyncMessage` | `primary`, `from?`, `kind?` (`sent`, `read`, `viewed`, or a SyncMessage content field), `timeout?` | `{source, kind, syncMessage}` of the next sync message (not a sync request) from a linked device |
| `createGroup` | `from` (primary, administrator), `title`, `members?` (refs of other primaries), `admins?` (refs among them), `description?`, `timer?` (s), `avatar?` (image bytes, base64: encrypted with the group's blob key on CDN0), `announcementsOnly?` | the group `{id, title, masterKey, revision, members (ACIs), avatarKey}` at revision 0 |
| `modifyGroup` | `from` (a member), `group`, `title?`, `description?`, `timer?` (s, 0 = off), `avatar?` (base64; `""` removes it), `addMembers?`, `removeMembers?` (refs), `admins?` (refs among the added), `announcementsOnly?`, `to?` (devices that get the update: `DataMessage.groupV2` with the new revision and the signed change; `from`'s own linked devices get a sync transcript), `timestamp?` | `{revision, timestamp, group}`: one signed change, logged by the server without its signature as the mock does (the mock's own modify has no member adds or avatar changes) |
| `sendGroupText` | `from` (a member), `group`, `to` (device refs), `text`, `timestamp?`, `senderKey?` (false: pairwise; true: a sender-key distribution message to each device once, then a sealed sender-key message as Desktop sends groups), `sealed?` (pairwise only) | `{timestamp, to}` (`to` the refs) |
| `sendData` | `from`, `to` (a ref, or refs for a group), `group?`, `senderKey?`, `sealed?`, `timestamp?`, `dataTimestamp?`, `serverTimestamp?` (default now: the envelope is stamped like a real server does, whatever `timestamp` claims), `text?`, `withProfileKey?`, `expireTimer?`, `expireTimerVersion?`, `flags?` (1 end session, 2 timer update, 4 profile key update), `viewOnce?`, `requiredProtocolVersion?`, `reaction?` `{emoji, remove?, targetAuthor, targetTimestamp}`, `delete?` `{targetTimestamp}`, `adminDelete?` `{targetAuthor, targetTimestamp}`, `quote?` `{id, author, text?, attachments?}`, `groupCallUpdate?` `{eraId?}` (a group-call update; with `sync` the phone's own), `unsupported?` (`contact`, `payment`, `giftBadge`, `poll`, `pollEnd`, `storyReply`, `groupCall`, `pin`), `edit?` `{targetTimestamp}` (an EditMessage around it), `sync?` `{destination?, unidentified?, expirationStartTimestamp?, isRecipientUpdate?}` (as the phone's transcript to its linked device), `awaitDelivery?` | `{timestamp, serverTimestamp, to}` (`serverTimestamp`: what the envelope was stamped with; it is read from the clock after `timestamp` and can be a millisecond later). A DataMessage from parts: edits, deletes, reactions, quotes, timers and placeholders from the phone or a contact. Targets are not checked, so it is also the hostile sender (an edit or delete of someone else's message, a reaction with a wrong author) |
| `sendCall` | `from` (primary: the caller or ringer), `to` (device), `timestamp?`, `sealed?`, any of `offer` `{id, video?}`, `hangup` `{id, type? (`normal`, `accepted`, `declined`, `busy`, `needPermission`, or the wire number), deviceId?}`, `answer` `{id}`, `busy` `{id}`, `iceUpdate` `{id}`, `ring` `{group (a group id) or groupId (base64 bytes of any id), ringId, cancelled?}` (RingRTC's `ring_intention` in `opaque.data`), `opaque` (base64: raw `opaque.data` in place of `ring`), `destinationDeviceId?`, `ageMs?` (0: how long the envelope waited on the server; its `serverTimestamp` is the server's clock minus that), `serverTimestamp?`, `awaitDelivery?`, `timeout?` | `{timestamp, serverTimestamp, to}` (`timestamp` the sender's own, `serverTimestamp` the envelope's; compare a client's server-timed line with the second): call signalling (`CallMessage`); ids are numbers or decimal strings (a call id is a uint64, a ring id an int64) |
| `sendSync` | `from`, `to`, `timestamp?`, one of `callEvent` `{conversation ({aci}, {group} or {raw}: base64), callId (number or decimal string), timestamp?, type (`audio`, `video`, `group`, `adHoc`), direction (`incoming`, `outgoing`), event (`accepted`, `notAccepted`, `delete`, `observed`)}` (what the phone syncs about a call), `deleteForMe` `{messages? [{conversation, author, timestamp}], conversations? [{conversation, isFullDelete? (true), mostRecent?, mostRecentNonExpiring?}], localOnly? [{conversation}], attachments? [{conversation, author, timestamp, clientUuid?, digest?, plaintextHash?}]}` (`conversation`: `{aci}`, `{group}` or `{e164}`; the names of an attachment as base64 or byte arrays, as a pointer from `waitForMessage` has them), `viewOnceOpen` `{sender, timestamp}`, `viewed` / `read` `[{sender, timestamp}]` | `{timestamp, to}`: a sync message other than a transcript from the phone |
| `waitForEdit` | `primary`, `from?`, `group?`, `timeout?` | `{source, targetTimestamp, timestamp, text, sealed, messageType, groupId, sealedGroupId, dataMessage, remaining}` of the next EditMessage the primary decrypted |
| `failGroupFetch` | `group`, `fail?` (true) | `{}`; while set, every Storage Service request for the group answers 500 (`modifyGroup` still works) |
| `sendUndecryptable` | `from`, `to` (device), `text?`, `timestamp?`, `hint?` (`default`, `resendable`, `implicit`) | `{timestamp, to}`: a sealed sender-key message whose distribution message is never sent, so the recipient cannot decrypt it; the content hint is as given, and the failure has no group context (treated 1:1) |
| `sendRetryRequest` | `from`, `to` (device), `timestamp`, `senderDevice` (the recipient's device id) | `{timestamp, to}`: a `DecryptionErrorMessage` for the message that device sent at `timestamp`, in an ordinary encrypted Content; no ratchet key, so the recipient resends from its Message Send Log without archiving |
| `waitForDecryptionError` | `primary` (alias `device`), `from?`, `timeout?` | `{source, timestamp, senderDevice, hasRatchetKey}` of the next retry request the primary decrypted |
| `setStorageState` | `primary`, `contacts?` (`[{ref, profileKey? (true), givenName? (their profile name), familyName?, systemGivenName?, systemFamilyName?, whitelisted? (true), blocked?, archived?, markedUnread?, mutedUntil? (ms), pinned?, identityKey? (true), byPni? (the record names the contact's PNI and number only: a contact the phone knows by number), withPni? (the record carries the PNI beside the ACI: the phone joined the two)}]`), `groups?` (`[{id, blocked?, whitelisted? (true), archived?, markedUnread?, mutedUntil? (ms), pinned?}]`: `GroupV2Record`s), `extraContacts?` (synthetic "Contact n" records with random ACIs), `account?` (`{readReceipts? (true), typingIndicators? (true), linkPreviews?, sealedSenderIndicators?, phoneNumberSharing? (everybody\|nobody), universalExpireTimer? (s), noteToSelfArchived?, noteToSelfMarkedUnread?}`), `recordIkm?` (true; false = legacy record keys) | `{version, records}`: the phone's whole Storage Service state as a new manifest version |
| `storeAttachment` | `data` (base64), `contentType?` | the `AttachmentPointer` fields `{cdnKey, cdnNumber, key, digest, size, contentType}` of the data encrypted like Desktop (padding included) on CDN0 |
| `sendAttachments` | `from` (primary), `to` (device), `attachments` (`[{data (base64), contentType?, fileName?, width?, height?, flags? (1 voice note, 8 GIF), caption?, blurHash?, durationSeconds?, cdn? (3; 0 = legacy store), forge? (`{digest? (true), size?, cdnNumber?, cdnKey?}`: a hostile sender's pointer, with a wrong digest or these values in place of the true ones; the blob is stored as usual)}]`), `text?` (the body; longer than 2048 bytes it goes out trimmed with a `text/x-signal-plain` attachment first, as Desktop sends it), `sticker?` (the first attachment becomes the sticker), `sealed?`, `withProfileKey?` (true), `timestamp?`, `editTarget?` (a sent timestamp: send it as an EditMessage for that message), `awaitDelivery?`, `timeout?` | `{timestamp, to, pointers}`: each file encrypted like Desktop's `encryptAttachmentV2` and stored on the mock's CDN; the mock's protobufs drop `audioDurationSeconds` and `audioWaveform` |
| `fetchAttachment` | `pointer` (one of `waitForMessage`'s `dataMessage.attachments`, or from a transcript) | `{data (base64), size, blobLength, macValid, digestValid, incrementalMacValid (null without one; checked with libsignal-client), chunkSize}`: what a phone downloads from CDN 3 (the TUS store) or CDN 0, decrypted and stripped to `size` |
| `storageVersion` | `primary` | `{version}` of its manifest on the server (`null`: none) |
| `sendFetchLatest` | `from`, `to` (its linked device), `type` (`storageManifest`, `localProfile`, `subscriptionStatus`), `awaitDelivery?`, `timeout?` | `{timestamp, to}` |
| `setRemoteConfig` | `key`, `enabled?` (true), `value?` | `{}` |
| `rateLimit` | `target` (ref), `count?` (until stopped), `retryAfter?` (s, 1), `stop?` | `{}`; client 1:1 sends to `target` answer 429 with `Retry-After` for the next `count` sends (multi-recipient sends are not limited per recipient, like the real server) |
| `rejectGroupSendTokens` | `reject?` (true) | `{}`; every multi-recipient send is answered `FailedUnidentifiedAuthorization` (as for an expired token). Tokens are always verified against the recipients of the payload, which the mock itself does not do |
| `answerMultiRecipientUnexpectedly` | `on?` (true) | `{}`; every multi-recipient send is answered with a gRPC response without its `response` case, which libsignal's typed API reports as unexpected (as a server version the client does not know might answer) |
| `breakDownloads` | `afters?` (`[n, …]`: the body bytes after which each of the next downloads is reset, 0 = before the first byte), or `after?` (0) for the next `times?` (1; -1 = until stopped); `ignoreRange?`; `stop?` | `{}`: `GET /cdn3/attachments/<key>` answers are reset part way, as a connection that dropped (an answer no longer than its cut is left alone). The harness serves these requests itself, with ranges as a real CDN does (`Range: bytes=N-` gets 206 and `Content-Range`, a start beyond the blob 416); with `ignoreRange` it answers a range request with the whole blob (200) |
| `downloadStats` | `reset?` | `{downloads: [{key, range, status, start, sent, broken}]}`: every attachment download asked of CDN 3, in order (`range`: the request's header or null, `start`: the first byte served, `sent`: body bytes written) |
| `setEphemeralBackupKey` | `primary`, `key?` (base64 of 32 bytes; random when absent), `clear?`, `force?` (true: the key is sent also when the URL offers no `backup5`, which no real phone does) | `{key}`: the phone's user chose "Transfer message history": every later `completeProvision` by this primary whose URL offers `backup5` puts this ephemeral backup key into the provisioning message (`clear`: "Don't transfer" again). `completeProvision` then also returns `capabilities`, `sentTransferKey`, `linkRequestAfterMs` and `linkedAfterMs` (ms from the provisioning message leaving the phone to the link request reaching the server, and to its answer) |
| `storeTransferArchive` | `path` (a local file) | `{cdn: 3, key, size}`: the file is on CDN 3 as the phone's uploaded archive; downloads of it are served with ranges and obey `breakDownloads` (`downloadStats`) |
| `provideTransferArchive` | `device`, and `cdn` + `key`, or `error` (`"RELINK_REQUESTED"`, `"CONTINUE_WITHOUT_UPLOAD"`, or any string) | `{}`: the phone's answer to the device's long-poll `GET /v1/devices/transfer_archive?timeout=<s>`. The harness answers that request itself as the real server does: 204 when nothing was provided within the timeout the client asked for (the mock alone would wait for ever) |
| `holdTransferArchive` | `silent?` (true: no long-poll is answered at all; one left unanswered stays so when this is switched off), `fail?` (answer the next n with `status?`, 500 unless given), `immediate?` (true: every long-poll gets 204 at once instead of being held for its timeout) | `{}` |
| `transferStats` | `reset?` | `{polls: [{at, timeout, status}]}`: every transfer long-poll a client sent (`status`: 204, 200, 500 or `"silent"`; null while one is held) |
| `sendStats` | | `{single: {<service id>: n}, multi, multiRecipients, refusedTokens, unexpectedAnswers, groupFetches: {<group id>: n}, uploads, sendRequests}`: client 1:1 sends per destination, multi-recipient sends served and their recipients, refused tokens, sends answered by `answerMultiRecipientUnexpectedly`, Storage Service group requests per group, attachment uploads clients started on CDN 3 (TUS creations); `sendRequests` = every request a client made to send a message to anybody, answered with success or not (the 1:1 route: messages, receipts, typing, call signalling, sync messages; plus every multi-recipient send on either route): "the client sent nothing" is this number before and after. Every envelope the server delivers carries `X-Signal-Timestamp` (the server's clock, as the real server states it; the mock sends none) |
| `breakSenderKeys` | `primary`, `from` (ref) | `{forgotten}`; the primary forgets the sender keys it holds from `from`, so the next sender-key message from them fails to decrypt |
| `waitForDecryptFailure` | `primary`, `from?`, `timeout?` | `{source, timestamp, envelopeType}` of the next client envelope a primary could not decrypt. Like a real client, the primary then sends the sender's device a retry request (`DecryptionErrorMessage` for the original) instead of failing the delivery |
| `shutdown` | | `{}`, then the process exits 0 |

`timeout` is in milliseconds (default 30000, at most 2^31 − 1). Planned next (§10.3):
`sendEdit`, `sendReaction`; the edit and story inboxes are already buffered.

### Adding a command

One entry in the `commands` table of `harness.mjs` (validate args with the `req*`/`opt*`
helpers, throw `HarnessError` codes), a row above, a node test in `smoke.mjs`, and a typed
wrapper in `crates/kourier-signal/tests/support/harness.rs` if tests use it often (anything
else goes through `Harness::request`). Bump `PROTOCOL_VERSION` in both drivers only for
incompatible changes.

## Notes and gaps

- Profiles: the mock serves only the name and keys. The harness pads the name as real clients
  do (`given \0 family`, 53/257 bytes) and adds `avatar`, `about` and `aboutEmoji` to
  `GET /v1/profile/…` answers by wrapping the mock's websocket router
  (`src/server/ws/router.js`); avatars are CDN0 attachments (`attachments/<sha256>`).
- `setStorageState` writes like a phone (`PrimaryDevice.setStorageState`) but does not notify:
  send `sendFetchLatest{type: storageManifest}` to make a linked device resync. The mock's
  contact sync blob carries profile names as address-book names and no avatars; its BLOCKED
  sync answer is always empty.

- Linking flow as Desktop's bootstrap does it: the client opens the provisioning socket and
  shows its URL; the test calls `waitForProvision` (or already knows the URL) and
  `completeProvision{url, primary}` while the client finishes linking.
- The mock reports sealed envelopes by their inner type; the harness tags sealed decrypts
  itself (`sealed`, `envelopeType: "SealedSender"`, the inner type in `messageType`).
- Messages to a primary are handed to it directly (the mock only routes client sends to
  primaries); messages to linked devices go through the server and wait for their socket.
- Scripted secondary devices (`linkedDevices`, `addDevice`) have keys but no client: the mock
  drops what clients send them, after the server checked the device list (so a client must
  include them, or get 409). The harness records those envelopes for `waitForEnvelope`.
- The mock loses an envelope it delivers from its offline queue when that socket dies (or
  answers non-200) before the ack; only its live path re-queues. The harness's server subclass
  queues such an envelope again (up to 50 times), as the real server keeps every unacknowledged
  envelope, so crash tests can kill a client mid-delivery.
- Where the harness makes the mock answer like the real server (S15): a websocket upgrade
  with credentials the server does not know is refused with 403 (a wrong password: 401); the
  mock accepts it and closes the socket with 3000. Every upgrade states the server's clock in
  `x-signal-timestamp`. `DELETE /v1/devices/<id>` removes a linked device (the device itself or
  its primary may ask). A key upload without one-time keys (a rotation of the signed prekey and
  the last-resort key) is accepted and leaves the one-time keys alone; the mock's schema wants a
  `preKeys` array and its `setKeys` drops the keys an upload does not bring. A second one-time
  key upload of the identity that uploaded first while linking is accepted; the mock answers it
  400 ("Duplicate service id kind") after storing the keys. `PUT /v1/challenge` checks the
  token and the captcha; what the real server answers to a wrong captcha is not documented, the
  harness answers 428 with the same token.
- After `unlink`, a relink of the same account gets the freed device id again.
- Not covered by the mock (R:signal-desktop-receive §7.6): CDSI, SVR, `SERVER_DELIVERY_RECEIPT`
  envelopes, real push delays. Those are checked on the real server with a dedicated test
  account only, with the user's OK.
