//! The TL objects the fake DC answers with (layer 232: the generated types of the vendored
//! grammers-tl-types, `vendor/grammers-tl-types/VENDORED.md`). Every field is written out, so a layer bump
//! fails to compile here first. Written by the spike's script from api.tl (2026-10-16); the addresses and
//! host names now come from the dialled destination and `example.invalid`.

use grammers_tl_types as tl;
use std::net::SocketAddr;

/// The host every link the fake DC hands out points to (never a Telegram host).
pub const LINK_HOST: &str = "example.invalid";
/// A user as the server sends it (`is_self` for the account itself).
pub fn user(id: i64, first: &str, phone: &str, is_self: bool) -> tl::enums::User {
    tl::enums::User::User(tl::types::User {
        is_self,
        contact: !is_self,
        mutual_contact: !is_self,
        deleted: false,
        bot: false,
        bot_chat_history: false,
        bot_nochats: false,
        verified: false,
        restricted: false,
        min: false,
        bot_inline_geo: false,
        support: false,
        scam: false,
        apply_min_photo: false,
        fake: false,
        bot_attach_menu: false,
        premium: false,
        attach_menu_enabled: false,
        bot_can_edit: false,
        close_friend: false,
        stories_hidden: false,
        stories_unavailable: false,
        contact_require_premium: false,
        bot_business: false,
        bot_has_main_app: false,
        bot_forum_view: false,
        bot_forum_can_manage_topics: false,
        bot_can_manage_bots: false,
        bot_guestchat: false,
        bot_guard: false,
        id,
        access_hash: Some(1234567),
        first_name: Some(first.to_string()),
        last_name: None,
        username: None,
        phone: Some(phone.to_string()),
        photo: None,
        status: Some(tl::enums::UserStatus::Empty),
        bot_info_version: None,
        restriction_reason: None,
        bot_inline_placeholder: None,
        lang_code: None,
        emoji_status: None,
        usernames: None,
        stories_max_id: None,
        color: None,
        profile_color: None,
        bot_active_users: None,
        bot_verification_icon: None,
        send_paid_messages_stars: None,
        linked_community_id: None,
    })
}
/// A peer as the server names it.
pub fn peer_user(user_id: i64) -> tl::enums::Peer {
    tl::enums::Peer::User(tl::types::PeerUser { user_id })
}

/// What [`message`] writes: `peer` is the chat (for a private chat the OTHER user, also for an
/// outgoing message), `from` the sender (`None` for a broadcast channel's post).
pub struct MessageParts<'a> {
    pub id: i32,
    pub peer: tl::enums::Peer,
    pub from: Option<i64>,
    pub out: bool,
    pub post: bool,
    pub date: i32,
    pub text: &'a str,
    pub edit_date: Option<i32>,
}

/// A text message.
pub fn message(m: MessageParts<'_>) -> tl::enums::Message {
    let MessageParts {
        id,
        peer,
        from,
        out,
        post,
        date,
        text,
        edit_date,
    } = m;
    tl::enums::Message::Message(tl::types::Message {
        out,
        mentioned: false,
        media_unread: false,
        silent: false,
        post,
        from_scheduled: false,
        legacy: false,
        edit_hide: false,
        pinned: false,
        noforwards: false,
        invert_media: false,
        offline: false,
        video_processing_pending: false,
        paid_suggested_post_stars: false,
        paid_suggested_post_ton: false,
        id,
        from_id: from.map(peer_user),
        from_boosts_applied: None,
        from_rank: None,
        peer_id: peer,
        saved_peer_id: None,
        fwd_from: None,
        via_bot_id: None,
        via_business_bot_id: None,
        guestchat_via_from: None,
        reply_to: None,
        date,
        message: text.to_string(),
        media: None,
        reply_markup: None,
        entities: None,
        views: None,
        forwards: None,
        replies: None,
        edit_date,
        post_author: None,
        grouped_id: None,
        reactions: None,
        restriction_reason: None,
        ttl_period: None,
        quick_reply_shortcut_id: None,
        effect: None,
        factcheck: None,
        report_delivery_until_date: None,
        paid_message_stars: None,
        suggested_post: None,
        schedule_repeat_period: None,
        summary_from_language: None,
        rich_message: None,
    })
}
/// `help.getConfig`: this DC (`this_dc`) at the address the client dialled, links on [`LINK_HOST`].
pub fn config(now: i32, this_dc: i32, dialled: SocketAddr) -> tl::enums::Config {
    tl::enums::Config::Config(tl::types::Config {
        default_p2p_contacts: false,
        preload_featured_stickers: false,
        revoke_pm_inbox: false,
        blocked_mode: false,
        force_try_ipv6: false,
        date: now,
        expires: now + 3600,
        test_mode: true,
        this_dc,
        dc_options: vec![tl::enums::DcOption::Option(tl::types::DcOption {
            ipv6: dialled.is_ipv6(),
            media_only: false,
            tcpo_only: false,
            cdn: false,
            r#static: false,
            this_port_only: false,
            id: this_dc,
            ip_address: dialled.ip().to_string(),
            port: i32::from(dialled.port()),
            secret: None,
        })],
        dc_txt_domain_name: LINK_HOST.into(),
        chat_size_max: 200,
        megagroup_size_max: 200000,
        forwarded_count_max: 100,
        online_update_period_ms: 120000,
        offline_blur_timeout_ms: 5000,
        offline_idle_timeout_ms: 30000,
        online_cloud_timeout_ms: 300000,
        notify_cloud_delay_ms: 30000,
        notify_default_delay_ms: 1500,
        push_chat_period_ms: 60000,
        push_chat_limit: 2,
        edit_time_limit: 172800,
        revoke_time_limit: 2147483647,
        revoke_pm_time_limit: 2147483647,
        rating_e_decay: 2419200,
        stickers_recent_limit: 200,
        channels_read_media_period: 604800,
        tmp_sessions: None,
        call_receive_timeout_ms: 20000,
        call_ring_timeout_ms: 90000,
        call_connect_timeout_ms: 30000,
        call_packet_timeout_ms: 10000,
        me_url_prefix: format!("https://{LINK_HOST}/"),
        autoupdate_url_prefix: None,
        gif_search_username: None,
        venue_search_username: None,
        img_search_username: None,
        static_maps_provider: None,
        caption_length_max: 1024,
        message_length_max: 4096,
        webfile_dc_id: 4,
        suggested_lang_code: None,
        lang_pack_version: None,
        base_lang_pack_version: None,
        reactions_default: None,
        autologin_token: None,
    })
}

/// A basic group the account is a member of.
/// `migrated_to`: the supergroup it became (the group is then deactivated).
pub fn basic_chat(
    id: i64,
    title: &str,
    participants: i32,
    date: i32,
    migrated_to: Option<i64>,
) -> tl::enums::Chat {
    tl::enums::Chat::Chat(tl::types::Chat {
        creator: false,
        left: false,
        deactivated: migrated_to.is_some(),
        call_active: false,
        call_not_empty: false,
        noforwards: false,
        id,
        title: title.to_string(),
        photo: tl::enums::ChatPhoto::Empty,
        participants_count: participants,
        date,
        version: 1,
        migrated_to: migrated_to.map(|channel_id| {
            tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id,
                access_hash: CHANNEL_ACCESS_HASH,
            })
        }),
        admin_rights: None,
        default_banned_rights: None,
    })
}

/// The access hash of every channel the fake DC hands out (TDLib sends it back in `InputChannel`).
pub const CHANNEL_ACCESS_HASH: i64 = 7654321;

/// A supergroup (`broadcast` false) or a broadcast channel the account is a member of.
pub fn channel(
    id: i64,
    title: &str,
    username: Option<&str>,
    broadcast: bool,
    date: i32,
) -> tl::enums::Chat {
    tl::enums::Chat::Channel(tl::types::Channel {
        creator: false,
        left: false,
        broadcast,
        verified: false,
        megagroup: !broadcast,
        restricted: false,
        signatures: false,
        min: false,
        scam: false,
        has_link: false,
        has_geo: false,
        slowmode_enabled: false,
        call_active: false,
        call_not_empty: false,
        fake: false,
        gigagroup: false,
        noforwards: false,
        join_to_send: false,
        join_request: false,
        forum: false,
        stories_hidden: false,
        stories_hidden_min: false,
        stories_unavailable: false,
        signature_profiles: false,
        autotranslation: false,
        broadcast_messages_allowed: false,
        monoforum: false,
        forum_tabs: false,
        in_news: false,
        id,
        access_hash: Some(CHANNEL_ACCESS_HASH),
        title: title.to_string(),
        username: username.map(str::to_string),
        photo: tl::enums::ChatPhoto::Empty,
        date,
        restriction_reason: None,
        admin_rights: None,
        banned_rights: None,
        default_banned_rights: None,
        participants_count: None,
        usernames: None,
        stories_max_id: None,
        color: None,
        profile_color: None,
        emoji_status: None,
        level: None,
        subscription_until_date: None,
        bot_verification_icon: None,
        send_paid_messages_stars: None,
        linked_monoforum_id: None,
        linked_community_id: None,
        pinned_msg_id: None,
    })
}

/// A service message (`m.text` is not used).
pub fn service_message(
    m: MessageParts<'_>,
    action: tl::enums::MessageAction,
) -> tl::enums::Message {
    let MessageParts {
        id,
        peer,
        from,
        out,
        post,
        date,
        ..
    } = m;
    tl::enums::Message::Service(tl::types::MessageService {
        out,
        mentioned: false,
        media_unread: false,
        reactions_are_possible: false,
        silent: false,
        post,
        legacy: false,
        id,
        from_id: from.map(peer_user),
        peer_id: peer,
        saved_peer_id: None,
        reply_to: None,
        date,
        action,
        reactions: None,
        ttl_period: None,
    })
}

/// The default notification settings (nothing set: the client's defaults).
pub fn notify_settings() -> tl::enums::PeerNotifySettings {
    tl::enums::PeerNotifySettings::Settings(tl::types::PeerNotifySettings {
        show_previews: None,
        silent: None,
        mute_until: None,
        ios_sound: None,
        android_sound: None,
        other_sound: None,
        stories_muted: None,
        stories_hide_sender: None,
        stories_ios_sound: None,
        stories_android_sound: None,
        stories_other_sound: None,
    })
}

/// What [`channel_full`] writes.
pub struct ChannelFullParts {
    pub id: i64,
    pub read_inbox_max_id: i32,
    pub read_outbox_max_id: i32,
    pub unread_count: i32,
    pub pts: i32,
    /// The basic group it was made from and that group's last message id.
    pub migrated_from: Option<(i64, i32)>,
}

/// A supergroup's or channel's full info (no about text, no photo, nothing optional).
pub fn channel_full(p: ChannelFullParts) -> tl::enums::ChatFull {
    tl::enums::ChatFull::ChannelFull(tl::types::ChannelFull {
        can_view_participants: false,
        can_set_username: false,
        can_set_stickers: false,
        hidden_prehistory: false,
        can_set_location: false,
        has_scheduled: false,
        can_view_stats: false,
        blocked: false,
        can_delete_channel: false,
        antispam: false,
        participants_hidden: false,
        translations_disabled: false,
        stories_pinned_available: false,
        view_forum_as_messages: false,
        restricted_sponsored: false,
        can_view_revenue: false,
        paid_media_allowed: false,
        can_view_stars_revenue: false,
        paid_reactions_available: false,
        stargifts_available: false,
        paid_messages_available: false,
        has_welcome_messages: false,
        id: p.id,
        about: String::new(),
        participants_count: Some(3),
        admins_count: None,
        kicked_count: None,
        banned_count: None,
        online_count: None,
        read_inbox_max_id: p.read_inbox_max_id,
        read_outbox_max_id: p.read_outbox_max_id,
        unread_count: p.unread_count,
        chat_photo: tl::enums::Photo::Empty(tl::types::PhotoEmpty { id: 0 }),
        notify_settings: notify_settings(),
        exported_invite: None,
        bot_info: Vec::new(),
        migrated_from_chat_id: p.migrated_from.map(|(c, _)| c),
        migrated_from_max_id: p.migrated_from.map(|(_, m)| m),
        pinned_msg_id: None,
        stickerset: None,
        available_min_id: None,
        folder_id: None,
        linked_chat_id: None,
        location: None,
        slowmode_seconds: None,
        slowmode_next_send_date: None,
        stats_dc: None,
        pts: p.pts,
        call: None,
        ttl_period: None,
        pending_suggestions: None,
        groupcall_default_join_as: None,
        theme_emoticon: None,
        requests_pending: None,
        recent_requesters: None,
        default_send_as: None,
        available_reactions: None,
        reactions_limit: None,
        stories: None,
        wallpaper: None,
        boosts_applied: None,
        boosts_unrestrict: None,
        emojiset: None,
        bot_verification: None,
        stargifts_count: None,
        send_paid_messages_stars: None,
        main_tab: None,
        guard_bot_id: None,
        share_theme: None,
    })
}

/// A basic group's full info: its members (each invited by the first, at `date`).
pub fn chat_full(id: i64, members: &[i64], date: i32) -> tl::enums::ChatFull {
    let inviter = members.first().copied().unwrap_or(0);
    tl::enums::ChatFull::Full(tl::types::ChatFull {
        can_set_username: false,
        has_scheduled: false,
        translations_disabled: false,
        has_welcome_messages: false,
        id,
        about: String::new(),
        participants: tl::enums::ChatParticipants::Participants(tl::types::ChatParticipants {
            chat_id: id,
            participants: members
                .iter()
                .map(|u| {
                    tl::enums::ChatParticipant::Participant(tl::types::ChatParticipant {
                        user_id: *u,
                        inviter_id: inviter,
                        date,
                        rank: None,
                    })
                })
                .collect(),
            version: 1,
        }),
        chat_photo: None,
        notify_settings: notify_settings(),
        exported_invite: None,
        bot_info: None,
        pinned_msg_id: None,
        folder_id: None,
        call: None,
        ttl_period: None,
        groupcall_default_join_as: None,
        theme_emoticon: None,
        requests_pending: None,
        recent_requesters: None,
        available_reactions: None,
        reactions_limit: None,
    })
}

/// An incoming call from `caller` (`phoneCallRequested`; with patch 0002 TDLib never answers it).
pub fn call_requested(
    id: i64,
    access_hash: i64,
    date: i32,
    caller: i64,
    callee: i64,
) -> tl::enums::PhoneCall {
    tl::enums::PhoneCall::Requested(tl::types::PhoneCallRequested {
        video: false,
        id,
        access_hash,
        date,
        admin_id: caller,
        participant_id: callee,
        g_a_hash: vec![0x42; 32],
        protocol: tl::enums::PhoneCallProtocol::Protocol(tl::types::PhoneCallProtocol {
            udp_p2p: true,
            udp_reflector: true,
            min_layer: 65,
            max_layer: 92,
            library_versions: Vec::new(),
        }),
    })
}

/// The call ended unanswered (`phoneCallDiscarded`, reason missed).
pub fn call_missed(id: i64) -> tl::enums::PhoneCall {
    tl::enums::PhoneCall::Discarded(tl::types::PhoneCallDiscarded {
        need_rating: false,
        need_debug: false,
        video: false,
        id,
        reason: Some(tl::enums::PhoneCallDiscardReason::Missed),
        duration: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::{Identifiable, Serializable};

    /// The constructor id a boxed object starts with.
    fn id_of(bytes: &[u8]) -> u32 {
        u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"))
    }

    /// The two objects layer 232 renamed that the fake DC serves (a supergroup in every chat list and
    /// `channels.getFullChannel`): TDLib 1.8.69 cannot parse the layer-230 ids (`d49f34c6`, `a04e8d3a`),
    /// so a schema that falls back to 230 fails here, not at the re-record.
    #[test]
    fn a_supergroup_and_its_full_info_are_written_with_the_layer_232_ids() {
        assert_eq!(tl::LAYER, 232, "the vendored schema is not layer 232");
        assert_eq!(tl::types::Channel::CONSTRUCTOR_ID, 0x7ad1_5240);
        assert_eq!(tl::types::ChannelFull::CONSTRUCTOR_ID, 0xc4c4_dd40);
        let chat = channel(401, "A group", Some("agroup"), false, 1_800_000_000);
        assert_eq!(id_of(&chat.to_bytes()), 0x7ad1_5240, "channel");
        let full = channel_full(ChannelFullParts {
            id: 401,
            read_inbox_max_id: 0,
            read_outbox_max_id: 0,
            unread_count: 0,
            pts: 1,
            migrated_from: None,
        });
        assert_eq!(id_of(&full.to_bytes()), 0xc4c4_dd40, "channelFull");
        assert_eq!(tl::name_for_id(0x7ad1_5240), "channel");
        assert_eq!(tl::name_for_id(0xc4c4_dd40), "channelFull");
        for old in [0xd49f_34c6u32, 0xa04e_8d3a] {
            let name = tl::name_for_id(old);
            assert!(
                name != "channel" && name != "channelFull",
                "a layer-230 id is still known as {name}"
            );
        }
    }
}
