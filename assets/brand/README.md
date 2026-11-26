# rcmake brand kit

This directory holds the SVG brand marks, wordmarks, favicon, and Open Graph artwork for rcmake.

## Assets

| File | Dimensions or viewBox | Background | Purpose |
| --- | --- | --- | --- |
| `rcmake-mark.svg` | `0 0 128 128` | Light backgrounds, `#FFFFFF` to `#F4F4F5` | Primary square emblem |
| `rcmake-mark-dark.svg` | `0 0 128 128` | Dark backgrounds, `#09090B` to `#18181B` | Dark-mode square emblem |
| `rcmake-wordmark-light.svg` | `0 0 264 80` | Light backgrounds | Horizontal mark and `rcmake` wordmark |
| `rcmake-wordmark-dark.svg` | `0 0 264 80` | Dark backgrounds | Horizontal mark and `rcmake` wordmark |
| `favicon.svg` | `0 0 128 128` | Light and dark browser tabs | 16 by 16 pixel icon with `prefers-color-scheme` support |
| `og-card.svg` | `0 0 1200 630` | Social and link previews | Open Graph SVG source |
| `og-card.png` | `1200 x 630` | Social and link previews | Rasterized Open Graph preview card |

Self-contained copies of these assets also live in `site/assets/` so publishing `site/` directly as a GitHub Pages root needs no parent-directory traversal.

## Color tokens

### Light mode

| Role | Token | Hex | RGB |
| --- | --- | --- | --- |
| Top triangle, red | `--brand-red` | `#D92626` | `rgb(217, 38, 38)` |
| Lower-left triangle, green | `--brand-green` | `#15803D` | `rgb(21, 128, 61)` |
| Lower-right triangle, blue | `--brand-blue` | `#1D4ED8` | `rgb(29, 78, 216)` |
| Primary ink | `--ink-primary` | `#09090B` | `rgb(9, 9, 11)` |
| Secondary ink | `--ink-muted` | `#52525B` | `rgb(82, 82, 91)` |

### Dark mode

| Role | Token | Hex | RGB |
| --- | --- | --- | --- |
| Top triangle, red | `--brand-red` | `#EF4444` | `rgb(239, 68, 68)` |
| Lower-left triangle, green | `--brand-green` | `#22B55A` | `rgb(34, 181, 90)` |
| Lower-right triangle, blue | `--brand-blue` | `#3B82F6` | `rgb(59, 130, 246)` |
| Primary ink | `--ink-primary` | `#FAFAFA` | `rgb(250, 250, 250)` |
| Secondary ink | `--ink-muted` | `#A1A1AA` | `rgb(161, 161, 170)` |

## Geometry and usage

The emblem uses three congruent `30-120-30` isosceles triangles arranged with 3-fold rotational symmetry at 120 degrees along a regular pointy-top hexagon. In the `128x128` viewBox, the hexagon has circumradius `R = 56` and an inner parallel inset of `d = 3.0`, which leaves a central inverted negative-space triangle and three uniform negative-space channels. At `24px` and below, use `favicon.svg` instead. It increases the circumradius to `R = 60` and the channel inset to `d = 4.5` so every gap stays sharp on 1x and 2x displays.

Keep at least `0.25x` of the mark height as clear padding on all four sides of the emblem and wordmark. Do not rotate individual triangles, break the 3-fold symmetry, add drop shadows or gradients to the mark, or place the light variant on a dark background.

## License

All files in `assets/brand/` are dual-licensed under `MIT OR Apache-2.0`, the same license as the rcmake Rust engine. See `LICENSE-MIT` and `LICENSE-APACHE` at the repository root.
