//! The SQL of the saved filters (core.db `filters`, migration `_filters`): the list in the
//! user's order, a save, a delete and a move. Positions are dense from 0 after every call.
//! Nothing here logs: patterns and names never leave the rows.

use kourier_db::rusqlite::{Connection, params};
use kourier_model::Redacted;

use crate::error::{CoreError, Result};
use crate::filters::{Filter, FilterId, FilterRefusal, FilterSpec};

const COLUMNS: &str = "id, name, text_pattern, sender_pattern, chat_pattern, match_case, notify,
    notify_muted, highlight, colour";

fn pattern(s: Option<String>) -> Option<Redacted<String>> {
    s.filter(|p| !p.is_empty()).map(Redacted::new)
}

/// Every filter, in list order.
pub(crate) fn list(conn: &Connection) -> Result<Vec<Filter>> {
    let sql = format!("SELECT {COLUMNS} FROM filters ORDER BY position");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(Filter {
            id: FilterId(r.get(0)?),
            spec: FilterSpec {
                name: Redacted::new(r.get(1)?),
                text_pattern: pattern(r.get(2)?),
                sender_pattern: pattern(r.get(3)?),
                chat_pattern: pattern(r.get(4)?),
                match_case: r.get(5)?,
                notify: r.get(6)?,
                notify_muted: r.get(7)?,
                highlight: r.get(8)?,
                // A colour that does not fit (a hand-edited file) shows as black, not as an
                // error that hides the whole list.
                colour: u32::try_from(r.get::<_, i64>(9)?).unwrap_or(0),
            },
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn text(p: &Option<Redacted<String>>) -> Option<&str> {
    p.as_ref().map(|p| p.expose().as_str())
}

/// Inserts (`id` `None`, at the end) or updates (`Some`, in place) a VALIDATED spec; the id.
pub(crate) fn save(
    conn: &mut Connection,
    id: Option<FilterId>,
    spec: &FilterSpec,
    now: i64,
) -> Result<FilterId> {
    let tx = conn.transaction()?;
    let out = match id {
        None => {
            let position: i64 = tx.query_row(
                "SELECT coalesce(max(position) + 1, 0) FROM filters",
                [],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO filters(position, name, text_pattern, sender_pattern, chat_pattern,
                   match_case, notify, notify_muted, highlight, colour, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    position,
                    spec.name.expose(),
                    text(&spec.text_pattern),
                    text(&spec.sender_pattern),
                    text(&spec.chat_pattern),
                    spec.match_case,
                    spec.notify,
                    spec.notify_muted,
                    spec.highlight,
                    i64::from(spec.colour),
                    now,
                ],
            )?;
            FilterId(tx.last_insert_rowid())
        }
        Some(id) => {
            let n = tx.execute(
                "UPDATE filters SET name = ?2, text_pattern = ?3, sender_pattern = ?4,
                   chat_pattern = ?5, match_case = ?6, notify = ?7, notify_muted = ?8,
                   highlight = ?9, colour = ?10, updated = ?11 WHERE id = ?1",
                params![
                    id.0,
                    spec.name.expose(),
                    text(&spec.text_pattern),
                    text(&spec.sender_pattern),
                    text(&spec.chat_pattern),
                    spec.match_case,
                    spec.notify,
                    spec.notify_muted,
                    spec.highlight,
                    i64::from(spec.colour),
                    now,
                ],
            )?;
            if n == 0 {
                return Err(CoreError::Filter(FilterRefusal::Unknown));
            }
            id
        }
    };
    tx.commit()?;
    Ok(out)
}

/// The ids in list order.
fn order(tx: &Connection) -> Result<Vec<i64>> {
    let mut stmt = tx.prepare("SELECT id FROM filters ORDER BY position")?;
    let ids = stmt.query_map([], |r| r.get(0))?;
    Ok(ids.collect::<std::result::Result<_, _>>()?)
}

/// Writes `ids` as the order, positions 0.. (the unique key is cleared out of the way first,
/// so no intermediate state meets it).
fn renumber(tx: &Connection, ids: &[i64]) -> Result<()> {
    tx.execute("UPDATE filters SET position = -position - 1", [])?;
    let mut stmt = tx.prepare_cached("UPDATE filters SET position = ?2 WHERE id = ?1")?;
    for (i, id) in ids.iter().enumerate() {
        stmt.execute(params![id, i64::try_from(i).unwrap_or(i64::MAX)])?;
    }
    Ok(())
}

/// Removes a filter; the others close the gap.
pub(crate) fn delete(conn: &mut Connection, id: FilterId) -> Result<()> {
    let tx = conn.transaction()?;
    let gone = tx.execute("DELETE FROM filters WHERE id = ?1", [id.0])?;
    if gone == 0 {
        return Err(CoreError::Filter(FilterRefusal::Unknown));
    }
    let ids = order(&tx)?;
    renumber(&tx, &ids)?;
    tx.commit()?;
    Ok(())
}

/// Moves a filter to position `to` (clamped to the list's end); the others keep their order.
pub(crate) fn move_to(conn: &mut Connection, id: FilterId, to: usize) -> Result<()> {
    let tx = conn.transaction()?;
    let mut ids = order(&tx)?;
    let Some(from) = ids.iter().position(|i| *i == id.0) else {
        return Err(CoreError::Filter(FilterRefusal::Unknown));
    };
    let moved = ids.remove(from);
    ids.insert(to.min(ids.len()), moved);
    renumber(&tx, &ids)?;
    tx.commit()?;
    Ok(())
}

/// Whether some filter has Notify (the catch-up then records its message ids).
/// Judged on the list the set was built from, so an invalid stored row does not count.
pub(crate) fn wants_notify(filters: &[Filter]) -> bool {
    filters.iter().any(|f| f.spec.notify)
}

/// Whether some filter needs the muted chats' live messages (`notify` and `notify_muted`).
/// Judged on the list the set was built from, so an invalid stored row does not count.
pub(crate) fn wants_muted(filters: &[Filter]) -> bool {
    filters.iter().any(|f| f.spec.notify && f.spec.notify_muted)
}
