//! WhatsApp's history at the link (W4): the chunks the phone pushes after a pairing.
//!
//! The phone uploads its history as many `HistorySync` chunks over minutes or hours, each announced
//! by a message from our own phone. Upstream hands each one to the capture hook before it answers the
//! phone, so a chunk is durable in Kourier's own stage file before the phone is told it arrived; the
//! feeder then reads the stage and delivers the messages to the core in rows, once, whatever kills
//! the process in between (the Signal transfer's shape, per chunk).
//!
//! Pieces, as the chunks of W4 add them:
//! - `capture`: one chunk into the stage in one zone, Ok only after its commit, idempotent by the notice's id.
//! - `map` (pure): one message of a chunk -> what the core is given for it (the live road's ids and content, plus
//!   what only a history row has: read, the state of an own message, reactions; what this build cannot show is kept).
//! - `chat` (pure): what a chunk says of a chat besides its messages (the pair of its person, a group's title, the
//!   disappearing timer, a community's parent) and the rule of the unread hint (once per chat, never after a read).
//! - `rest` (pure): the rest of a chunk: its people (pairs, then push names) and the parts that are no message and
//!   are kept for a later build (call records, former members, the phone's address-book names and settings).
//! - `feed`: the feeder, first pass: a captured chunk read once into a plan (its chats in the order of delivery,
//!   its sync type, order and progress), or set aside as bad.
//! - `rows`: the feeder, second pass: a planned chunk cut into the rows the delivery appends (its people, its
//!   chats, its messages), from wherever the cursor stands, within the row bounds.
//! - `reads`: a chat the user read on this computer (a `SyncReadMarker`): the delivery states no unread hint for
//!   it afterwards.
//! - `secrets` (pure): the message secrets of a row's messages, keyed as upstream's own seeding keys them.
//! - `settle`: which delivery rows the core answered for, the window of two unsettled rows, and what is done about a row the
//!   core does not take (taken back, split into singles, dropped and counted; the bounds of idle time and starts).
//! - `pump`: one step of the delivery (settle, then the next row or the next unit of a taken-back one), the shape the
//!   actor's driver runs.
//! - `progress` (pure): which sync types say `has_more`, the order the chunks are delivered in and when a sync
//!   type's push is over (its chats then get their end).
//! - `feeder`: the real [`pump::Rows`]: chooses the chunk, reads its rows on a thread of its own, makes the rows
//!   that end a chunk and the push.
//! - `driver`: the actor's task: waits for a chunk and the open link hold, then steps the pump, wakes the forwarder for
//!   each row appended and waits for the core's acknowledgements.
//! - `deliver`: the delivery zone, one row of a chunk appended to `core_outbox` exactly once: the look at the
//!   cursor, the row's secrets and kept items, the chats' states, the entry and the cursor, in one transaction.
//! - `retention`: a delivered chunk is retired (its bytes dropped), a `bad` one is forgotten after 14 days, and the books
//!   start over when the stage they point into was replaced.
//! - `lost`: a chunk the phone announced and Kourier never captured: a quit waits for the ones in flight (up to 15 s, only
//!   while there are such), the next start counts the rest lost, and the person is told once (the typed notice).
//! - `live`: the live messages of others stage 2 hands over meanwhile, which the phone's unread count includes
//!   and the hint therefore subtracts.
//! - `stage`: the stage file where a captured chunk waits (its own SQLCipher file, key, schema component and
//!   writer thread; opened when first needed; migrated, never removed for a version).
//! - `kill` (tests only): the kill loop. A child process plays a push (four captures, the driver, a fake core) and is
//!   aborted at every hit of the eight kill points; the recovery must end where an unkilled run ends.
//! - `testing` (tests only): the blob builder, hand-made `HistorySync` chunks for the unit tests; the
//!   integration tests include the same source (`tests/support/history.rs`).

pub(crate) mod capture;
pub(crate) mod chat;
pub(crate) mod contacts;
pub(crate) mod deliver;
pub(crate) mod driver;
pub(crate) mod feed;
pub(crate) mod feeder;
#[cfg(test)]
mod kill;
pub(crate) mod live;
pub(crate) mod lost;
pub(crate) mod map;
pub(crate) mod named;
pub(crate) mod progress;
pub(crate) mod pump;
pub(crate) mod reads;
pub(crate) mod rest;
pub(crate) mod retention;
pub(crate) mod rows;
pub(crate) mod secrets;
pub(crate) mod settle;
pub(crate) mod stage;
#[cfg(test)]
pub(crate) mod testing;
