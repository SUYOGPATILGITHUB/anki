// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use super::parser::{Node, PropertyKind, SearchNode, StateKind, TemplateKind};
use crate::card::CardQueue;
use crate::decks::child_ids;
use crate::decks::get_deck;
use crate::err::{AnkiError, Result};
use crate::notes::field_checksum;
use crate::text::matches_wildcard;
use crate::{
    collection::RequestContext, text::strip_html_preserving_image_filenames, types::ObjID,
};
use std::fmt::Write;

struct SqlWriter<'a, 'b> {
    req: &'a mut RequestContext<'b>,
    sql: String,
    args: Vec<String>,
}

pub(super) fn node_to_sql(req: &mut RequestContext, node: &Node) -> Result<(String, Vec<String>)> {
    let mut sctx = SqlWriter::new(req);
    sctx.write_node_to_sql(&node)?;
    Ok((sctx.sql, sctx.args))
}

impl SqlWriter<'_, '_> {
    fn new<'a, 'b>(req: &'a mut RequestContext<'b>) -> SqlWriter<'a, 'b> {
        let sql = String::new();
        let args = vec![];
        SqlWriter { req, sql, args }
    }

    fn write_node_to_sql(&mut self, node: &Node) -> Result<()> {
        match node {
            Node::And => write!(self.sql, " and ").unwrap(),
            Node::Or => write!(self.sql, " or ").unwrap(),
            Node::Not(node) => {
                write!(self.sql, "not ").unwrap();
                self.write_node_to_sql(node)?;
            }
            Node::Group(nodes) => {
                write!(self.sql, "(").unwrap();
                for node in nodes {
                    self.write_node_to_sql(node)?;
                }
                write!(self.sql, ")").unwrap();
            }
            Node::Search(search) => self.write_search_node_to_sql(search)?,
        };
        Ok(())
    }

    fn write_search_node_to_sql(&mut self, node: &SearchNode) -> Result<()> {
        match node {
            SearchNode::UnqualifiedText(text) => self.write_unqualified(text),
            SearchNode::SingleField { field, text } => {
                self.write_single_field(field.as_ref(), text.as_ref())?
            }
            SearchNode::AddedInDays(days) => self.write_added(*days)?,
            SearchNode::CardTemplate(template) => self.write_template(template)?,
            SearchNode::Deck(deck) => self.write_deck(deck.as_ref())?,
            SearchNode::NoteTypeID(ntid) => {
                write!(self.sql, "n.mid = {}", ntid).unwrap();
            }
            SearchNode::NoteType(notetype) => self.write_note_type(notetype.as_ref())?,
            SearchNode::Rated { days, ease } => self.write_rated(*days, *ease)?,
            SearchNode::Tag(tag) => self.write_tag(tag),
            SearchNode::Duplicates { note_type_id, text } => self.write_dupes(*note_type_id, text),
            SearchNode::State(state) => self.write_state(state)?,
            SearchNode::Flag(flag) => {
                write!(self.sql, "(c.flags & 7) == {}", flag).unwrap();
            }
            SearchNode::NoteIDs(nids) => {
                write!(self.sql, "n.id in ({})", nids).unwrap();
            }
            SearchNode::CardIDs(cids) => {
                write!(self.sql, "c.id in ({})", cids).unwrap();
            }
            SearchNode::Property { operator, kind } => self.write_prop(operator, kind)?,
        };
        Ok(())
    }

    fn write_unqualified(&mut self, text: &str) {
        // implicitly wrap in %
        let text = format!("%{}%", text);
        self.args.push(text.into());
        write!(
            self.sql,
            "(n.sfld like ?{n} escape '\\' or n.flds like ?{n} escape '\\')",
            n = self.args.len(),
        )
        .unwrap();
    }

    fn write_tag(&mut self, text: &str) {
        if text == "none" {
            write!(self.sql, "n.tags = ''").unwrap();
            return;
        }

        let tag = format!(" %{}% ", text.replace('*', "%"));
        write!(self.sql, "n.tags like ?").unwrap();
        self.args.push(tag.into());
    }

    fn write_rated(&mut self, days: u32, ease: Option<u8>) -> Result<()> {
        let today_cutoff = self.req.storage.timing_today()?.next_day_at;
        let days = days.min(31) as i64;
        let target_cutoff = today_cutoff - 86_400 * days;
        write!(
            self.sql,
            "c.id in (select cid from revlog where id>{}",
            target_cutoff
        )
        .unwrap();
        if let Some(ease) = ease {
            write!(self.sql, "and ease={})", ease).unwrap();
        } else {
            write!(self.sql, ")").unwrap();
        }

        Ok(())
    }

    fn write_prop(&mut self, op: &str, kind: &PropertyKind) -> Result<()> {
        let timing = self.req.storage.timing_today()?;
        match kind {
            PropertyKind::Due(days) => {
                let day = days + (timing.days_elapsed as i32);
                write!(
                    self.sql,
                    "(c.queue in ({rev},{daylrn}) and due {op} {day})",
                    rev = CardQueue::Review as u8,
                    daylrn = CardQueue::DayLearn as u8,
                    op = op,
                    day = day
                )
            }
            PropertyKind::Interval(ivl) => write!(self.sql, "ivl {} {}", op, ivl),
            PropertyKind::Reps(reps) => write!(self.sql, "reps {} {}", op, reps),
            PropertyKind::Lapses(days) => write!(self.sql, "lapses {} {}", op, days),
            PropertyKind::Ease(ease) => write!(self.sql, "ease {} {}", op, (ease * 1000.0) as u32),
        }
        .unwrap();
        Ok(())
    }

    fn write_state(&mut self, state: &StateKind) -> Result<()> {
        let timing = self.req.storage.timing_today()?;
        match state {
            StateKind::New => write!(self.sql, "c.queue = {}", CardQueue::New as u8),
            StateKind::Review => write!(self.sql, "c.queue = {}", CardQueue::Review as u8),
            StateKind::Learning => write!(
                self.sql,
                "c.queue in ({},{})",
                CardQueue::Learn as u8,
                CardQueue::DayLearn as u8
            ),
            StateKind::Buried => write!(
                self.sql,
                "c.queue in ({},{})",
                CardQueue::SchedBuried as u8,
                CardQueue::UserBuried as u8
            ),
            StateKind::Suspended => write!(self.sql, "c.queue = {}", CardQueue::Suspended as u8),
            StateKind::Due => write!(
                self.sql,
                "
    (c.queue in ({rev},{daylrn}) and c.due <= {today}) or
    (c.queue = {lrn} and c.due <= {daycutoff})",
                rev = CardQueue::Review as u8,
                daylrn = CardQueue::DayLearn as u8,
                today = timing.days_elapsed,
                lrn = CardQueue::Learn as u8,
                daycutoff = timing.next_day_at,
            ),
        }
        .unwrap();
        Ok(())
    }

    fn write_deck(&mut self, deck: &str) -> Result<()> {
        match deck {
            "*" => write!(self.sql, "true").unwrap(),
            "filtered" => write!(self.sql, "c.odid > 0").unwrap(),
            deck => {
                let all_decks: Vec<_> = self
                    .req
                    .storage
                    .all_decks()?
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect();
                let dids_with_children = if deck == "current" {
                    let config = self.req.storage.all_config()?;
                    let mut dids_with_children = vec![config.current_deck_id];
                    let current = get_deck(&all_decks, config.current_deck_id)
                        .ok_or_else(|| AnkiError::invalid_input("invalid current deck"))?;
                    for child_did in child_ids(&all_decks, &current.name) {
                        dids_with_children.push(child_did);
                    }
                    dids_with_children
                } else {
                    let mut dids_with_children = vec![];
                    for deck in all_decks.iter().filter(|d| matches_wildcard(&d.name, deck)) {
                        dids_with_children.push(deck.id);
                        for child_id in child_ids(&all_decks, &deck.name) {
                            dids_with_children.push(child_id);
                        }
                    }
                    dids_with_children
                };

                self.sql.push_str("c.did in ");
                ids_to_string(&mut self.sql, &dids_with_children);
            }
        };
        Ok(())
    }

    fn write_template(&mut self, template: &TemplateKind) -> Result<()> {
        match template {
            TemplateKind::Ordinal(n) => {
                write!(self.sql, "c.ord = {}", n).unwrap();
            }
            TemplateKind::Name(name) => {
                let note_types = self.req.storage.all_note_types()?;
                let mut id_ords = vec![];
                for nt in note_types.values() {
                    for tmpl in &nt.templates {
                        if matches_wildcard(&tmpl.name, name) {
                            id_ords.push((nt.id, tmpl.ord));
                        }
                    }
                }

                // sort for the benefit of unit tests
                id_ords.sort();

                if id_ords.is_empty() {
                    self.sql.push_str("false");
                } else {
                    let v: Vec<_> = id_ords
                        .iter()
                        .map(|(ntid, ord)| format!("(n.mid = {} and c.ord = {})", ntid, ord))
                        .collect();
                    write!(self.sql, "({})", v.join(" or ")).unwrap();
                }
            }
        };
        Ok(())
    }

    fn write_note_type(&mut self, nt_name: &str) -> Result<()> {
        let ntids: Vec<_> = self
            .req
            .storage
            .all_note_types()?
            .values()
            .filter(|nt| matches_wildcard(&nt.name, nt_name))
            .map(|nt| nt.id)
            .collect();
        self.sql.push_str("n.mid in ");
        ids_to_string(&mut self.sql, &ntids);
        Ok(())
    }

    fn write_single_field(&mut self, field_name: &str, val: &str) -> Result<()> {
        let note_types = self.req.storage.all_note_types()?;

        let mut field_map = vec![];
        for nt in note_types.values() {
            for field in &nt.fields {
                if field.name.eq_ignore_ascii_case(field_name) {
                    field_map.push((nt.id, field.ord));
                }
            }
        }

        // for now, sort the map for the benefit of unit tests
        field_map.sort();

        if field_map.is_empty() {
            write!(self.sql, "false").unwrap();
            return Ok(());
        }

        self.args.push(val.to_string().into());
        let arg_idx = self.args.len();
        let searches: Vec<_> = field_map
            .iter()
            .map(|(ntid, ord)| {
                format!(
                    "(n.mid = {mid} and field_at_index(n.flds, {ord}) like ?{n})",
                    mid = ntid,
                    ord = ord,
                    n = arg_idx
                )
            })
            .collect();
        write!(self.sql, "({})", searches.join(" or ")).unwrap();

        Ok(())
    }

    fn write_dupes(&mut self, ntid: ObjID, text: &str) {
        let text_nohtml = strip_html_preserving_image_filenames(text);
        let csum = field_checksum(text_nohtml.as_ref());
        write!(
            self.sql,
            "(n.mid = {} and n.csum = {} and field_at_index(n.flds, 0) = ?",
            ntid, csum
        )
        .unwrap();
        self.args.push(text.to_string().into())
    }

    fn write_added(&mut self, days: u32) -> Result<()> {
        let timing = self.req.storage.timing_today()?;
        let cutoff = timing.next_day_at - (86_400 * (days as i64));
        write!(self.sql, "c.id > {}", cutoff).unwrap();
        Ok(())
    }
}

// Write a list of IDs as '(x,y,...)' into the provided string.
fn ids_to_string<T>(buf: &mut String, ids: &[T])
where
    T: std::fmt::Display,
{
    buf.push('(');
    if !ids.is_empty() {
        for id in ids.iter().skip(1) {
            write!(buf, "{},", id).unwrap();
        }
        write!(buf, "{}", ids[0]).unwrap();
    }
    buf.push(')');
}

#[cfg(test)]
mod test {
    use super::ids_to_string;
    use crate::{collection::open_collection, i18n::I18n, log};
    use std::{fs, path::PathBuf};
    use tempfile::tempdir;

    #[test]
    fn ids_string() {
        let mut s = String::new();
        ids_to_string::<u8>(&mut s, &[]);
        assert_eq!(s, "()");
        s.clear();
        ids_to_string(&mut s, &[7]);
        assert_eq!(s, "(7)");
        s.clear();
        ids_to_string(&mut s, &[7, 6]);
        assert_eq!(s, "(6,7)");
        s.clear();
        ids_to_string(&mut s, &[7, 6, 5]);
        assert_eq!(s, "(6,5,7)");
        s.clear();
    }

    use super::super::parser::parse;
    use super::*;

    // shortcut
    fn s(req: &mut RequestContext, search: &str) -> (String, Vec<String>) {
        let node = Node::Group(parse(search).unwrap());
        node_to_sql(req, &node).unwrap()
    }

    #[test]
    fn sql() -> Result<()> {
        // re-use the mediacheck .anki2 file for now
        use crate::media::check::test::MEDIACHECK_ANKI2;
        let dir = tempdir().unwrap();
        let col_path = dir.path().join("col.anki2");
        fs::write(&col_path, MEDIACHECK_ANKI2).unwrap();

        let i18n = I18n::new(&[""], "", log::terminal());
        let col = open_collection(
            &col_path,
            &PathBuf::new(),
            &PathBuf::new(),
            false,
            i18n,
            log::terminal(),
        )
        .unwrap();

        col.with_ctx(|ctx| {
            // unqualified search
            assert_eq!(
                s(ctx, "test"),
                (
                    "((n.sfld like ?1 escape '\\' or n.flds like ?1 escape '\\'))".into(),
                    vec!["%test%".into()]
                )
            );
            assert_eq!(s(ctx, "te%st").1, vec!["%te%st%".to_string()]);
            // user should be able to escape sql wildcards
            assert_eq!(s(ctx, r#"te\%s\_t"#).1, vec!["%te\\%s\\_t%".to_string()]);

            // qualified search
            assert_eq!(
                s(ctx, "front:test"),
                (
                    concat!(
                        "(((n.mid = 1581236385344 and field_at_index(n.flds, 0) like ?1) or ",
                        "(n.mid = 1581236385345 and field_at_index(n.flds, 0) like ?1) or ",
                        "(n.mid = 1581236385346 and field_at_index(n.flds, 0) like ?1) or ",
                        "(n.mid = 1581236385347 and field_at_index(n.flds, 0) like ?1)))"
                    )
                    .into(),
                    vec!["test".into()]
                )
            );

            // added
            let t = ctx.storage.timing_today().unwrap();
            assert_eq!(
                s(ctx, "added:3").0,
                format!("(c.id > {})", t.next_day_at - (86_400 * 3))
            );

            // deck
            assert_eq!(s(ctx, "deck:default"), ("(c.did in (1))".into(), vec![],));
            assert_eq!(s(ctx, "deck:current"), ("(c.did in (1))".into(), vec![],));
            assert_eq!(s(ctx, "deck:missing"), ("(c.did in ())".into(), vec![],));
            assert_eq!(s(ctx, "deck:d*"), ("(c.did in (1))".into(), vec![],));
            assert_eq!(s(ctx, "deck:filtered"), ("(c.odid > 0)".into(), vec![],));

            // card
            assert_eq!(s(ctx, "card:front"), ("(false)".into(), vec![],));
            assert_eq!(
                s(ctx, r#""card:card 1""#),
                (
                    concat!(
                        "(((n.mid = 1581236385344 and c.ord = 0) or ",
                        "(n.mid = 1581236385345 and c.ord = 0) or ",
                        "(n.mid = 1581236385346 and c.ord = 0) or ",
                        "(n.mid = 1581236385347 and c.ord = 0)))"
                    )
                    .into(),
                    vec![],
                )
            );

            // todo:
            // card
            // mid
            // nid
            // note
            // rated
            // tag
            // is
            // dupe
            // flag
            // cid
            // prop

            Ok(())
        })
        .unwrap();

        Ok(())
    }
}
