# rcmake Brand Kit

Vector brand marks, wordmarks, favicon, and Open Graph artwork for **rcmake**.

## Assets

| File | Dimensions / ViewBox | Intended Surface | Purpose |
| --- | --- | --- | --- |
| `rcmake-mark.svg` | `0 0 128 128` | Light backgrounds (`#FFFFFF` to `#F4F4F5`) | Primary square emblem |
| `rcmake-mark-dark.svg` | `0 0 128 128` | Dark backgrounds (`#09090B` to `#18181B`) | Dark-mode square emblem |
| `rcmake-wordmark-light.svg` | `0 0 360 80` | Light backgrounds | Horizontal mark + `rcmake` lockup |
| `rcmake-wordmark-dark.svg` | `0 0 360 80` | Dark backgrounds | Horizontal mark + `rcmake` lockup |
| `favicon.svg` | `0 0 128 128` | Light & dark browser tabs | High-legibility 16x16 icon with `prefers-color-scheme` support |
| `og-card.svg` | `0 0 1200 630` | Social & link unfurls | Open Graph vector source |
| `og-card.png` | `1200 x 630` | Social & link unfurls | Rasterized Open Graph preview card |

Self-contained copies of these assets also live in `site/assets/` so publishing `site/` directly as a GitHub Pages root requires no parent-directory traversal.

## Color Tokens

### Light Mode

| Role | Token | Hex | RGB |
| --- | --- | --- | --- |
| Top Triangle (Red) | `--brand-red` | `#D92626` | `rgb(217, 38, 38)` |
| Lower-Left Triangle (Green) | `--brand-green` | `#15803D` | `rgb(21, 128, 61)` |
| Lower-Right Triangle (Blue) | `--brand-blue` | `#1D4ED8` | `rgb(29, 78, 216)` |
| Primary Ink | `--ink-primary` | `#09090B` | `rgb(9, 9, 11)` |
| Secondary Ink | `--ink-muted` | `#52525B` | `rgb(82, 82, 91)` |

### Dark Mode

| Role | Token | Hex | RGB |
| --- | --- | --- | --- |
| Top Triangle (Red) | `--brand-red` | `#EF4444` | `rgb(239, 68, 68)` |
| Lower-Left Triangle (Green) | `--brand-green` | `#22B55A` | `rgb(34, 181, 90)` |
| Lower-Right Triangle (Blue) | `--brand-blue` | `#3B82F6` | `rgb(59, 130, 246)` |
| Primary Ink | `--ink-primary` | `#FAFAFA` | `rgb(250, 250, 250)` |
| Secondary Ink | `--ink-muted` | `#A1A1AA` | `rgb(161, 161, 170)` |

## Geometry & Usage Guidelines

- **Construction:** Three congruent `30-120-30` isosceles triangles with exact 3-fold rotational symmetry (`120 deg`) arranged along a regular pointy-top hexagon (`R = 56` in `128x128`, inner parallel inset `d = 3.0`), framing a central inverted negative-space triangle and three uniform negative-space channels.
- **Small sizes:** Use `favicon.svg` at `24px` and below; it increases circumradius (`R = 60`) and channel inset (`d = 4.5`) so every gap stays sharp on 1x and 2x displays.
- **Clearspace:** Maintain at least `0.25x` mark height of clear padding around all four edges of the emblem and wordmark.
- **Do not:** Rotate individual triangles, break the 3-fold symmetry, add drop shadows or gradients to the mark, or place the light variant on dark backgrounds.

## License

All files in `assets/brand/` are dual-licensed under **MIT OR Apache-2.0**, matching the rcmake Rust engine. See `LICENSE-MIT` and `LICENSE-APACHE` at the repository root.
