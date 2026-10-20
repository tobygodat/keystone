These minimal regression fonts are generated solely for Keystone's tests. They
contain simple rectangular glyphs, not copied font artwork. `symbol.ttf` has only
a Windows Symbol cmap and is deliberately loaded first; `sans.ttf` and `mono.ttf`
have Unicode Latin mappings. They reproduce an unavailable theme font without
depending on installed system fonts or proprietary font assets.

Regenerate with Python and `fonttools==4.67.0`: `python generate.py`.
