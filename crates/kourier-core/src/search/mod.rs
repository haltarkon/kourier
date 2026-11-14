//! Message search (G1): what the index holds and how a typed query is cut into terms.
//!
//! G1a is the pure half and nothing else: [`normalize::normalize`] turns a text into tokens
//! (each with its byte range in the source), [`normalize::words`] says what a message body
//! puts into the index, and [`normalize::query_terms`] cuts a typed query. G1s:
//! [`snippet::snippet`] cuts the piece of a hit's message that shows, with its spoiler and
//! mention ranges, and drops a stale row or joined terms that do not stand side by side. G1b to
//! G1d write the index into its own file, `search.db` ([`file`]), from the log core.db keeps
//! ([`log`]), on its own writer ([`index`], [`compact`]); G1e asks it ([`query`]:
//! `Query::Search`), each hit looked up in core.db. The
//! rules of the tokens are the whole contract between what is written into `messages_fts` and
//! what is asked of it: one function for both, so a change of a rule is a change of
//! [`NORMALIZER_VERSION`].
//!
//! Nothing here logs a text, a name or a number, and no `Debug` shows one: counts only
//! (architecture §9.1).

pub(crate) mod compact;
pub(crate) mod file;
pub(crate) mod index;
pub(crate) mod log;
pub(crate) mod normalize;
pub(crate) mod pattern;
pub(crate) mod query;
pub(crate) mod snippet;

/// The shortest term, in characters, that a query matches as a PREFIX ("фай" finds "файли").
/// A shorter term matches whole words only. His answer on short letters (Q4) changes this
/// one constant: 2 is the default (prefixes from the second letter; the FTS5 table carries
/// no `prefix=` option, so the choice costs time and no file size).
pub(crate) const MIN_PREFIX: usize = 2;

/// The version of the token rules. The index records the version it was written with
/// (search.db's `meta 'norm'`); a bump builds the file again and the paced backfill writes it. Raise it
/// with ANY change to `normalize`, `fold_token`, `words` or `searched_texts`: a token that
/// changes without it leaves old rows under the old form for good.
pub(crate) const NORMALIZER_VERSION: u32 = 1;
