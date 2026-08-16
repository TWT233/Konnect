use anyhow::{bail, Context, Result};
use konnect_ipc::gen::kiapi;
use prost::Message;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
struct LibraryFootprint {
    library_id: String,
    definition: kiapi::board::types::Footprint,
    attributes: kiapi::board::types::FootprintAttributes,
    pads: Vec<konnect_ipc::IpcPadDefinition>,
    graphics: Vec<konnect_ipc::IpcGraphicDefinition>,
    models: Vec<kiapi::board::types::Footprint3DModel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangedDomain {
    Pads,
    Graphics,
    Attributes,
    Metadata,
    Models,
}

#[derive(Debug)]
struct PreparedUpdate {
    item: prost_types::Any,
    changed_domains: BTreeSet<ChangedDomain>,
}

fn parse_library_footprint(library_id: &str, source: &str) -> Result<LibraryFootprint> {
    let (library_nickname, entry_name) = library_id
        .split_once(':')
        .filter(|(nickname, entry)| !nickname.is_empty() && !entry.is_empty())
        .context("footprint identifier must use non-empty Library:Footprint syntax")?;
    let root = konnect_sexp::parse_sexp(source).context("invalid footprint S-expression")?;
    if root.head() != Some("footprint") {
        bail!("library source root must be a footprint");
    }

    validate_supported_children(&root)?;
    let pads = super::pcb_components::extract_pad_definitions(source)?;
    let graphics = super::pcb_components::extract_graphic_definitions(source)?;
    let models = parse_models(&root)?;
    let attributes = parse_attributes(&root)?;
    let definition = kiapi::board::types::Footprint {
        id: Some(kiapi::common::types::LibraryIdentifier {
            library_nickname: library_nickname.to_string(),
            entry_name: entry_name.to_string(),
        }),
        attributes: Some(kiapi::board::types::FootprintAttributes {
            description: root.find_str("descr").unwrap_or_default().to_string(),
            keywords: root.find_str("tags").unwrap_or_default().to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };

    Ok(LibraryFootprint {
        library_id: library_id.to_string(),
        definition,
        attributes,
        pads,
        graphics,
        models,
    })
}

fn validate_supported_children(root: &konnect_sexp::SexpNode) -> Result<()> {
    for child in root.children().unwrap_or_default().iter().skip(2) {
        let Some(tag) = child.head() else {
            continue;
        };
        match tag {
            "version" | "generator" | "generator_version" | "layer" | "descr" | "tags" | "attr"
            | "property" | "fp_text" | "fp_line" | "fp_rect" | "fp_circle" | "fp_arc"
            | "fp_poly" | "pad" | "model" => {}
            unsupported => {
                bail!("footprint child '{unsupported}' is not supported by typed library refresh")
            }
        }
        match tag {
            "pad" => validate_pad(child)?,
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" => {
                validate_graphic(child, tag)?
            }
            "fp_text" => {
                let kind = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("fp_text is missing its kind")?;
                if !matches!(kind, "reference" | "value") {
                    bail!(
                        "fp_text kind '{kind}' is not supported losslessly by typed library refresh"
                    );
                }
            }
            "property" => {
                let name = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("property is missing its name")?;
                if !matches!(name, "Reference" | "Value" | "Datasheet" | "Description") {
                    bail!("property '{name}' is not supported losslessly by typed library refresh");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_graphic(graphic: &konnect_sexp::SexpNode, kind: &str) -> Result<()> {
    let allowed: &[&str] = match kind {
        "fp_line" => &["start", "end", "stroke", "layer", "uuid", "tstamp"],
        "fp_rect" => &["start", "end", "stroke", "fill", "layer", "uuid", "tstamp"],
        "fp_circle" => &["center", "end", "stroke", "fill", "layer", "uuid", "tstamp"],
        "fp_arc" => &["start", "mid", "end", "stroke", "layer", "uuid", "tstamp"],
        "fp_poly" => &["pts", "stroke", "fill", "layer", "uuid", "tstamp"],
        _ => bail!("unsupported footprint graphic '{kind}'"),
    };
    for child in graphic.children().unwrap_or_default().iter().skip(1) {
        let Some(tag) = child.head() else {
            bail!("{kind} contains an unsupported atom");
        };
        if !allowed.contains(&tag) {
            bail!("{kind} clause '{tag}' is not supported by typed library refresh");
        }
        match tag {
            "stroke" => {
                for stroke_child in child.children().unwrap_or_default().iter().skip(1) {
                    let stroke_tag = stroke_child
                        .head()
                        .context("graphic stroke contains an unsupported atom")?;
                    match stroke_tag {
                        "width" => {}
                        "type" => {
                            let stroke_type = stroke_child
                                .get(1)
                                .and_then(konnect_sexp::SexpNode::as_str)
                                .context("graphic stroke type is missing")?;
                            if !matches!(stroke_type, "solid" | "default") {
                                bail!(
                                    "graphic stroke type '{stroke_type}' is not supported losslessly"
                                );
                            }
                        }
                        unsupported => bail!(
                            "graphic stroke clause '{unsupported}' is not supported losslessly"
                        ),
                    }
                }
            }
            "fill" => {
                let fill = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("graphic fill is missing")?;
                if !matches!(fill, "none" | "no" | "solid" | "yes") {
                    bail!("graphic fill '{fill}' is not supported losslessly");
                }
            }
            "pts"
                if child
                    .children()
                    .unwrap_or_default()
                    .iter()
                    .skip(1)
                    .any(|point| point.head() != Some("xy")) =>
            {
                bail!("fp_poly contains a non-xy point");
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_pad(pad: &konnect_sexp::SexpNode) -> Result<()> {
    let shape = pad
        .get(3)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("footprint pad is missing its shape")?;
    if !matches!(shape, "circle" | "rect" | "oval" | "roundrect") {
        bail!("pad shape '{shape}' is not supported by typed library refresh");
    }
    for child in pad.children().unwrap_or_default().iter().skip(4) {
        let Some(tag) = child.head() else {
            continue;
        };
        if !matches!(
            tag,
            "at" | "size" | "layers" | "drill" | "roundrect_rratio" | "uuid" | "tstamp"
        ) {
            bail!("pad clause '{tag}' is not supported by typed library refresh");
        }
        if tag == "drill" {
            for nested in child.children().unwrap_or_default().iter().skip(1) {
                if nested.head().is_some() {
                    bail!("nested drill clauses are not supported by typed library refresh");
                }
            }
        }
    }
    Ok(())
}

fn parse_attributes(
    root: &konnect_sexp::SexpNode,
) -> Result<kiapi::board::types::FootprintAttributes> {
    use kiapi::board::types::FootprintMountingStyle;

    let mut attributes = kiapi::board::types::FootprintAttributes::default();
    let Some(attr) = root.find("attr") else {
        return Ok(attributes);
    };
    for value in attr.children().unwrap_or_default().iter().skip(1) {
        match value
            .as_str()
            .context("footprint attr contains a non-atom")?
        {
            "smd" => attributes.mounting_style = FootprintMountingStyle::FmsSmd as i32,
            "through_hole" => {
                attributes.mounting_style = FootprintMountingStyle::FmsThroughHole as i32
            }
            "board_only" => attributes.not_in_schematic = true,
            "exclude_from_pos_files" => attributes.exclude_from_position_files = true,
            "exclude_from_bom" => attributes.exclude_from_bill_of_materials = true,
            "allow_missing_courtyard" => attributes.exempt_from_courtyard_requirement = true,
            "dnp" => attributes.do_not_populate = true,
            "allow_soldermask_bridges" => attributes.allow_soldermask_bridges = true,
            unsupported => bail!(
                "footprint attribute '{unsupported}' is not supported by typed library refresh"
            ),
        }
    }
    Ok(attributes)
}

fn parse_models(
    root: &konnect_sexp::SexpNode,
) -> Result<Vec<kiapi::board::types::Footprint3DModel>> {
    root.find_all("model")
        .into_iter()
        .map(|model| {
            for child in model.children().unwrap_or_default().iter().skip(2) {
                let Some(tag) = child.head() else {
                    if child.as_str() == Some("hide") {
                        continue;
                    }
                    bail!("3D model contains an unsupported atom");
                };
                if !matches!(tag, "offset" | "scale" | "rotate" | "opacity") {
                    bail!("3D model clause '{tag}' is not supported");
                }
            }
            let vector = |tag: &str, default: [f64; 3]| -> Result<kiapi::common::types::Vector3D> {
                let Some(wrapper) = model.find(tag) else {
                    return Ok(kiapi::common::types::Vector3D {
                        x_nm: default[0],
                        y_nm: default[1],
                        z_nm: default[2],
                    });
                };
                let xyz = wrapper
                    .find("xyz")
                    .with_context(|| format!("3D model {tag} is missing xyz"))?;
                Ok(kiapi::common::types::Vector3D {
                    x_nm: xyz
                        .get_f64(1)
                        .with_context(|| format!("3D model {tag}.x is invalid"))?,
                    y_nm: xyz
                        .get_f64(2)
                        .with_context(|| format!("3D model {tag}.y is invalid"))?,
                    z_nm: xyz
                        .get_f64(3)
                        .with_context(|| format!("3D model {tag}.z is invalid"))?,
                })
            };
            Ok(kiapi::board::types::Footprint3DModel {
                filename: model
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("3D model is missing its filename")?
                    .to_string(),
                scale: Some(vector("scale", [1.0, 1.0, 1.0])?),
                rotation: Some(vector("rotate", [0.0, 0.0, 0.0])?),
                offset: Some(vector("offset", [0.0, 0.0, 0.0])?),
                visible: !model
                    .children()
                    .unwrap_or_default()
                    .iter()
                    .any(|child| child.as_str() == Some("hide")),
                opacity: model.find_f64("opacity").unwrap_or(1.0),
            })
        })
        .collect()
}

fn build_updated_instance(
    current: &kiapi::board::types::FootprintInstance,
    library: &LibraryFootprint,
    net_codes: &BTreeMap<String, i32>,
    routed_nets: &BTreeSet<String>,
) -> Result<PreparedUpdate> {
    let current_definition = current
        .definition
        .as_ref()
        .context("board footprint has no embedded definition")?;
    let current_id = current_definition
        .id
        .as_ref()
        .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
        .unwrap_or_default();
    if current_id != library.library_id {
        bail!(
            "board footprint library id '{current_id}' does not match '{}'",
            library.library_id
        );
    }

    let mut old_nets = BTreeMap::<String, kiapi::board::types::Net>::new();
    for item in &current_definition.items {
        if !item.type_url.ends_with("kiapi.board.types.Pad") {
            continue;
        }
        let pad = kiapi::board::types::Pad::decode(item.value.as_slice())
            .context("board footprint contains an invalid pad")?;
        let Some(net) = pad.net.filter(|net| !net.name.is_empty()) else {
            continue;
        };
        if let Some(existing) = old_nets.insert(pad.number.clone(), net.clone()) {
            if existing.name != net.name {
                bail!(
                    "logical pad {} carries multiple nets ('{}' and '{}')",
                    pad.number,
                    existing.name,
                    net.name
                );
            }
        }
    }
    let new_numbers = library
        .pads
        .iter()
        .map(|pad| pad.number.as_str())
        .collect::<BTreeSet<_>>();
    for (number, net) in &old_nets {
        if !new_numbers.contains(number.as_str()) {
            let routed = routed_nets.contains(&net.name);
            bail!(
                "library update removes connected pad {number} on net '{}'{}",
                net.name,
                if routed { " with routed copper" } else { "" }
            );
        }
    }

    let position = current.position.as_ref().cloned().unwrap_or_default();
    let rotation = current
        .orientation
        .as_ref()
        .map(|angle| angle.value_degrees)
        .unwrap_or(0.0);
    let is_back = current.layer == kiapi::board::types::BoardLayer::BlBCu as i32;
    let layer = if is_back { "B.Cu" } else { "F.Cu" };
    let (pads, graphics) = if is_back {
        (
            library
                .pads
                .iter()
                .map(mirror_pad)
                .collect::<Result<Vec<_>>>()?,
            library
                .graphics
                .iter()
                .map(mirror_graphic)
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        (library.pads.clone(), library.graphics.clone())
    };
    let client = konnect_ipc::KiCadIpcClient::new("tcp://never-dialed");
    let packed = client.build_footprint_item(
        &library.library_id,
        &field_text(&current.reference_field),
        &field_text(&current.value_field),
        &pads,
        &graphics,
        &Default::default(),
        konnect_ipc::builders::nm_to_mm(position.x_nm),
        konnect_ipc::builders::nm_to_mm(position.y_nm),
        rotation,
        layer,
    )?;
    let built = kiapi::board::types::FootprintInstance::decode(packed.value.as_slice())
        .context("typed footprint builder returned an invalid item")?;

    let mut updated = current.clone();
    let mut definition = built
        .definition
        .context("typed footprint builder returned no definition")?;
    definition.attributes = library.definition.attributes.clone();
    definition.reference_field = current_definition.reference_field.clone();
    definition.value_field = current_definition.value_field.clone();
    definition.datasheet_field = current_definition.datasheet_field.clone();
    definition.description_field = current_definition.description_field.clone();
    definition.items.extend(
        library.models.iter().map(|model| {
            konnect_ipc::builders::pack_any(model, "kiapi.board.types.Footprint3DModel")
        }),
    );
    for item in &mut definition.items {
        if item.type_url.ends_with("kiapi.board.types.Pad") {
            let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
            pad.net = old_nets
                .get(&pad.number)
                .map(|old| kiapi::board::types::Net {
                    code: net_codes
                        .get(&old.name)
                        .copied()
                        .or_else(|| old.code.as_ref().map(|code| code.value))
                        .map(|value| kiapi::board::types::NetCode { value }),
                    name: old.name.clone(),
                });
            *item = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
        } else if is_back && item.type_url.ends_with("kiapi.board.types.BoardText") {
            let mut text = kiapi::board::types::BoardText::decode(item.value.as_slice())?;
            if is_side_specific_layer(text.layer) {
                if let Some(attributes) =
                    text.text.as_mut().and_then(|text| text.attributes.as_mut())
                {
                    attributes.mirrored = true;
                }
            }
            *item = konnect_ipc::builders::pack_any(&text, "kiapi.board.types.BoardText");
        }
    }
    updated.definition = Some(definition);
    let mut attributes = library.attributes.clone();
    if let Some(current_attributes) = current.attributes.as_ref() {
        attributes.not_in_schematic = current_attributes.not_in_schematic;
        attributes.do_not_populate = current_attributes.do_not_populate;
    }
    updated.attributes = Some(attributes);

    let changed_domains = changed_domains(current, &updated)?;
    Ok(PreparedUpdate {
        item: konnect_ipc::builders::pack_any(&updated, "kiapi.board.types.FootprintInstance"),
        changed_domains,
    })
}

fn mirror_pad(pad: &konnect_ipc::IpcPadDefinition) -> Result<konnect_ipc::IpcPadDefinition> {
    let mut mirrored = pad.clone();
    mirrored.y = -mirrored.y;
    mirrored.rotation = -mirrored.rotation;
    mirrored.layers = mirrored
        .layers
        .iter()
        .map(|layer| flip_layer_name(layer))
        .collect::<Result<_>>()?;
    Ok(mirrored)
}

fn mirror_graphic(
    graphic: &konnect_ipc::IpcGraphicDefinition,
) -> Result<konnect_ipc::IpcGraphicDefinition> {
    use konnect_ipc::IpcGraphicDefinition as Graphic;

    let point = |(x, y): (f64, f64)| (x, -y);
    Ok(match graphic {
        Graphic::Line {
            start,
            end,
            layer,
            width,
        } => Graphic::Line {
            start: point(*start),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
        },
        Graphic::Rect {
            start,
            end,
            layer,
            width,
            filled,
        } => Graphic::Rect {
            start: point(*start),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Circle {
            center,
            end,
            layer,
            width,
            filled,
        } => Graphic::Circle {
            center: point(*center),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Arc {
            start,
            mid,
            end,
            layer,
            width,
        } => Graphic::Arc {
            start: point(*end),
            mid: point(*mid),
            end: point(*start),
            layer: flip_layer_name(layer)?,
            width: *width,
        },
        Graphic::Poly {
            points,
            layer,
            width,
            filled,
        } => Graphic::Poly {
            points: points.iter().copied().map(point).collect(),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Text {
            text,
            position,
            rotation,
            layer,
            size,
        } => Graphic::Text {
            text: text.clone(),
            position: point(*position),
            rotation: 180.0 - rotation,
            layer: flip_layer_name(layer)?,
            size: *size,
        },
    })
}

fn flip_layer_name(layer: &str) -> Result<String> {
    let flipped = match layer {
        "F.Cu" => "B.Cu",
        "B.Cu" => "F.Cu",
        "F.Adhes" => "B.Adhes",
        "B.Adhes" => "F.Adhes",
        "F.Paste" => "B.Paste",
        "B.Paste" => "F.Paste",
        "F.SilkS" | "F.Silkscreen" => "B.SilkS",
        "B.SilkS" | "B.Silkscreen" => "F.SilkS",
        "F.Mask" => "B.Mask",
        "B.Mask" => "F.Mask",
        "F.CrtYd" | "F.Courtyard" => "B.CrtYd",
        "B.CrtYd" | "B.Courtyard" => "F.CrtYd",
        "F.Fab" => "B.Fab",
        "B.Fab" => "F.Fab",
        "*.Cu" | "*.Mask" | "*.Paste" => layer,
        other if other.starts_with("F.") || other.starts_with("B.") => {
            bail!("unsupported side-specific footprint layer '{other}'")
        }
        other => other,
    };
    Ok(flipped.to_string())
}

fn is_side_specific_layer(layer: i32) -> bool {
    use kiapi::board::types::BoardLayer;

    matches!(
        BoardLayer::try_from(layer).ok(),
        Some(
            BoardLayer::BlFCu
                | BoardLayer::BlBCu
                | BoardLayer::BlFAdhes
                | BoardLayer::BlBAdhes
                | BoardLayer::BlFPaste
                | BoardLayer::BlBPaste
                | BoardLayer::BlFSilkS
                | BoardLayer::BlBSilkS
                | BoardLayer::BlFMask
                | BoardLayer::BlBMask
                | BoardLayer::BlFCrtYd
                | BoardLayer::BlBCrtYd
                | BoardLayer::BlFFab
                | BoardLayer::BlBFab
        )
    )
}

fn changed_domains(
    current: &kiapi::board::types::FootprintInstance,
    updated: &kiapi::board::types::FootprintInstance,
) -> Result<BTreeSet<ChangedDomain>> {
    let mut changed = BTreeSet::new();
    let current_definition = current
        .definition
        .as_ref()
        .context("missing current definition")?;
    let updated_definition = updated
        .definition
        .as_ref()
        .context("missing updated definition")?;

    if normalized_items(current_definition, "Pad")? != normalized_items(updated_definition, "Pad")?
    {
        changed.insert(ChangedDomain::Pads);
    }
    if normalized_graphics(current_definition)? != normalized_graphics(updated_definition)? {
        changed.insert(ChangedDomain::Graphics);
    }
    if normalized_items(current_definition, "Footprint3DModel")?
        != normalized_items(updated_definition, "Footprint3DModel")?
    {
        changed.insert(ChangedDomain::Models);
    }
    if current_definition.attributes != updated_definition.attributes {
        changed.insert(ChangedDomain::Metadata);
    }
    if library_owned_attributes(current.attributes.as_ref())
        != library_owned_attributes(updated.attributes.as_ref())
    {
        changed.insert(ChangedDomain::Attributes);
    }
    Ok(changed)
}

fn normalized_items(
    definition: &kiapi::board::types::Footprint,
    suffix: &str,
) -> Result<Vec<Vec<u8>>> {
    definition
        .items
        .iter()
        .filter(|item| item.type_url.ends_with(suffix))
        .map(|item| {
            if suffix == "Pad" {
                let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
                pad.id = None;
                pad.net = None;
                Ok(pad.encode_to_vec())
            } else {
                Ok(item.value.clone())
            }
        })
        .collect()
}

fn normalized_graphics(definition: &kiapi::board::types::Footprint) -> Result<Vec<Vec<u8>>> {
    definition
        .items
        .iter()
        .filter_map(|item| {
            if item.type_url.ends_with("BoardGraphicShape") {
                Some(
                    kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice()).map(
                        |mut shape| {
                            shape.id = None;
                            shape.encode_to_vec()
                        },
                    ),
                )
            } else if item.type_url.ends_with("BoardText") {
                Some(
                    kiapi::board::types::BoardText::decode(item.value.as_slice()).map(
                        |mut text| {
                            text.id = None;
                            text.encode_to_vec()
                        },
                    ),
                )
            } else {
                None
            }
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn library_owned_attributes(
    attributes: Option<&kiapi::board::types::FootprintAttributes>,
) -> Option<(bool, bool, bool, bool, i32, bool)> {
    attributes.map(|attributes| {
        (
            attributes.exclude_from_position_files,
            attributes.exclude_from_bill_of_materials,
            attributes.exempt_from_courtyard_requirement,
            attributes.allow_soldermask_bridges,
            attributes.mounting_style,
            attributes.not_in_schematic,
        )
    })
}

fn field_text(field: &Option<kiapi::board::types::Field>) -> String {
    field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|text| text.text.as_ref())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_ipc::builders;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::collections::{BTreeMap, BTreeSet};

    const LIBRARY_FOOTPRINT: &str = r#"
(footprint "Socket"
  (version 20240108)
  (generator "konnect")
  (layer "F.Cu")
  (descr "updated description")
  (tags "keyboard socket")
  (attr smd exclude_from_pos_files)
  (fp_line (start -1 -2) (end 3 4)
    (stroke (width 0.12) (type solid))
    (layer "F.SilkS"))
  (fp_poly (pts (xy -2 -1) (xy 2 -1) (xy 2 1) (xy -2 1))
    (stroke (width 0.05) (type solid))
    (fill none)
    (layer "B.CrtYd"))
  (pad "1" smd roundrect (at -2 0 15) (size 2 1)
    (layers "B.Cu" "B.Paste" "B.Mask")
    (roundrect_rratio 0.2))
  (pad "1" smd rect (at -1 0) (size 1 1) (layers "B.Cu"))
  (pad "2" thru_hole circle (at 2 0) (size 3 3)
    (layers "*.Cu" "*.Mask") (drill 1))
  (pad "3" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
  (fp_text reference "REF**" (at 0 -4 0) (layer "F.SilkS")
    (effects (font (size 1 1) (thickness 0.15))))
  (fp_text value "Socket" (at 0 4 0) (layer "F.Fab")
    (effects (font (size 1 1) (thickness 0.15))))
  (model "../models/Socket.step"
    (offset (xyz 1 2 3))
    (scale (xyz -1 1 1))
    (rotate (xyz 90 0 45)))
)
"#;

    fn field(name: &str, value: &str, x: f64, y: f64, visible: bool) -> kiapi::board::types::Field {
        kiapi::board::types::Field {
            name: name.to_string(),
            visible,
            text: Some(kiapi::board::types::BoardText {
                text: Some(kiapi::common::types::Text {
                    position: Some(builders::vec2(x, y)),
                    attributes: Some(kiapi::common::types::TextAttributes {
                        size: Some(builders::vec2(1.25, 1.25)),
                        angle: Some(kiapi::common::types::Angle {
                            value_degrees: 17.0,
                        }),
                        mirrored: true,
                        ..Default::default()
                    }),
                    text: value.to_string(),
                    ..Default::default()
                }),
                layer: kiapi::board::types::BoardLayer::BlBSilkS as i32,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn current_pad(number: &str, net_name: &str, net_code: i32) -> prost_types::Any {
        builders::pack_any(
            &kiapi::board::types::Pad {
                id: Some(kiapi::common::types::Kiid {
                    value: format!("old-pad-{number}-{net_code}"),
                }),
                number: number.to_string(),
                net: Some(kiapi::board::types::Net {
                    code: Some(kiapi::board::types::NetCode { value: net_code }),
                    name: net_name.to_string(),
                }),
                position: Some(builders::vec2(100.0, 50.0)),
                ..Default::default()
            },
            "kiapi.board.types.Pad",
        )
    }

    fn current_instance(
        layer: kiapi::board::types::BoardLayer,
    ) -> kiapi::board::types::FootprintInstance {
        let reference = field("Reference", "SW1", 101.0, 48.0, true);
        let value = field("Value", "Socket Value", 99.0, 52.0, false);
        kiapi::board::types::FootprintInstance {
            id: Some(kiapi::common::types::Kiid {
                value: "instance-kiid".to_string(),
            }),
            position: Some(builders::vec2(100.0, 50.0)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: 37.0,
            }),
            layer: layer as i32,
            locked: kiapi::common::types::LockedState::LsLocked as i32,
            definition: Some(kiapi::board::types::Footprint {
                id: Some(kiapi::common::types::LibraryIdentifier {
                    library_nickname: "Test".to_string(),
                    entry_name: "Socket".to_string(),
                }),
                reference_field: Some(reference.clone()),
                value_field: Some(value.clone()),
                items: vec![current_pad("1", "ROW1", 11), current_pad("2", "COL1", 12)],
                ..Default::default()
            }),
            reference_field: Some(reference),
            value_field: Some(value),
            datasheet_field: Some(field("Datasheet", "placed-datasheet", 0.0, 0.0, false)),
            description_field: Some(field("Description", "placed-description", 0.0, 0.0, false)),
            attributes: Some(kiapi::board::types::FootprintAttributes {
                not_in_schematic: false,
                exclude_from_bill_of_materials: true,
                do_not_populate: true,
                ..Default::default()
            }),
            overrides: Some(kiapi::board::types::FootprintDesignRuleOverrides {
                copper_clearance: Some(builders::distance(0.3)),
                ..Default::default()
            }),
            symbol_path: Some(kiapi::common::types::SheetPath {
                path: vec![kiapi::common::types::Kiid {
                    value: "sheet-kiid".to_string(),
                }],
                path_human_readable: "/Keyboard".to_string(),
            }),
            symbol_sheet_name: "Keyboard".to_string(),
            symbol_sheet_filename: "keyboard.kicad_sch".to_string(),
            symbol_footprint_filters: "Test:*".to_string(),
        }
    }

    fn decoded_pads(
        instance: &kiapi::board::types::FootprintInstance,
    ) -> Vec<kiapi::board::types::Pad> {
        instance
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|item| item.type_url.ends_with("kiapi.board.types.Pad"))
            .map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap())
            .collect()
    }

    #[test]
    fn parses_supported_library_definition_without_dropping_domains() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();

        assert_eq!(library.library_id, "Test:Socket");
        assert_eq!(
            library.definition.attributes.as_ref().unwrap().description,
            "updated description"
        );
        assert_eq!(
            library.definition.attributes.as_ref().unwrap().keywords,
            "keyboard socket"
        );
        assert_eq!(library.pads.len(), 4);
        assert_eq!(library.graphics.len(), 2);
        assert_eq!(library.models.len(), 1);
        let model = &library.models[0];
        assert_eq!(model.filename, "../models/Socket.step");
        assert_eq!(model.offset.as_ref().unwrap().x_nm, 1.0);
        assert_eq!(model.scale.as_ref().unwrap().x_nm, -1.0);
        assert_eq!(model.rotation.as_ref().unwrap().z_nm, 45.0);
    }

    #[test]
    fn merge_preserves_instance_state_and_nets_by_logical_pad_number() {
        let current = current_instance(kiapi::board::types::BoardLayer::BlBCu);
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let prepared = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::from(["ROW1".to_string(), "COL1".to_string()]),
        )
        .unwrap();
        let updated =
            kiapi::board::types::FootprintInstance::decode(prepared.item.value.as_slice()).unwrap();

        assert_eq!(updated.id, current.id);
        assert_eq!(updated.position, current.position);
        assert_eq!(updated.orientation, current.orientation);
        assert_eq!(updated.layer, current.layer);
        assert_eq!(updated.locked, current.locked);
        assert_eq!(updated.reference_field, current.reference_field);
        assert_eq!(updated.value_field, current.value_field);
        assert_eq!(updated.datasheet_field, current.datasheet_field);
        assert_eq!(updated.description_field, current.description_field);
        let attributes = updated.attributes.as_ref().unwrap();
        let current_attributes = current.attributes.as_ref().unwrap();
        assert_eq!(
            attributes.not_in_schematic,
            current_attributes.not_in_schematic
        );
        assert_eq!(
            attributes.do_not_populate,
            current_attributes.do_not_populate
        );
        assert!(attributes.exclude_from_position_files);
        assert!(!attributes.exclude_from_bill_of_materials);
        assert_eq!(
            attributes.mounting_style,
            kiapi::board::types::FootprintMountingStyle::FmsSmd as i32
        );
        assert_eq!(updated.overrides, current.overrides);
        assert_eq!(updated.symbol_path, current.symbol_path);
        assert_eq!(updated.symbol_sheet_name, current.symbol_sheet_name);
        assert_eq!(updated.symbol_sheet_filename, current.symbol_sheet_filename);
        assert_eq!(
            updated.symbol_footprint_filters,
            current.symbol_footprint_filters
        );

        let pads = decoded_pads(&updated);
        assert_eq!(pads.iter().filter(|pad| pad.number == "1").count(), 2);
        assert!(pads.iter().filter(|pad| pad.number == "1").all(|pad| pad
            .net
            .as_ref()
            .map(|net| net.name.as_str())
            == Some("ROW1")));
        assert_eq!(
            pads.iter()
                .find(|pad| pad.number == "2")
                .and_then(|pad| pad.net.as_ref())
                .map(|net| net.name.as_str()),
            Some("COL1")
        );
        assert!(pads
            .iter()
            .find(|pad| pad.number == "3")
            .unwrap()
            .net
            .is_none());
        let flipped_pad = pads
            .iter()
            .find(|pad| pad.number == "3")
            .expect("new pad 3");
        let flipped_stack = flipped_pad.pad_stack.as_ref().expect("pad stack");
        assert_eq!(
            flipped_stack.layers,
            vec![kiapi::board::types::BoardLayer::BlBCu as i32],
            "a front-copper library pad moves to back copper with a B.Cu instance"
        );
        let expected = konnect_sexp::geometry::transform_pad(0.0, -3.0, 100.0, 50.0, 37.0);
        let actual = flipped_pad.position.as_ref().expect("pad position");
        let actual = (
            builders::nm_to_mm(actual.x_nm),
            builders::nm_to_mm(actual.y_nm),
        );
        assert!((actual.0 - expected.0).abs() <= 0.000_001);
        assert!((actual.1 - expected.1).abs() <= 0.000_001);
        assert!(prepared.changed_domains.contains(&ChangedDomain::Pads));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Graphics));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Metadata));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Models));
    }

    #[test]
    fn merge_conflicts_when_library_removes_a_connected_logical_pad() {
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let without_pad_two = LIBRARY_FOOTPRINT.replace(
            "  (pad \"2\" thru_hole circle (at 2 0) (size 3 3)\n    (layers \"*.Cu\" \"*.Mask\") (drill 1))\n",
            "",
        );
        let library = parse_library_footprint("Test:Socket", &without_pad_two).unwrap();

        let error = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::from(["COL1".to_string()]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("connected pad 2"), "{error:#}");
    }

    #[test]
    fn parser_rejects_unsupported_children_before_any_update_can_be_built() {
        let unsupported = LIBRARY_FOOTPRINT.replace(
            "  (model \"../models/Socket.step\"",
            "  (zone (net 0) (layers \"F.Cu\"))\n  (model \"../models/Socket.step\"",
        );

        let error = parse_library_footprint("Test:Socket", &unsupported).unwrap_err();

        assert!(error.to_string().contains("zone"), "{error:#}");
    }

    #[test]
    fn parser_rejects_lossy_graphic_and_pad_clauses() {
        let dashed = LIBRARY_FOOTPRINT.replace(
            "(stroke (width 0.12) (type solid))",
            "(stroke (width 0.12) (type dash))",
        );
        let error = parse_library_footprint("Test:Socket", &dashed).unwrap_err();
        assert!(error.to_string().contains("stroke type"), "{error:#}");

        let solder_margin = LIBRARY_FOOTPRINT.replace(
            "(roundrect_rratio 0.2))",
            "(roundrect_rratio 0.2) (solder_mask_margin 0.05))",
        );
        let error = parse_library_footprint("Test:Socket", &solder_margin).unwrap_err();
        assert!(
            error.to_string().contains("solder_mask_margin"),
            "{error:#}"
        );
    }

    #[test]
    fn rebuilding_an_applied_instance_is_a_noop_at_every_rotation_and_side() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let routed = BTreeSet::from(["ROW1".to_string(), "COL1".to_string()]);

        for layer in [
            kiapi::board::types::BoardLayer::BlFCu,
            kiapi::board::types::BoardLayer::BlBCu,
        ] {
            for rotation in [0.0, 90.0, 180.0, 270.0, 37.0] {
                let mut current = current_instance(layer);
                current.orientation = Some(kiapi::common::types::Angle {
                    value_degrees: rotation,
                });
                let first =
                    build_updated_instance(&current, &library, &net_codes, &routed).unwrap();
                let applied =
                    kiapi::board::types::FootprintInstance::decode(first.item.value.as_slice())
                        .unwrap();

                let second =
                    build_updated_instance(&applied, &library, &net_codes, &routed).unwrap();

                assert!(
                    second.changed_domains.is_empty(),
                    "{layer:?} at {rotation} degrees changed again: {:?}",
                    second.changed_domains
                );
            }
        }
    }
}
