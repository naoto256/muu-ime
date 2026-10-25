# Muu mode icon artwork

These three SVG files are the authoritative, repository-owned design source
for Muu's public input-mode and component icons. They were delivered for Muu
on 2026-07-31 and contain no third-party font outline or artwork.

The production generator accepts only Muu's canonical path-only SVG subset:
one exact view box, an orange `#F28C28` background path, and a white `#FFFFFF`
nonzero glyph path. Fonts, text, images, transforms, effects, masks, and
external references are forbidden.

The component assets render both canonical paths. All three mode surfaces
(Menu, Alternate, and Palette) render the existing glyph path as a black mask
on a fully transparent canvas and are byte-identical within each mode. The
mode dictionaries mark these masks as templates and provide `Primary` labels;
macOS supplies the selected/unselected surface color. This does not introduce
a second artwork source.

Authoritative delivery SHA-256:

- `MuuComponent.svg`: `2f8cbf932ebd34b5013feb6a0d7e18ee206830c9dd936f3fa957f6c53a55b15e`
- `MuuHiragana.svg`: `68688b5f270f0d19b9be04baadabbda119d3c6476316609860daad73d40c66f8`
- `MuuRoman.svg`: `8a02a2d5dcfa1b748e407c66d013de9f7c8e59a22f5ef7c5af40a7e77e12cfa2`
