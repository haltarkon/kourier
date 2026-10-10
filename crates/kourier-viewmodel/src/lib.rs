//! Qt-free UI projections (architecture §8.4): rows for the accounts drawer, the conversation
//! list and the timeline; grouping flags, date headers and the unread marker; previews and
//! relative time labels; message text as whitelisted RichText with spoiler ranges; UTF-16
//! helpers for the composer; the `ListOp` diff engine; and a fixture data set.
//!
//! Nothing here touches Qt or a database: the Qt layer (`kourier-qt`) converts rows to
//! `QString`/`QVariant` and applies [`diff::ListOp`]s, and a [`source::UiSource`] supplies the
//! rows: [`CoreSource`] over the core facade (§8.5), or the fixture for pure UI tests.
//! Everything is tested with plain `cargo test`. Of `kourier_core` only `facade` is used.
//!
//! Log safety (§9.1): rows contain names and message texts. Never log a row or a field of one.
//! The `Debug` impls of rows and blocks print only ids, kinds, flags and counts.

/// Implements a log-safe `Debug` (§9.1) that prints only the listed fields (ids, kinds, flags,
/// counts) and leaves names and texts out.
macro_rules! safe_debug {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl ::std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), &self.$field))*
                    .finish_non_exhaustive()
            }
        }
    };
}
pub(crate) use safe_debug;

pub mod composer;
pub mod conversations;
pub mod core_source;
pub mod diff;
pub mod edit;
pub mod files;
pub mod fixture;
pub mod i18n;
pub mod login;
pub mod media;
pub mod names;
pub mod notify;
pub mod preview;
pub mod readmore;
pub mod richtext;
pub mod source;
pub mod time;
pub mod timeline;
pub mod transfer;

pub use conversations::{
    AccountRow, AccountStatus, ChatHeader, ConversationFilter, ConversationRow, PreviewSource,
};
pub use core_source::{
    AvatarUrls, Changes, CommandSink, CoreSource, CoreSourceConfig, Paging, QueryTracker,
};
pub use diff::{ListOp, RoleSet};
pub use files::{
    ArchiveRule, Counted, FileFacts, FileRow, FileStrip, FolderCount, FolderJob, FolderQuestion,
    FolderStage, HandedFile, ListedFile, PlanAnswer, PlanRequest, StripRules, StripSummary,
};
pub use login::{LoginView, LoginViewKind, NetworkChoice};
pub use names::{NameMap, Names};
pub use readmore::ReadMore;
pub use richtext::{RichText, Span16};
pub use source::{
    EmptySource, FinishedSend, FixtureSource, MediaAnswer, RefusedSend, ReturnedFile, RevisionList,
    RevisionRow, SendOutcome, SourceError, UiSource,
};
pub use time::{Calendar, FixedOffset, TimeContext};
pub use timeline::{Block, MessageRow, Receipt, RowKey, RowKind};
pub use transfer::{ReplaceQuestion, TransferPhase, TransferView};
