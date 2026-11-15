# Third-party notices

Keystone itself is licensed under the MIT License (see [`LICENSE`](LICENSE)). It includes code and fonts adapted from or bundled with the projects below.

## Pele

`crates/keystone-ui/vendor/pele/pele.global.js` embeds Pele 1.6.0 by Obsidian
under the MIT license, including Lucide icon data under ISC. The full upstream
licenses and exact bundle provenance are in
[`crates/keystone-ui/vendor/pele`](crates/keystone-ui/vendor/pele/README.md).

## Inter

`crates/keystone-ui/vendor/inter` holds seven unmodified static faces of Inter 4.1
by The Inter Project Authors (https://github.com/rsms/inter), which Keystone embeds
as its default interface font. Inter is licensed under the SIL Open Font License 1.1;
the full license and the exact provenance are in
[`crates/keystone-ui/vendor/inter`](crates/keystone-ui/vendor/inter/README.md).

## gpui-component

The structure of six `crates/keystone-ui` components follows gpui-component 0.7.3
and gpui-base 0.7.3 by Longbridge (https://github.com/longbridge/gpui-kit),
Copyright 2024 - 2026 Longbridge, licensed under the Apache License 2.0. Each file
is a modified, rewritten adaptation and says so in its header; no code was copied
verbatim. The full license text is in [`LICENSES/Apache-2.0.txt`](LICENSES/Apache-2.0.txt).

| Keystone file | Upstream source |
| --- | --- |
| `crates/keystone-ui/src/select.rs` | gpui-component `select.rs` |
| `crates/keystone-ui/src/slider.rs` | gpui-component `slider.rs` |
| `crates/keystone-ui/src/split.rs` | gpui-component `resizable.rs`, gpui-base `resizable/` |
| `crates/keystone-ui/src/switch.rs` | gpui-component `switch.rs` |
| `crates/keystone-ui/src/tabs.rs` | gpui-component `tab/tab_bar.rs`, `tab/tab.rs` |
| `crates/keystone-ui/src/tree.rs` | gpui-component `tree.rs`, gpui-base `tree.rs` |

## Zeron

`crates/keystone-ui/src/markdown/incremental.rs` and `crates/keystone-ui/src/markdown/mend.rs` adapt the incremental Markdown parser and the inline-marker mending from [Zeron](https://github.com/zeronsh/zeron) (`crates/markdown/src/parser.rs` and `crates/markdown/src/mend.rs`).

```
MIT License

Copyright (c) 2026 Wing

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
