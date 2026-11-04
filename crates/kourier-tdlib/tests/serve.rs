//! The serve loop over the scripted TDLib (TG-1 plan §5.4 T-a2..T-a7, T-a8's serve half, T-a10;
//! decisions D3 B1, H3, M1, M2, M3, M4).

use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kourier_tdlib::protocol::{
    Bye, FailureCode, FromHelper, GATE_PROXY_EXTRA, HELPER_EXTRA_MIN, LineReader, ToHelper,
    decode_from_helper,
};
use kourier_tdlib::scripted::{Call, Rule, Script, ScriptedTd};
use kourier_tdlib::serve::{
    ACCOUNT_READS, CALL_READS, FORBIDDEN_REQUESTS, ServeEnd, ServeOptions, is_forbidden_request,
    serve,
};
use serde_json::{Value, json};

/// The app's side of one serve loop on a thread.
struct Session {
    app: UnixStream,
    lines: LineReader<UnixStream>,
    end: mpsc::Receiver<ServeEnd>,
}

fn start(td: &ScriptedTd, opts: ServeOptions) -> Session {
    start_after(td, opts, &[])
}

/// Like [`start`], with these frames already in the socket before the serve loop runs: a loop that
/// ends at once (a failed hardening) cannot close its end under the app's write.
fn start_after(td: &ScriptedTd, opts: ServeOptions, frames: &[ToHelper]) -> Session {
    let mut bytes = Vec::new();
    for frame in frames {
        frame.encode(&mut bytes).expect("encode");
    }
    start_raw(td, opts, &bytes)
}

/// Like [`start_after`], with these bytes (lines a peer wrote without the encoder).
fn start_raw(td: &ScriptedTd, opts: ServeOptions, bytes: &[u8]) -> Session {
    use std::io::Write;
    let (app, helper) = UnixStream::pair().expect("socketpair");
    (&app)
        .write_all(bytes)
        .expect("a small frame fits the socket");
    app.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    app.set_write_timeout(Some(Duration::from_secs(30)))
        .expect("write timeout");
    let engine = td.clone();
    let (tx, end) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(serve(engine, helper, opts));
    });
    let lines = LineReader::new(app.try_clone().expect("clone"));
    Session { app, lines, end }
}

fn opts() -> ServeOptions {
    ServeOptions {
        close_deadline: Duration::from_secs(3),
        ..ServeOptions::default()
    }
}

impl Session {
    fn write(&mut self, f: ToHelper) {
        use std::io::Write;
        let mut buf = Vec::new();
        f.encode(&mut buf).expect("encode");
        if self.app.write_all(&buf).is_err() {
            panic!("the helper stopped reading the app's frames");
        }
    }

    fn send(&mut self, client: i32, v: Value) {
        self.write(ToHelper::Send {
            client,
            json: v.to_string(),
        });
    }

    fn send_raw(&mut self, client: i32, json: &str) {
        self.write(ToHelper::Send {
            client,
            json: json.to_owned(),
        });
    }

    /// The next frame; heartbeats are skipped.
    fn next(&mut self) -> FromHelper {
        loop {
            let line = match self.lines.next_line() {
                Ok(Some(l)) => l,
                Ok(None) => panic!("the helper closed its end"),
                Err(_) => panic!("no frame from the helper in time"),
            };
            match decode_from_helper(&line) {
                Ok(FromHelper::Heartbeat) => continue,
                Ok(f) => return f,
                Err(_) => panic!("the helper wrote a line that is no frame"),
            }
        }
    }

    fn update(&mut self) -> Value {
        match self.next() {
            FromHelper::Update(j) => serde_json::from_str(&j).expect("U is JSON"),
            other => panic!("expected an update, got {other:?}"),
        }
    }

    /// Reads updates until one has this `@type` (and, for a state, this state).
    fn until(&mut self, ty: &str) -> (Vec<Value>, Value) {
        let mut seen = Vec::new();
        loop {
            let v = self.update();
            if label(&v) == ty {
                return (seen, v);
            }
            seen.push(v);
        }
    }

    fn create(&mut self, seq: u64) -> i32 {
        self.write(ToHelper::Create { seq });
        match self.next() {
            FromHelper::Created { seq: s, client } if s == seq => client,
            other => panic!("expected K {seq}, got {other:?}"),
        }
    }

    /// `Q`, then every frame up to the bye, and how the loop ended.
    fn quit(mut self) -> (Vec<FromHelper>, Bye, ServeEnd) {
        self.write(ToHelper::Quit);
        let mut before = Vec::new();
        let bye = loop {
            match self.next() {
                FromHelper::Bye(b) => break b,
                f => before.push(f),
            }
        };
        let end = self
            .end
            .recv_timeout(Duration::from_secs(10))
            .expect("serve did not return after its bye");
        (before, bye, end)
    }
}

fn label(v: &Value) -> String {
    let t = v["@type"].as_str().unwrap_or_default();
    match v["authorization_state"]["@type"].as_str() {
        Some(s) => format!("{t}:{s}"),
        None => t.to_owned(),
    }
}

const PHONE: &str = "updateAuthorizationState:authorizationStateWaitPhoneNumber";

/// M2: the app writes a `C` and an `S` BEFORE it reads the hello, so a loop that read frames before
/// its log hardening would show a client before the hardening.
#[test]
fn the_log_is_hardened_before_the_hello_and_before_any_client() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.write(ToHelper::Create { seq: 1 });
    s.send(1, json!({"@type": "getAuthorizationState", "@extra": 1}));
    assert!(
        matches!(s.next(), FromHelper::Hello(_)),
        "the first frame is not the hello"
    );
    assert!(matches!(
        s.next(),
        FromHelper::Created { seq: 1, client: 1 }
    ));
    let calls = td.calls();
    assert_eq!(
        calls[..3],
        [
            Call::Execute("setLogStream".into()),
            Call::Execute("setLogVerbosityLevel".into()),
            Call::SetFatalCallback,
        ],
        "the log is not hardened first"
    );
    let first_client = calls
        .iter()
        .position(|c| matches!(c, Call::Create | Call::Send { .. }))
        .expect("a client");
    let last_option = calls
        .iter()
        .rposition(|c| *c == Call::Execute("getOption".into()))
        .expect("the hello's getOption");
    assert!(
        first_client > last_option,
        "a client was made before the hello was ready"
    );
    let _ = s.quit();
}

/// K5: the hello carries the library's Kourier patch tag, read with `getOption` through
/// `td_execute` like the version; a library that does not know the option (stock, 0001-only,
/// pre-0002) says the empty tag, which the app's `check_pin` refuses.
#[test]
fn the_hello_carries_the_patch_tag_of_the_library() {
    for (script, want) in [
        (Script::default(), "0002"),
        (
            Script {
                patch_level: String::new(),
                ..Script::default()
            },
            "",
        ),
        (
            Script {
                patch_level: "0001".to_owned(),
                ..Script::default()
            },
            "0001",
        ),
    ] {
        let td = ScriptedTd::new(script);
        let mut s = start(&td, opts());
        let FromHelper::Hello(hello) = s.next() else {
            panic!("the first frame is not the hello");
        };
        assert_eq!(hello.patch_level, want, "the hello's patch tag");
        assert_eq!(hello.tdlib, Script::default().version);
        let _ = s.quit();
    }
}

/// A log hardening that TDLib does not answer `ok` ends the helper with its fixed code and no
/// client, whatever the app wrote.
#[test]
fn a_failed_log_hardening_ends_before_any_client() {
    let td = ScriptedTd::new(Script {
        execute_ok: vec!["setLogStream".to_owned()],
        ..Script::default()
    });
    // The app's `C` is already in the socket when the helper starts (it cannot be written under a
    // helper that has ended: that was a race of this test).
    let mut s = start_after(&td, opts(), &[ToHelper::Create { seq: 1 }]);
    match s.next() {
        FromHelper::Failure {
            code: FailureCode::LogHardeningFailed,
            extra: None,
        } => {}
        other => panic!("expected F log-hardening-failed, got {other:?}"),
    }
    assert_eq!(
        s.end.recv_timeout(Duration::from_secs(5)),
        Ok(ServeEnd::Fatal(FailureCode::LogHardeningFailed))
    );
    assert!(
        !td.calls()
            .iter()
            .any(|c| matches!(c, Call::Create | Call::Send { .. })),
        "a client was made after the hardening failed"
    );
}

#[test]
fn the_hello_names_the_library_version_and_commit() {
    let td = ScriptedTd::new(Script {
        version: "9.8.7".to_owned(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        ..Script::default()
    });
    let mut s = start(
        &td,
        ServeOptions {
            landlock: "unavailable",
            ..opts()
        },
    );
    match s.next() {
        FromHelper::Hello(h) => {
            assert_eq!(h.protocol, kourier_tdlib::PROTOCOL);
            assert_eq!(h.tdlib, "9.8.7");
            assert_eq!(h.commit, "0123456789abcdef0123456789abcdef01234567");
            assert_eq!(h.landlock, "unavailable");
        }
        other => panic!("expected the hello, got {other:?}"),
    }
    let _ = s.quit();
}

/// Reads frames up to the `F` of a refused request; nothing else may come in between.
fn refused(s: &mut Session) -> (FailureCode, Option<u64>) {
    match s.next() {
        FromHelper::Failure { code, extra } => (code, extra),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Neither as `S` nor as `E` does a log request of the app reach TDLib.
#[test]
fn log_requests_from_the_app_are_refused() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.next();
    let c = s.create(1);
    let kinds = [
        json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 5}),
        json!({"@type": "setLogStream", "log_stream": {"@type": "logStreamDefault"}}),
        json!({"@type": "setLogTagVerbosityLevel", "tag": "net_query", "new_verbosity_level": 5}),
        json!({"@type": "addLogMessage", "verbosity_level": 0, "text": "x"}),
    ];
    for (n, k) in kinds.iter().enumerate() {
        let extra = 10 + n as u64;
        let mut v = k.clone();
        v["@extra"] = json!(extra);
        s.send(c, v);
        assert_eq!(
            refused(&mut s),
            (FailureCode::RefusedLogRequest, Some(extra))
        );
        s.write(ToHelper::Execute {
            seq: 20 + n as u64,
            json: k.to_string(),
        });
        assert_eq!(refused(&mut s), (FailureCode::RefusedLogRequest, None));
        match s.next() {
            FromHelper::Executed { seq, json } => {
                assert_eq!(seq, 20 + n as u64);
                let v: Value = serde_json::from_str(&json).unwrap();
                assert_eq!(
                    v["@type"], "error",
                    "a refused execute is answered an error"
                );
            }
            other => panic!("expected the refused execute's X, got {other:?}"),
        }
    }
    let calls = td.calls();
    let log_calls = calls
        .iter()
        .filter(|c| match c {
            Call::Execute(t) => t.starts_with("setLog") || t == "addLogMessage",
            Call::Send { ty, .. } => ty.starts_with("setLog") || ty == "addLogMessage",
            _ => false,
        })
        .count();
    assert_eq!(log_calls, 2, "only the helper's own two hardening calls");
    let _ = s.quit();
}

/// D3 M1: the helper owns the proxy rule; every proxy request of the app is refused.
#[test]
fn proxy_requests_from_the_app_are_refused() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.next();
    let c = s.create(1);
    let kinds = [
        "addProxy",
        "editProxy",
        "enableProxy",
        "disableProxy",
        "removeProxy",
        "getProxies",
        "pingProxy",
        "testProxy",
        "testNetwork",
        // LC-20 (L1, PROTOCOL 5): the web-proxy carrier's two requests.
        "processWebProxyCarrierMessage",
        "reportWebProxyCarrierError",
    ];
    for (n, k) in kinds.iter().enumerate() {
        s.send(c, json!({"@type": k, "proxy_id": 1, "@extra": n + 1}));
        assert_eq!(
            refused(&mut s),
            (FailureCode::RefusedProxyRequest, Some(n as u64 + 1)),
            "a proxy request was not refused"
        );
    }
    s.write(ToHelper::Execute {
        seq: 1,
        json: json!({"@type": "testProxy"}).to_string(),
    });
    assert_eq!(refused(&mut s), (FailureCode::RefusedProxyRequest, None));
    assert!(matches!(s.next(), FromHelper::Executed { seq: 1, .. }));
    assert!(
        td.sent_types().is_empty(),
        "a proxy request reached TDLib: {:?}",
        td.sent_types()
    );
    let _ = s.quit();
}

/// LC-20 (PROTOCOL 5): the call family's WRITEs, pinned BY NAME in the test (typed a second time, apart from
/// `serve.rs`: a renamed, dropped or added entry in the helper's list fails the set comparison below). The
/// 71 functions that match `GroupCall|VideoChat|LiveStory|Call(?!back)` are these 49, the 21 `CALL_READS`
/// and `toggleSessionCanAcceptCalls` (a session toggle, in the account's C-sessions).
const CALL_WRITES: [&str; 49] = [
    // the call family (P-8, K4, the live stories)
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
    "startLiveStory",
    "joinLiveStory",
    "replaceLiveStoryRtmpUrl",
    "setLiveStoryMessageSender",
    "addPendingLiveStoryReaction",
    "commitPendingLiveStoryReactions",
    "removePendingLiveStoryReactions",
];

/// The account's WRITEs by class (`account-classes.md` of LC-20; the rules in `serve.rs`'s doc and in
/// `telegram.md`): the second half of the forbidden list.
const ACCOUNT_WRITES: [&str; 116] = [
    // C-third
    "acceptOauthRequest",
    "checkEmailAddressVerificationCode",
    "checkOauthRequestMatchCode",
    "confirmBusinessConnectedBot",
    "confirmQrCodeAuthentication",
    "deleteBusinessConnectedBot",
    "deletePassportElement",
    "disconnectAllWebsites",
    "disconnectWebsite",
    "getAllPassportElements",
    "getExternalLink",
    "getLoginUrl",
    "getPassportAuthorizationFormAvailableElements",
    "getPassportElement",
    "removeBusinessConnectedBotFromChat",
    "resendEmailAddressVerificationCode",
    "sendEmailAddressVerificationCode",
    "sendPassportAuthorizationForm",
    "setBusinessConnectedBot",
    "setPassportElement",
    "toggleBusinessConnectedBotChatIsPaused",
    // C-password
    "addLoginPasskey",
    "cancelPasswordReset",
    "cancelRecoveryEmailAddressVerification",
    "checkAuthenticationPasskey",
    "checkAuthenticationPasswordRecoveryCode",
    "checkPasswordRecoveryCode",
    "checkRecoveryEmailAddressCode",
    "createTemporaryPassword",
    "getRecoveryEmailAddress",
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
    "transferChatOwnership",
    // C-login-email
    "checkLoginEmailAddressCode",
    "resendLoginEmailAddressCode",
    "resetAuthenticationEmailAddress",
    "setLoginEmailAddress",
    // C-sessions
    "confirmSession",
    "setInactiveSessionTtl",
    "terminateAllOtherSessions",
    "terminateSession",
    "toggleSessionCanAcceptCalls",
    "toggleSessionCanAcceptSecretChats",
    // C-phone
    "changePhoneNumber",
    "checkPhoneNumberCode",
    "reportAuthenticationCodeMissing",
    "reportPhoneNumberCodeMissing",
    "resendPhoneNumberCode",
    "sendAuthenticationFirebaseSms",
    "sendPhoneNumberCode",
    "sendPhoneNumberFirebaseSms",
    "setAccountTtl",
    // C-account
    "checkAuthenticationBotToken",
    "checkAuthenticationWebToken",
    "deleteAccount",
    "registerUser",
    // C-money
    "addPendingPaidMessageReaction",
    "allowUnpaidMessagesFromUser",
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
    "deleteGift",
    "deleteSavedCredentials",
    "deleteSavedOrderInfo",
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
    "removePendingPaidMessageReactions",
    "replaceTonWallet",
    "searchPublicPosts",
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
    "toggleDirectMessagesChatTopicCanSendUnpaidMessages",
    "transferGift",
    "upgradeGift",
    "validateOrderInfo",
    "withdrawUpgradedGift",
];

/// The account's READs by class: allowed, named in `serve::ACCOUNT_READS`.
const ACCOUNT_READ_NAMES: [&str; 78] = [
    // C-third
    "declineOauthRequest",
    "getBusinessConnectedBot",
    "getConnectedWebsites",
    "getExternalLinkInfo",
    "getLoginUrlInfo",
    "getOauthLinkInfo",
    "getPassportAuthorizationForm",
    "getPreferredCountryLanguage",
    // C-password
    "getAuthenticationPasskeyParameters",
    "getLoginPasskeys",
    "getPasskeyParameters",
    "getPasswordState",
    "getTemporaryPasswordState",
    // C-login-email
    "isLoginEmailAddressRequired",
    // C-sessions
    "getActiveSessions",
    // C-phone
    "getAccountTtl",
    // C-account
    "dismissWebToken",
    // C-money
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

/// TG-2 M5: every request Kourier never makes is refused `F refused-request` with its `@extra`, as
/// `S` and as `E`, and never reaches TDLib; the login requests Kourier does make pass.
#[test]
fn every_forbidden_request_is_refused_and_never_reaches_tdlib() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.next();
    let c = s.create(1);
    assert_eq!(FORBIDDEN_REQUESTS.len(), 165, "84 before LC-20, +81");
    // Pinned by name, not only through the list: a renamed entry would let the real request pass.
    // Both halves, exactly: the forbidden list IS the account's 116 WRITEs and the call family's 49.
    assert_eq!(ACCOUNT_WRITES.len() + CALL_WRITES.len(), 165);
    for name in ACCOUNT_WRITES.iter().chain(CALL_WRITES.iter()) {
        assert!(is_forbidden_request(name), "{name} is forbidden");
    }
    let mut named: Vec<&str> = ACCOUNT_WRITES
        .iter()
        .chain(CALL_WRITES.iter())
        .copied()
        .collect();
    named.sort_unstable();
    let mut listed = FORBIDDEN_REQUESTS.to_vec();
    listed.sort_unstable();
    assert_eq!(
        listed, named,
        "the forbidden list is not the two named halves"
    );
    // The READs stay allowed, and no name is on both sides (a call READ is no account READ either).
    for name in CALL_READS.iter().chain(ACCOUNT_READS.iter()) {
        assert!(
            !is_forbidden_request(name),
            "{name} is a read, not forbidden"
        );
    }
    assert_eq!(CALL_READS.len(), 21);
    assert_eq!(ACCOUNT_READS.len(), 78);
    let mut reads = ACCOUNT_READS.to_vec();
    reads.sort_unstable();
    let mut read_names = ACCOUNT_READ_NAMES.to_vec();
    read_names.sort_unstable();
    assert_eq!(reads, read_names, "ACCOUNT_READS is not the named READs");
    for name in ACCOUNT_READS {
        assert!(!CALL_READS.contains(&name), "{name} is on both READ lists");
    }
    // The login's own requests are members of no class (the classes say "outside the login").
    for name in [
        "setAuthenticationPhoneNumber",
        "checkAuthenticationCode",
        "resendAuthenticationCode",
        "checkAuthenticationPassword",
        "setAuthenticationEmailAddress",
        "checkAuthenticationEmailCode",
        "requestQrCodeAuthentication",
        "logOut",
        "destroy",
    ] {
        assert!(
            !is_forbidden_request(name) && !ACCOUNT_READS.contains(&name),
            "{name} is the login machine's, in no class"
        );
    }
    let mut unique = FORBIDDEN_REQUESTS.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        FORBIDDEN_REQUESTS.len(),
        "a name is listed twice"
    );
    for (n, k) in FORBIDDEN_REQUESTS.iter().enumerate() {
        let extra = 100 + n as u64;
        s.send(c, json!({"@type": k, "@extra": extra}));
        assert_eq!(
            refused(&mut s),
            (FailureCode::RefusedRequest, Some(extra)),
            "a forbidden request was not refused (index {n})"
        );
        let seq = 500 + n as u64;
        s.write(ToHelper::Execute {
            seq,
            json: json!({"@type": k}).to_string(),
        });
        assert_eq!(refused(&mut s), (FailureCode::RefusedRequest, None));
        match s.next() {
            FromHelper::Executed { seq: got, json } => {
                assert_eq!(got, seq);
                let v: Value = serde_json::from_str(&json).unwrap();
                assert_eq!(
                    v["@type"], "error",
                    "a refused execute is answered an error"
                );
            }
            other => panic!("expected the refused execute's X, got {other:?}"),
        }
    }
    let reached = |td: &ScriptedTd| {
        td.calls()
            .iter()
            .filter(|c| match c {
                Call::Execute(t) | Call::Send { ty: t, .. } => {
                    FORBIDDEN_REQUESTS.contains(&t.as_str())
                }
                _ => false,
            })
            .count()
    };
    assert_eq!(reached(&td), 0, "a forbidden request reached TDLib");
    // The requests the login machine makes are not on the list: they reach TDLib (which answers
    // them before the parameters with its 400; the answer is not this test's concern).
    for (n, k) in [
        "setAuthenticationPhoneNumber",
        "requestQrCodeAuthentication",
        "checkAuthenticationCode",
        "checkAuthenticationPassword",
        "setAuthenticationEmailAddress",
        "checkAuthenticationEmailCode",
        "resendAuthenticationCode",
        "logOut",
        "destroy",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            !is_forbidden_request(k),
            "a login request is forbidden (index {n})"
        );
        s.send(c, json!({"@type": k, "@extra": 900 + n as u64}));
    }
    // Each reaches TDLib, which answers it before the parameters with its 400 (after the client's
    // first state): no refusal comes in between.
    let mut answered = Vec::new();
    while answered.len() < 9 {
        let v = s.update();
        if v["@type"] == "error" {
            answered.push(v["@extra"].as_u64().expect("an answer carries its extra"));
        }
    }
    assert_eq!(answered, (900..909).collect::<Vec<u64>>());
    assert_eq!(
        td.sent_types().len(),
        9,
        "every login request reached TDLib"
    );
    assert_eq!(reached(&td), 0);
    let _ = s.quit();
}

/// D3 M1: `setTdlibParameters` for a client without a gate is refused and never reaches TDLib.
#[test]
fn parameters_without_a_gate_are_refused() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.next();
    let c = s.create(1);
    s.send(c, json!({"@type": "setTdlibParameters", "@extra": 7}));
    assert_eq!(
        refused(&mut s),
        (FailureCode::RefusedParametersWithoutGate, Some(7))
    );
    assert!(
        !td.sent_types().iter().any(|t| t == "setTdlibParameters"),
        "the parameters reached TDLib without a proxy"
    );
    let _ = s.quit();
}

/// TG-2b (PROTOCOL 4): with the helper locked to two directories, parameters naming any other
/// (another path, one missing, a prefix, a trailing slash) are refused `F refused-parameters-paths`
/// with their `@extra` and never reach TDLib, also with a gate in place; the same parameters with
/// the right directories (one written with a JSON escape) go on, the gate's `addProxy` first.
#[test]
fn parameters_naming_other_directories_are_refused() {
    let db = "/acct/a/tdlib";
    let files = "/media/telegram/0123abcd";
    let td = ScriptedTd::new(Script::default());
    let mut s = start(
        &td,
        ServeOptions {
            dirs: Some((db.to_owned(), files.to_owned())),
            ..opts()
        },
    );
    s.next();
    let c = s.create(1);
    s.write(ToHelper::Gate {
        client: c,
        port: 40001,
        user: "u".to_owned(),
        pass: "p".to_owned(),
    });
    for (n, (d, f)) in [
        (json!("/acct/b/tdlib"), json!(files)),
        (json!(db), json!("/media/telegram/other")),
        (json!(db), Value::Null),
        (Value::Null, json!(files)),
        (json!("/acct/a/tdlib/"), json!(files)),
        (json!("/acct/a"), json!(files)),
        (json!(db), json!(db)),
    ]
    .into_iter()
    .enumerate()
    {
        let extra = 10 + n as u64;
        let mut p = json!({"@type": "setTdlibParameters", "@extra": extra});
        if !d.is_null() {
            p["database_directory"] = d;
        }
        if !f.is_null() {
            p["files_directory"] = f;
        }
        s.send(c, p);
        assert_eq!(
            refused(&mut s),
            (FailureCode::RefusedParametersPaths, Some(extra)),
            "case {n} was not refused"
        );
    }
    // K12 (the security review's L6): a key written TWICE is refused whichever comes first. serde's
    // derived struct refuses the repeated field; a probe through `serde_json::Value` (the last one
    // wins) or a hand parser could disagree with TDLib's own JSON reader about which one counts.
    let (other_db, other_files) = ("/acct/b/tdlib", "/media/telegram/other");
    for (n, body) in [
        // database_directory twice: right then wrong, wrong then right.
        format!(
            r#"{{"@type":"setTdlibParameters","@extra":30,"database_directory":"{db}","database_directory":"{other_db}","files_directory":"{files}"}}"#
        ),
        format!(
            r#"{{"@type":"setTdlibParameters","@extra":31,"database_directory":"{other_db}","database_directory":"{db}","files_directory":"{files}"}}"#
        ),
        // files_directory twice, both orders.
        format!(
            r#"{{"@type":"setTdlibParameters","@extra":32,"database_directory":"{db}","files_directory":"{files}","files_directory":"{other_files}"}}"#
        ),
        format!(
            r#"{{"@type":"setTdlibParameters","@extra":33,"database_directory":"{db}","files_directory":"{other_files}","files_directory":"{files}"}}"#
        ),
        // Both right, but one of them twice: still a repeated key.
        format!(
            r#"{{"@type":"setTdlibParameters","@extra":34,"database_directory":"{db}","database_directory":"{db}","files_directory":"{files}"}}"#
        ),
    ]
    .into_iter()
    .enumerate()
    {
        s.send_raw(c, &body);
        assert_eq!(
            refused(&mut s),
            (FailureCode::RefusedParametersPaths, Some(30 + n as u64)),
            "repeated-key case {n} was not refused"
        );
    }
    assert!(
        !td.sent_types()
            .iter()
            .any(|t| t == "setTdlibParameters" || t == "addProxy"),
        "parameters with other directories reached TDLib"
    );
    // The right ones, the database path written with an escaped slash: forwarded, proxy first.
    let escaped = format!(
        r#"{{"@type":"setTdlibParameters","@extra":1,"database_directory":"{}","files_directory":"{files}"}}"#,
        db.replace('/', "\\/")
    );
    s.send_raw(c, &escaped);
    let _ = s.until(PHONE);
    let sent = td.sent_types();
    let at = sent
        .iter()
        .position(|t| t == "setTdlibParameters")
        .expect("the right parameters were sent");
    assert_eq!(sent[at - 1], "setOption");
    assert_eq!(sent[at - 2], "addProxy");
    let _ = s.quit();
}

/// D3 B1 + M1: the helper writes the gate's `addProxy` right before the parameters, never waiting
/// in between; the app sees the gate's `addedProxy`; the helper's clean-up keeps ONE proxy over
/// three starts with fresh gate ports, and none of its own answers reaches the app.
#[test]
fn the_gate_proxy_goes_right_before_the_parameters_and_stale_ones_are_removed() {
    let td = ScriptedTd::new(Script::default());
    for (run, port) in [40001_u16, 40002, 40003].into_iter().enumerate() {
        let mut s = start(&td, opts());
        s.next();
        let c = s.create(1);
        s.write(ToHelper::Gate {
            client: c,
            port,
            user: format!("user{run}"),
            pass: format!("pass{run}"),
        });
        s.send(c, json!({"@type": "setTdlibParameters", "@extra": 1}));
        let (seen, _) = s.until(PHONE);
        let sent = td.sent_types();
        let at = sent
            .iter()
            .rposition(|t| t == "setTdlibParameters")
            .expect("the parameters were sent");
        assert_eq!(
            sent[at - 2..at],
            ["addProxy", "setOption"],
            "addProxy and the files option are not written right before the parameters"
        );
        let proxy = seen
            .iter()
            .find(|v| v["@type"] == "addedProxy")
            .expect("the app sees the gate's addedProxy");
        assert_eq!(proxy["@extra"], json!(GATE_PROXY_EXTRA));
        assert_eq!(proxy["is_enabled"], true);
        assert_eq!(proxy["proxy"]["server"], "127.0.0.1");
        assert_eq!(proxy["proxy"]["port"], port);
        let deadline = Instant::now() + Duration::from_secs(5);
        while td.proxies().len() != 1 {
            assert!(
                Instant::now() < deadline,
                "the stale proxies were not removed"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let (before, bye, end) = s.quit();
        assert_eq!(end, ServeEnd::AllClosed);
        assert_eq!(bye.closed, [c]);
        let after: Vec<Value> = before
            .iter()
            .filter_map(|f| match f {
                FromHelper::Update(j) => serde_json::from_str(j).ok(),
                _ => None,
            })
            .collect();
        for v in seen.iter().chain(&after) {
            let own = v["@extra"]
                .as_u64()
                .is_some_and(|e| e >= HELPER_EXTRA_MIN && e != GATE_PROXY_EXTRA);
            assert!(!own, "a helper's own answer reached the app");
        }
        assert!(
            !seen
                .iter()
                .chain(&after)
                .any(|v| v["@type"] == "addedProxies"),
            "the helper's getProxies answer reached the app"
        );
    }
    let list = td.proxies();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].2, 40003, "the last start's gate is the one kept");
}

/// Fix F1: one gate per client. A second `G` would change what a parked `addProxy` dials (a gate
/// restart means a new client), so it is a bad frame: the helper ends, and no `addProxy` is written
/// for either gate.
#[test]
fn a_second_gate_for_a_client_is_a_bad_frame() {
    let td = ScriptedTd::new(Script::default());
    // Every frame is in the socket before the helper starts: it cannot end under a write.
    let gate = |port| ToHelper::Gate {
        client: 1,
        port,
        user: "u".to_owned(),
        pass: "p".to_owned(),
    };
    let mut s = start_after(
        &td,
        opts(),
        &[
            ToHelper::Create { seq: 1 },
            gate(40001),
            gate(40002),
            ToHelper::Send {
                client: 1,
                json: json!({"@type": "setTdlibParameters", "@extra": 1}).to_string(),
            },
        ],
    );
    assert!(matches!(s.next(), FromHelper::Hello(_)));
    assert!(matches!(s.next(), FromHelper::Created { client: 1, .. }));
    assert_eq!(
        refused(&mut s),
        (FailureCode::BadFrame, None),
        "the second gate was taken"
    );
    let end = s
        .end
        .recv_timeout(Duration::from_secs(10))
        .expect("the helper ended");
    assert_eq!(end, ServeEnd::Fatal(FailureCode::BadFrame));
    assert!(
        !td.sent_types()
            .iter()
            .any(|t| t == "addProxy" || t == "setTdlibParameters"),
        "a proxy or the parameters were written for a client with two gates"
    );
}

/// TG-1b B-3: the helper binary is locked down to ONE gate port (`--gate-port`); a `G` frame for
/// another port would point TDLib at a port the lock-down refuses (or, without Landlock, at
/// something else on loopback): a bad frame, nothing reaches TDLib. The gate's own port is taken.
#[test]
fn a_gate_on_another_port_than_the_lock_down_is_a_bad_frame() {
    let locked = ServeOptions {
        gate_port: Some(40001),
        ..opts()
    };
    let gate = |port| ToHelper::Gate {
        client: 1,
        port,
        user: "u".to_owned(),
        pass: "p".to_owned(),
    };
    let params = || ToHelper::Send {
        client: 1,
        json: json!({"@type": "setTdlibParameters", "@extra": 1}).to_string(),
    };
    let td = ScriptedTd::new(Script::default());
    let mut s = start_after(
        &td,
        locked.clone(),
        &[ToHelper::Create { seq: 1 }, gate(40002), params()],
    );
    assert!(matches!(s.next(), FromHelper::Hello(_)));
    assert!(matches!(s.next(), FromHelper::Created { client: 1, .. }));
    assert_eq!(
        refused(&mut s),
        (FailureCode::BadFrame, None),
        "a gate on another port was taken"
    );
    let end = s
        .end
        .recv_timeout(Duration::from_secs(10))
        .expect("the helper ended");
    assert_eq!(end, ServeEnd::Fatal(FailureCode::BadFrame));
    assert!(
        !td.sent_types()
            .iter()
            .any(|t| t == "addProxy" || t == "setTdlibParameters"),
        "a request reached TDLib after a gate on another port"
    );

    let td = ScriptedTd::new(Script::default());
    let mut s = start_after(
        &td,
        locked,
        &[ToHelper::Create { seq: 1 }, gate(40001), params()],
    );
    assert!(matches!(s.next(), FromHelper::Hello(_)));
    assert!(matches!(s.next(), FromHelper::Created { client: 1, .. }));
    let _ = s.quit();
    assert!(
        td.sent_types().iter().any(|t| t == "addProxy"),
        "the gate on the locked port was not written"
    );
}

/// G6: a gate on a privileged port or on one of TDLib's data-centre ports is a bad frame, with or
/// without a lock-down port (Landlock names a port only, never an address: a gate port that is
/// also a data-centre port would let a TDLib without its proxy reach every data centre on it).
#[test]
fn a_gate_on_a_data_centre_or_privileged_port_is_a_bad_frame() {
    for (port, locked) in [
        (443, None),
        (80, None),
        (5222, None),
        (1023, None),
        (5222, Some(5222)),
    ] {
        let td = ScriptedTd::new(Script::default());
        let opts = ServeOptions {
            gate_port: locked,
            ..opts()
        };
        let mut s = start_after(
            &td,
            opts,
            &[
                ToHelper::Create { seq: 1 },
                ToHelper::Gate {
                    client: 1,
                    port,
                    user: "u".to_owned(),
                    pass: "p".to_owned(),
                },
                ToHelper::Send {
                    client: 1,
                    json: json!({"@type": "setTdlibParameters", "@extra": 1}).to_string(),
                },
            ],
        );
        assert!(matches!(s.next(), FromHelper::Hello(_)));
        assert!(matches!(s.next(), FromHelper::Created { client: 1, .. }));
        assert_eq!(
            refused(&mut s),
            (FailureCode::BadFrame, None),
            "a gate on port {port} was taken"
        );
        let end = s
            .end
            .recv_timeout(Duration::from_secs(10))
            .expect("the helper ended");
        assert_eq!(end, ServeEnd::Fatal(FailureCode::BadFrame));
        assert!(
            !td.sent_types()
                .iter()
                .any(|t| t == "addProxy" || t == "setTdlibParameters"),
            "a request reached TDLib after a gate on port {port}"
        );
    }
}

/// Fix F1: a gate outside TDLib's limits that a peer wrote without the encoder (port 0, a login of
/// 128 bytes) is a bad frame too; the decoder is the helper's own wall.
#[test]
fn a_gate_outside_the_limits_on_the_wire_ends_the_helper_before_tdlib() {
    for line in [
        "G 1 0 u p".to_owned(),
        format!("G 1 40001 {} p", "u".repeat(128)),
        format!("G 1 40001 u {}", "p".repeat(128)),
    ] {
        let td = ScriptedTd::new(Script::default());
        let bytes = format!(
            "C 1\n{line}\nS 1 {}\n",
            json!({"@type": "setTdlibParameters", "@extra": 1})
        );
        let mut s = start_raw(&td, opts(), bytes.as_bytes());
        assert!(matches!(s.next(), FromHelper::Hello(_)));
        assert!(matches!(s.next(), FromHelper::Created { client: 1, .. }));
        assert_eq!(
            refused(&mut s),
            (FailureCode::BadFrame, None),
            "a gate outside the limits was taken (a line of {} bytes)",
            line.len()
        );
        let end = s
            .end
            .recv_timeout(Duration::from_secs(10))
            .expect("the helper ended");
        assert_eq!(end, ServeEnd::Fatal(FailureCode::BadFrame));
        assert!(
            !td.sent_types()
                .iter()
                .any(|t| t == "addProxy" || t == "setTdlibParameters"),
            "a request reached TDLib after a bad gate"
        );
    }
}

/// Fix F2: the helper checks the answer to its own `addProxy`. TDLib refuses it (a 500 here): that
/// client is closed and the helper says `F gate-proxy-failed`; the other client is left alone, the
/// helper goes on, and the app still sees TDLib's answer.
#[test]
fn a_gate_proxy_tdlib_does_not_take_closes_that_client() {
    let td = ScriptedTd::new(Script {
        fail_add_proxy: true,
        ..Script::default()
    });
    let mut s = start(&td, opts());
    s.next();
    let (a, b) = (s.create(1), s.create(2));
    s.write(ToHelper::Gate {
        client: a,
        port: 40001,
        user: "u".to_owned(),
        pass: "p".to_owned(),
    });
    s.send(a, json!({"@type": "setTdlibParameters", "@extra": 1}));
    let (mut failed, mut closed, mut answer) = (false, false, false);
    while !(failed && closed && answer) {
        match s.next() {
            FromHelper::Failure {
                code: FailureCode::GateProxyFailed,
                extra: None,
            } => failed = true,
            FromHelper::Update(j) => {
                let v: Value = serde_json::from_str(&j).expect("U is JSON");
                if v["@extra"] == json!(GATE_PROXY_EXTRA) {
                    assert_eq!(
                        v["@type"], "error",
                        "TDLib's answer was not passed on as it was"
                    );
                    answer = true;
                }
                if label(&v) == "updateAuthorizationState:authorizationStateClosed"
                    && v["@client_id"] == a
                {
                    closed = true;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let closes = |client: i32| {
        td.calls()
            .iter()
            .filter(|c| matches!(c, Call::Send { client: k, ty } if *k == client && ty == "close"))
            .count()
    };
    assert_eq!(
        (closes(a), closes(b)),
        (1, 0),
        "the wrong client was closed"
    );
    let (_, bye, end) = s.quit();
    assert_eq!(
        end,
        ServeEnd::AllClosed,
        "the helper did not go on after the failure"
    );
    assert!(bye.closed.contains(&a) && bye.closed.contains(&b));
}

/// Fix F4: TDLib refuses the first parameters (400) and keeps the parked `addProxy`; the helper's
/// retry sends the parameters again WITHOUT a second `addProxy` (the first one is still parked), and
/// the gate proxy is applied by the second parameters.
#[test]
fn refused_first_parameters_are_retried_without_a_second_add_proxy() {
    let td = ScriptedTd::new(Script {
        refuse_first_parameters: true,
        ..Script::default()
    });
    let mut s = start(&td, opts());
    s.next();
    let c = s.create(1);
    s.write(ToHelper::Gate {
        client: c,
        port: 40001,
        user: "u".to_owned(),
        pass: "p".to_owned(),
    });
    s.send(c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    let (seen, refusal) = s.until("error");
    assert_eq!(refusal["@extra"], 1);
    assert!(
        !seen.iter().any(|v| v["@type"] == "addedProxy"),
        "the proxy was applied by refused parameters"
    );
    s.send(c, json!({"@type": "setTdlibParameters", "@extra": 2}));
    let (seen, _) = s.until(PHONE);
    assert!(
        seen.iter()
            .any(|v| v["@type"] == "addedProxy" && v["@extra"] == json!(GATE_PROXY_EXTRA)),
        "the retry did not apply the parked gate proxy"
    );
    let sent = td.sent_types();
    let count = |t: &str| sent.iter().filter(|s| *s == t).count();
    assert_eq!(
        (
            count("addProxy"),
            count("setOption"),
            count("setTdlibParameters")
        ),
        (1, 1, 2),
        "the retry wrote a second addProxy or files option"
    );
    let (_, _, end) = s.quit();
    assert_eq!(end, ServeEnd::AllClosed);
}

/// K2 (review H2): every client gets `setOption store_all_files_in_files_directory = true` from the
/// helper itself, between the gate's `addProxy` and the parameters, and the answer is checked and
/// not passed on (the only helper answer the app sees is the gate's `addedProxy`).
#[test]
fn the_helper_sets_the_files_option_for_every_client_and_keeps_its_answer() {
    let td = ScriptedTd::new(Script::default());
    assert_eq!(td.files_option(), None, "the option starts unset");
    let mut s = start(&td, opts());
    s.next();
    for (n, port) in [(1_u64, 40001_u16), (2, 40002)] {
        let c = s.create(n);
        s.write(ToHelper::Gate {
            client: c,
            port,
            user: "u".to_owned(),
            pass: "p".to_owned(),
        });
        s.send(c, json!({"@type": "setTdlibParameters", "@extra": 1}));
        let (seen, _) = s.until(PHONE);
        let helper_answers: Vec<&Value> = seen
            .iter()
            .filter(|v| v["@extra"].as_u64().is_some_and(|e| e >= HELPER_EXTRA_MIN))
            .collect();
        assert!(
            helper_answers
                .iter()
                .all(|v| v["@extra"] == json!(GATE_PROXY_EXTRA)),
            "an answer to the helper's own files option reached the app: {helper_answers:?}"
        );
    }
    assert_eq!(
        td.files_option(),
        Some(true),
        "the helper did not set the option"
    );
    let requests = td.requests();
    let option: Vec<&Value> = requests
        .iter()
        .map(|(_, r)| r)
        .filter(|r| r["@type"] == "setOption")
        .collect();
    assert_eq!(option.len(), 2, "one setOption per client");
    for r in option {
        assert_eq!(r["name"], "store_all_files_in_files_directory");
        assert_eq!(
            r["value"],
            json!({"@type": "optionValueBoolean", "value": true})
        );
    }
    let (_, _, end) = s.quit();
    assert_eq!(end, ServeEnd::AllClosed);
}

/// K2: the app's own `setOption` of the option is refused whatever its value or spelling, as `S`
/// and as `E`, and never reaches TDLib; another option passes.
#[test]
fn the_apps_own_files_option_is_refused_any_value() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.next();
    let c = s.create(1);
    let bodies = [
        r#"{"@type":"setOption","name":"store_all_files_in_files_directory","value":{"@type":"optionValueBoolean","value":false},"@extra":1}"#,
        r#"{"@type":"setOption","name":"store_all_files_in_files_directory","value":{"@type":"optionValueBoolean","value":true},"@extra":2}"#,
        r#"{"@type":"setOption","name":"store_all_files_in_files_directory","value":{"@type":"optionValueEmpty"},"@extra":3}"#,
        // An escape in the name stands for the same string.
        r#"{"@type":"setOption","name":"store_all_files_in_files_director\u0079","value":{"@type":"optionValueBoolean","value":false},"@extra":4}"#,
        // A name twice: serde refuses it, so it is refused (the helper cannot tell which one
        // TDLib's own reader would keep).
        r#"{"@type":"setOption","name":"online","name":"store_all_files_in_files_directory","value":{"@type":"optionValueBoolean","value":false},"@extra":5}"#,
        r#"{"@type":"setOption","name":"store_all_files_in_files_directory","name":"online","value":{"@type":"optionValueBoolean","value":false},"@extra":6}"#,
        // A name that is no string.
        r#"{"@type":"setOption","name":7,"value":{"@type":"optionValueBoolean","value":false},"@extra":7}"#,
    ];
    for (n, body) in bodies.iter().enumerate() {
        s.send_raw(c, body);
        assert_eq!(
            refused(&mut s),
            (FailureCode::RefusedRequest, Some(n as u64 + 1)),
            "body {n} was not refused"
        );
    }
    for body in &bodies[..2] {
        s.write(ToHelper::Execute {
            seq: 9,
            json: (*body).to_owned(),
        });
        assert_eq!(refused(&mut s), (FailureCode::RefusedRequest, None));
        assert!(matches!(s.next(), FromHelper::Executed { seq: 9, .. }));
    }
    assert!(
        td.sent_types().is_empty(),
        "the app's files option reached TDLib: {:?}",
        td.sent_types()
    );
    assert_eq!(td.files_option(), None);
    // The option's name is the whole rule: another option, and a name only starting the same, pass.
    for (n, name) in ["online", "store_all_files_in_files_directory_x"]
        .into_iter()
        .enumerate()
    {
        s.send(
            c,
            json!({
                "@type": "setOption", "name": name, "@extra": 20 + n,
                "value": {"@type": "optionValueBoolean", "value": true},
            }),
        );
    }
    let _ = s.quit();
    // (The helper's own `close` and its wake of the receiver at the quit are not the app's requests.)
    let sent: Vec<String> = td
        .sent_types()
        .into_iter()
        .filter(|t| t != "close" && t != "getOption")
        .collect();
    assert_eq!(
        sent,
        ["setOption", "setOption"],
        "another option did not pass"
    );
}

/// K2: TDLib refuses the helper's files option (a 500 here): that client is closed and the helper
/// says `F files-option-failed`; the other client is left alone, the helper goes on, and TDLib's
/// answer to the helper's own request is not passed on.
#[test]
fn a_files_option_tdlib_does_not_take_closes_that_client() {
    let td = ScriptedTd::new(Script {
        fail_files_option: true,
        ..Script::default()
    });
    let mut s = start(&td, opts());
    s.next();
    let (a, b) = (s.create(1), s.create(2));
    s.write(ToHelper::Gate {
        client: a,
        port: 40001,
        user: "u".to_owned(),
        pass: "p".to_owned(),
    });
    s.send(a, json!({"@type": "setTdlibParameters", "@extra": 1}));
    let (mut failed, mut closed) = (false, false);
    while !(failed && closed) {
        match s.next() {
            FromHelper::Failure {
                code: FailureCode::FilesOptionFailed,
                extra: None,
            } => failed = true,
            FromHelper::Update(j) => {
                let v: Value = serde_json::from_str(&j).expect("U is JSON");
                assert!(
                    v["@extra"] == json!(GATE_PROXY_EXTRA)
                        || v["@extra"].as_u64().is_none_or(|e| e < HELPER_EXTRA_MIN),
                    "TDLib's answer to the helper's files option was passed on: {v}"
                );
                if label(&v) == "updateAuthorizationState:authorizationStateClosed"
                    && v["@client_id"] == a
                {
                    closed = true;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let closes = |client: i32| {
        td.calls()
            .iter()
            .filter(|c| matches!(c, Call::Send { client: k, ty } if *k == client && ty == "close"))
            .count()
    };
    assert_eq!(
        (closes(a), closes(b)),
        (1, 0),
        "the wrong client was closed"
    );
    let (_, bye, end) = s.quit();
    assert_eq!(
        end,
        ServeEnd::AllClosed,
        "the helper did not go on after the failure"
    );
    assert!(bye.closed.contains(&a) && bye.closed.contains(&b));
}

/// D3 M4: a helper whose TDLib is quiet still writes its heartbeat.
#[test]
fn a_quiet_receiver_still_beats() {
    let td = ScriptedTd::new(Script::default());
    let (app, helper) = UnixStream::pair().expect("socketpair");
    app.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let engine = td.clone();
    let t = thread::spawn(move || {
        serve(
            engine,
            helper,
            ServeOptions {
                heartbeat: Duration::from_millis(100),
                ..opts()
            },
        )
    });
    let mut lines = LineReader::new(app.try_clone().unwrap());
    let start = Instant::now();
    let mut beats = 0;
    while start.elapsed() < Duration::from_millis(1000) {
        let line = lines
            .next_line()
            .ok()
            .flatten()
            .expect("the helper went quiet without a heartbeat");
        if matches!(decode_from_helper(&line), Ok(FromHelper::Heartbeat)) {
            beats += 1;
        }
    }
    assert!(beats >= 4, "only {beats} heartbeats in a second at 100 ms");
    drop(lines);
    drop(app);
    assert_eq!(t.join().unwrap(), ServeEnd::AllClosed);
}

/// TP: an idle helper does not poll: `td_receive` waits until the next heartbeat is due, so calls
/// come at the heartbeat's rate (read as a count over the time that passed, never a wall time).
/// Mutant: the wait back at half a second.
#[test]
fn an_idle_receiver_waits_for_the_heartbeat() {
    let td = ScriptedTd::new(Script::default());
    let (app, helper) = UnixStream::pair().expect("socketpair");
    app.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let beat = Duration::from_secs(1);
    let engine = td.clone();
    let t = thread::spawn(move || {
        serve(
            engine,
            helper,
            ServeOptions {
                heartbeat: beat,
                ..opts()
            },
        )
    });
    let mut lines = LineReader::new(app.try_clone().unwrap());
    let mut wait_for_a_beat = || loop {
        let line = lines.next_line().ok().flatten().expect("no heartbeat");
        if matches!(decode_from_helper(&line), Ok(FromHelper::Heartbeat)) {
            return;
        }
    };
    wait_for_a_beat();
    let (before, from) = (td.receive_calls(), Instant::now());
    for _ in 0..3 {
        wait_for_a_beat();
    }
    let calls = td.receive_calls() - before;
    let beats = from.elapsed().as_secs_f64() / beat.as_secs_f64();
    assert!(
        calls as f64 <= beats + 1.0,
        "{calls} receive calls in {beats:.1} heartbeats: the idle receiver polls"
    );
    drop(lines);
    drop(app);
    assert_eq!(t.join().unwrap(), ServeEnd::AllClosed);
}

/// TP: a stop wakes the receiver that waits in `td_receive` for its next heartbeat, so the end comes
/// long before the close deadline the join would hold it to, and the receiver has ended: the bound
/// is far above a clean end (milliseconds) and far below that deadline. Mutant: no wake.
#[test]
fn a_stop_wakes_the_receivers_long_wait() {
    let close_deadline = Duration::from_secs(40);
    let td = ScriptedTd::new(Script::default());
    let (app, helper) = UnixStream::pair().expect("socketpair");
    let engine = td.clone();
    let t = thread::spawn(move || {
        serve(
            engine,
            helper,
            ServeOptions {
                heartbeat: Duration::from_secs(300),
                close_deadline,
                ..opts()
            },
        )
    });
    // The hello is out and the receiver is in its wait.
    let mut lines = LineReader::new(app.try_clone().unwrap());
    app.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    lines.next_line().expect("the hello");
    let by = Instant::now() + Duration::from_secs(20);
    while !td.is_receiving() {
        assert!(Instant::now() < by, "the receiver never started");
        thread::sleep(Duration::from_millis(5));
    }
    let from = Instant::now();
    drop(lines);
    drop(app);
    assert_eq!(t.join().unwrap(), ServeEnd::AllClosed);
    assert!(
        from.elapsed() < close_deadline / 2,
        "the end waited for the receiver"
    );
    assert!(!td.is_receiving(), "the receiver still waits");
}

/// T-a4: only TDLib's real closed state counts, not a message that holds its words.
#[test]
fn only_the_closed_state_marks_a_client_closed() {
    let td = ScriptedTd::new(Script {
        hang_on_close: true,
        ..Script::default()
    });
    let mut s = start(
        &td,
        ServeOptions {
            close_deadline: Duration::from_millis(500),
            ..opts()
        },
    );
    s.next();
    let c = s.create(1);
    td.push_update(json!({
        "@type": "updateNewMessage",
        "@client_id": c,
        "message": {
            "content": {"@type": "messageText", "text": {"text": "updateAuthorizationState authorizationStateClosed"}},
            "authorization_state": {"@type": "authorizationStateClosed"},
        },
    }));
    assert_eq!(s.update()["@type"], "updateNewMessage");
    let (_, bye, end) = s.quit();
    assert_eq!(end, ServeEnd::Forced, "a message closed the client");
    assert!(bye.forced && bye.closed.is_empty());
}

/// T-a5: `Q` closes every client; TDLib's one NULL while closing does not end the receiving.
#[test]
fn quit_closes_every_client_and_polls_past_the_null() {
    let td = ScriptedTd::new(Script::default());
    let mut s = start(&td, opts());
    s.next();
    let a = s.create(1);
    let b = s.create(2);
    let (_, bye, end) = s.quit();
    assert_eq!(end, ServeEnd::AllClosed);
    assert_eq!(
        bye,
        Bye {
            closed: vec![a, b],
            forced: false
        }
    );
}

/// T-a6: a client that never closes ends the loop at its deadline, forced.
#[test]
fn a_client_that_never_closes_ends_forced_after_the_deadline() {
    let td = ScriptedTd::new(Script {
        hang_on_close: true,
        ..Script::default()
    });
    let mut s = start(
        &td,
        ServeOptions {
            close_deadline: Duration::from_millis(300),
            ..opts()
        },
    );
    s.next();
    s.create(1);
    let t0 = Instant::now();
    let (_, bye, end) = s.quit();
    assert_eq!(end, ServeEnd::Forced);
    assert!(bye.forced, "the bye does not say forced");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "the deadline did not end the close"
    );
}

/// F6: the helper's whole shutdown (the close deadline, the receiver's end, the bye's write) ends
/// inside `close_deadline + shutdown_tail` from `Q`, so the app's SIGKILL at t0 + 17 s never meets
/// a helper still waiting (the real numbers are 15 s + 1.5 s; scaled here). The hard case: a client
/// that never closes AND an app that stopped reading, so the writer is stuck in `write` on a full
/// socket and the bye cannot go out.
#[test]
fn the_whole_shutdown_ends_inside_its_budget_when_the_app_stopped_reading() {
    let pad = "x".repeat(100_000);
    let td = ScriptedTd::new(Script {
        hang_on_close: true,
        rules: vec![Rule::new(
            "testCallEmpty",
            vec![json!({"@type": "ok", "pad": pad})],
        )],
        ..Script::default()
    });
    let mut s = start(
        &td,
        ServeOptions {
            close_deadline: Duration::from_millis(600),
            shutdown_tail: Duration::from_millis(400),
            ..opts()
        },
    );
    assert!(matches!(s.next(), FromHelper::Hello(_)));
    let client = s.create(1);
    // 40 answers of 100 kB: far more than the socket holds, and the app reads none of them.
    for i in 0..40 {
        s.send(client, json!({"@type": "testCallEmpty", "@extra": i + 1}));
    }
    thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    s.write(ToHelper::Quit);
    let end = s
        .end
        .recv_timeout(Duration::from_secs(10))
        .expect("serve did not return");
    let took = t0.elapsed();
    assert_eq!(end, ServeEnd::Forced);
    assert!(
        took >= Duration::from_millis(550),
        "the close deadline was not waited for ({took:?})"
    );
    assert!(
        took < Duration::from_millis(1800),
        "the shutdown outlasted its budget of 1.0 s: {took:?}"
    );
}

/// T-a7 (the spike's trap, D3 H3): 20,000 frames, every 10th an `E`, written before one is read;
/// then every answer arrives, within 30 s.
#[test]
fn twenty_thousand_requests_before_one_read_do_not_block() {
    let td = ScriptedTd::new(Script {
        rules: vec![Rule::new("testCallEmpty", vec![json!({"@type": "ok"})])],
        ..Script::default()
    });
    let mut s = start(&td, opts());
    let t0 = Instant::now();
    s.write(ToHelper::Create { seq: 1 });
    let n = 20_000_u64;
    for i in 0..n {
        if i % 10 == 9 {
            s.write(ToHelper::Execute {
                seq: i,
                json: json!({"@type": "getOption", "name": "version"}).to_string(),
            });
        } else {
            s.send(1, json!({"@type": "testCallEmpty", "@extra": i + 1}));
        }
    }
    let (mut oks, mut executed) = (0, 0);
    assert!(matches!(s.next(), FromHelper::Hello(_)));
    while oks < n - n / 10 || executed < n / 10 {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "the answers did not arrive within 30 s"
        );
        match s.next() {
            FromHelper::Update(j) => {
                if j.contains(r#""@type":"ok""#) {
                    oks += 1;
                }
            }
            FromHelper::Executed { .. } => executed += 1,
            _ => {}
        }
    }
    let _ = s.quit();
}

/// T-a8, the serve half: one receiving thread serves every client.
#[test]
fn one_receiver_thread_serves_every_client() {
    let td = ScriptedTd::new(Script {
        rules: vec![Rule::new("testCallEmpty", vec![json!({"@type": "ok"})])],
        ..Script::default()
    });
    let mut s = start(&td, opts());
    s.next();
    let a = s.create(1);
    let b = s.create(2);
    s.send(a, json!({"@type": "testCallEmpty", "@extra": 1}));
    s.send(b, json!({"@type": "testCallEmpty", "@extra": 2}));
    s.until("ok");
    s.until("ok");
    let _ = s.quit();
    let threads: std::collections::HashSet<_> = td
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::ReceiveThread(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(threads.len(), 1, "more than one thread received from TDLib");
}
