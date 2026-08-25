//! Structural queries over a `.kicad_pcb` tree.
//!
//! These exist because `SexpNode::find_all` is **direct children only**, which
//! is easy to forget: `footprint`, `segment`, `via` and `net` *are* direct
//! children of `(kicad_pcb …)`, so `tree.find_all("footprint")` is right — and
//! `pad` is not, so `tree.find_all("pad")` silently returns 0 on every board
//! ever written. Design review reported `pads: 0` for the whole life of its
//! coverage block because of exactly that (#246).

use crate::parser::SexpNode;

/// Every footprint on a board, in file order.
///
/// Footprints are direct children of `(kicad_pcb …)`, so this is a thin
/// wrapper — it exists so pad counting has an obvious partner and callers stop
/// reaching for `find_all` directly on the root.
pub fn footprints(tree: &SexpNode) -> Vec<&SexpNode> {
    tree.find_all("footprint")
}

/// Every pad on the board, across all footprints.
///
/// Pads live one level down, inside each `(footprint …)`. Call this rather
/// than `tree.find_all("pad")`, which cannot ever match.
pub fn pads(tree: &SexpNode) -> Vec<&SexpNode> {
    footprints(tree)
        .into_iter()
        .flat_map(|fp| fp.find_all("pad"))
        .collect()
}

/// How many pads the board has. Zero from a board that has footprints means
/// something is wrong with the board or the parse — it is not a normal state.
pub fn count_pads(tree: &SexpNode) -> usize {
    pads(tree).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_sexp;

    /// A board carrying two footprints of two pads each, in KiCad's own
    /// tab-indented layout.
    const BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(footprint \"R_0402\"\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(pad \"1\" smd roundrect\n\
        \t\t\t(at -0.51 0)\n\
        \t\t\t(size 0.54 0.64)\n\
        \t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\
        \t\t\t(at 0.51 0)\n\
        \t\t\t(size 0.54 0.64)\n\
        \t\t)\n\
        \t)\n\
        \t(footprint \"C_0402\"\n\
        \t\t(layer \"F.Cu\")\n\
        \t\t(pad \"1\" smd roundrect\n\
        \t\t\t(at -0.51 0)\n\
        \t\t)\n\
        \t\t(pad \"2\" smd roundrect\n\
        \t\t\t(at 0.51 0)\n\
        \t\t)\n\
        \t)\n\
        )";

    /// The bug this module exists to prevent: `find_all` does not recurse, so
    /// asking the root for pads is not merely inaccurate, it is always zero.
    #[test]
    fn pads_are_nested_so_the_root_never_sees_them() {
        let tree = parse_sexp(BOARD).unwrap();

        assert_eq!(
            tree.find_all("pad").len(),
            0,
            "if this ever becomes non-zero, find_all started recursing and \
             every caller needs rechecking"
        );
        assert_eq!(count_pads(&tree), 4);
        assert_eq!(footprints(&tree).len(), 2);
    }

    #[test]
    fn a_board_with_no_footprints_has_no_pads() {
        let tree = parse_sexp("(kicad_pcb\n\t(version 20260206)\n)").unwrap();
        assert_eq!(count_pads(&tree), 0);
        assert_eq!(footprints(&tree).len(), 0);
    }
}
