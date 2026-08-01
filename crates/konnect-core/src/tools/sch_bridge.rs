//! Bridge between konnect-schematic-editor types and konnect-sexp types.
//!
//! During the migration, this module converts the new crate's typed collections
//! into the konnect-sexp types that build_net_graph and other analysis functions expect.
//! Once all analysis is migrated to use the new crate's types natively, this module
//! can be removed.

use konnect_schematic_editor as cse;

/// Convert a konnect-schematic-editor Wire to a konnect-sexp Wire.
pub fn wire_to_sexp(w: &cse::Wire) -> konnect_sexp::schematic::Wire {
    konnect_sexp::schematic::Wire {
        x1: w.start.0,
        y1: w.start.1,
        x2: w.end.0,
        y2: w.end.1,
        uuid: Some(w.uuid.clone()),
    }
}

/// Convert a konnect-schematic-editor Label to a konnect-sexp Label.
pub fn label_to_sexp(l: &cse::Label) -> konnect_sexp::schematic::Label {
    konnect_sexp::schematic::Label {
        net: l.text.clone(),
        x: l.at.x,
        y: l.at.y,
        rotation: l.at.rotation.unwrap_or(0.0),
        uuid: Some(l.uuid.clone()),
        kind: konnect_sexp::schematic::LabelKind::NetLabel,
    }
}

/// Convert a konnect-schematic-editor GlobalLabel to a konnect-sexp Label.
pub fn global_label_to_sexp(l: &cse::GlobalLabel) -> konnect_sexp::schematic::Label {
    konnect_sexp::schematic::Label {
        net: l.text.clone(),
        x: l.at.x,
        y: l.at.y,
        rotation: l.at.rotation.unwrap_or(0.0),
        uuid: Some(l.uuid.clone()),
        kind: konnect_sexp::schematic::LabelKind::GlobalLabel,
    }
}

/// Convert a konnect-schematic-editor HierarchicalLabel to a konnect-sexp Label.
pub fn hier_label_to_sexp(l: &cse::HierarchicalLabel) -> konnect_sexp::schematic::Label {
    konnect_sexp::schematic::Label {
        net: l.text.clone(),
        x: l.at.x,
        y: l.at.y,
        rotation: l.at.rotation.unwrap_or(0.0),
        uuid: Some(l.uuid.clone()),
        kind: konnect_sexp::schematic::LabelKind::HierarchicalLabel,
    }
}

/// Collect all labels from a schematic into konnect-sexp Label format.
pub fn all_labels_as_sexp(sch: &cse::Schematic) -> Vec<konnect_sexp::schematic::Label> {
    let mut labels = Vec::new();
    for l in sch.labels.iter() {
        labels.push(label_to_sexp(l));
    }
    for g in sch.global_labels.iter() {
        labels.push(global_label_to_sexp(g));
    }
    for h in sch.hierarchical_labels.iter() {
        labels.push(hier_label_to_sexp(h));
    }
    labels
}

/// Collect all junction dot positions from a schematic.
pub fn all_junctions(sch: &cse::Schematic) -> Vec<(f64, f64)> {
    sch.junctions.iter().map(|j| (j.x, j.y)).collect()
}

/// Collect all wires from a schematic into konnect-sexp Wire format.
pub fn all_wires_as_sexp(sch: &cse::Schematic) -> Vec<konnect_sexp::schematic::Wire> {
    sch.wires.iter().map(wire_to_sexp).collect()
}
