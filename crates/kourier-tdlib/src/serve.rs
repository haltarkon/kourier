//! The serve loop the helper runs (TG-1 plan §2.3, §2.4, §2.6; decisions D3 B1, H3, M1, M3, M4).
//!
//! Order of a run:
//!
//! 1. `harden_log`: TDLib's log stream is set to empty and its verbosity to 0 through
//!    `td_execute`, both answers must be `ok`, then the fatal callback is set. Nothing else
//!    touches the engine before that, and no frame of the app is read before it: a failure is
//!    `F log-hardening-failed` and the end, without a client.
//! 2. The hello `H`, with the library's version, commit and Kourier patch tag from `getOption`.
//! 3. Three threads: the READER (this thread: fd 3 -> the engine), the RECEIVER (the ONE thread
//!    that receives from TDLib; it owns the only [`TdReceiver`]) and the WRITER (the queues ->
//!    fd 3). Answers of the reader (`K`, `X`, `F`) and the receiver's heartbeat `P` go to a
//!    CONTROL queue that never blocks and that the writer drains first; TDLib's results `U` go to
//!    a bounded queue that blocks the receiver when the app does not read (TDLib buffers
//!    internally). So the reader never waits for the writer, and no cycle of full queues can
//!    form across the two processes (D3 H3).
//! 4. The helper owns two rules the app cannot change: TDLib's log (requests that set it are
//!    refused, `F refused-log-request`) and the proxy (D3 M1): every proxy request of the app is
//!    refused (`F refused-proxy-request`); the app names a client's SOCKS5 gate once with a `G`
//!    frame, and the helper writes `addProxy` for it right before it forwards that client's
//!    `setTdlibParameters`, back to back, never waiting in between (TDLib parks the `addProxy`
//!    until the parameters are set and applies it before its first connection, `Td.cpp:1043-1069`,
//!    D3 B1); parameters for a client without a gate are refused. A client has ONE gate (a second
//!    `G` for it is a bad frame: a gate restart means a new client, fix F1). The helper checks the
//!    answer to its own `addProxy` (fix F2): an error, a proxy that is not enabled, another server
//!    or another port than it wrote closes THAT client with `F gate-proxy-failed`; the answer is
//!    passed on to the app either way. Once a good `addedProxy` is in, the helper removes every
//!    other stored proxy. The helper binary is locked down to ONE gate port (`--gate-port`,
//!    `ServeOptions::gate_port`): a `G` frame naming another port is a bad frame. Between the
//!    `addProxy` and the parameters the helper also writes `setOption` of [`FILES_OPTION`] (true; K2)
//!    and checks its answer: anything but `ok` closes THAT client with `F files-option-failed`
//!    (the answer is not passed on); the app's own `setOption` of that name is refused.
//! 5. On `Q` or the end of the stream the helper is the ONE closer (D3 M3): `close` to every
//!    client, then it waits until TDLib reports each `authorizationStateClosed` (receiving past the
//!    one NULL TDLib returns while closing), or until its deadline: `B {"forced":true}`. The whole
//!    shutdown, the joins and the bye's write included, ends inside `close_deadline +
//!    shutdown_tail` (15 s + 1.5 s) from the moment the quit is seen, so the app's SIGKILL at 17 s
//!    never meets a helper that is about to `_exit(4)` (fix F6).
//!
//! Nothing here logs or prints a line of TDLib or of the app: frames are passed on or refused by
//! fixed codes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::CString;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{
    Bye, FailureCode, FrameError, FromHelper, GATE_PROXY_EXTRA, HELPER_EXTRA_MIN, Hello,
    LineReader, ToHelper, decode_to_helper, gate_port_allowed,
};
use crate::td::{Engine, TdHandle, TdReceiver, split};

/// The `@extra` of the helper's `getProxies` after the gate's proxy is in.
const PROXIES_EXTRA: u64 = HELPER_EXTRA_MIN + 2;
/// The `@extra` of the helper's `removeProxy` of a stale proxy.
const REMOVE_EXTRA: u64 = HELPER_EXTRA_MIN + 3;
/// The `@extra` of the helper's `close`.
const CLOSE_EXTRA: u64 = HELPER_EXTRA_MIN + 4;
/// An id TDLib never hands out (its ids start at 1).
const NO_CLIENT: i32 = 0;
/// The `@extra` of the helper's `setOption` of [`FILES_OPTION`].
const FILES_OPTION_EXTRA: u64 = HELPER_EXTRA_MIN + 5;

/// The option that makes TDLib keep EVERY download in the files directory (pre3 K2, review H2).
/// Without it TDLib puts thumbnails, profile photos, stickers, stories and view-once media in the
/// DATABASE directory (`FileType.cpp:323-345`, `FileLoaderUtils.cpp:233-256`, `Global.h:132-136`),
/// where the filesystem rules give no REFER (the finished download's rename from `temp/` into its
/// directory would fail EXDEV) and nothing empties it. The helper sets it for every client itself,
/// right after the gate's `addProxy` and before it forwards `setTdlibParameters` (TDLib parks a
/// `setOption` until the parameters and applies it in `Td::init` before it reads the option,
/// `Td.cpp:873-884` then `:922-923`, read at 0e88653 (1.8.69; `:866-876` and `:914-915` at
/// a94b83d), BSL-1.0); the app's own `setOption` of this name is refused, any value.
pub const FILES_OPTION: &str = "store_all_files_in_files_directory";

/// The requests that change TDLib's log: never forwarded.
const LOG_REQUESTS: [&str; 4] = [
    "setLogStream",
    "setLogVerbosityLevel",
    "setLogTagVerbosityLevel",
    "addLogMessage",
];

/// The requests that change or use TDLib's proxies: never forwarded (D3 M1). `testProxy` makes
/// TDLib connect to any server it is given; `testNetwork` connects past the proxy rule's checks.
/// LC-20 (L1, PROTOCOL 5): `processWebProxyCarrierMessage` and `reportWebProxyCarrierError` are
/// proxy requests too, although TDLib counts them among the PRE-INITIALIZATION requests (`Td.cpp`
/// `is_preinitialization_request`, two more cases at 1.8.69: it answers them before the parameters).
/// They are the app's half of the web-proxy carrier (`proxyTypeWeb`): TDLib relays MTProto through an
/// HTTPS page the APP hosts and listens on 127.0.0.1 for it, and the carrier opens only for a web proxy
/// that is the active proxy or is pinged or tested. The refused requests above can never make one, so
/// nothing starts in Kourier; refusing these two costs nothing and closes the door should a TDLib ever
/// open a carrier on its own.
const PROXY_REQUESTS: [&str; 11] = [
    "addProxy",
    "editProxy",
    "enableProxy",
    "disableProxy",
    "removeProxy",
    "getProxies",
    "pingProxy",
    "testProxy",
    "testNetwork",
    "processWebProxyCarrierMessage",
    "reportWebProxyCarrierError",
];

/// The requests Kourier never makes: refused from `S` and `E` alike, before TDLib sees them. The helper
/// is the last wall: none of them ever reaches TDLib. The list has two parts, each DERIVED BY A STATED
/// RULE from the pinned `td_api.tl` and RE-DERIVED at every pin bump (LC-20 derived the account part
/// over the 1,076 functions of 1.8.69 and the call part over its 71 matching ones).
///
/// 1. THE ACCOUNT'S DOORS, in seven classes (his U-2 and U-3 answers: Kourier never resets or changes a
///    password and never manages the account; the reason recorded for `getLoginUrl`, that it logs the
///    account into a third-party website; the privacy review's `confirmQrCodeAuthentication`, a THIRD
///    party into his account). A class is a rule; a function the rule names is a WRITE (it changes the
///    account's state on Telegram's side, spends or moves money or assets, exports a credential or a link
///    that grants access or moves an asset, or lets a third party in: forbidden, below) or a READ (a
///    getter, a search, a local computation, a decline or cancel that only refuses a third party:
///    allowed, named in [`ACCOUNT_READS`]). The classes are C-third (OAuth, Telegram Passport, a connected
///    business bot, a QR confirmation, a website login by a button or a link), C-password (the 2FA
///    password and its recovery e-mail: set, reset, cancel, check or resend; a temporary password; the
///    wallet's secret phrase; passkeys), C-login-email, C-sessions, C-phone (the number, its codes, the
///    account's lifetime), C-account (creation, deletion, the logins of bots and web tokens) and C-money
///    (payment forms, Stars, gifts, Premium codes, the TON wallet, paid reactions). THE LOGIN'S OWN
///    REQUESTS ARE NEVER MEMBERS: `setAuthenticationPhoneNumber`, `checkAuthenticationCode`,
///    `resendAuthenticationCode`, `checkAuthenticationPassword`, `setAuthenticationEmailAddress`,
///    `checkAuthenticationEmailCode`, `requestQrCodeAuthentication`, `logOut` and `destroy` are the login
///    machine's and pass (a test sends each); the classes say "outside the login". A getter that sends
///    the 2FA password to Telegram outside the login is a WRITE (B-1, B-14: Kourier uses the password
///    only in the login). `changePhoneNumber` is not in the pinned TDLib (`sendPhoneNumberCode` replaced
///    it); it stays listed for an older or newer library. Not in either array, no class names them
///    (a note for T7 in the rules): web apps, `toggleBotCanManageEmojiStatus`, `registerDevice`,
///    suggested posts. The matcher sees the `@type` and no argument, so a spend carried in a FIELD of a
///    request Kourier makes (`paid_message_star_count` of `sendMessage` and its kin, a stake dice) is
///    the send step's to refuse (plan T5), not this list's.
/// 2. THE CALL FAMILY since PROTOCOL 4 (pre3 decision P-8, with TDLib patch 0002): every call request
///    Kourier never makes; it shows calls and never takes, starts, ends, rates or joins one (calls.md:
///    never a reply on the wire). It is COMPLETE for the pinned `td_api.tl` (pre3 K4, the security
///    review's M2): derived from EVERY function whose name matches
///    `GroupCall|VideoChat|LiveStory|Call(?!back)` (71 at 1.8.68 and at 1.8.69; `LiveStory`, a story
///    streamed through a group call, since the K4 re-check) and classified as a READ ([`CALL_READS`])
///    or a WRITE (everything else, including a function that exports a link or acts on a call only a
///    joined account has: forbidden). `toggleSessionCanAcceptCalls` matches the regex too and stands in
///    C-sessions.
///
/// A new matching function at the next bump lands in one of the lists; PROTOCOL moves with every change
/// (5 at LC-20: the account classes and the two web-proxy-carrier requests of [`PROXY_REQUESTS`]).
pub const FORBIDDEN_REQUESTS: [&str; 165] = [
    // C-third: a third party into the account or its data (OAuth, Telegram Passport, a
    //   connected business bot, a QR confirmation, a website login).
    "acceptOauthRequest",
    "checkEmailAddressVerificationCode",
    "checkOauthRequestMatchCode",
    "confirmBusinessConnectedBot",
    "confirmQrCodeAuthentication",
    "deleteBusinessConnectedBot",
    "deletePassportElement",
    "disconnectAllWebsites",
    "disconnectWebsite",
    "getAllPassportElements", // B-1: sends the 2FA password
    "getExternalLink",
    "getLoginUrl",
    "getPassportAuthorizationFormAvailableElements", // B-1: sends the 2FA password
    "getPassportElement",                            // B-1: sends the 2FA password
    "removeBusinessConnectedBotFromChat",
    "resendEmailAddressVerificationCode",
    "sendEmailAddressVerificationCode",
    "sendPassportAuthorizationForm",
    "setBusinessConnectedBot",
    "setPassportElement",
    "toggleBusinessConnectedBotChatIsPaused",
    // C-password: the 2FA password and its recovery e-mail, outside the login (set, reset,
    //   cancel, check, resend), a temporary password, the wallet secret, passkeys.
    "addLoginPasskey",
    "cancelPasswordReset",
    "cancelRecoveryEmailAddressVerification",
    "checkAuthenticationPasskey",
    "checkAuthenticationPasswordRecoveryCode",
    "checkPasswordRecoveryCode",
    "checkRecoveryEmailAddressCode",
    "createTemporaryPassword",
    "getRecoveryEmailAddress", // B-1: sends the 2FA password
    "getTonWalletSecretPhrase",
    "recoverAuthenticationPassword",
    "recoverPassword",
    "removeLoginPasskey",
    "requestAuthenticationPasswordRecovery",
    "requestPasswordRecovery",
    "resendRecoveryEmailAddressCode",
    "resetPassword",
    "setPassword",
    "setRecoveryEmailAddress",
    "transferChatOwnership", // B-14: sends the 2FA password
    // C-login-email: the LOGIN e-mail, outside the login (set, resend, check, reset).
    "checkLoginEmailAddressCode",
    "resendLoginEmailAddressCode",
    "resetAuthenticationEmailAddress",
    "setLoginEmailAddress",
    // C-sessions: sessions and their toggles.
    "confirmSession",
    "setInactiveSessionTtl",
    "terminateAllOtherSessions",
    "terminateSession",
    "toggleSessionCanAcceptCalls",
    "toggleSessionCanAcceptSecretChats",
    // C-phone: the phone number and its codes outside the login, the account's lifetime.
    "changePhoneNumber",
    "checkPhoneNumberCode",
    "reportAuthenticationCodeMissing",
    "reportPhoneNumberCodeMissing",
    "resendPhoneNumberCode",
    "sendAuthenticationFirebaseSms",
    "sendPhoneNumberCode",
    "sendPhoneNumberFirebaseSms",
    "setAccountTtl",
    // C-account: account creation and deletion, the logins of bots and web tokens.
    "checkAuthenticationBotToken",
    "checkAuthenticationWebToken",
    "deleteAccount",
    "registerUser",
    // C-money: money and assets moved, spent, created or exported (payment forms, Stars,
    //   gifts, Premium codes, the TON wallet, paid reactions).
    "addPendingPaidMessageReaction",
    "allowUnpaidMessagesFromUser", // B-8: moves Stars
    "answerTonConnectRequest",
    "applyPremiumGiftCode",
    "assignStoreTransaction",
    "buyGiftUpgrade",
    "checkAuthenticationPremiumPurchase",
    "claimTonConnectRequest",
    "commitPendingPaidMessageReactions",
    "craftGift",
    "createOnRampPaymentSession",
    "createTonConnectSession",
    "createUserTonWallet",
    "deleteGift",             // B-10: deletes a gift
    "deleteSavedCredentials", // B-6: saved payment data
    "deleteSavedOrderInfo",   // B-6: saved payment data
    "deleteTonWallet",
    "disableTonWalletBackup",
    "disableTonWalletBackupWithProof",
    "disconnectTonConnectSession",
    "dropGiftOriginalDetails",
    "editStarSubscription",
    "enableTonWalletBackup",
    "getChatRevenueWithdrawalUrl",
    "getGramWithdrawalUrl",
    "getStarAdAccountUrl",
    "getStarWithdrawalUrl",
    "getUpgradedGiftWithdrawalUrl",
    "increaseGiftAuctionBid",
    "launchPrepaidGiveaway",
    "placeGiftAuctionBid",
    "processGiftPurchaseOffer",
    "removePendingPaidMessageReactions", // B-7: a paid reaction
    "replaceTonWallet",
    "searchPublicPosts", // B-9: its star_count pays
    "sellGift",
    "sendGift",
    "sendGiftPurchaseOffer",
    "sendPaymentForm",
    "sendResoldGift",
    "sendTonCenterApiRequest",
    "sendTonConnectSessionConnectResult",
    "sendTonWalletTransfer",
    "setAuthenticationPremiumPurchaseTransaction",
    "setGiftResalePrice",
    "setPaidMessageReactionType",
    "setTonConnectSessionWalletClientId",
    "toggleDirectMessagesChatTopicCanSendUnpaidMessages", // B-8: moves Stars
    "transferGift",
    "upgradeGift",
    "validateOrderInfo",
    "withdrawUpgradedGift",
    // P-8: calls (1:1 and group), never made by Kourier.
    "createCall",
    "acceptCall",
    "discardCall",
    "sendCallSignalingData",
    "sendCallRating",
    "sendCallDebugInformation",
    "sendCallLog",
    "createVideoChat",
    "createGroupCall",
    "joinGroupCall",
    "joinVideoChat",
    "declineGroupCallInvitation",
    "inviteGroupCallParticipant",
    // K4: the other WRITE functions of the pinned td_api.tl that match the call regex. They send
    // on the wire WITHOUT a prior join (`endGroupCall`, `startScheduledVideoChat`,
    // `inviteVideoChatParticipants`, the recording and title toggles, `replaceVideoChatRtmpUrl`,
    // `banGroupCallParticipants`, `deleteAllCallMessages` ...) or act on a call only a joined
    // account has (`leaveGroupCall`, the participant toggles); Kourier never joins.
    "deleteAllCallMessages",
    "startScheduledVideoChat",
    "toggleVideoChatEnabledStartNotification",
    "setVideoChatDefaultParticipant",
    "replaceVideoChatRtmpUrl",
    "startGroupCallScreenSharing",
    "toggleGroupCallScreenSharingIsPaused",
    "endGroupCallScreenSharing",
    "setVideoChatTitle",
    "toggleVideoChatMuteNewParticipants",
    "toggleGroupCallAreMessagesAllowed",
    "sendGroupCallMessage",
    "deleteGroupCallMessages",
    "deleteGroupCallMessagesBySender",
    "banGroupCallParticipants",
    "inviteVideoChatParticipants",
    "getVideoChatInviteLink",
    "revokeGroupCallInviteLink",
    "startGroupCallRecording",
    "endGroupCallRecording",
    "toggleGroupCallIsMyVideoPaused",
    "toggleGroupCallIsMyVideoEnabled",
    "setGroupCallPaidMessageStarCount",
    "setGroupCallParticipantIsSpeaking",
    "toggleGroupCallParticipantIsMuted",
    "setGroupCallParticipantVolumeLevel",
    "toggleGroupCallParticipantIsHandRaised",
    "leaveGroupCall",
    "endGroupCall",
    // A live story is a story streamed through a group call: starting, joining or streaming one,
    // choosing who speaks in it and its paid (Stars) reactions are all writes (the K4 re-check).
    "startLiveStory",
    "joinLiveStory",
    "replaceLiveStoryRtmpUrl",
    "setLiveStoryMessageSender",
    "addPendingLiveStoryReaction",
    "commitPendingLiveStoryReactions",
    "removePendingLiveStoryReactions",
];

/// The functions of the pinned `td_api.tl` that match `GroupCall|VideoChat|LiveStory|Call(?!back)` and are
/// NOT forbidden (K4): getters and searches that change nothing on the server or in the account,
/// TDLib's local crypto helpers and its `testCall*` echoes. The list documents the classification
/// (a test pins it apart from [`FORBIDDEN_REQUESTS`]); like it, it is re-derived at every pin bump.
pub const CALL_READS: [&str; 21] = [
    "searchCallMessages",
    "getVideoChatAvailableParticipants",
    "getVideoChatRtmpUrl",
    "getGroupCall",
    "getGroupCallParticipants",
    "getGroupCallStreams",
    "getGroupCallStreamSegment",
    "loadGroupCallParticipants",
    "encryptGroupCallData",
    "decryptGroupCallData",
    "testCallEmpty",
    "testCallString",
    "testCallBytes",
    "testCallVectorInt",
    "testCallVectorIntObject",
    "testCallVectorString",
    "testCallVectorStringObject",
    "getLiveStoryRtmpUrl",
    "getLiveStoryStreamer",
    "getLiveStoryAvailableMessageSenders",
    "getLiveStoryTopDonors",
];

/// The READs of the account's classes (LC-20, [`FORBIDDEN_REQUESTS`] part 1) that are NOT forbidden:
/// getters of the account's state and balances, searches, local computations and the declines that only
/// refuse a third party (`declineOauthRequest`, `dismissWebToken`), grouped by class. The list documents
/// the classification (a test pins it by name and apart from [`FORBIDDEN_REQUESTS`], like
/// [`CALL_READS`]); like it, it is re-derived at every pin bump.
pub const ACCOUNT_READS: [&str; 78] = [
    // C-third.
    "declineOauthRequest",
    "getBusinessConnectedBot",
    "getConnectedWebsites",
    "getExternalLinkInfo",
    "getLoginUrlInfo",
    "getOauthLinkInfo",
    "getPassportAuthorizationForm",
    "getPreferredCountryLanguage",
    // C-password.
    "getAuthenticationPasskeyParameters",
    "getLoginPasskeys",
    "getPasskeyParameters",
    "getPasswordState",
    "getTemporaryPasswordState",
    // C-login-email.
    "isLoginEmailAddressRequired",
    // C-sessions.
    "getActiveSessions",
    // C-phone.
    "getAccountTtl",
    // C-account.
    "dismissWebToken",
    // C-money.
    "canPurchaseFromStore",
    "canSendGift",
    "checkPremiumGiftCode",
    "checkWalletBotBalance",
    "closeGiftAuction",
    "getAddressTonWallet",
    "getAvailableGifts",
    "getChatAvailablePaidMessageReactionSenders",
    "getChatRevenueStatistics",
    "getChatRevenueTransactions",
    "getCurrencyExchangeRates",
    "getDirectMessagesChatTopicRevenue",
    "getGiftAuctionAcquiredGifts",
    "getGiftAuctionState",
    "getGiftUpgradePreview",
    "getGiftsForCrafting",
    "getGiveawayInfo",
    "getGramRevenueStatistics",
    "getOnRampPaymentAvailability",
    "getOnRampPaymentLimits",
    "getOnRampPaymentQuote",
    "getOnRampProviderBaseCurrencies",
    "getOnRampProviders",
    "getPaidMessageRevenue",
    "getPaymentForm",
    "getPaymentReceipt",
    "getPremiumGiftPaymentOptions",
    "getPremiumGiveawayPaymentOptions",
    "getPublicPostSearchLimits",
    "getReceivedGift",
    "getReceivedGifts",
    "getSavedOrderInfo",
    "getStakeDiceState",
    "getStarGiftPaymentOptions",
    "getStarGiveawayPaymentOptions",
    "getStarPaymentOptions",
    "getStarRevenueStatistics",
    "getStarSubscriptions",
    "getStarTransactions",
    "getTonCenterStreamingApiUrl",
    "getTonConnectDAppPendingRequests",
    "getTonConnectSessionNextEventId",
    "getTonConnectSessionPendingRequests",
    "getTonConnectSessions",
    "getTonTransactions",
    "getTonWalletCardBackgroundGiftCollectionNames",
    "getTonWalletNfts",
    "getTonWalletOwnershipProofChallenge",
    "getTonWalletTransaction",
    "getTonWalletTransactionByMsgHash",
    "getTonWalletTransactions",
    "getUpgradedGift",
    "getUpgradedGiftDepositInfo",
    "getUpgradedGiftValueInfo",
    "getUpgradedGiftVariants",
    "getUpgradedGiftsPromotionalAnimation",
    "getUserTonWalletAddresses",
    "loadTonWalletGaslessTransfersInfo",
    "loadTonWalletState",
    "openGiftAuction",
    "searchGiftsForResale",
];

/// The answer `X <seq>` to a refused `E` request, so the app's waiting call ends.
const REFUSED_EXECUTE: &str = r#"{"@type":"error","code":400,"message":"Refused by the helper"}"#;

/// How a serve loop runs.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// How long the close of every client may take after `Q` or the end of the stream.
    pub close_deadline: Duration,
    /// What the rest of the shutdown (the receiver's end, the bye's write) may take after the
    /// close deadline: the helper's whole shutdown is `close_deadline + shutdown_tail` from `Q`.
    pub shutdown_tail: Duration,
    /// How many `U` frames wait for the writer before the receiver waits.
    pub out_queue: usize,
    /// The receiver writes `P` at least this often (D3 M4: at most 30 s).
    pub heartbeat: Duration,
    /// What the hello says of the helper's lock-down (`on`, `unavailable`; `off` where none ran).
    pub landlock: &'static str,
    /// The gate's port the helper was locked down to (`--gate-port`): a `G` frame naming another
    /// port is a bad frame (TG-1b B-3). `None` only for a serve loop under a test.
    pub gate_port: Option<u16>,
    /// The directories the helper was locked to (`--database-dir`, `--files-dir`; TG-2b): a
    /// `setTdlibParameters` whose `database_directory` and `files_directory` are not these, byte
    /// for byte, is refused `F refused-parameters-paths` and never reaches TDLib. `None` only for
    /// a serve loop under a test.
    pub dirs: Option<(String, String)>,
    /// TDLib's fatal path (CHECK, LOG(FATAL)): after `F tdlib-fatal` the process ends with
    /// `_exit(7)` instead of returning to TDLib's `abort` (pre3 L6, P-3: an exit code, no SIGABRT,
    /// no core handler). True only in the helper binary (and the tests' helper that proves it); a
    /// serve loop in a test process returns to TDLib as before.
    pub exit_on_fatal: bool,
}

impl ServeOptions {
    /// The options of the SHIPPED helper binary (`main.rs`) and of nothing else (K9, the security
    /// review's L3): its Landlock word, the gate's port, the two directories it was locked to, and
    /// `exit_on_fatal: true`. The test helper that proves the fatal exit builds its options through
    /// this too, so that proof is about the product's setting, not about a flag the test chose.
    pub fn helper(
        landlock: &'static str,
        gate_port: u16,
        database_dir: String,
        files_dir: String,
    ) -> Self {
        Self {
            landlock,
            gate_port: Some(gate_port),
            dirs: Some((database_dir, files_dir)),
            exit_on_fatal: true,
            ..Self::default()
        }
    }
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            close_deadline: Duration::from_secs(15),
            shutdown_tail: Duration::from_millis(1500),
            out_queue: 256,
            heartbeat: Duration::from_secs(25),
            landlock: "off",
            gate_port: None,
            dirs: None,
            exit_on_fatal: false,
        }
    }
}

/// How a serve loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServeEnd {
    /// Every client reported closed.
    AllClosed,
    /// The close deadline passed with a client still open.
    Forced,
    /// A fatal code was written (after closing what was open, where there was something).
    Fatal(FailureCode),
}

/// Serves the app on `sock` with `engine` until the app quits or the stream ends.
pub fn serve<E: Engine>(engine: E, sock: UnixStream, opts: ServeOptions) -> ServeEnd {
    let (td, rx) = split(engine);
    let Ok(mut direct) = sock.try_clone() else {
        return ServeEnd::Fatal(FailureCode::BadFrame);
    };
    let wire = Arc::new(Mutex::new(()));
    if !harden_log(&td, &direct, &wire, opts.exit_on_fatal) {
        write_now(
            &mut direct,
            &FromHelper::Failure {
                code: FailureCode::LogHardeningFailed,
                extra: None,
            },
        );
        return ServeEnd::Fatal(FailureCode::LogHardeningFailed);
    }
    if !write_now(&mut direct, &FromHelper::Hello(hello(&td, opts.landlock))) {
        return ServeEnd::Fatal(FailureCode::BadFrame);
    }

    let outbox = Arc::new(Outbox::new(opts.out_queue));
    let shared = Arc::new(Shared {
        dirs: opts.dirs.clone(),
        ..Shared::default()
    });
    let writer = {
        let outbox = Arc::clone(&outbox);
        let wire = Arc::clone(&wire);
        thread::Builder::new()
            .name("tdlib-writer".to_owned())
            .spawn(move || writer_loop(&outbox, direct, &wire))
    };
    let receiver = {
        let (td, shared, outbox) = (td.clone(), Arc::clone(&shared), Arc::clone(&outbox));
        let beat = opts.heartbeat;
        thread::Builder::new()
            .name("tdlib-receiver".to_owned())
            .spawn(move || receiver_loop(rx, &td, &shared, &outbox, beat))
    };
    let (Ok(writer), Ok(receiver)) = (writer, receiver) else {
        return ServeEnd::Fatal(FailureCode::BadFrame);
    };

    let bad = reader_loop(&td, &sock, &shared, &outbox, opts.gate_port).is_err();
    // t0 of the shutdown timeline (D3 M3): the close, the joins and the bye all end by `budget_end`.
    let close_end = Instant::now() + opts.close_deadline;
    let budget_end = close_end + opts.shutdown_tail;
    if bad {
        outbox.control(&FromHelper::Failure {
            code: FailureCode::BadFrame,
            extra: None,
        });
    }
    let all_closed = close_all(&td, &shared, close_end);
    shared.stop.store(true, Ordering::SeqCst);
    outbox.abandon();
    // The receiver waits in `td_receive` until its next heartbeat is due. TDLib answers a request
    // to a client that does not exist at once (`Client.cpp:493-496`), and that ends the wait; a
    // receiver stuck longer is left to `_exit`.
    let wake = json!({"@type": "getOption", "name": "version", "@extra": CLOSE_EXTRA}).to_string();
    let _ = send(&td, NO_CLIENT, &wake);
    join_before(receiver, close_end + opts.shutdown_tail / 2);
    let closed = lock(&shared.clients).closed.clone();
    outbox.finish(&FromHelper::Bye(Bye {
        closed,
        forced: !all_closed,
    }));
    // The writer ends once the bye is out; an app that stopped reading cannot hold the end past the
    // budget.
    let grace = WRITER_GRACE.min(budget_end.saturating_duration_since(Instant::now()));
    if outbox.wait_done(grace) {
        let _ = writer.join();
    }
    match (bad, all_closed) {
        (true, _) => ServeEnd::Fatal(FailureCode::BadFrame),
        (false, true) => ServeEnd::AllClosed,
        (false, false) => ServeEnd::Forced,
    }
}

/// How long the end waits for the writer to put the bye out (never past the shutdown budget).
const WRITER_GRACE: Duration = Duration::from_secs(2);

/// Joins `handle` if it ends before `limit`; otherwise leaves it (the process is about to `_exit`).
fn join_before(handle: thread::JoinHandle<()>, limit: Instant) {
    while !handle.is_finished() && Instant::now() < limit {
        thread::sleep(Duration::from_millis(5));
    }
    if handle.is_finished() {
        let _ = handle.join();
    }
}

/// TDLib's log off, before anything else (plan §2.6): the stream empty, the verbosity 0, both
/// answered `ok`; then the fatal callback, which writes `F tdlib-fatal` and nothing else.
fn harden_log<E: Engine>(
    td: &TdHandle<E>,
    sock: &UnixStream,
    wire: &Arc<Mutex<()>>,
    exit_on_fatal: bool,
) -> bool {
    let calls = [
        json!({"@type": "setLogStream", "log_stream": {"@type": "logStreamEmpty"}}),
        json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 0}),
    ];
    for call in calls {
        if !is_ok(execute(td, &call).as_deref()) {
            return false;
        }
    }
    let Ok(fatal_sock) = sock.try_clone() else {
        return false;
    };
    let wire = Arc::clone(wire);
    td.set_fatal_callback(Box::new(move || {
        fatal_frame(&fatal_sock, &wire);
        if exit_on_fatal {
            // SAFETY: `_exit` ends the process at once (no destructor, no `abort`).
            unsafe { libc::_exit(FATAL_EXIT) }
        }
    }));
    true
}

/// The helper's exit code after `F tdlib-fatal` ([`ServeOptions::exit_on_fatal`]).
pub const FATAL_EXIT: i32 = 7;

/// One write that never blocks (`MSG_DONTWAIT`, and no SIGPIPE): the fatal path must not wait for
/// an app that does not read (P-3); what does not fit is lost, and the exit tells the rest.
fn send_now(sock: &UnixStream, bytes: &[u8]) {
    use std::os::fd::AsRawFd as _;
    // SAFETY: send on this process's socket with a buffer that outlives the call.
    unsafe {
        libc::send(
            sock.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        );
    }
}

/// The fatal callback's body: TDLib aborts right after it returns (unless the helper exits
/// first). It waits at most 200 ms for the writer to finish its current frame; when the writer is
/// stuck (the app does not read) it writes anyway behind a newline, so the app sees a broken line
/// and then the end. Neither write blocks.
fn fatal_frame(sock: &UnixStream, wire: &Mutex<()>) {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match wire.try_lock() {
            Ok(_guard) => {
                send_now(sock, b"F tdlib-fatal\n");
                return;
            }
            Err(std::sync::TryLockError::Poisoned(_)) => break,
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::TryLockError::WouldBlock) => break,
        }
    }
    send_now(sock, b"\nF tdlib-fatal\n");
}

fn execute<E: Engine>(td: &TdHandle<E>, v: &Value) -> Option<String> {
    let c = CString::new(v.to_string()).ok()?;
    td.execute(&c)
}

fn is_ok(answer: Option<&str>) -> bool {
    #[derive(Deserialize)]
    struct Ty {
        #[serde(rename = "@type")]
        ty: String,
    }
    answer
        .and_then(|a| serde_json::from_str::<Ty>(a).ok())
        .is_some_and(|t| t.ty == "ok")
}

/// The hello: the protocol and what the library says it is (`getOption` through `td_execute`;
/// `kourier_patch_level` is the run-time marker of patch 0002, empty in a library without it).
fn hello<E: Engine>(td: &TdHandle<E>, landlock: &str) -> Hello {
    let option = |name: &str| -> String {
        #[derive(Deserialize)]
        struct Opt {
            #[serde(rename = "@type")]
            ty: String,
            #[serde(default)]
            value: Option<String>,
        }
        execute(td, &json!({"@type": "getOption", "name": name}))
            .and_then(|a| serde_json::from_str::<Opt>(&a).ok())
            .filter(|o| o.ty == "optionValueString")
            .and_then(|o| o.value)
            .unwrap_or_default()
    };
    Hello {
        protocol: crate::PROTOCOL,
        tdlib: option("version"),
        commit: option("commit_hash"),
        patch_level: option("kourier_patch_level"),
        landlock: landlock.to_owned(),
    }
}

/// Writes one frame at once (before the writer thread exists).
fn write_now(sock: &mut UnixStream, f: &FromHelper) -> bool {
    let mut buf = Vec::new();
    f.encode(&mut buf).is_ok() && sock.write_all(&buf).is_ok()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether a request type changes TDLib's log.
fn is_log_request(ty: &str) -> bool {
    LOG_REQUESTS.contains(&ty)
}

/// Whether a request type changes or uses TDLib's proxies.
fn is_proxy_request(ty: &str) -> bool {
    PROXY_REQUESTS.contains(&ty)
}

/// Whether a request type is one Kourier never makes ([`FORBIDDEN_REQUESTS`]).
pub fn is_forbidden_request(ty: &str) -> bool {
    FORBIDDEN_REQUESTS.contains(&ty)
}

// ---- the queues of the writer ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Queue {
    /// Never bounded: the reader and the heartbeat must never wait (D3 H3).
    Control,
    /// Bounded by [`ServeOptions::out_queue`].
    Updates,
}

#[derive(Default)]
struct OutState {
    control: VecDeque<Vec<u8>>,
    updates: VecDeque<Vec<u8>>,
    last: Option<Vec<u8>>,
    finishing: bool,
    /// The receiver no longer waits for room (the end): a full queue drops what comes.
    abandoned: bool,
    /// The socket is gone: everything is dropped.
    dead: bool,
    done: bool,
}

struct Outbox {
    st: Mutex<OutState>,
    /// The writer waits here for frames.
    ready: Condvar,
    /// The receiver waits here for room; [`Outbox::wait_done`] for the writer's end.
    space: Condvar,
    cap: usize,
}

impl Outbox {
    fn new(cap: usize) -> Self {
        Self {
            st: Mutex::new(OutState::default()),
            ready: Condvar::new(),
            space: Condvar::new(),
            cap: cap.max(1),
        }
    }

    /// A control frame (`K`, `X`, `F`, `P`): never waits.
    fn control(&self, f: &FromHelper) {
        let mut buf = Vec::new();
        if f.encode(&mut buf).is_ok() {
            self.enqueue(buf, Queue::Control);
        }
    }

    /// One TDLib result: waits while the queue of results is full.
    fn update(&self, line: String) {
        let mut buf = Vec::new();
        // A line TDLib never writes (a newline, over the limit) is dropped, not passed on broken.
        if FromHelper::Update(line).encode(&mut buf).is_ok() {
            self.enqueue(buf, Queue::Updates);
        }
    }

    fn enqueue(&self, buf: Vec<u8>, queue: Queue) {
        let mut st = lock(&self.st);
        if queue == Queue::Updates {
            while st.updates.len() >= self.cap && !st.dead && !st.abandoned {
                st = self.space.wait(st).unwrap_or_else(|p| p.into_inner());
            }
            if st.abandoned && st.updates.len() >= self.cap {
                return;
            }
        }
        if st.dead {
            return;
        }
        match queue {
            Queue::Control => st.control.push_back(buf),
            Queue::Updates => st.updates.push_back(buf),
        }
        drop(st);
        self.ready.notify_one();
    }

    fn abandon(&self) {
        lock(&self.st).abandoned = true;
        self.space.notify_all();
    }

    /// The last frame: written after everything queued.
    fn finish(&self, bye: &FromHelper) {
        let mut buf = Vec::new();
        let _ = bye.encode(&mut buf);
        let mut st = lock(&self.st);
        st.last = Some(buf);
        st.finishing = true;
        drop(st);
        self.ready.notify_one();
    }

    fn wait_done(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.st);
        while !st.done {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .space
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }
}

/// Drains the control queue first, then up to 64 results per write.
fn writer_loop(outbox: &Outbox, mut sock: UnixStream, wire: &Mutex<()>) {
    loop {
        let (batch, last) = {
            let mut st = lock(&outbox.st);
            while st.control.is_empty() && st.updates.is_empty() && !st.finishing {
                st = outbox.ready.wait(st).unwrap_or_else(|p| p.into_inner());
            }
            let mut buf = Vec::new();
            while let Some(c) = st.control.pop_front() {
                buf.extend_from_slice(&c);
            }
            for _ in 0..64 {
                match st.updates.pop_front() {
                    Some(u) => buf.extend_from_slice(&u),
                    None => break,
                }
            }
            let last = st.control.is_empty() && st.updates.is_empty() && st.finishing;
            if last && let Some(b) = st.last.take() {
                buf.extend_from_slice(&b);
            }
            drop(st);
            outbox.space.notify_all();
            (buf, last)
        };
        let ok = {
            let _w = lock(wire);
            batch.is_empty() || sock.write_all(&batch).is_ok()
        };
        if !ok {
            lock(&outbox.st).dead = true;
            break;
        }
        if last {
            let _ = sock.flush();
            break;
        }
    }
    lock(&outbox.st).done = true;
    outbox.space.notify_all();
}

// ---- the clients ----

struct GateArgs {
    port: u16,
    user: String,
    pass: String,
}

#[derive(Default)]
struct Clients {
    /// Created and not yet reported closed.
    open: BTreeSet<i32>,
    /// Reported closed, in order.
    closed: Vec<i32>,
    gates: BTreeMap<i32, GateArgs>,
    /// Clients whose `addProxy` was written.
    proxied: BTreeSet<i32>,
    /// The id TDLib gave each client's gate proxy.
    gate_proxy: BTreeMap<i32, i64>,
}

#[derive(Default)]
struct Shared {
    clients: Mutex<Clients>,
    changed: Condvar,
    stop: AtomicBool,
    /// [`ServeOptions::dirs`].
    dirs: Option<(String, String)>,
}

/// What the helper reads of a request of the app: its type and `@extra`, nothing else.
#[derive(Deserialize)]
struct RequestProbe {
    #[serde(rename = "@type")]
    ty: String,
    #[serde(rename = "@extra", default)]
    extra: Option<u64>,
}

/// What the helper reads of a TDLib result (a serde probe, never a substring: plan §2.3).
#[derive(Deserialize)]
struct ResultProbe {
    #[serde(rename = "@type", default)]
    ty: Option<String>,
    #[serde(rename = "@client_id", default)]
    client: Option<i32>,
    #[serde(rename = "@extra", default)]
    extra: Option<Value>,
    #[serde(default)]
    authorization_state: Option<Value>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    is_enabled: Option<Value>,
    #[serde(default)]
    proxies: Option<Value>,
    #[serde(default)]
    proxy: Option<Value>,
}

/// The client a TDLib result reports closed: ONLY `updateAuthorizationState` with
/// `authorizationStateClosed` counts (a message that holds the word does not).
fn closed_client(p: &ResultProbe) -> Option<i32> {
    let state = p.authorization_state.as_ref()?.get("@type")?.as_str()?;
    (p.ty.as_deref() == Some("updateAuthorizationState") && state == "authorizationStateClosed")
        .then_some(p.client)
        .flatten()
}

/// Reads the app's frames until `Q` or the end of the stream; `Err` for a bad frame.
fn reader_loop<E: Engine>(
    td: &TdHandle<E>,
    sock: &UnixStream,
    shared: &Shared,
    outbox: &Outbox,
    gate_port: Option<u16>,
) -> Result<(), ()> {
    let mut reader = LineReader::new(sock);
    loop {
        let line = match reader.next_line() {
            Ok(Some(l)) => l,
            // The end of the stream (an app that ended or crashed, also inside a line).
            Ok(None) | Err(FrameError::Truncated | FrameError::Io(_)) => return Ok(()),
            Err(_) => return Err(()),
        };
        match decode_to_helper(&line).map_err(|_| ())? {
            ToHelper::Quit => return Ok(()),
            // The lock-down lets TCP reach only the gate's port: a gate elsewhere is a bad frame.
            // A gate on a data-centre or privileged port is a bad frame too (G6).
            ToHelper::Gate { port, .. }
                if !gate_port_allowed(port) || gate_port.is_some_and(|p| p != port) =>
            {
                return Err(());
            }
            frame => dispatch(td, shared, outbox, frame)?,
        }
    }
}

fn dispatch<E: Engine>(
    td: &TdHandle<E>,
    shared: &Shared,
    outbox: &Outbox,
    frame: ToHelper,
) -> Result<(), ()> {
    match frame {
        ToHelper::Create { seq } => {
            let client = td.create_client();
            lock(&shared.clients).open.insert(client);
            outbox.control(&FromHelper::Created { seq, client });
        }
        ToHelper::Gate {
            client,
            port,
            user,
            pass,
        } => {
            let mut clients = lock(&shared.clients);
            // One gate per client (fix F1): a second `G` would change what a parked `addProxy`
            // dials, so it fails closed as a bad frame.
            if clients.gates.contains_key(&client) {
                return Err(());
            }
            clients.gates.insert(client, GateArgs { port, user, pass });
        }
        ToHelper::Send { client, json } => {
            let probe = probe_request(&json)?;
            let refuse = |code| {
                outbox.control(&FromHelper::Failure {
                    code,
                    extra: probe.extra,
                })
            };
            if is_log_request(&probe.ty) {
                refuse(FailureCode::RefusedLogRequest);
            } else if is_proxy_request(&probe.ty) {
                refuse(FailureCode::RefusedProxyRequest);
            } else if is_forbidden_request(&probe.ty) || sets_files_option(&probe.ty, &json) {
                refuse(FailureCode::RefusedRequest);
            } else if probe.ty == "setTdlibParameters"
                && !shared
                    .dirs
                    .as_ref()
                    .is_none_or(|dirs| parameters_name_dirs(&json, dirs))
            {
                refuse(FailureCode::RefusedParametersPaths);
            } else if probe.ty == "setTdlibParameters" {
                let mut clients = lock(&shared.clients);
                if clients.proxied.contains(&client) {
                    drop(clients);
                    send(td, client, &json)?;
                } else if let Some(gate) = clients.gates.get(&client) {
                    let proxy = gate_proxy(gate);
                    clients.proxied.insert(client);
                    drop(clients);
                    // Back to back, never waiting in between (D3 B1): the gate's proxy, the files
                    // option (K2), the parameters. TDLib parks the first two until the parameters
                    // and applies both before it connects or reads the option.
                    send(td, client, &proxy)?;
                    send(td, client, &files_option())?;
                    send(td, client, &json)?;
                } else {
                    drop(clients);
                    refuse(FailureCode::RefusedParametersWithoutGate);
                }
            } else {
                send(td, client, &json)?;
            }
        }
        ToHelper::Execute { seq, json } => {
            let probe = probe_request(&json)?;
            let refused = if is_log_request(&probe.ty) {
                Some(FailureCode::RefusedLogRequest)
            } else if is_proxy_request(&probe.ty) {
                Some(FailureCode::RefusedProxyRequest)
            } else if is_forbidden_request(&probe.ty) || sets_files_option(&probe.ty, &json) {
                Some(FailureCode::RefusedRequest)
            } else {
                None
            };
            let answer = match refused {
                Some(code) => {
                    outbox.control(&FromHelper::Failure { code, extra: None });
                    REFUSED_EXECUTE.to_owned()
                }
                None => {
                    let c = CString::new(json).map_err(|_| ())?;
                    td.execute(&c).unwrap_or_else(|| "null".to_owned())
                }
            };
            outbox.control(&FromHelper::Executed { seq, json: answer });
        }
        ToHelper::Quit => {}
    }
    Ok(())
}

/// The two directories of a `setTdlibParameters`, decoded (a JSON escape in a path is compared as
/// the string it stands for; critic L-a). Nothing else of the request is kept: the key and the
/// API pair in the same object are skipped unread.
#[derive(Deserialize)]
struct ParameterDirs<'a> {
    #[serde(borrow, default)]
    database_directory: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    files_directory: Option<std::borrow::Cow<'a, str>>,
}

/// Whether the parameters name exactly the helper's two directories (both present, byte equal).
fn parameters_name_dirs(json: &str, (database, files): &(String, String)) -> bool {
    serde_json::from_str::<ParameterDirs<'_>>(json).is_ok_and(|p| {
        p.database_directory.as_deref() == Some(database.as_str())
            && p.files_directory.as_deref() == Some(files.as_str())
    })
}

/// The one field of a `setOption` the helper reads: the option's name, decoded (an escape in the
/// name is compared as the string it stands for, like the directories').
#[derive(Deserialize)]
struct OptionName<'a> {
    #[serde(borrow, default)]
    name: Option<std::borrow::Cow<'a, str>>,
}

/// Whether the request is a `setOption` of [`FILES_OPTION`], any value (K2). A `setOption` whose
/// name the probe cannot read as one string (a repeated `name` key, a number) counts as one too:
/// serde refuses a repeated key, TDLib's own reader might keep the last, and a refusal costs the
/// app nothing it ever sends.
fn sets_files_option(ty: &str, json: &str) -> bool {
    ty == "setOption"
        && serde_json::from_str::<OptionName<'_>>(json)
            .map_or(true, |o| o.name.as_deref() == Some(FILES_OPTION))
}

/// The helper's own `setOption` of [`FILES_OPTION`] to true.
fn files_option() -> String {
    json!({
        "@type": "setOption",
        "name": FILES_OPTION,
        "value": {"@type": "optionValueBoolean", "value": true},
        "@extra": FILES_OPTION_EXTRA,
    })
    .to_string()
}

/// The type and `@extra` of a request; a request that is no JSON object with a string `@type`,
/// or that carries one of the helper's own `@extra` values, is a bad frame.
fn probe_request(json: &str) -> Result<RequestProbe, ()> {
    let p: RequestProbe = serde_json::from_str(json).map_err(|_| ())?;
    if p.extra.is_some_and(|e| e >= HELPER_EXTRA_MIN) {
        return Err(());
    }
    Ok(p)
}

fn send<E: Engine>(td: &TdHandle<E>, client: i32, json: &str) -> Result<(), ()> {
    let c = CString::new(json).map_err(|_| ())?;
    td.send(client, &c);
    Ok(())
}

/// The `addProxy` of a client's gate: SOCKS5 on IPv4 loopback, enabled.
fn gate_proxy(g: &GateArgs) -> String {
    json!({
        "@type": "addProxy",
        "proxy": {
            "@type": "proxy",
            "server": "127.0.0.1",
            "port": g.port,
            "type": {"@type": "proxyTypeSocks5", "username": g.user, "password": g.pass},
        },
        "enable": true,
        "comment": "",
        "@extra": GATE_PROXY_EXTRA,
    })
    .to_string()
}

/// The ONE receiving thread: passes every result on, notes closed clients, answers the proxy
/// clean-up, and writes a heartbeat.
fn receiver_loop<E: Engine>(
    mut rx: TdReceiver<E>,
    td: &TdHandle<E>,
    shared: &Shared,
    outbox: &Outbox,
    heartbeat: Duration,
) {
    let mut beat = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        // `td_receive` wakes for a result, so it waits until the next heartbeat is due: a shorter
        // wait would only wake an idle helper (2 of its 2.7 wake-ups a second at 0.5 s, TP).
        let timeout = heartbeat.saturating_sub(beat.elapsed()).as_secs_f64();
        // `None` is a timeout or TDLib's one NULL while a client closes: keep receiving past it.
        if let Some(line) = rx.receive(timeout) {
            on_result(td, shared, outbox, line);
        }
        if beat.elapsed() >= heartbeat {
            outbox.control(&FromHelper::Heartbeat);
            beat = Instant::now();
        }
    }
}

fn on_result<E: Engine>(td: &TdHandle<E>, shared: &Shared, outbox: &Outbox, line: String) {
    let Ok(p) = serde_json::from_str::<ResultProbe>(&line) else {
        outbox.update(line);
        return;
    };
    let extra = p.extra.as_ref().and_then(Value::as_u64);
    match (extra, p.client) {
        (Some(PROXIES_EXTRA), Some(client)) => {
            remove_stale_proxies(td, shared, client, &p);
            return;
        }
        (Some(REMOVE_EXTRA | CLOSE_EXTRA), _) => return,
        // K2: the answer to the helper's own `setOption`. Never passed on (an error's text is not
        // the app's); anything but `ok` means this client would put downloads in `tdlib/`, where the
        // rules give no REFER: it is closed, typed (like the gate's proxy, F2).
        (Some(FILES_OPTION_EXTRA), client) => {
            if p.ty.as_deref() != Some("ok")
                && let Some(client) = client
            {
                outbox.control(&FromHelper::Failure {
                    code: FailureCode::FilesOptionFailed,
                    extra: None,
                });
                let _ = send(
                    td,
                    client,
                    &json!({"@type": "close", "@extra": CLOSE_EXTRA}).to_string(),
                );
            }
            return;
        }
        (Some(GATE_PROXY_EXTRA), Some(client)) => {
            let port = lock(&shared.clients).gates.get(&client).map(|g| g.port);
            match port.and_then(|port| gate_proxy_id(&p, port)) {
                Some(id) => {
                    lock(&shared.clients).gate_proxy.insert(client, id);
                    let _ = send(
                        td,
                        client,
                        &json!({"@type": "getProxies", "@extra": PROXIES_EXTRA}).to_string(),
                    );
                }
                // Not the proxy the helper wrote (fix F2): this client must not run on without the
                // gate. The app still sees the answer (passed on below).
                None => {
                    outbox.control(&FromHelper::Failure {
                        code: FailureCode::GateProxyFailed,
                        extra: None,
                    });
                    let _ = send(
                        td,
                        client,
                        &json!({"@type": "close", "@extra": CLOSE_EXTRA}).to_string(),
                    );
                }
            }
        }
        _ => {}
    }
    if let Some(client) = closed_client(&p) {
        let mut clients = lock(&shared.clients);
        if clients.open.remove(&client) {
            clients.closed.push(client);
        }
        drop(clients);
        shared.changed.notify_all();
    }
    outbox.update(line);
}

/// The id TDLib gave the gate's proxy, when its answer is exactly what the helper wrote: an
/// `addedProxy` that is enabled, SOCKS5, on 127.0.0.1 and `port` (fix F2). A serde probe, never a
/// substring; the answer's other fields are not looked at and never kept.
fn gate_proxy_id(p: &ResultProbe, port: u16) -> Option<i64> {
    let proxy = p.proxy.as_ref()?;
    let right = p.ty.as_deref() == Some("addedProxy")
        && p.is_enabled.as_ref().and_then(Value::as_bool) == Some(true)
        && proxy.get("server").and_then(Value::as_str) == Some("127.0.0.1")
        && proxy.get("port").and_then(Value::as_i64) == Some(i64::from(port))
        && proxy
            .get("type")
            .and_then(|t| t.get("@type"))
            .and_then(Value::as_str)
            == Some("proxyTypeSocks5");
    if right {
        p.id.as_ref().and_then(Value::as_i64)
    } else {
        None
    }
}

/// Removes every stored proxy but the client's gate (TDLib keeps each start's proxy otherwise).
fn remove_stale_proxies<E: Engine>(
    td: &TdHandle<E>,
    shared: &Shared,
    client: i32,
    p: &ResultProbe,
) {
    let Some(keep) = lock(&shared.clients).gate_proxy.get(&client).copied() else {
        return;
    };
    let ids = p
        .proxies
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.get("id").and_then(Value::as_i64));
    for id in ids.filter(|&id| id != keep) {
        let _ = send(
            td,
            client,
            &json!({"@type": "removeProxy", "proxy_id": id, "@extra": REMOVE_EXTRA}).to_string(),
        );
    }
}

/// `close` to every open client, then waits for each Closed up to the deadline; whether all closed.
fn close_all<E: Engine>(td: &TdHandle<E>, shared: &Shared, until: Instant) -> bool {
    let open: Vec<i32> = lock(&shared.clients).open.iter().copied().collect();
    let close = json!({"@type": "close", "@extra": CLOSE_EXTRA}).to_string();
    for client in open {
        let _ = send(td, client, &close);
    }
    let mut clients = lock(&shared.clients);
    while !clients.open.is_empty() {
        let now = Instant::now();
        if now >= until {
            return false;
        }
        clients = shared
            .changed
            .wait_timeout(clients, until - now)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(v: Value) -> ResultProbe {
        serde_json::from_value(v).expect("a probe")
    }

    fn answer(ty: &str, enabled: bool, server: &str, port: i64, kind: &str) -> ResultProbe {
        probe(json!({
            "@type": ty, "id": 7, "is_enabled": enabled,
            "proxy": {"server": server, "port": port, "type": {"@type": kind}},
        }))
    }

    /// Fix F2: only the answer that is exactly the proxy the helper wrote counts, each field on its
    /// own.
    #[test]
    fn only_the_proxy_the_helper_wrote_is_the_gate() {
        let good = answer("addedProxy", true, "127.0.0.1", 40001, "proxyTypeSocks5");
        assert_eq!(gate_proxy_id(&good, 40001), Some(7));
        assert_eq!(gate_proxy_id(&good, 40002), None, "another port");
        for (what, bad) in [
            (
                "an error",
                answer("error", true, "127.0.0.1", 40001, "proxyTypeSocks5"),
            ),
            (
                "not enabled",
                answer("addedProxy", false, "127.0.0.1", 40001, "proxyTypeSocks5"),
            ),
            (
                "another server",
                answer("addedProxy", true, "10.0.0.1", 40001, "proxyTypeSocks5"),
            ),
            (
                "another type",
                answer("addedProxy", true, "127.0.0.1", 40001, "proxyTypeHttp"),
            ),
            (
                "another port",
                answer("addedProxy", true, "127.0.0.1", 40009, "proxyTypeSocks5"),
            ),
        ] {
            assert_eq!(
                gate_proxy_id(&bad, 40001),
                None,
                "{what} counted as the gate"
            );
        }
        let no_proxy = probe(json!({"@type": "addedProxy", "id": 7, "is_enabled": true}));
        assert_eq!(
            gate_proxy_id(&no_proxy, 40001),
            None,
            "an answer without a proxy"
        );
        let no_id = probe(json!({
            "@type": "addedProxy", "is_enabled": true,
            "proxy": {"server": "127.0.0.1", "port": 40001, "type": {"@type": "proxyTypeSocks5"}},
        }));
        assert_eq!(
            gate_proxy_id(&no_id, 40001),
            None,
            "an answer without an id"
        );
    }
}
