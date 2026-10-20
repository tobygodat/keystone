from pathlib import Path
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib.tables._c_m_a_p import CmapSubtable

for name, family, mono, symbolic in [
    ("sans", "Keystone Test Sans", False, False),
    ("mono", "Keystone Test Mono", True, False),
    ("symbol", "Keystone Test Symbol", False, True),
]:
    builder = FontBuilder(1000, isTTF=True)
    characters = range(32, 127)
    names = {c: f"uni{c:04X}" for c in characters}
    order = [".notdef", *names.values()]
    builder.setupGlyphOrder(order)
    builder.setupCharacterMap(names)
    glyphs = {}
    for glyph in order:
        pen = TTGlyphPen(None)
        pen.moveTo((100, 0))
        pen.lineTo((500, 0))
        pen.lineTo((500, 700))
        pen.lineTo((100, 700))
        pen.closePath()
        glyphs[glyph] = pen.glyph()
    builder.setupGlyf(glyphs)
    builder.setupHorizontalMetrics({glyph: (600, 0) for glyph in order})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable({"familyName": family, "styleName": "Regular", "uniqueFontIdentifier": family,
                           "fullName": family, "psName": family.replace(" ", ""), "version": "Version 1.0"})
    builder.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200,
                     sFamilyClass=12 << 8 if symbolic else 8 << 8)
    builder.setupPost(isFixedPitch=int(mono))
    builder.setupMaxp()
    builder.font["head"].created = builder.font["head"].modified = 2082844800
    if symbolic:
        cmap = CmapSubtable.newSubtable(4)
        cmap.platformID, cmap.platEncID, cmap.language, cmap.cmap = 3, 0, 0, names
        builder.font["cmap"].tables = [cmap]
    builder.save(Path(__file__).with_name(name + ".ttf"))
