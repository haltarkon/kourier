//! The guards of `send` and `act` (T2, decision C7): "the open chat is the one the driver means"
//! and "the act hits exactly this message" are decided ON THE QT THREAD in the very call that
//! sends or acts, never by a look a moment before. The run tool's send run (`cargo xtask wa-run
//! start --for-send`) names the chat and the message in every frame it lets through; a click
//! into another chat between its look at `state` and the frame would otherwise put a synthetic
//! text into a chat the run never remembered, and noticing it afterwards cannot take a frame
//! back.
//!
//! Both checks are plain functions of ids so a unit test can pin them without a window. Without
//! the optional fields the commands behave as they always did (the demo and the smoke tool call
//! them without).

use super::DevError;
use super::protocol::ActOn;

/// Refuses a command that named a chat while another one is open. `open` is the open
/// conversation's id (0: none). Nothing is sent or done when this says no.
pub(crate) fn the_named_chat_is_open(open: i64, wanted: Option<i64>) -> Result<(), DevError> {
    match wanted {
        Some(wanted) if wanted != open => Err(DevError::Refused(
            "refused: the open conversation is not the one the command names".into(),
        )),
        _ => Ok(()),
    }
}

/// Refuses a guarded `send` (or the composer half of a guarded `edit`) unless the composer is PLAIN: no edit in
/// progress (`edit_id` 0), no reply picked (`reply_to_id` 0) and no file in its strip (`files`: the listed ones and
/// the ones still being prepared). The composer is the user's: a reply he picked would ride along with the tool's
/// text (quoting a message of his), an edit in progress would turn the send into an EDIT of another message, and the
/// strip's files would go with the text as their caption (T2 F3, SENDS-3). Nothing is sent when this says no, and the
/// run tool reads the refusal as a nack. Without a named chat the commands behave as ever (demo, smoke).
pub(crate) fn the_composer_is_plain(
    edit_id: i64,
    reply_to_id: i64,
    files: i32,
) -> Result<(), DevError> {
    if edit_id == 0 && reply_to_id == 0 && files == 0 {
        Ok(())
    } else {
        Err(DevError::Refused(
            "refused: the composer is replying / editing / holds files".into(),
        ))
    }
}

/// Refuses a guarded `send` unless the composer holds exactly the `wanted` files, none of them still being prepared,
/// with no edit or reply in progress (the rule of [`the_composer_is_plain`] when `wanted` is 0). A driver that sends
/// files says how many, so a file of the user's in the strip is never taken along with the tool's.
pub(crate) fn the_composer_holds(
    edit_id: i64,
    reply_to_id: i64,
    files: i32,
    preparing: i32,
    wanted: i32,
) -> Result<(), DevError> {
    if edit_id == 0 && reply_to_id == 0 && files == wanted && preparing == 0 {
        Ok(())
    } else {
        Err(DevError::Refused(
            "refused: the composer is replying / editing / holds other files than the send names"
                .into(),
        ))
    }
}

/// The message an `act` takes. `rows` are the messages of the open conversation that are not
/// deleted, newest first, as `(message id if the row has one, own)`. Without `message` it is the
/// newest one of the kind `on` names (the command's old rule: a newest row with no id yet is
/// "not found", an older row is never taken in its place). With `message` it is exactly that
/// one, and only when it is a message of the open conversation, not deleted, of the kind `on`
/// names; anything else is refused, not replaced by "the newest".
pub(crate) fn the_message_to_act_on(
    rows: impl IntoIterator<Item = (Option<i64>, bool)>,
    on: ActOn,
    message: Option<i64>,
) -> Result<i64, DevError> {
    let own = on == ActOn::Own;
    let mut kind_matches = rows.into_iter().filter(|(_, row_own)| *row_own == own);
    match message {
        None => kind_matches
            .next()
            .and_then(|(id, _)| id)
            .ok_or_else(|| DevError::NotFound("no such message in the open conversation".into())),
        Some(wanted) => kind_matches
            .find(|(id, _)| *id == Some(wanted))
            .map(|_| wanted)
            .ok_or_else(|| {
                DevError::Refused(
                    "refused: that is not a message of the open conversation (or it is deleted, or not of the kind \"on\" names)"
                        .into(),
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(r: &Result<impl std::fmt::Debug, DevError>) -> bool {
        matches!(r, Err(DevError::Refused(why)) if why.starts_with("refused:"))
    }

    #[test]
    fn a_named_chat_must_be_the_open_one_and_no_name_is_no_check() {
        assert!(the_named_chat_is_open(7, None).is_ok());
        assert!(the_named_chat_is_open(0, None).is_ok());
        assert!(the_named_chat_is_open(7, Some(7)).is_ok());
        assert!(refused(&the_named_chat_is_open(7, Some(8))));
        // No chat open is not the named one either.
        assert!(refused(&the_named_chat_is_open(0, Some(7))));
    }

    #[test]
    fn a_guarded_send_goes_only_from_a_plain_composer() {
        assert!(the_composer_is_plain(0, 0, 0).is_ok());
        // Each thing that makes the composer the user's alone refuses, nothing else is needed.
        assert!(
            refused(&the_composer_is_plain(5, 0, 0)),
            "an edit in progress"
        );
        assert!(
            refused(&the_composer_is_plain(0, 9, 0)),
            "a reply he picked"
        );
        assert!(
            refused(&the_composer_is_plain(0, 0, 1)),
            "a file in the strip"
        );
        assert!(refused(&the_composer_is_plain(5, 9, 3)));
        let why = match the_composer_is_plain(0, 9, 0) {
            Err(DevError::Refused(why)) => why,
            other => panic!("{other:?}"),
        };
        assert!(why.contains("replying") && why.contains("editing") && why.contains("files"));
    }

    #[test]
    fn a_guarded_send_takes_exactly_the_files_it_names() {
        assert!(the_composer_holds(0, 0, 0, 0, 0).is_ok());
        assert!(the_composer_holds(0, 0, 1, 0, 1).is_ok());
        assert!(
            refused(&the_composer_holds(0, 0, 1, 0, 0)),
            "a file it does not name"
        );
        assert!(refused(&the_composer_holds(0, 0, 0, 0, 1)), "no file");
        assert!(refused(&the_composer_holds(0, 0, 2, 0, 1)), "one too many");
        assert!(
            refused(&the_composer_holds(0, 0, 1, 1, 1)),
            "still preparing"
        );
        assert!(refused(&the_composer_holds(5, 0, 1, 0, 1)), "an edit");
        assert!(refused(&the_composer_holds(0, 9, 1, 0, 1)), "a reply");
    }

    #[test]
    fn a_named_message_is_the_only_one_taken_and_only_if_it_is_of_the_kind() {
        // Newest first: 9 is the peer's, 8 and 6 are ours, 5 the peer's.
        let rows = [
            (Some(9), false),
            (Some(8), true),
            (Some(6), true),
            (Some(5), false),
        ];
        // Without a name: the newest of the kind, as before.
        assert_eq!(the_message_to_act_on(rows, ActOn::Own, None).ok(), Some(8));
        assert_eq!(
            the_message_to_act_on(rows, ActOn::Other, None).ok(),
            Some(9)
        );
        // With a name: exactly that one, though a newer one of the kind exists.
        assert_eq!(
            the_message_to_act_on(rows, ActOn::Own, Some(6)).ok(),
            Some(6)
        );
        // A message of another chat, a deleted one (not in `rows`) and a message of the other
        // kind are all refused, never replaced by the newest.
        assert!(refused(&the_message_to_act_on(
            rows,
            ActOn::Own,
            Some(1_000)
        )));
        assert!(refused(&the_message_to_act_on(rows, ActOn::Own, Some(9))));
        assert!(refused(&the_message_to_act_on(rows, ActOn::Other, Some(8))));
        // No message of the kind and no name: the old "not found".
        assert!(matches!(
            the_message_to_act_on([(Some(5), false)], ActOn::Own, None),
            Err(DevError::NotFound(_))
        ));
        assert!(refused(&the_message_to_act_on(
            [(Some(5), false)],
            ActOn::Own,
            Some(5)
        )));
        // The newest own row has no id yet (still being made): "not found", and the older own
        // message is NOT taken in its place; a named older one is still found.
        let pending = [(None, true), (Some(4), true)];
        assert!(matches!(
            the_message_to_act_on(pending, ActOn::Own, None),
            Err(DevError::NotFound(_))
        ));
        assert_eq!(
            the_message_to_act_on(pending, ActOn::Own, Some(4)).ok(),
            Some(4)
        );
    }
}
