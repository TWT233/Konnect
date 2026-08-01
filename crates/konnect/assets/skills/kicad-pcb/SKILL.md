---
name: kicad-pcb
description: |
  Workflow skill for KiCAD PCB layout and routing via MCP tools. Triggers on: "layout the board",
  "route traces", "PCB", "place footprints", "copper pour", "board outline", "differential pair",
  "board setup", "track width", "via", "zone", "design rules", "stackup", "silkscreen".
argument-hint: "[layout task]"
---

# KiCAD PCB Layout Workflow

This skill guides Claude to perform PCB layout using Konnect MCP tools.
ALL modifications go through MCP tools — never edit .kicad_pcb files directly.

---

## Prerequisites

PCB layout operations require KiCAD to be running with the board file open. The IPC
connection communicates with the running KiCAD instance in real-time.

If connection fails:
- Tell the user to open KiCAD and load the project
- The board (.kicad_pcb) must be open in the PCB editor
- KiCAD's IPC API must be enabled (default in KiCAD 8+)

---

## Toolset Loading

Before any PCB work, load the required toolsets:

```
load_toolset('pcb_board')        # board outline, layers, setup, stackup
load_toolset('pcb_components')   # place, move, rotate, align footprints
load_toolset('pcb_routing')      # traces, vias, differential pairs
```

Zones (`pcb_board`: add_zone; `pcb_routing`: add_copper_pour), component/net queries (`pcb_components`: find_component, get_component_list; `pcb_board`: get_board_info), and bulk placement (`pcb_components`: place_component_array, align_components, duplicate_component) are already covered by the toolsets loaded above.

Load additional toolsets as needed:

```
load_toolset('config')           # design rule storage: add_design_rule, list_design_rules
load_toolset('verification')     # run_drc, set_design_rules, get_design_rules, check_clearance
```

Always call `get_active_toolsets()` first to see what is already loaded.

---

## Layout Order

Follow this sequence for a clean PCB workflow:

1. **Board outline** — `set_board_size` or draw Edge.Cuts geometry
2. **Import netlist** — sync with schematic (update_pcb_from_schematic)
3. **Place components** — position all footprints
4. **Route traces** — connect all nets
5. **Copper pour** — add ground/power zones last
6. **DRC** — run design rule check
7. **Save** — `save_project`

Do NOT add copper pours before routing is complete — they interfere with interactive routing.

---

## Placement

### Strategy

- Group components by functional block (power, digital, analog, connectors)
- Place ICs first, then their associated passives
- Decoupling caps: within 2mm of their IC power pins, on same layer
- Connectors: at board edges, accessible for cables
- High-frequency components: minimize trace lengths between them
- Thermal considerations: power components away from sensitive analog

### Placement Tools

| Tool                      | Use Case                                    |
|---------------------------|---------------------------------------------|
| `place_component`         | Position a single footprint at x,y          |
| `move_component`          | Relocate an existing footprint              |
| `rotate_component`        | Rotate footprint (0/90/180/270)             |
| `align_components`        | Align multiple components (top/bottom/left/right/center) |
| `place_component_array`   | Grid placement for repeated elements        |

### Placement Tips

- Use mm coordinates (KiCAD default for PCB)
- Standard grid: 0.5mm for placement, 0.25mm for fine adjustment
- Check component courtyard overlaps after placement
- Reference designator text: F.SilkS layer, 1mm height default

---

## Routing

### Routing Tools

| Tool                      | Use Case                                    |
|---------------------------|---------------------------------------------|
| `route_pad_to_pad`        | Direct connection, auto L-bend routing      |
| `route_trace`             | Manual segment-by-segment routing           |
| `route_differential_pair` | Matched-length USB/LVDS/Ethernet pairs      |
| `add_via`                 | Layer transition                            |
| `create_netclass`         | Define width/clearance rules for net groups |

### route_pad_to_pad

The primary routing tool. Automatically creates an L-shaped trace between two pads.

```
route_pad_to_pad(from_reference, from_pad, to_reference, to_pad, width, layer)
```

- Handles 90-degree bends automatically
- Specify width in mm (e.g., 0.25 for signal, 0.5 for power)
- Will use vias if pads are on different layers

### route_trace

For manual control over trace path. Specify waypoints.

```
route_trace(net_name, points, width, layer)
```

- Use when auto-routing creates suboptimal paths
- Provide intermediate points for complex routes
- Each segment is a straight line between points

### route_differential_pair

For high-speed differential signals (USB, HDMI, Ethernet, LVDS).

```
route_differential_pair(net_positive, net_negative, width, spacing)
```

- Maintains constant spacing between P and N traces
- Matches trace lengths automatically
- Common pairs: USB_D+/USB_D-, LVDS_P/LVDS_N

### Netclasses

Define routing rules for groups of nets:

```
create_netclass(name, track_width, clearance, via_drill, via_diameter)
```

Common netclass configurations:
- Signal: 0.25mm track, 0.2mm clearance
- Power: 0.5-1.0mm track, 0.3mm clearance
- USB: 0.3mm track, 0.15mm spacing (90 ohm differential)

### Via Defaults

- Standard signal via: 0.4mm drill, 0.8mm pad diameter
- Power via: 0.6mm drill, 1.0mm pad diameter
- Micro via (HDI): 0.1mm drill, 0.3mm pad diameter

---

## Copper Pour

Zone tools live in the `pcb_board` toolset.

### add_zone

Creates a copper pour area (polygon fill).

```
add_zone(net_name, layer, outline_points, priority)
```

- Almost always GND net on both F.Cu and B.Cu
- Define outline slightly inside board edge (0.5mm inset)
- Priority: higher number fills on top of lower
- Add thermal relief to pads (default behavior)

### refill_zones

**Must call `refill_zones` after any change that affects copper pour:**
- After adding/moving components
- After routing new traces
- After modifying zone outlines
- After changing design rules

Zones do not auto-update — stale fills cause DRC errors.

### Zone Tips

- GND pour on both layers is standard practice
- Leave spoke thermal reliefs for through-hole pads (easier soldering)
- Use keepout zones to prevent copper in sensitive areas
- Zone clearance typically 0.3-0.5mm from traces

---

## Layer Reference

| Layer    | Name     | Purpose                              |
|----------|----------|--------------------------------------|
| F.Cu     | Front Copper   | Top copper traces and pads     |
| B.Cu     | Back Copper    | Bottom copper traces and pads  |
| F.SilkS  | Front Silk     | Top silkscreen (text, outlines)|
| B.SilkS  | Back Silk      | Bottom silkscreen              |
| F.Mask   | Front Mask     | Top solder mask openings       |
| B.Mask   | Back Mask      | Bottom solder mask openings    |
| Edge.Cuts| Board Outline  | Physical board boundary        |
| F.Fab    | Front Fab      | Top fabrication drawing        |
| B.Fab    | Back Fab       | Bottom fabrication drawing     |
| F.CrtYd  | Front Courtyard| Top component clearance area   |
| B.CrtYd  | Back Courtyard | Bottom component clearance area|
| In1.Cu   | Inner 1        | Internal copper layer 1        |
| In2.Cu   | Inner 2        | Internal copper layer 2        |

### Layer Usage Guidelines

- Route signals on F.Cu and B.Cu (2-layer) or add inner layers for complex boards
- Board outline MUST be on Edge.Cuts (closed polygon or rectangle)
- Silkscreen for reference designators and polarity marks
- Courtyard defines minimum spacing between components
- Use F.Fab/B.Fab for assembly drawings and component outlines

---

## Design Rule Check

After completing layout:

```
run_drc()
```

Common DRC errors and fixes:
- **Clearance violation**: move trace or component further apart
- **Unconnected net**: route missing connection
- **Track too close to edge**: move inward from board outline
- **Courtyard overlap**: increase spacing between components
- **Zone fill error**: run `refill_zones`

---

## Rules

1. **Never edit .kicad_pcb directly** — all changes go through MCP tools
2. **Always verify placement after moves** — components may snap to unexpected positions
3. **Board outline first** — define the physical boundary before placing anything
4. **Refill zones after changes** — stale zone fills cause phantom DRC errors
5. **Check DRC before finishing** — run `run_drc()` and resolve all errors
6. **Use netclasses for consistency** — define track widths per net type, not per trace
7. **KiCAD must be running** — PCB tools require the live IPC connection
8. **Save frequently** — call `save_project` after major operations
9. **Load toolsets first** — check `get_active_toolsets()` and load what you need
10. **Copper pour last** — add zones only after routing is substantially complete
