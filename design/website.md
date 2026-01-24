# Trueflow Landing Page (v1)

## Goals
- Present Trueflow as a precise, high-trust developer tool.
- Keep the page minimal, fast, and focused on the review flow.
- Emphasize calm polish over visual noise.

## Inspiration
- Linear (clarity, hierarchy, restrained visual language).
- Gwern-adjacent editorial restraint (long-form legibility).

## Information Architecture
- Single page only.
- Header: logo + minimal nav (Docs, GitHub, Blog optional).
- Hero: short headline, 2-3 line blurb, single primary CTA, hero video/gif.
- Optional below-fold: 3 short benefits and a small "how it works" row.
- Footer: legal + links.

## Layout
- Max width: 1100px; content column: 640-720px.
- Two-column hero on desktop, single column on mobile.
- Hero media aligned right, framed like a calm tool window.

## Visual System

### Typography
- Headlines: "Space Grotesk" (brand-aligned).
- Body: "Source Serif 4" for editorial tone.
- Code/UI monospace: "JetBrains Mono".
- Type scale: 14 / 16 / 18 / 24 / 32 / 48.

### Color Palette (Light)
- Paper: #F7F5F2
- Surface: #FFFFFF
- Ink: #1C2024
- Muted: #5F6B72
- Border: #E3E7EB
- Accent (laminar-derived): #0E9F86
- Accent Soft: #DDF4EF
- Link: #1B4DD8
- Warning (kinetic): #F2B134
- Code Panel: #F3F4F6

### Components
- Buttons: solid accent + subtle shadow; outline secondary.
- Links: underline on hover; minimal focus rings.
- Panels: 1px border, 8px radius, slight inset shadow.

## Content Draft
- Headline: "Code review that keeps you in flow."
- Blurb: "Trueflow gives you a calm, precise review workspace-fast diffs, low friction, and zero noise."
- Primary CTA: "Get started"
- Secondary CTA: "Read the docs"

## Hero Media
- Placeholder: static frame or low-framerate loop (replace later).
- Size: ~560-720px wide on desktop.
- Styling: light frame, minimal border, subtle shadow.
- Fallback: poster image; prefers-reduced-motion static.

## Technical Direction
- Axum SSR HTML, streamed response (head -> hero -> footer).
- Templates: Askama (preferred) or Maud.
- CSS: custom; small reset; no framework.
- JS: none for v1; consider htmx only if needed later.

## Repository Layout
- Website root: `www/trueflow-web-server/`.
- Static assets: `www/trueflow-web-server/static/`.
- Templates: `www/trueflow-web-server/templates/`.

## TOML Data (to add)
- `website` section for hero copy, CTA labels, and nav links.
- `color_palette_light` tokens (semantic names).
- `media` fields for hero video/poster paths.

## Accessibility & Performance
- WCAG AA contrast, visible focus, skip link.
- One CSS file, one font load per family.
- Lazy load hero media; preload critical fonts.

## Project Management
- Status: planning complete, awaiting implementation.
- Completed: initial design brief, layout and palette guidance, repo layout decision.
- Next: add `trueflow.toml` website data, scaffold Axum crate, implement SSR templates + CSS.
