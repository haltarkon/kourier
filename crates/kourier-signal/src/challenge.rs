//! The server's send challenge (§4.6, §6.4; Desktop's `SD/ts/challenge.dom.ts`): a send is
//! refused with 428 and a token until a person has solved a captcha. The send path reports it
//! as `ErrorKind::Challenge { token, url }`, the core pauses its outbox and shows the URL; the
//! browser hands the solution back as a `signalcaptcha://<solution>` link, the core passes it on
//! with `Command::SubmitChallenge`, and this module tells the server
//! (`PUT /v1/challenge {type: "captcha", token, captcha}`, `WebAPI.preload.ts`
//! `sendChallengeResponse`). Once the server accepted it, the core sends what waited.
//!
//! A job can meet the same 428 (receipts, read syncs, timer updates and sync transcripts are
//! sends too): [`announce`] tells the core with `BackendNotice::ChallengeRequired`, which shows
//! the same account state without holding any send, and the job waits for the server's
//! `Retry-After`. One challenge is one notice: while it stands (until the time the server
//! named, or, when it named none, until its answer or the next connect) further jobs and
//! tries that meet it announce nothing, and the jobs that would meet it are held without a
//! request (`jobs::hold_for_challenge`), so fifty queued receipts are one refused request and
//! one notice. An accepted answer makes the jobs that waited for the challenge due at once,
//! and only those: a job that waits out a rate limit of its own keeps its time.
//!
//! The standing challenge and its token live in memory, the hold in `backend.db` (`jobs.challenged`,
//! `held_from`), and the notice in the core, which ends it when the backend reports `Online`
//! anew or the actor restarts (`BackendNotice::ChallengeRequired`). The three end together
//! ([`Shared::forget_job_challenge`]): at a fresh `Online` that the core takes as the end of
//! its notice, which is also the first `Online` of an actor that was restarted, the held jobs
//! get their own times back. The first of them asks the server once: if the challenge is
//! over they all go out, and if it is not, the 428 brings a token again (an answer could not
//! be sent without one), the notice is announced again and the others are held again. A hold
//! never outlives the notice, and the user is never asked to verify for a challenge whose
//! token is gone. Desktop keeps waiting for the server's time across a restart
//! (`challenge.dom.ts` `load`, `onOnline`); Kourier asks once per connect instead, as its core
//! does for the messages that wait (§4.8).
//!
//! Only a 428 that names a token and offers a captcha is such a challenge; one that does not
//! (the account has used up its captcha attempts) is reported as a rate limit with the
//! server's `Retry-After` (`send::status_error`), which the outbox waits out by itself.

use std::rc::Rc;

use http::{Method, StatusCode};
use kourier_model::{BackendError, ErrorKind, Redacted};
use serde::{Deserialize, Serialize};

use crate::actor::Shared;
use crate::error::CHALLENGE_URL;
use crate::net::{SignalTransport, json_request, parse_json};

/// Path of the challenge answer.
const CHALLENGE_PATH: &str = "/v1/challenge";

/// The scheme of the link the captcha page opens with its solution (`signalRoutes.std.ts`
/// `captchaRoute`: `signalcaptcha://<solution>`, an optional `/` at the end).
const CAPTCHA_SCHEME: &str = "signalcaptcha://";

/// How long a refused answer asks to wait when the server names no time (`challenge.dom.ts`:
/// "should have a retry-after in the response, but just in case, let's wait a minute").
const DEFAULT_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Body of `PUT /v1/challenge` (`ChallengeType`).
#[derive(Serialize)]
struct Answer<'a> {
    r#type: &'static str,
    token: &'a str,
    captcha: &'a str,
}

/// Body of a 428 (`SendMessageChallengeData`).
#[derive(Default, Deserialize)]
#[serde(default)]
struct Challenge {
    token: String,
}

/// The captcha solution in what the browser handed back: the bare solution, or the whole
/// `signalcaptcha://…` link it arrived in. `None` when nothing is left.
pub(crate) fn captcha_of(answer: &str) -> Option<&str> {
    let answer = answer.trim();
    let solution = match answer.get(..CAPTCHA_SCHEME.len()) {
        Some(scheme) if scheme.eq_ignore_ascii_case(CAPTCHA_SCHEME) => {
            &answer[CAPTCHA_SCHEME.len()..]
        }
        _ => answer,
    };
    let solution = solution.strip_suffix('/').unwrap_or(solution);
    (!solution.is_empty()).then_some(solution)
}

/// A send challenge a job met and announced, while it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Standing {
    /// Jobs that send to other accounts wait until then (ms since the epoch): the server's
    /// `Retry-After`, a minute without one.
    pub(crate) until: i64,
    /// The server named no time, so the core shows the notice until the challenge is answered
    /// or the account connects anew: it is not announced again before that.
    untimed: bool,
    /// The account has been `Online` with this challenge announced. The core counts the same
    /// way: a notice that arrived during a catch-up is on show from the `Online` that ends the
    /// catch-up, and only the next one after that ends it.
    shown: bool,
}

impl Standing {
    /// The account came `Online` anew: `Some` when the challenge stands on (it was announced
    /// during the catch-up that just ended, so the core starts to show it now), `None` when
    /// the core takes this `Online` as its end.
    pub(crate) fn at_online(self) -> Option<Self> {
        (!self.shown).then_some(Self {
            shown: true,
            ..self
        })
    }
}

/// A job met a send challenge: remembers its token for the answer, tells the core
/// (`BackendNotice::ChallengeRequired`, durable like every notice) unless the challenge was
/// announced already and still stands, and returns how long the job waits before it tries
/// again: the server's `Retry-After`, a minute without one.
pub(crate) fn announce<T: SignalTransport>(
    shared: &Shared<T>,
    challenge: &crate::error::ChallengeInfo<'_>,
    now: i64,
) -> crate::error::Result<std::time::Duration> {
    // The server may hand out a new token with every 428; an answer counts for the newest.
    if !challenge.token.expose().is_empty() {
        *shared.challenge.borrow_mut() = Some(challenge.token.clone());
    }
    let wait = challenge.retry_after.unwrap_or(DEFAULT_RETRY_AFTER);
    let until = now.saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX));
    // The core ends a notice after its `retry_after` (and keeps one without a time): while
    // the announced one stands there, another would only be the same notice again.
    let stands = shared
        .job_challenge
        .get()
        .is_some_and(|standing| standing.untimed || standing.until > now);
    if stands {
        // The jobs wait at least as long as the server says now.
        let standing = shared.job_challenge.get().map(|standing| Standing {
            until: standing.until.max(until),
            ..standing
        });
        shared.job_challenge.set(standing);
        tracing::debug!("a job met the send challenge that is already announced");
        return Ok(wait);
    }
    let notice = kourier_backend_api::BackendEvent::Notice(
        kourier_backend_api::BackendNotice::ChallengeRequired {
            url: CHALLENGE_URL.to_owned(),
            retry_after: challenge.retry_after,
        },
    );
    shared.db.zone(|tx| {
        crate::outbox::append_events(tx, std::slice::from_ref(&notice), now, &shared.wake)
    })?;
    shared.job_challenge.set(Some(Standing {
        until,
        untimed: challenge.retry_after.is_none(),
        shown: shared.is_online(),
    }));
    tracing::warn!(
        retry_after_secs = challenge.retry_after.map(|wait| wait.as_secs()),
        "a job met a send challenge; the account asks the user to verify"
    );
    Ok(wait)
}

/// An error the user can act on.
fn refused(kind: ErrorKind, detail: &str, message: &str) -> BackendError {
    BackendError::new(kind, detail).with_user_message(message)
}

/// `Command::SubmitChallenge`: sends the user's captcha solution for the challenge `token`
/// (the core's, or the newest one this actor saw). `Ok` means the server accepted it and
/// refused sends may be tried again. Neither the token nor the solution is ever logged.
pub(crate) async fn submit<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    token: Option<Redacted<String>>,
    answer: &Redacted<String>,
) -> Result<(), BackendError> {
    let Some(captcha) = captcha_of(answer.expose()) else {
        return Err(refused(
            ErrorKind::InvalidInput,
            "empty challenge answer",
            "The verification did not return an answer. Try again.",
        ));
    };
    // The server may have asked again since the core paused: the newest token is the one a
    // solution counts for.
    let newest = shared.challenge.borrow().clone();
    let Some(token) = newest.or(token).filter(|t| !t.expose().is_empty()) else {
        return Err(refused(
            ErrorKind::InvalidInput,
            "no challenge to answer",
            "Nothing is waiting for a verification.",
        ));
    };
    let socket = shared.socket.borrow().clone();
    let Some(socket) = socket else {
        return Err(BackendError::transient("not connected")
            .with_user_message("Kourier is offline. Verify again once it is connected."));
    };
    let body = Answer {
        r#type: "captcha",
        token: token.expose(),
        captcha,
    };
    let request = json_request(Method::PUT, CHALLENGE_PATH, &body)
        .map_err(|e| BackendError::internal(e.to_string()))?;
    let response = shared
        .net
        .request(&socket, request)
        .await
        .map_err(|e| crate::error::Error::from(e).to_backend_error())?;
    let status = response.status;
    if status.is_success() {
        tracing::info!("challenge answer accepted");
        shared.challenge.borrow_mut().take();
        shared.job_challenge.set(None);
        // The jobs that waited for the challenge run now; one that waits out a rate limit of
        // its own keeps its time.
        let now = shared.now();
        match shared
            .db
            .zone(|tx| crate::jobs::release_challenged(tx, now))
        {
            Ok(woken) => tracing::debug!(woken, "jobs released after the challenge"),
            Err(e) => tracing::warn!(error = %e, "waking the jobs after a challenge failed"),
        }
        shared.jobs_wake.notify_one();
        return Ok(());
    }
    tracing::warn!(status = status.as_u16(), "challenge answer refused");
    let detail = format!(
        "server answered {} to the challenge answer",
        status.as_u16()
    );
    Err(match status {
        // Too many answers: the server says when the next one may come.
        StatusCode::PAYLOAD_TOO_LARGE | StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = response
                .headers
                .get(http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(DEFAULT_RETRY_AFTER, std::time::Duration::from_secs);
            refused(
                ErrorKind::RateLimited {
                    retry_after: Some(retry_after),
                },
                &detail,
                "Too many verification attempts. Wait a little, then try again.",
            )
        }
        // Another captcha is wanted: its token replaces the one just used.
        StatusCode::PRECONDITION_REQUIRED => {
            let next = parse_json::<Challenge>(&response)
                .map(|challenge| challenge.token)
                .unwrap_or_default();
            let next = if next.is_empty() {
                token
            } else {
                Redacted::new(next)
            };
            *shared.challenge.borrow_mut() = Some(next.clone());
            refused(
                ErrorKind::Challenge {
                    token: next,
                    url: CHALLENGE_URL.to_owned(),
                    retry_after: None,
                },
                &detail,
                "The verification was not accepted. Verify once more.",
            )
        }
        status if status.is_server_error() => BackendError::transient(detail)
            .with_user_message("The server could not check the verification. Try again."),
        _ => refused(
            ErrorKind::InvalidInput,
            &detail,
            "The verification was not accepted. Try again.",
        ),
    })
}

#[cfg(test)]
mod tests {
    use kourier_backend_api::TransmitOutcome;
    use kourier_model::{RecipientState, UserId};

    use super::*;
    use crate::actor::{Shared, testing};
    use crate::receive::tests::fixture;

    fn challenge_error(token: &str) -> BackendError {
        BackendError::new(
            ErrorKind::Challenge {
                token: Redacted::new(token.to_owned()),
                url: CHALLENGE_URL.to_owned(),
                retry_after: None,
            },
            "428",
        )
    }

    /// A challenge a job met reaches the core as a durable notice with the server's retry
    /// time, its token is kept for the answer, and the job is told how long to wait. While
    /// that challenge stands, further jobs and tries that meet it announce nothing: one
    /// challenge is one notice.
    #[tokio::test(flavor = "current_thread")]
    async fn a_challenge_is_announced_once_while_it_stands() {
        use kourier_backend_api::{BackendEvent, BackendNotice};
        let fixture = fixture();
        let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = test.shared;
        let notices = || -> Vec<BackendEvent> {
            fixture
                .db
                .read(|c| -> crate::error::Result<_> {
                    let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
                    let rows = stmt
                        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
                        .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .unwrap()
                .iter()
                .flat_map(|row| kourier_backend_api::decode_outbox_row(row).unwrap())
                .filter(|event| matches!(event, BackendEvent::Notice(_)))
                .collect()
        };
        let token = |text: &str| Redacted::new(text.to_owned());
        let remembered = || {
            shared
                .challenge
                .borrow()
                .as_ref()
                .map(|t| t.expose().clone())
        };
        let ninety = Some(std::time::Duration::from_secs(90));
        let first = token("job-token");
        let timed = crate::error::ChallengeInfo {
            token: &first,
            retry_after: ninety,
        };
        let wait = announce(&shared, &timed, 5_000).unwrap();
        assert_eq!(wait.as_secs(), 90, "the job waits for the server's time");
        assert_eq!(remembered().as_deref(), Some("job-token"));
        assert_eq!(
            notices(),
            [BackendEvent::Notice(BackendNotice::ChallengeRequired {
                url: CHALLENGE_URL.to_owned(),
                retry_after: ninety,
            })]
        );
        assert_eq!(shared.challenge_until(5_000), Some(95_000));
        assert_eq!(shared.challenge_until(95_000), None, "over at its time");

        // Forty-nine more jobs meet the same challenge (the server may name a new token and
        // a new time with each 428): no further notice, the newest token is the one to
        // answer, and nobody is let through earlier than the server last said.
        for n in 0..49u32 {
            let next = token(&format!("token-{n}"));
            let again = crate::error::ChallengeInfo {
                token: &next,
                retry_after: Some(std::time::Duration::from_secs(120)),
            };
            let wait = announce(&shared, &again, 6_000 + i64::from(n)).unwrap();
            assert_eq!(wait.as_secs(), 120);
        }
        assert_eq!(notices().len(), 1, "one challenge, one notice");
        assert_eq!(remembered().as_deref(), Some("token-48"));
        assert_eq!(shared.challenge_until(6_100), Some(6_048 + 120_000));

        // Its time passed and a job is refused again: the core ended its notice by then, so
        // this is announced anew.
        let late = token("late");
        let again = crate::error::ChallengeInfo {
            token: &late,
            retry_after: ninety,
        };
        announce(&shared, &again, 200_000).unwrap();
        assert_eq!(notices().len(), 2);

        // A challenge without a time: the job still does not knock again at once, and the
        // core keeps the notice until it is answered, so it is not repeated at the next try.
        shared.job_challenge.set(None);
        let untimed = crate::error::ChallengeInfo {
            token: &first,
            retry_after: None,
        };
        assert_eq!(
            announce(&shared, &untimed, 300_000).unwrap(),
            DEFAULT_RETRY_AFTER
        );
        let all = notices();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[2],
            BackendEvent::Notice(BackendNotice::ChallengeRequired {
                url: CHALLENGE_URL.to_owned(),
                retry_after: None,
            })
        );
        assert_eq!(shared.challenge_until(300_000), Some(360_000));
        announce(&shared, &untimed, 400_000).unwrap();
        announce(&shared, &untimed, 500_000).unwrap();
        assert_eq!(notices().len(), 3, "still the same challenge");
        // It was announced before the account was `Online` (during a catch-up): the core has
        // it on show from the `Online` that follows, so it stands on there.
        shared.set_conn(kourier_model::ConnState::Online { detail: None });
        assert_eq!(shared.challenge_until(500_000), Some(560_000));
        announce(&shared, &untimed, 510_000).unwrap();
        assert_eq!(notices().len(), 3, "on show, not announced again");
        // The next fresh `Online` ends the notice in the core: the next refusal is announced
        // again.
        shared.set_conn(kourier_model::ConnState::Offline { retry_at_ms: None });
        shared.set_conn(kourier_model::ConnState::Online { detail: None });
        assert_eq!(shared.challenge_until(500_000), None);
        announce(&shared, &untimed, 600_000).unwrap();
        assert_eq!(notices().len(), 4);
        // Announced while `Online`: the very next fresh `Online` ends it.
        shared.set_conn(kourier_model::ConnState::Offline { retry_at_ms: None });
        shared.set_conn(kourier_model::ConnState::Online { detail: None });
        assert_eq!(shared.challenge_until(600_000), None);
        assert!(
            !format!("{:?}", notices()).contains("token"),
            "no token is in a notice"
        );
    }

    /// The hold in `backend.db` ends where the standing challenge and the core's notice end:
    /// at a fresh `Online`, which is also the first `Online` after a restart, when nothing of
    /// the challenge is left in memory. The jobs get their own times back; the one that met
    /// the challenge is due at once and asks the server again.
    #[tokio::test(flavor = "current_thread")]
    async fn the_hold_ends_with_the_notice_at_a_fresh_online_and_after_a_restart() {
        use kourier_model::ConnState;

        use crate::jobs::{self, JobKind};
        let fixture = fixture();
        let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = test.shared;
        let hour = std::time::Duration::from_secs(3600);
        let hour_ms = 3_600_000;
        let token = Redacted::new("job-token".to_owned());
        let challenge = crate::error::ChallengeInfo {
            token: &token,
            retry_after: Some(hour),
        };
        // `(key, next_at)` of the jobs that wait for the challenge.
        let held = || -> Vec<(String, i64)> {
            fixture
                .db
                .read(|c| -> crate::error::Result<_> {
                    let mut stmt = c.prepare(
                        "SELECT key, next_at FROM jobs WHERE challenged = 1 ORDER BY key",
                    )?;
                    let rows = stmt
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .unwrap()
        };
        let next_at = |key: &str| -> i64 {
            fixture
                .db
                .read(|c| -> crate::error::Result<i64> {
                    Ok(
                        c.query_row("SELECT next_at FROM jobs WHERE key = ?1", [key], |r| {
                            r.get(0)
                        })?,
                    )
                })
                .unwrap()
        };
        // The receipt `1:a` meets the challenge as the job runner records it: its retry
        // after the server's hour, every other job that sends to others held until then.
        let meet = |shared: &Shared<testing::NoTransport>, now: i64| {
            let wait = announce(shared, &challenge, now).unwrap();
            fixture
                .db
                .zone(|tx| -> crate::error::Result<()> {
                    let first = jobs::next_due(tx, now, true, false)?.expect("a due job");
                    assert_eq!(first.key, "1:a");
                    jobs::retry(tx, &first, now, "challenge", false, Some(wait))?;
                    jobs::hold_for_challenge(tx, &first, now + hour_ms)?;
                    Ok(())
                })
                .unwrap();
        };
        let online = |shared: &Shared<testing::NoTransport>| {
            shared.set_conn(ConnState::Online { detail: None });
        };
        let offline = |shared: &Shared<testing::NoTransport>| {
            shared.set_conn(ConnState::Offline { retry_at_ms: None });
        };

        // --- Met while online: the next fresh `Online` ends hold and notice together.
        online(&shared);
        let now = shared.now();
        fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                for key in ["1:a", "2:b"] {
                    assert!(jobs::enqueue(tx, JobKind::Receipts, key, now, None)?);
                }
                Ok(())
            })
            .unwrap();
        meet(&shared, now);
        assert_eq!(
            held(),
            [
                ("1:a".to_owned(), now + hour_ms),
                ("2:b".to_owned(), now + hour_ms)
            ]
        );
        assert_eq!(shared.challenge_until(now), Some(now + hour_ms));
        offline(&shared);
        assert_eq!(held().len(), 2, "going offline ends nothing");
        online(&shared);
        assert_eq!(held(), [], "nothing waits for a challenge nobody shows");
        assert_eq!(shared.challenge_until(shared.now()), None);
        assert!(
            next_at("1:a") <= shared.now(),
            "the job that met it asks again"
        );
        assert!(
            (now..=shared.now()).contains(&next_at("2:b")),
            "its own time has passed: due now"
        );

        // --- The challenge still stands and the actor is restarted: the hold is on disk,
        // nothing is in memory. The first `Online` of the new actor releases it.
        let now = shared.now();
        meet(&shared, now);
        assert_eq!(held().len(), 2);
        let restarted = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = restarted.shared;
        assert_eq!(shared.challenge_until(now), None, "nothing in memory");
        assert!(
            shared.challenge.borrow().is_none(),
            "no token to answer with"
        );
        assert_eq!(held().len(), 2, "held on disk");
        online(&shared);
        assert_eq!(held(), []);
        assert!(next_at("1:a") <= shared.now());

        // --- Met during a catch-up: the `Online` that ends the catch-up is where the core
        // starts to show the notice, so the hold stands; the fresh `Online` after it ends both.
        offline(&shared);
        let now = shared.now();
        meet(&shared, now);
        online(&shared);
        assert_eq!(held().len(), 2, "on show now: still held");
        assert_eq!(shared.challenge_until(now), Some(now + hour_ms));
        offline(&shared);
        online(&shared);
        assert_eq!(held(), []);
        assert_eq!(shared.challenge_until(shared.now()), None);
    }

    /// The token of a refused send is remembered, whether the whole send was refused or one
    /// member of a group send (`Partial`); a later success does not forget a challenge that
    /// still stands for another chat.
    #[tokio::test(flavor = "current_thread")]
    async fn the_newest_challenge_of_a_send_is_remembered() {
        let fixture = fixture();
        let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = test.shared;
        let token = || {
            shared
                .challenge
                .borrow()
                .as_ref()
                .map(|t| t.expose().clone())
        };
        assert_eq!(token(), None);
        shared.note_challenge(&TransmitOutcome::Failed(BackendError::transient("offline")));
        assert_eq!(token(), None, "not every failure is a challenge");
        shared.note_challenge(&TransmitOutcome::Failed(challenge_error("first")));
        assert_eq!(token().as_deref(), Some("first"));
        let user = |n: u8| UserId::parse(&format!("aci:00000000-0000-4000-8000-0000000000{n:02}"));
        shared.note_challenge(&TransmitOutcome::Partial {
            delivered: vec![(user(1).unwrap(), RecipientState::Sent { sealed: true })],
            failed: vec![
                (user(2).unwrap(), BackendError::transient("later")),
                (user(3).unwrap(), challenge_error("second")),
            ],
        });
        assert_eq!(token().as_deref(), Some("second"), "a member's challenge");
        shared.note_challenge(&TransmitOutcome::Sent {
            per_recipient: Vec::new(),
            server_at: None,
        });
        assert_eq!(token().as_deref(), Some("second"));
        // A challenge without a token cannot be answered: the one we have stays.
        shared.note_challenge(&TransmitOutcome::Failed(challenge_error("")));
        assert_eq!(token().as_deref(), Some("second"));
    }

    /// What is refused before anything reaches the server: no answer, no challenge, no
    /// connection. The user is told which.
    #[tokio::test(flavor = "current_thread")]
    async fn an_answer_needs_a_solution_a_challenge_and_a_connection() {
        let fixture = fixture();
        let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = Rc::new(test.shared);
        let answer = |text: &str| Redacted::new(text.to_owned());
        let token = |text: &str| Some(Redacted::new(text.to_owned()));

        let empty = submit(&shared, token("t"), &answer(" signalcaptcha:// "))
            .await
            .unwrap_err();
        assert_eq!(empty.kind, ErrorKind::InvalidInput);
        assert!(empty.user_message.is_some());

        let nothing = submit(&shared, None, &answer("solved")).await.unwrap_err();
        assert_eq!(nothing.kind, ErrorKind::InvalidInput);
        assert_eq!(nothing.detail, "no challenge to answer");
        let no_token = submit(&shared, token(""), &answer("solved"))
            .await
            .unwrap_err();
        assert_eq!(no_token.detail, "no challenge to answer");

        // The core's token, or the one this actor remembers: both find no socket here.
        let offline = submit(&shared, token("t"), &answer("solved"))
            .await
            .unwrap_err();
        assert!(offline.is_retryable(), "{offline:?}");
        *shared.challenge.borrow_mut() = Some(Redacted::new("remembered".to_owned()));
        let offline = submit(&shared, None, &answer("solved")).await.unwrap_err();
        assert!(offline.is_retryable());
        assert!(
            shared.challenge.borrow().is_some(),
            "an answer that never left keeps the challenge"
        );
        // Neither the token nor the solution is in what gets logged.
        for error in [&empty, &nothing, &offline] {
            let text = format!("{error:?} {error}");
            assert!(
                !text.contains("solved") && !text.contains("remembered"),
                "{text}"
            );
        }
    }

    #[test]
    fn the_solution_is_taken_out_of_the_captcha_link() {
        assert_eq!(
            captcha_of("signalcaptcha://abc.def-123"),
            Some("abc.def-123")
        );
        assert_eq!(captcha_of("signalcaptcha://abc/"), Some("abc"));
        assert_eq!(captcha_of("  SignalCaptcha://abc\n"), Some("abc"));
        // The bare solution, as a paste would give it.
        assert_eq!(
            captcha_of("signal-hcaptcha.5fad97ac.challenge.token"),
            Some("signal-hcaptcha.5fad97ac.challenge.token")
        );
        // A solution may contain dots, dashes and slashes of its own; only the link's are cut.
        assert_eq!(captcha_of("signalcaptcha://a/b"), Some("a/b"));
        assert_eq!(captcha_of(""), None);
        assert_eq!(captcha_of("   "), None);
        assert_eq!(captcha_of("signalcaptcha://"), None);
        assert_eq!(captcha_of("signalcaptcha:///"), None);
        // Multi-byte input shorter than the scheme does not split a character.
        assert_eq!(captcha_of("ключ"), Some("ключ"));
    }

    #[test]
    fn the_answer_serializes_like_desktops() {
        let body = serde_json::to_value(Answer {
            r#type: "captcha",
            token: "t0k",
            captcha: "solved",
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({"type": "captcha", "token": "t0k", "captcha": "solved"})
        );
    }
}
