# Third-Party Notices

This project (`signalridge/mare`) is licensed under the Apache License, Version 2.0 ([`LICENSE`](LICENSE)). Portions incorporate or adapt third-party open-source files as listed below.

Release archives, Linux packages, package-manager installs, and the container image also ship `licenses/RUST-THIRD-PARTY-NOTICES.txt`: the license texts and NOTICE/COPYRIGHT files of every third-party Rust crate compiled into the `mare` binary, generated from `Cargo.lock` by the cargo-about version pinned in `.github/ci-tools.json`.

## Ported Third-Party Files

- **pi (`earendil-works/pi-mono`) & `chardetng`** — MIT License & Apache License 2.0:
  - `crates/mare-binding/src/outdecode.rs`: ported incremental child-process output decoder (MIT, Copyright (c) 2025 Mario Zechner, (c) 2026 contributors) and non-UTF-8 charset detection / IBM866 → Windows-1252 smart-punctuation handling (`chardetng`, Apache-2.0).
- **`sse-stream` 0.3.0** (`https://github.com/4t145/sse-stream`) — MIT License OR Apache License 2.0 (Copyright (c) 2024 4t145):
  - `crates/mare-binding/src/sse/mod.rs`: ported `Blocks` stream adapter (modified with line/event byte caps, `on_comment` callback, and `dispatch`).
  - `crates/mare-binding/src/sse/parser.rs`: ported from `sse-stream` 0.3.0 `src/stream/parser.rs` (modified for `on_comment` callback and std-only UTF-8 validation).
  - `crates/mare-binding/src/sse/parser/scan.rs`: ported from `sse-stream` 0.3.0 `src/stream/parser/scan.rs` (std-only UTF-8 validation).
  - `crates/mare-binding/src/sse/parser/data.rs`: ported from `sse-stream` 0.3.0 `src/stream/parser/data.rs`.
  - `crates/mare-binding/src/sse/parser/data_tests.rs`: ported from `sse-stream` 0.3.0 `src/stream/parser/data/tests.rs`.
  - `crates/mare-binding/src/sse/tests.rs`: `[sse-stream]` test cases ported from `tests/test_bytes_parse/{fields,fragments,input,lifecycle}.rs`, `tests/test_data_capacity.rs`, `tests/test_utf8.rs`, and `src/body/tests.rs`.
- **`eventsource-parser` 4.1.3** (`https://github.com/rexxars/eventsource-parser`, commit `f46b5575f72682527ed936763983bdda0e1283f5`) — MIT License (Copyright (c) 2026 Espen Hovlandsdal `<espen@hovlandsdal.com>`):
  - `crates/mare-binding/src/sse/tests.rs`: `[eventsource-parser]` test cases translated from `test/parse.test.ts` and `test/fixtures.ts`.
- **Kimi Code (`MoonshotAI/kimi-code`)** — MIT License & Apache License 2.0:
  - `crates/mare-shared/src/redact.rs`: includes key-name and raw secret redaction patterns (MIT, Copyright (c) 2026 Moonshot AI) and token redaction patterns (Apache-2.0).

## OpenAI Codex Derived Files (Apache-2.0)

The following files include code derived from **OpenAI Codex** (`https://github.com/openai/codex`, Apache License, Version 2.0, Copyright 2025 OpenAI), modified for mare:

- `crates/mare-tui/src/reflow.rs` (debounce and row caps)
- `crates/mare-tui/src/stream.rs` (stream chunking policy)
- `crates/mare-tui/src/winprobe.rs` (Windows terminal probing)
- `crates/mare-tui/src/paste.rs` (paste-burst handling)
- `crates/mare-shared/src/style/wrap.rs` (word/URL wrapping)
- `crates/mare-binding/src/winsandbox/default_win.rs` (default Windows sandbox runner)
- `crates/mare-binding/src/winsandbox/desktop_win.rs` (private Windows desktop setup)
- `crates/mare-binding/src/winsandbox/enhanced_win.rs` (enhanced Windows sandbox setup)
- `crates/mare-binding/src/winsandbox/enhanced_ipc_win.rs` (account-runner transport)
- `crates/mare-binding/src/winsandbox/ipc_io_win.rs` (overlapped named-pipe I/O)
- `crates/mare-binding/src/winsandbox/runner_ipc.rs` (enhanced account-runner framing)
- `crates/mare-cli/src/headless_output.rs` (bounded ordered stdout writer)

Integration tests under `crates/mare-cli/tests/` also adapt behavioral test cases from OpenAI Codex (Apache-2.0).

### Upstream OpenAI Codex NOTICE (Apache-2.0 §4(d))

> OpenAI Codex
> Copyright 2025 OpenAI
>
> This project includes code derived from [Ratatui](https://github.com/ratatui/ratatui), licensed under the MIT license.
> Copyright (c) 2016-2022 Florian Dehau
> Copyright (c) 2023-2025 The Ratatui Developers

## Vendored Runtime Libraries

- **Cordis / `cordis-lua`** (`crates/mare-kernel/lua/cordis/`, MIT License): vendored Lua port of Cordis (`https://github.com/cordiverse/cordis`, Copyright (c) 2021-present Shigma; `signalridge/cordis-lua`, Copyright (c) 2026 Signalridge). Full license accompanies release distributions at `licenses/cordis-LICENSE` (and `/usr/share/licenses/mare/cordis-LICENSE` in Linux packages and container images).

## Embedded Syntax Definitions and Themes

The `mare` binary embeds syntax definitions compiled from `crates/mare-tui/assets/syntaxes/` into `crates/mare-tui/assets/syntaxes.packdump` (via `syntect`) and two syntax themes dumped into `crates/mare-tui/assets/theme.themedump` and `crates/mare-tui/assets/theme-light.themedump`. All license texts referenced below are reproduced verbatim in the [License Texts](#license-texts) section of this file, which ships with every release archive, Linux package, package-manager install, and container image.

### Embedded Sublime Syntax Definitions (23 files)

All 23 grammar files originate from **Sublime Text Default Packages** (`https://github.com/sublimehq/Packages`), governed by the Sublime Packages License (`LICENSE`), with additional package/file notices noted below:

| File | Upstream (`sublimehq/Packages`) | Copyright / Attribution | License |
| --- | --- | --- | --- |
| `crates/mare-tui/assets/syntaxes/Bash.sublime-syntax` | `ShellScript/Bash.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/C.sublime-syntax` | `C++/C.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/C++.sublime-syntax` | `C++/C++.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/CSS.sublime-syntax` | `CSS/CSS.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Go.sublime-syntax` | `Go/Go.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/HTML (Plain).sublime-syntax` | `HTML/HTML (Plain).sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Java.sublime-syntax` | `Java/Java.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/JavaProperties.sublime-syntax` | `Java/JavaProperties.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/JavaScript.sublime-syntax` | `JavaScript/JavaScript.sublime-syntax` (derived from `https://github.com/Benvie/JavaScriptNext.tmLanguage`) | Sublime HQ Pty Ltd & contributors; Copyright (c) 2015 simonzack, zertosh, benvie | Sublime Packages License; MIT License |
| `crates/mare-tui/assets/syntaxes/JSON.sublime-syntax` | `JSON/JSON.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Lua.sublime-syntax` | `Lua/Lua.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Makefile.sublime-syntax` | `Makefile/Makefile.sublime-syntax` (converted from Makefile Improved by Kay-Uwe (Kiwi) Lorenz; rewritten by Raoul Wols, May 2017) | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Markdown.sublime-syntax` | `Markdown/Markdown.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Python.sublime-syntax` | `Python/Python.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/RegExp (Basic).sublime-syntax` | `Regular Expressions/RegExp (Basic).sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Regular Expressions (JavaScript).sublime-syntax` | `JavaScript/Regular Expressions (JavaScript).sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Ruby.sublime-syntax` | `Ruby/Ruby.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Rust.sublime-syntax` | `Rust/Rust.sublime-syntax` (see `Rust/LICENSE.txt`) | Sublime HQ Pty Ltd & contributors | MIT License; Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/Shell-Unix-Generic.sublime-syntax` | `ShellScript/Shell-Unix-Generic.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/SQL (basic).sublime-syntax` | `SQL/SQL (basic).sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/TOML.sublime-syntax` | `TOML/TOML.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/XML.sublime-syntax` | `XML/XML.sublime-syntax` | Sublime HQ Pty Ltd & contributors | Sublime Packages License |
| `crates/mare-tui/assets/syntaxes/YAML.sublime-syntax` | `YAML/YAML.sublime-syntax` | Copyright (c) 2015 FichteFoll `<fichtefoll2@googlemail.com>`; Sublime HQ Pty Ltd & contributors | MIT License; Sublime Packages License |

### Embedded Themes (2 files)

- **`base16-ocean.dark`** (`crates/mare-tui/assets/theme.themedump`): bundled via `syntect` defaults from **Spacegray** (`https://github.com/SublimeText/Spacegray`, formerly `https://github.com/kkga/spacegray`, MIT License, Copyright (c) 2024 Sublime Text Packages / Gadzhi Kharkharov), based on **Base16 for TextMate** (`https://github.com/chriskempson/base16-textmate`, MIT License, Copyright (C) 2012 Chris Kempson).
- **`InspiredGitHub`** (`crates/mare-tui/assets/theme-light.themedump`): bundled via `syntect` defaults from **InspiredGitHub.tmtheme** (`https://github.com/sethlopezme/InspiredGitHub.tmtheme`, MIT License, Copyright (c) 2015 Seth Lopez).

---

## License Texts

### Sublime Packages License (`sublimehq/Packages`)

```text
Permission to copy, use, modify, sell and distribute this software is granted.
This software is provided "as is" without express or implied warranty, and with
no claim as to its suitability for any purpose.
```

### MIT License

```text
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

### Apache License, Version 2.0

```text
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS
```
