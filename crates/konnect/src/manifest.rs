//! Skill and agent manifests embedded at compile time.
//!
//! The client-aware `init` subcommand installs shared skills for Claude or Codex.
//! Claude-specific agents and hooks remain scoped to Claude's directories.
//! Hook skills are also patched into `~/.claude/settings.json`.

/// A skill to install to `~/.claude/skills/<name>/SKILL.md`.
/// Optional reference files go into `~/.claude/skills/<name>/references/`.
pub struct SkillManifest {
    pub name: &'static str,
    pub content: &'static str,
    pub references: &'static [(&'static str, &'static str)],
}

/// An agent to install to `~/.claude/agents/<filename>`.
pub struct AgentManifest {
    pub filename: &'static str,
    pub content: &'static str,
}

/// A hook-bound skill: triggers before/after specific MCP tool calls.
/// Installed as a hook entry in `~/.claude/settings.json` that runs
/// `konnect.exe skill <name>` to emit the content to stdout.
pub struct HookSkillManifest {
    pub name: &'static str,
    pub content: &'static str,
    pub tool_matcher: &'static str,
    pub event: &'static str, // "PreToolUse" or "PostToolUse"
}

// ─── Skills ──────────────────────────────────────────────────────────────────

pub const SKILLS: &[SkillManifest] = &[
    SkillManifest {
        name: "konnect",
        content: include_str!("../assets/skills/konnect/SKILL.md"),
        references: &[],
    },
    SkillManifest {
        name: "kicad-schematic",
        content: include_str!("../assets/skills/kicad-schematic/SKILL.md"),
        references: &[
            (
                "common-lib-ids.md",
                include_str!("../assets/skills/kicad-schematic/references/common-lib-ids.md"),
            ),
            (
                "wiring-patterns.md",
                include_str!("../assets/skills/kicad-schematic/references/wiring-patterns.md"),
            ),
        ],
    },
    SkillManifest {
        name: "kicad-pcb",
        content: include_str!("../assets/skills/kicad-pcb/SKILL.md"),
        references: &[
            (
                "layer-reference.md",
                include_str!("../assets/skills/kicad-pcb/references/layer-reference.md"),
            ),
            (
                "trace-width-table.md",
                include_str!("../assets/skills/kicad-pcb/references/trace-width-table.md"),
            ),
            (
                "design-rules.md",
                include_str!("../assets/skills/kicad-pcb/references/design-rules.md"),
            ),
        ],
    },
    SkillManifest {
        name: "kicad-manufacture",
        content: include_str!("../assets/skills/kicad-manufacture/SKILL.md"),
        references: &[
            (
                "jlcpcb-rules.md",
                include_str!("../assets/skills/kicad-manufacture/references/jlcpcb-rules.md"),
            ),
            (
                "gerber-layers.md",
                include_str!("../assets/skills/kicad-manufacture/references/gerber-layers.md"),
            ),
        ],
    },
    SkillManifest {
        name: "kicad-review",
        content: include_str!("../assets/skills/kicad-review/SKILL.md"),
        references: &[
            (
                "error-taxonomy.md",
                include_str!("../assets/skills/kicad-review/references/error-taxonomy.md"),
            ),
            (
                "design-checklist.md",
                include_str!("../assets/skills/kicad-review/references/design-checklist.md"),
            ),
        ],
    },
    SkillManifest {
        name: "kicad-library",
        content: include_str!("../assets/skills/kicad-library/SKILL.md"),
        references: &[],
    },
];

// ─── Agents ──────────────────────────────────────────────────────────────────

pub const AGENTS: &[AgentManifest] = &[
    AgentManifest {
        filename: "kicad-design-review-agent.md",
        content: include_str!("../assets/agents/kicad-design-review-agent.md"),
    },
    AgentManifest {
        filename: "kicad-schematic-build-agent.md",
        content: include_str!("../assets/agents/kicad-schematic-build-agent.md"),
    },
];

// ─── Hook Skills ─────────────────────────────────────────────────────────────

pub const HOOK_SKILLS: &[HookSkillManifest] = &[
    HookSkillManifest {
        name: "pre-pcb-ipc",
        content: "These PCB tools require KiCAD to be running with the board file open.\n\
                  If you get a connection error, tell the user: \"Please open KiCAD and load\n\
                  your .kicad_pcb file, then try again.\" Do not retry more than once.",
        tool_matcher: "mcp__konnect__(place_component|move_component|rotate_component|route_trace|add_via|route_differential_pair|route_pad_to_pad|refill_zones)",
        event: "PreToolUse",
    },
];
