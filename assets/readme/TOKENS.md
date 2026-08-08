# PolyForge README — Frozen Visual Tokens

> **FREEZE.** This is the single source of truth for the `polyforge` README redesign
> (dark, "forge-native" GitHub readme). Every later asset — `hero.svg`,
> `lifecycle.svg`, any PNG/WebP, any `.gif` — MUST be built from these values and
> these values only. Do not introduce new hex values, new fonts, or new radii
> without reopening this freeze.

---

## 1. Palette — 5 role-named hex values (THE COUNT RULE)

The palette has **EXACTLY 5 role-named hex values**. A role is a token whose
purpose is structural (backgrounds, text, the one accent). Support variants
(Section 2) are NOT roles and do NOT count toward this number.

| Token | Value | Role / usage |
| --- | --- | --- |
| `--pf-bg` | `#0a0a0a` | Canvas / page background. Near-black. Fill of the hero and every full-width board. |
| `--pf-surface` | `#16130f` | Raised surface: cards, terminal window, panels, diagram nodes. Always sits on `--pf-bg`. |
| `--pf-text` | `#EDEBE0` | Primary text. Warm off-white. Titles, labels, terminal content, commands. |
| `--pf-subtext` | `#9B9690` | Secondary / muted text. Metadata, captions, secondary copy, terminal prompt dims. |
| `--pf-ember` | `#FF6A3D` | The single accent. Forge ember glow. Links, active node fills, "PROVES"/key paths, the ember accent stroke. |

Count check: bg, surface, text, subtext, ember = **5 roles**. Any future todo
that adds a new role-named hex breaks the freeze — add it as a support variant
(Section 2) instead, or extend this file first.

## 2. Support variants (NOT roles — do not count toward the 5)

These are derived/sparing accents. They exist to keep the 5-role rule intact.

| Token | Value | Usage |
| --- | --- | --- |
| `--pf-ember-dark` | `#FF5B26` | Ember hover/active variant. For interactive node states and pressed/active paths in SVG. |
| `--pf-hairline` | `rgba(255,255,255,0.06)` | 1px hairlines / borders. Card outlines, terminal frame, grid rules, node connector lines. NOT a hex role — a translucent white rule. |
| `--pf-amber` | `#E8A75B` | Optional support accent. "PASSED"/verified highlight, status markers. Use sparingly, never as a second theme color. |

## 3. Type

System stacks only. **No remote fonts, no @import, no webfonts — hard constraint.**

- **Sans (UI / body):** `-apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif`
- **Mono (terminal lines, commands, code, metadata):** `ui-monospace, "Cascadia Code", "JetBrains Mono", "SF Mono", Menlo, Consolas, monospace`

Hierarchy by size/weight first, color second (per visual-direction grammar):
one display scale for the repo name, one section scale for block titles, one
body scale for copy. Terminal/command blocks always use Mono.

## 4. Shape — small technical radii (≤ 12px)

| Token | Value | Usage |
| --- | --- | --- |
| `--pf-radius-sm` | `4px` | Small chips, terminal cursor block, node markers. |
| `--pf-radius-md` | `8px` | Cards, panels, terminal window, diagram nodes. |
| `--pf-radius-lg` | `12px` | Only the outermost canvas boards (hero board corners). Never above 12px. |

Keep radii small and technical. No pill/999px shapes, no playful rounded tiles.
Hairlines = 1px (`--pf-hairline`). Spacing unit is `8px` multiples (dense).

## 5. Recurring motif — Merkle-chain link node

**The one recurring visual motif.** Each evidence append is rendered as a linked
node: a node shape (rect or circle) joined by a connector path to the next node,
forming a chain — exactly like the append-only evidence ledger polyforge writes.

- Node: small rect or circle, fill `--pf-surface`, 1px `--pf-hairline` stroke, `--pf-radius-sm`.
- Link: short horizontal connector path (1px `--pf-hairline`); a filled `--pf-ember` node = the current/latest evidence append.
- Optional per-node mono label (e.g. `e1`, `e2`, `e3`) in `--pf-subtext`.

Reuse this chain lightly in the hero proof strip and in `lifecycle.svg` as the
backbone of the stage-gate flow. It is the project-native cue — repeat it with
restraint, never as wallpaper over the whole board (per visual-direction rule).

## 6. Density

Compact technical. Dense spacing and small type for code/terminal material;
generous whitespace between major blocks (hero title zone, proof strip, sections).
Conform to the `svg-production` conventions: full-width boards use a `1200`
viewBox, keep important content ≥ 48–64 units from edges, and design for a
`900px` rendered width (smallest essential label ≥ 18 units).

---

## Copy sources (crate naming facts)

Relevant for later README sections — reuse verbatim, do not re-derive:

- Workspace crates: `polyforge-core`, `polyforge-toolrunner`, `polyforge-mcp`, `polyforge-cli`.
- Library crate names differ by underscore: `polyforge_core`, `polyforge_toolrunner`.
- CLI binary: `polyforge-cli`.
