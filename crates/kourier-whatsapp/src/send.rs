//! The send path (W5a). In pieces: `address` (a conversation's id on the wire, the namespace a person's id
//! is written in, the key of a message a modifier or a quote names), `content` (the message each kind of
//! request makes, WhatsApp's markup), `ledger` (the server's answers, committed before the core hears them),
//! `known` (the store's PN <-> LID pairs a send reads) and `started` (D6 in the backend: which direct chats
//! the phone started; the features each chat was stated with). The frame itself leaves through
//! `Session::send` (`session.rs`); the server's answer comes back as an ack (`acks`: the registry the event
//! handler hands it to), and `errors` is the one table of why a send failed (nack codes and failures before
//! the wire). `attempt` is one `Transmit` in that order (the actor runs it off its loop); `receipts` the receipt
//! jobs and their runner, typing, and the commands that only need a confirmation; `tokens` the gate that
//! holds the first sends after a link while the history may still bring the chat's token (D12); `hold` is the reach-out lock
//! as the sends see it (the actor's shared view and the gate asked again right before a frame, fix F6); `stop` is an
//! attempt's stop state (W5b: the X on an upload) and the point of no return right before the frame; `tmp` the files
//! of an upload (the core's file it reads, the sealed temporary file it writes); `route` the one table of how a
//! file goes (its message shape and its upload and HKDF type) and `mp4` the box walk it reads an MP4 with; `upload`
//! the file work (prepare: the core's file into the sealed temporary file; the source the upload reads); `record`
//! the upload record of a sent file (backend.db 6: a resend sends the same file again, a caption edit reads it).

pub(crate) mod acks;
pub(crate) mod address;
pub(crate) mod attempt;
pub(crate) mod content;
pub(crate) mod errors;
pub(crate) mod hold;
pub(crate) mod known;
pub(crate) mod ledger;
pub(crate) mod mp4;
pub(crate) mod receipts;
pub(crate) mod record;
pub(crate) mod route;
pub(crate) mod started;
pub(crate) mod stop;
pub(crate) mod timer;
pub(crate) mod tmp;
pub(crate) mod tokens;
pub(crate) mod upload;
