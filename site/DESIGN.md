# Design

The visual system for the Bastion site. Aesthetic lane: **drafting-paper spec**,
a structural engineering document, not a sci-fi dashboard. Tokens live in
`src/styles/tokens.css`; this file is the human-readable rationale.

## Theme

Light, cool, high-contrast. The surface is a cool drafting paper (never warm
cream), carrying a faint structural grid. The mood lives in the typography and the
semantic color, not in a tinted-warm background. Deliberately the opposite of the
category's dark-gradient norm: a confident light treatment reads as trustworthy and
auditable, which is the whole pitch.

## Color (OKLCH)

Strategy: **restrained**. Tinted-neutral surfaces and ink, one structural brand
accent, and a semantic verdict triad that is the only saturated color.

- **Surfaces:** `--paper` 0.985 / `--paper-2` 0.967 / `--paper-sunk` 0.945 /
  `--paper-raised` white. Cool, ~0.004-0.006 chroma at hue 235.
- **Ink ramp:** `--ink` 0.23 (headings/body emphasis), `--ink-2` 0.41 (body),
  `--ink-3` 0.52 (labels/captions, large or non-body only). Cool hue 252.
- **Brand accent (steel):** `--steel` 0.50 / 0.105 / 252. Links, structural marks,
  key emphasis. Sits outside the verdict triad on purpose.
- **Verdict triad (semantic only):**
  - pass: `--pass` 0.60 / 0.135 / 150, `--pass-ink` 0.46 for text.
  - block: `--block` 0.56 / 0.195 / 26, `--block-ink` 0.50 for text.
  - advisor: `--advisor` 0.72 / 0.13 / 73, `--advisor-ink` 0.52 for text.
- **Rules:** `--line` 0.89, `--line-strong` 0.81. Full borders only, never
  side-stripes.

Verdict state is always paired with a text label, never color alone.

## Typography

One grotesque plus one mono, paired on a contrast axis. Mono is authentic here (a
Rust CLI that reads config files), not costume.

- **Sans (`--font-sans`):** Schibsted Grotesk Variable. Display, UI, and body, in
  multiple weights (640 for headings). Letter-spacing tightens to -0.022/-0.03em on
  display.
- **Mono (`--font-mono`):** JetBrains Mono Variable. All code, verdicts, labels,
  filenames, section clauses, install commands: the "artifact" voice.
- **Scale:** ~1.26 ratio, fluid `clamp()`. Display ceiling ~3.7rem in the hero (the
  layout is two-column; it is not a full-bleed shout). `--text-*` tokens from xs
  (0.78rem) to 4xl.
- Body line length capped near 66ch (`--measure`); `text-wrap: balance` on
  headings, `pretty` on prose.

## Layout

Structural and ruled, like a spec sheet. Major sections open with `SectionHeading`:
a 2px rule, the title, and a mono "clause" annotation to the right (wayfinding, not
a per-section eyebrow). Content max-width 78rem (`--width-content`), narrow blocks
52rem. Fluid `--gutter`. Responsive grids use `minmax(0, 1fr)` base tracks so a
child's `max-width` can't stretch a single column past the viewport. Crisp radii
(2-7px). Faint elevation; the page reads as paper, not glass.

## Components

- **Artifact**: a "file" card with a labeled tab (filename + lang) and an optional
  meta slot (a verdict pill). The recurring proof element.
- **Section ledgers/registries**: structural tables (reviewer registry, the gate
  ledger) that collapse to stacked records under ~860px.
- **Pills**: the verdict vocabulary as UI (`pill--pass/block/advisor/neutral`).
- **Terminals**: dark `--ink` panels for command/output, with a green pass color.

## Motion

Restrained and intentional. A measured hero reveal (staggered), bar/ledger entrances
on scroll via IntersectionObserver. Easing is ease-out-expo (`--ease-out`); no
bounce. Reveals enhance an already-visible default and have a load-time failsafe so
content never ships blank; everything has a `prefers-reduced-motion: reduce` path
(no `.js` reveal class is applied under reduced motion).
