# Inter 4.1

These are unmodified static TrueType faces from the official Inter 4.1 release,
https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip (`extras/ttf/`),
fetched 2026-10-20. Upstream: https://github.com/rsms/inter (tag `v4.1`).
Inter is licensed under the SIL Open Font License 1.1; `LICENSE.txt` is the
release's own copy.

Keystone embeds them in the binary and registers them at startup, so the default
interface font is Inter whether or not it is installed. Only the faces the
interface uses are included: Regular, Medium, SemiBold, Italic and SemiBold Italic
for text, and Inter Display SemiBold and SemiBold Italic for titles. Where an
installed Inter has a face of the same weight and style, that face is used
instead. The variable font is not used because GPUI rasterizes every face at its
default instance, so its weights would all draw at 400.

Diagrams are not drawn in the bundled Inter yet: their SVG text is measured and
rasterized against installed fonts only.

SHA-256:

- Inter-4.1.zip: `9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e`
- Inter-Regular.ttf: `40d692fce188e4471e2b3cba937be967878f631ad3ebbbdcd587687c7ebe0c82`
- Inter-Medium.ttf: `97ad806f526e41546d46365bb3a393145f75b7b1568913db74549ad8b8dba872`
- Inter-SemiBold.ttf: `78a843fade9d4612a5567302fb595b56976eb5fcebf4fea5a5912d638bafcde3`
- Inter-Italic.ttf: `bbc051dd204b5019a1aa0bc0ae2aa8a05ab13e7a3f979fa357631dc7feb6833a`
- Inter-SemiBoldItalic.ttf: `eff2930c3d3b35d3fcf5f76252b6baef4c3e907d9d2fde1d16cf5d417f8deef4`
- InterDisplay-SemiBold.ttf: `0310d7a325896129730c6c8cf9a6e0f81ee258bedf77b1ff059b2a7b75f74e02`
- InterDisplay-SemiBoldItalic.ttf: `7ff566a2fb8e811f6c846050444c3638b7d370fa3aabd08758fb8a0a2012b1ee`
- LICENSE.txt: `262481e844521b326f5ecd053e59b98c8b2da78c8ee1bdbb6e8174305e54935a`
