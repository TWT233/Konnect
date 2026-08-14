//! KiCad `(net …)` node shapes.
//!
//! A net node is written two different ways depending on the file format, and
//! the difference is positional, so a fixed index silently reads the wrong
//! child rather than failing:
//!
//! | file | declaration | reference on an item |
//! |---|---|---|
//! | KiCad ≤ 9 | `(net 1 "GND")` at top level | `(net 1)` |
//! | KiCad 10  | *no top-level table at all* | `(net "GND")` |
//!
//! So in KiCad 10 the name sits at index 1; in KiCad ≤ 9 index 1 is the
//! numeric id and the name is at index 2. Reading index 2 unconditionally —
//! which is what Konnect did — yields `None` on every KiCad 10 pad.
//!
//! These helpers read by **shape** instead: whether child 1 is a quoted string
//! or a bare atom is what distinguishes the formats, so no version sniffing is
//! needed and a file that mixes them still reads correctly.

use crate::SexpNode;
use std::collections::HashSet;

/// The net name carried by a `(net …)` node, whichever shape wrote it.
///
/// Returns `None` when the node carries no name — a bare `(net 1)` reference,
/// or a malformed node. `None` is *not* the same as `Some("")`: the empty name
/// is KiCad's unconnected pseudo-net, which is a real answer, whereas `None`
/// means this node did not tell us. Callers surfacing a net to a user should
/// keep that distinction rather than flattening both to `""` — an empty string
/// looks like a clean answer and is the failure mode this module exists to
/// remove.
///
/// ```
/// use konnect_sexp::{parse_sexp, net::net_name};
/// let k10 = parse_sexp("(net \"GND\")").unwrap();
/// let k9 = parse_sexp("(net 1 \"GND\")").unwrap();
/// assert_eq!(net_name(&k10), Some("GND"));
/// assert_eq!(net_name(&k9), Some("GND"));
/// assert_eq!(net_name(&parse_sexp("(net 1)").unwrap()), None);
/// ```
pub fn net_name(node: &SexpNode) -> Option<&str> {
    let children = node.children()?;
    if node.head() != Some("net") {
        return None;
    }
    match children.get(1)? {
        // (net "GND") — KiCad 10 names the net in place.
        SexpNode::Str(name) => Some(name.as_str()),
        // (net 1 "GND") — KiCad ≤ 9 declaration; the name follows the id.
        SexpNode::Atom(_) => match children.get(2)? {
            SexpNode::Str(name) => Some(name.as_str()),
            _ => None,
        },
        SexpNode::List(_) => None,
    }
}

/// The numeric net id of a `(net …)` node, for the formats that carry one.
///
/// `(net 1 "GND")` and `(net 1)` both give `Some("1")`; KiCad 10's
/// `(net "GND")` gives `None`, because the format dropped net numbers.
pub fn net_id(node: &SexpNode) -> Option<&str> {
    if node.head() != Some("net") {
        return None;
    }
    match node.children()?.get(1)? {
        SexpNode::Atom(id) => Some(id.as_str()),
        _ => None,
    }
}

/// Every distinct real net in a board tree, excluding the unconnected
/// pseudo-net.
///
/// Names are used as the key whenever any node carries one, and numeric ids
/// only as a fallback. That is what keeps both formats honest: in KiCad 10 the
/// name is the *only* identity a net has, while in KiCad ≤ 9 the same net
/// appears once as `(net 1 "GND")` and again as `(net 1)` on every item that
/// touches it — keying on the name collapses those correctly, and keying on
/// the id would drop KiCad 10 entirely. De-duplication is not a nicety: a
/// 4-layer KiCad 10 board reported in #133 has 872 `(net …)` occurrences for 65
/// distinct nets, and an 8-layer one measured here has 5423 for 776 — roughly
/// 7–13× over if a collector simply counts occurrences.
///
/// The walk is recursive because KiCad 10 has no top-level table: the nets are
/// wherever the pads, segments, vias and zones are.
pub fn collect_net_keys(tree: &SexpNode) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    walk(tree, &mut names, &mut ids);
    if names.is_empty() {
        ids
    } else {
        names
    }
}

/// How many distinct real nets a board tree contains. See [`collect_net_keys`].
pub fn count_distinct_nets(tree: &SexpNode) -> usize {
    collect_net_keys(tree).len()
}

fn walk(node: &SexpNode, names: &mut HashSet<String>, ids: &mut HashSet<String>) {
    let Some(children) = node.children() else {
        return;
    };
    if node.head() == Some("net") {
        // The unconnected pseudo-net is net 0 / the empty name. It is not a
        // net a user has, and counting it makes an empty board look wired.
        match net_name(node) {
            Some(name) if !name.is_empty() => {
                names.insert(name.to_string());
            }
            Some(_) => {}
            None => {
                if let Some(id) = net_id(node) {
                    if id != "0" {
                        ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    for child in children {
        walk(child, names, ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_sexp;

    fn node(src: &str) -> SexpNode {
        parse_sexp(src).unwrap()
    }

    #[test]
    fn reads_the_kicad_10_name_only_shape() {
        assert_eq!(net_name(&node("(net \"GND\")")), Some("GND"));
        assert_eq!(net_id(&node("(net \"GND\")")), None);
    }

    #[test]
    fn reads_the_legacy_id_and_name_shape() {
        assert_eq!(net_name(&node("(net 1 \"GND\")")), Some("GND"));
        assert_eq!(net_id(&node("(net 1 \"GND\")")), Some("1"));
    }

    #[test]
    fn a_bare_reference_has_an_id_but_no_name() {
        assert_eq!(net_name(&node("(net 7)")), None);
        assert_eq!(net_id(&node("(net 7)")), Some("7"));
    }

    #[test]
    fn the_unconnected_pseudo_net_has_a_name_and_it_is_empty() {
        // Some("") and None are different answers: this node *did* tell us the
        // net, and the answer is "unconnected".
        assert_eq!(net_name(&node("(net 0 \"\")")), Some(""));
        assert_eq!(net_name(&node("(net \"\")")), Some(""));
    }

    #[test]
    fn a_malformed_or_foreign_node_yields_nothing() {
        assert_eq!(net_name(&node("(net)")), None);
        assert_eq!(net_id(&node("(net)")), None);
        // A node that is not a net at all must not be read as one, or a
        // shape-based reader becomes a shape-based guesser.
        assert_eq!(net_name(&node("(layer \"F.Cu\")")), None);
        assert_eq!(net_id(&node("(at 1 2)")), None);
    }

    #[test]
    fn a_name_that_looks_like_a_number_is_still_a_name() {
        // Quoting is the discriminator, not the character class.
        assert_eq!(net_name(&node("(net \"12\")")), Some("12"));
        assert_eq!(net_id(&node("(net \"12\")")), None);
    }

    /// KiCad 10: no top-level table, the name on every item, tab-indented.
    const KICAD_10: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(footprint \"R\"\n\t\t(pad \"1\" smd rect (at 0 0) (net \"GND\"))\n\
        \t\t(pad \"2\" smd rect (at 1 0) (net \"VCC\"))\n\t)\n\
        \t(segment (start 0 0) (end 1 0) (net \"GND\"))\n\
        \t(via (at 1 0) (net \"GND\"))\n\
        \t(zone (net \"GND\") (polygon (pts (xy 0 0) (xy 1 1))))\n\
        )\n";

    /// KiCad ≤ 9: a top-level table plus numeric references on the items.
    const KICAD_9: &str = "(kicad_pcb\n\
        \t(version 20241229)\n\
        \t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"VCC\")\n\
        \t(footprint \"R\"\n\t\t(pad \"1\" smd rect (at 0 0) (net 1 \"GND\"))\n\
        \t\t(pad \"2\" smd rect (at 1 0) (net 2 \"VCC\"))\n\t)\n\
        \t(segment (start 0 0) (end 1 0) (net 1))\n\
        \t(via (at 1 0) (net 1))\n\
        )\n";

    #[test]
    fn counts_a_kicad_10_board_that_has_no_net_table() {
        assert_eq!(count_distinct_nets(&node(KICAD_10)), 2);
    }

    #[test]
    fn counts_a_kicad_9_board_without_double_counting_declarations() {
        // 2, not 7: GND is declared once and referenced four more times.
        assert_eq!(count_distinct_nets(&node(KICAD_9)), 2);
    }

    #[test]
    fn the_unconnected_pseudo_net_does_not_count() {
        assert_eq!(count_distinct_nets(&node("(kicad_pcb (net 0 \"\"))")), 0);
        assert_eq!(
            count_distinct_nets(&node("(kicad_pcb (segment (net 0)))")),
            0
        );
        assert_eq!(
            count_distinct_nets(&node("(kicad_pcb (pad (net \"\")))")),
            0
        );
    }

    #[test]
    fn a_file_with_only_bare_references_falls_back_to_ids() {
        // No name anywhere — the ids are the only identity available.
        let t = node("(kicad_pcb (segment (net 1)) (segment (net 1)) (via (net 2)))");
        assert_eq!(count_distinct_nets(&t), 2);
    }

    #[test]
    fn names_win_over_ids_when_both_are_present() {
        // Mixed file: the two shapes name the same two nets. Keying on ids
        // here would count the bare references as extra nets.
        let t = node("(kicad_pcb (net 1 \"GND\") (segment (net 1)) (pad (net \"VCC\")))");
        assert_eq!(count_distinct_nets(&t), 2);
    }

    #[test]
    fn nets_are_found_at_any_depth() {
        let t = node("(kicad_pcb (group (footprint (pad (net \"DEEP\")))))");
        assert_eq!(collect_net_keys(&t), HashSet::from(["DEEP".to_string()]));
    }
}
