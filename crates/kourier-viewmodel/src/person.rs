//! The person's page (people pages P1; architecture §8.3): what the details panel shows of one
//! person, built from the core's [`PersonDetail`]. Qt-free: the names, the number rule, the
//! order of the groups and every text live here, the front end only draws them.

use kourier_core::facade::{LinkState, PersonDetail, ShareState};
use kourier_model::{
    AccountId, BlobRef, ConversationId, IdentifierKind, NetworkKind, Redacted, TrustLevel, UserId,
};

use crate::conversations::{
    TimerOption, TimerSpan, network_name, timer_menu, timer_secs, timer_span,
};
use crate::i18n::{trc, trfc};
use crate::names::{fallback_name, single_line};
use crate::preview;

/// How many groups in common the page lists before "Show All" (when it hides at least two).
pub const GROUPS_PREVIEW: usize = 5;

/// One group two people share.
#[derive(Clone, PartialEq)]
pub struct PersonGroup {
    /// The group's conversation.
    pub id: ConversationId,
    /// Its name ("Unnamed group" without one).
    pub title: String,
    /// Its picture URL, empty for initials.
    pub avatar_url: String,
}

crate::safe_debug!(PersonGroup { id });

/// What the details panel shows for a person ([`crate::source::UiSource::take_person`]).
#[derive(Clone, PartialEq)]
pub struct PersonView {
    /// The account the person is seen through.
    pub account: AccountId,
    /// The canonical id (what `RefreshContact` and the open question name).
    pub user: UserId,
    /// The user's own page.
    pub is_self: bool,
    /// The name on top (nickname, address-book name, profile name, else a label made from the
    /// id; the user's own name on their own page). A WhatsApp person's profile name carries
    /// WhatsApp's "~" (B8c).
    pub name: String,
    /// The profile name when it differs from `name`; empty otherwise (marked like `name` on
    /// WhatsApp).
    pub profile_name: String,
    /// The picture URL, empty for initials.
    pub avatar_url: String,
    /// The about text (plain text, untrusted); empty when there is none.
    pub about: String,
    /// The emoji that goes with it; empty when none.
    pub about_emoji: String,
    /// What the page shows of it (F1): the emoji, a space and the about text, as whitelisted HTML
    /// for a selectable text ([`crate::pagetext`]: escaped, a link around each address when
    /// [`Self::about_links`], cut like a group's description); empty when there is nothing.
    pub about_html: String,
    /// Links are made in the about text: only when her 1:1 chat exists and is accepted
    /// ([`crate::pagetext::links_allowed`]); a person without a chat, or whose chat is a request,
    /// is a stranger, and her text is plain.
    pub about_links: bool,
    /// The addresses of the about text's links, in order, each once (the keyboard's menu).
    pub about_hrefs: Vec<String>,
    /// The phone number to show; empty = no line at all (a number the person does not share, or
    /// that nobody told).
    pub number: String,
    /// Short lines under the name ("Blocked", "Not on Signal any more"); usually none.
    pub notes: Vec<String>,
    /// The 1:1 chat with them, when there is one ("Message", the timer).
    pub chat: Option<ConversationId>,
    /// The chat's disappearing timer in seconds (0 = off).
    pub timer_secs: u32,
    /// The timer as the page shows it ("1 week"); empty when off.
    pub timer: String,
    /// The timers the chat offers, "Off" first; empty when the timer cannot be changed here
    /// (no chat, a network without timers, or we cannot send).
    pub timers: Vec<TimerOption>,
    /// The range of "Custom Time…"; `None`: only the offered values.
    pub timer_range: Option<TimerSpan>,
    /// Why the last change of her chat's timer failed ("The disappearing-message timer could
    /// not be changed."), a line under the selector (F4); empty when none did. The source
    /// keeps it with the chat: the next change of any timer or the next page clears it.
    pub timer_failed: String,
    /// Groups both are in, by name.
    pub groups: Vec<PersonGroup>,
    /// How many of `groups` the page lists before "Show All": all of them unless the button would
    /// hide two or more ([`crate::group::preview_rows`], the rule of the members list too).
    pub groups_preview: usize,
    /// "Show All (7)" when the page hides groups; empty otherwise.
    pub groups_more: String,
    /// "View Safety Number" is offered (P5a): the network has safety numbers, and this is
    /// another person known by more than a number. What the level shows, the backend answers.
    pub safety_shown: bool,
    /// The quiet line under it ("Verified", "Safety number changed"); empty otherwise.
    pub safety_state: String,
    /// "Copy Link" is shown (P5b): the network has links to people and this is not the own
    /// page (BQ-4).
    pub link_shown: bool,
    /// Why the button is disabled ("%1's phone number is not shown."), a quiet line under
    /// it and its accessible description; empty when there is a link.
    pub link_note: String,
    /// The link itself. It holds a third party's number: the front end keeps it in Rust (it
    /// never goes into a QML map) and copies it only on the user's click.
    pub link: Option<Redacted<String>>,
    /// "Share Contact…" is shown (P5c): the network sends contact cards and this is not the own
    /// page (BQ-4).
    pub share_shown: bool,
    /// Why it is disabled (Copy Link's number sentences, or "%1 has no name to share.": SC1),
    /// a quiet line under it and its accessible description; empty when a card can be sent.
    /// The card itself is the core's: nothing of it is held here.
    pub share_note: String,
    /// The one line under the picker's title (P5c, SD9): what the card holds, "The card holds
    /// %1 and %2." with the profile name and the number the CORE puts into it
    /// ([`PersonDetail::contact_card`]; this never names them by a rule of its own). Empty when
    /// no card can be sent. Holds a third party's number like `number`: not in `Debug`.
    pub share_card: String,
}

crate::safe_debug!(PersonView {
    account,
    is_self,
    chat,
    timer_secs,
    safety_shown
});

/// Builds the page of `detail`. `avatar` turns a picture into its URL (empty for none).
pub fn person_view(
    detail: &PersonDetail,
    network: NetworkKind,
    avatar: &dyn Fn(Option<&BlobRef>) -> String,
) -> PersonView {
    let contact = detail.contact.as_ref();
    let clean = |s: &Option<String>| {
        s.as_deref()
            .map(single_line)
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let (nickname, system, profile) = match contact {
        Some(c) => (
            clean(&c.nickname),
            clean(&c.system_name),
            clean(&c.profile_name),
        ),
        None => (None, None, None),
    };
    let own = detail.is_self.then(|| clean(&detail.own_name)).flatten();
    // The name the person chose for themself, as it is SHOWN: WhatsApp's own apps put a "~" in
    // front of it, and so does the core's rule for a name (`query::display_name`, his answer
    // "~Name, as the phone", W6a2 B8c); every other network shows it as it is. The contact card
    // is built from the stored name, not from this.
    let profile = profile.map(|p| match network {
        NetworkKind::WhatsApp => format!("~{p}"),
        _ => p,
    });
    // The core's rule for a name (`query::display_name`): nickname, address book, profile; the
    // user's own page takes the account's name first.
    let name = own
        .clone()
        .or_else(|| {
            nickname
                .clone()
                .or_else(|| system.clone())
                .or_else(|| profile.clone())
        })
        .unwrap_or_else(|| fallback_name(&detail.user));
    let profile_name = profile.filter(|p| *p != name).unwrap_or_default();

    // A person known only by their number is named by it: a second line with the same text
    // would only repeat the name.
    let number = Some(shown_number(detail))
        .filter(|n| *n != name)
        .unwrap_or_default();

    let mut notes = Vec::new();
    if contact.is_some_and(|c| c.blocked) || detail.chat.as_ref().is_some_and(|c| c.blocked) {
        notes.push(trc("@info:status a person you blocked", "Blocked").into_owned());
    }
    if contact.is_some_and(|c| c.unregistered_at.is_some()) {
        notes.push(trfc(
            "@info:status a person who left the network, %1 the network",
            "Not on %1 any more",
            &[network_name(network)],
        ));
    }

    let (chat, timer, timers, timer_range) = match &detail.chat {
        Some(c) => {
            let secs = timer_secs(c.timer);
            // A chat the user deleted from the list is still there for "Message", but a change of
            // its timer would send a notice and bring it back (P2 F6).
            // (Nor for a blocked one: the header offers none either, `core_source.rs`, and Apply could only fail.)
            let can_change = c.features.cannot_send.is_none() && !c.hidden && !c.blocked;
            (
                Some(c.id),
                c.timer.map(preview::timer_label).unwrap_or_default(),
                if can_change {
                    timer_menu(&c.features, secs)
                } else {
                    Vec::new()
                },
                if can_change {
                    timer_span(&c.features)
                } else {
                    None
                },
            )
        }
        None => (None, String::new(), Vec::new(), None),
    };

    let mut groups: Vec<PersonGroup> = detail
        .groups
        .iter()
        .map(|g| PersonGroup {
            id: g.id,
            title: g
                .title
                .as_deref()
                .map(single_line)
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    trc(
                        "@item:intext title of a group without a name",
                        "Unnamed group",
                    )
                    .into_owned()
                }),
            avatar_url: avatar(g.avatar.as_ref()),
        })
        .collect();
    // By name, the way a person reads a list (Desktop sorts the same); the id keeps ties stable.
    groups.sort_by_cached_key(|g| (g.title.to_lowercase(), g.id.0));
    let groups_preview = crate::group::preview_rows(groups.len(), GROUPS_PREVIEW);
    let groups_more = if groups_preview < groups.len() {
        trfc(
            "@action:button show every group two people share, %1 how many",
            "Show All (%1)",
            &[&groups.len().to_string()],
        )
    } else {
        String::new()
    };

    // The row (P5a, critic F5): a static capability of the network, never for the own page or
    // a person known by a number alone (no account id the number is made of).
    let safety_shown = detail.safety_numbers && !detail.is_self && !detail.user.is_phone_number();
    // The line follows the contact's trust; a person never verified whose sends wait for the new
    // number (the same fact the level gets as `waits`) says it changed as well (U7).
    let safety_state = match contact.map(|c| c.trust) {
        Some(TrustLevel::Verified) if safety_shown => crate::safety::verified_text(),
        Some(TrustLevel::Changed) if safety_shown => crate::safety::changed_text(),
        _ if safety_shown && detail.safety_waits => crate::safety::changed_text(),
        _ => String::new(),
    };

    // "Copy Link" (P5b): the core made the link with the number rule; the page only words why
    // there is none.
    let (link_shown, link_note, link) = match &detail.link {
        LinkState::Hidden => (false, String::new(), None),
        LinkState::Link(l) => (true, String::new(), Some(l.clone())),
        LinkState::NotShared => (true, not_shared_text(&name), None),
        LinkState::NoNumber => (true, no_number_text(&name), None),
    };

    // "Share Contact…" (P5c): the core decided with the same number rule and the profile name;
    // the page only words why there is no card.
    let (share_shown, share_note) = match detail.share {
        ShareState::Hidden => (false, String::new()),
        ShareState::Ready => (true, String::new()),
        ShareState::NotShared => (true, not_shared_text(&name)),
        ShareState::NoNumber => (true, no_number_text(&name)),
        ShareState::NoName => (true, no_name_text(&name)),
    };

    // The picker's line (P5c, SD9): taken from the card the core would send, so it shows exactly
    // what goes out. Only for a page that offers the card (`Ready`: the network sends cards).
    let share_card = match (detail.share, detail.contact_card()) {
        (ShareState::Ready, Some(card)) => card_line(&card),
        _ => String::new(),
    };

    // The about text with its emoji, as the page shows it. Links only when her 1:1 chat exists and
    // none of the reasons of an unaccepted chat holds (F1, M2), and neither she nor the chat is
    // blocked: no backend states `CannotSend::Blocked`, the block is a fact of its own (H1).
    let about = clean(&contact.and_then(|c| c.about.clone())).unwrap_or_default();
    let about_emoji = clean(&contact.and_then(|c| c.about_emoji.clone())).unwrap_or_default();
    let about_links = !contact.is_some_and(|c| c.blocked)
        && detail.chat.as_ref().is_some_and(|c| {
            !c.blocked && crate::pagetext::links_allowed(c.features.cannot_send.as_ref())
        });
    let shown = match (about_emoji.is_empty(), about.is_empty()) {
        (true, true) => None,
        (true, false) => Some(about.clone()),
        (false, _) => Some(format!("{about_emoji} {about}")),
    };
    let about_text = crate::pagetext::page_text(shown.as_deref(), about_links);

    PersonView {
        account: detail.account,
        user: detail.user.clone(),
        is_self: detail.is_self,
        name,
        profile_name,
        avatar_url: avatar(contact.and_then(|c| c.avatar.as_ref())),
        about,
        about_emoji,
        about_html: about_text.html,
        about_links,
        about_hrefs: about_text.hrefs,
        number,
        notes,
        chat,
        timer_secs: detail.chat.as_ref().map_or(0, |c| timer_secs(c.timer)),
        timer,
        timers,
        timer_range,
        timer_failed: String::new(),
        groups,
        groups_preview,
        groups_more,
        safety_shown,
        safety_state,
        link_shown,
        link_note,
        link,
        share_shown,
        share_note,
        share_card,
    }
}

/// "The card holds <name> and <number>." for the card the core built. A card without a name or
/// a phone number is no card of ours (the core builds neither), so the line stays empty.
fn card_line(card: &kourier_model::ContactCard) -> String {
    let Some(name) = card.name.as_deref() else {
        return String::new();
    };
    let Some(number) = card
        .identifiers
        .iter()
        .find(|i| i.kind == IdentifierKind::Phone)
    else {
        return String::new();
    };
    // TRANSLATORS: %1 is the person's own profile name, %2 their phone number: what the contact
    // card about to be sent contains.
    trfc(
        "@info under the title of the contact card picker, %1 the name and %2 the phone number on the card",
        "The card holds %1 and %2.",
        &[name, &number.value],
    )
}

/// The disabled "Copy Link"'s line when the network states the number is not shown
/// (`LinkState::NotShared`). Neutral on purpose (Z1): Signal states "not shown" also for a person
/// whose profile was never fetched, so the line must not claim that she does not share it.
pub fn not_shared_text(name: &str) -> String {
    trfc(
        "@info under the disabled Copy Link button, %1 the person's name",
        "%1's phone number is not shown.",
        &[name],
    )
}

/// The disabled "Copy Link"'s line when Kourier has no number it may use.
pub fn no_number_text(name: &str) -> String {
    trfc(
        "@info under the disabled Copy Link button, %1 the person's name",
        "Kourier does not know %1's phone number.",
        &[name],
    )
}

/// The disabled "Share Contact…"'s line when the person has no profile name a card may carry
/// (SC1: never the address-book name or a nickname, which are the user's own naming).
pub fn no_name_text(name: &str) -> String {
    trfc(
        "@info under the disabled Share Contact button, %1 the person's name",
        "%1 has no name to share.",
        &[name],
    )
}

/// The number the page may show, or an empty string. Signal's rule is applied below the
/// seam (the contact's `number_shown`); here: a number is shown only when the backend said so
/// (`Some(true)`; "never said" is NOT shown), a person known by their number alone shows it
/// (nothing else names them), and the user's own page shows the account's number.
fn shown_number(detail: &PersonDetail) -> String {
    if detail.is_self {
        return detail
            .own_number
            .as_deref()
            .map(single_line)
            .unwrap_or_default();
    }
    if detail.user.is_phone_number() {
        return fallback_name(&detail.user);
    }
    let Some(contact) = detail.contact.as_ref() else {
        return String::new();
    };
    if contact.number_shown != Some(true) {
        return String::new();
    }
    contact
        .identifiers
        .iter()
        .find(|i| i.kind == IdentifierKind::Phone)
        .map(|i| single_line(&i.value))
        .unwrap_or_default()
}
