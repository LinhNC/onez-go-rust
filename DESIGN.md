---
name: Shorten
description: A precise, minimal control surface for managing short links.
colors:
  canvas: "#f6f7f9"
  surface: "#ffffff"
  surface-subtle: "#f0f2f5"
  ink: "#101216"
  ink-soft: "#363b45"
  muted: "#626a78"
  inkSecondary: "#576170"
  inkPlaceholder: "#616b7a"
  line: "#dce1e8"
  line-strong: "#c7ced8"
  signal-blue: "#315dfb"
  signal-blue-hover: "#244bd6"
  signal-blue-soft: "#eef2ff"
  success: "#147a4b"
  success-soft: "#eaf7f0"
  warning: "#8a5b0a"
  warning-soft: "#fff5d8"
  danger: "#b92b24"
  danger-soft: "#fff0ef"
typography:
  headline:
    fontFamily: "Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "32px"
    fontWeight: 760
    lineHeight: 1.12
    letterSpacing: "-0.035em"
  title:
    fontFamily: "Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "17px"
    fontWeight: 700
    lineHeight: 1.3
  body:
    fontFamily: "Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "15px"
    fontWeight: 450
    lineHeight: 1.55
  label:
    fontFamily: "Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif"
    fontSize: "13px"
    fontWeight: 700
    lineHeight: 1.3
rounded:
  sm: "8px"
  md: "12px"
spacing:
  xs: "6px"
  sm: "10px"
  md: "16px"
  lg: "24px"
  xl: "32px"
  section: "48px"
components:
  button-primary:
    backgroundColor: "{colors.signal-blue}"
    textColor: "{colors.surface}"
    rounded: "{rounded.sm}"
    padding: "0 14px"
    height: "40px"
  button-secondary:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink-soft}"
    rounded: "{rounded.sm}"
    padding: "0 14px"
    height: "40px"
  input:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0 13px"
    height: "42px"
  data-surface:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
---

# Design System: Shorten

## 1. Overview

**Creative North Star: "The Precision Console"**

Shorten should feel like a single-purpose instrument that opens directly into the administrator's work. The interface is technical without pretending to be a terminal, minimal without hiding state, and modern without decorative novelty. Familiar tables, forms, links, and confirmations carry the experience.

The canvas is a true cool gray, working surfaces are white, and Signal Blue appears only where it communicates priority or selection. Density is moderate: enough information for fast scanning, with whitespace used to separate tasks rather than turn every item into a card.

**Key Characteristics:**

- Open data tables and divided form sections instead of card grids.
- One obvious primary action per view.
- Stable, familiar controls with complete interaction states.
- Status represented by both text and color.
- Fast, state-driven motion only.

## 2. Colors

The palette uses cool neutral infrastructure with one precise blue signal and restrained semantic colors.

### Primary

- **Signal Blue** (`#315dfb`): Primary actions, active navigation, links, and focus borders only.
- **Deep Signal Blue** (`#244bd6`): Hover/pressed treatment for Signal Blue controls.

### Neutral

- **Cool Canvas** (`#f6f7f9`): Application background; never substitute cream, beige, or warm paper.
- **Control Surface** (`#ffffff`): Forms, tables, navigation, and focused working surfaces.
- **Carbon Ink** (`#101216`): Headings and high-emphasis text.
- **Graphite** (`#363b45`): Body text and data values.
- **Operational Gray** (`#626a78`): Secondary copy that still meets WCAG contrast.
- **Secondary Ink** (`--ink-secondary`, `#576170`): The readable secondary text colour — 6.2:1 on white, 5.7:1 on canvas. Every `color:` use goes through this token (and `--ink-placeholder`, `#616b7a`, for placeholders). `--muted` resolves to it as well, because the console's `app.css` reads `--muted` as text; the recessed wash itself is a separate token (`--surface-subtle`, `#f0f2f5`) and is only ever a background. Never paint text with a wash.
- **Structural Line** (`#dce1e8`): Section and row boundaries.

### Secondary

- **Operational Green** (`#147a4b`) / `#eaf7f0`: Active and success state.
- **Expiration Amber** (`#8a5b0a`) / `#fff5d8`: Expired or time-sensitive state.
- **Destructive Red** (`#b92b24`) / `#fff0ef`: Errors and destructive actions.

**The Signal Rule.** Signal Blue occupies less than 10% of a normal screen. Its rarity communicates priority.

## 3. Typography

**Display Font:** Inter with system sans-serif fallbacks  
**Body Font:** Inter with system sans-serif fallbacks  
**Label/Mono Font:** The same family, with tabular numerals for counts

**Character:** One disciplined sans family keeps the tool neutral and fast. Hierarchy comes from weight, size, alignment, and space—not a decorative display face.

### Hierarchy

- **Headline** (760, 32px, 1.12): Page titles only; `-0.035em` tracking is the tightest allowed.
- **Title** (700, 17px, 1.3): Form sections and compact panel headings.
- **Body** (450, 15px, 1.55): Guidance and primary prose, capped at 65–75ch.
- **Data** (500–730, 14px, 1.4): Table content; visits use tabular numerals.
- **Label** (700, 13px, 1.3): Form labels and table summaries; sentence case, never decorative uppercase tracking.

**The Working Type Rule.** Product copy is concise and direct. Do not use marketing-scale headings or display typography in controls.

## 4. Elevation

The system is flat by default. Depth comes from tonal layers, borders, and sticky positioning. Wide ambient shadows are not part of the vocabulary; focus uses a compact blue ring, and overlays may use a single structural shadow only when they leave document flow.

### Shadow Vocabulary

- **Focus ring** (`0 0 0 3px rgba(49, 93, 251, .15)`): Inputs and interactive controls on keyboard or field focus.
- **Floating overlay** (`0 6px 8px rgba(16, 18, 22, .12)`): Menus/popovers only, without an accompanying decorative border-and-wide-shadow combination.

**The Flat-By-Default Rule.** A surface must earn elevation through interaction or layering; decoration alone is not enough.

## 5. Components

### Buttons

- **Shape:** Compact rounded rectangle (`8px`), 40px minimum height.
- **Primary:** Signal Blue with white text, 14px/700 label, icon only when it clarifies the verb.
- **Hover / Focus:** Deep Signal Blue on hover; visible blue focus ring; 1px active translation only.
- **Secondary / Ghost:** White structural-border buttons and low-emphasis text actions use the same geometry.
- **Danger:** Solid Destructive Red only inside explicit destructive workflows.

### Chips

- **Style:** Used only for lifecycle status; compact full pill with text, semantic tint, and a same-color dot.
- **State:** Active, Disabled, and Expired always remain readable without color.

### Cards / Containers

- **Corner Style:** `12px` maximum for working surfaces.
- **Background:** White on Cool Canvas.
- **Shadow Strategy:** None at rest.
- **Border:** One `#dce1e8` structural border where a bounded surface is necessary.
- **Internal Padding:** 16–26px depending on density.

### Inputs / Fields

- **Style:** White, 42px minimum height, `8px` radius, `#c7ced8` border, 13–15px deliberate control typography.
- **Focus:** Signal Blue border and compact focus ring.
- **Error / Disabled:** Errors use text plus color; disabled controls keep legible text and suppress pointer cues.

### Navigation

The desktop top bar contains the brand, Links, New link, and session action. Active navigation is Carbon Ink with a 2px Signal Blue underline. Mobile keeps the brand and primary creation action and removes redundant navigation.

### Link Table

Desktop uses a true table with a neutral header and 72px rows. Mobile removes the header and turns each row into an open stacked record separated by lines—not a card. Short URL and destination remain the dominant scan path; status, visits, and actions follow.

## 6. Do's and Don'ts

### Do:

- **Do** keep one obvious primary action per view and use `#315dfb` consistently for it.
- **Do** show status with readable text, a dot, and semantic color.
- **Do** retain table anatomy on desktop and open divided records on mobile.
- **Do** use 150–200ms state transitions with `cubic-bezier(.16, 1, .3, 1)` and a reduced-motion fallback.
- **Do** preserve WCAG 2.2 AA contrast, visible focus, semantic labels, and keyboard operation.

### Don't:

- **Don't** build flashy crypto-style dashboards with neon gradients, glowing chrome, or decorative terminal effects.
- **Don't** expose a dense enterprise control wall; reveal advanced and destructive actions progressively.
- **Don't** use generic SaaS marketing patterns, oversized marketing headlines, decorative metric cards, glassmorphism, or gradient text.
- **Don't** replace familiar form, table, navigation, and confirmation conventions with novel controls.
- **Don't** use cream, sand, beige, paper, or warm-tinted body backgrounds; the canvas is `#f6f7f9`.
- **Don't** use colored side stripes, nested cards, radius above 16px on working surfaces, or borders paired with wide decorative shadows.
