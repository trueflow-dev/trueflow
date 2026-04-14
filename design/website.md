# Trueflow Landing Page (v1)

## Goals
- Present Trueflow as a precise, high-trust developer tool.
- Keep the page minimal, fast, and focused on the review flow.
- Emphasize calm polish over visual noise.

## Inspiration
- Linear (clarity, hierarchy, restrained visual language).
- Gwern-adjacent editorial restraint (long-form legibility).

## Information Architecture
- Primary surfaces:
  - `/` landing page
  - `/about/` about page
  - `/install/` install page
  - `/install.sh` one-line installer
  - `/download/<artifact_name>` raw release artifacts
- Header: logo + minimal nav (Install, About, Docs, GitHub).
- Hero: short headline, 2-3 line blurb, install command above the fold, hero screenshot.
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
- Blurb: "Trueflow turns files and diffs into semantic review blocks so you can move faster and spend less time fighting noisy hunks."
- Primary hero action: show `curl -fsSL https://trueflow.dev/install.sh | sh`
- Secondary CTAs: "Install details" and "Read the docs"
- Current support note: Apple Silicon macOS first; other targets later.

## Hero Media
- Placeholder: static frame or low-framerate loop (replace later).
- Size: ~560-720px wide on desktop.
- Styling: light frame, minimal border, subtle shadow.
- Fallback: poster image; prefers-reduced-motion static.

## Technical Direction
- Static HTML served from object storage + CDN.
- Markup: plain HTML; no framework for v1.
- CSS: custom; small reset; no framework.
- JS: none for v1.
- Hosting target: S3 + CloudFront, with Route53 DNS.
- Public-safe infra source in repo: Terraform-compatible OpenTofu definitions with no committed credentials or account-specific secrets.

## Repository Layout
- Website root: `website/`.
- Static assets: `website/assets/`.
- Main files: `website/index.html`, `website/about/index.html`, `website/install/index.html`, `website/install.sh`, `website/site.css`.
- Infra source: `infra/terraform/`.
- Deploy helpers: `scripts/deploy-website.sh`, `scripts/deploy-downloads.sh`.

## TOML Data (optional follow-up)
- `website` section for hero copy, CTA labels, and nav links if we later want templated generation.
- `color_palette_light` tokens (semantic names) if we want metadata-driven theming.
- `media` fields for hero video/poster paths if hero media becomes more dynamic.

## Accessibility & Performance
- WCAG AA contrast, visible focus, skip link.
- One CSS file, one font load per family.
- Lazy load hero media; preload critical fonts.

## Project Management
- Status: static v1 implementation underway.
- Completed: initial design brief, layout and palette guidance, static repo layout decision, initial same-domain install/download contract.
- Next: plan/review the OpenTofu stack locally, wire deployment, and publish the first Apple Silicon macOS binary artifact set.
