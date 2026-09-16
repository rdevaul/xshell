# xshell Markdown Rendering Test Suite

This document exercises every rendering path in the xshell Markdown renderer.
It includes headings, paragraphs, lists, quotes, tables, links, inline code,
and fenced code blocks. Long paragraphs should wrap to the detected terminal
width while code blocks remain structurally intact and left-aligned.

## 1. Paragraphs and Text Formatting

A *single* paragraph with **bold**, _italic_, ~~strikethrough~~, and `inline code`.
Inline code must never wrap or receive Markdown formatting. This is a longer
paragraph designed to exercise terminal-width wrapping behavior. When rendered
in a narrow terminal, the prose should reflow gracefully without breaking words
or inserting mid-word hyphens. When rendered in a wide terminal, it should span
more comfortably. The renderer should also strip any ANSI control sequences
before display and only apply styling when the terminal supports it or when
the user explicitly enables color.

A **second** paragraph to verify paragraph spacing. Markdown parsers should
emit a blank line between blocks, and the renderer should respect that spacing
without adding excessive vertical gaps.

## 2. Lists

### 2.1 Unordered List

- Alpha component
- Bravo component
  - Sub-item bravo-one
  - Sub-item bravo-two
- Charlie component
  - Sub-item charlie-one
    - Nested sub-item
- Delta component

### 2.2 Ordered List

1. Initialize the session daemon
2. Negotiate protocol version
3. Discover session capabilities
4. Begin the agent loop
   1. Receive user input
   2. Classify input form
   3. Route to handler
5. Await approval or stream response

## 3. Blockquotes

> This is a blockquote that tests single-line rendering. It should be visually
> distinct from surrounding prose, typically with a left-bar or indent.

> A longer blockquote that spans multiple lines to verify that the renderer
> preserves the quote block across line breaks without accidentally splitting
> it into separate paragraphs. The wrapping behavior should be consistent with
> normal prose but maintain the visual quote indicator.
>
> This is a second paragraph inside the same blockquote, testing nested
> paragraph handling within a single quote block.

## 4. Tables

### 4.1 Session Fabric Capabilities Matrix

The table below has six rows and five columns, testing alignment, cell content
varying from single words to multi-word phrases, and header rendering.

| Session State | Transport | Agent Adapter | Approval Policy | Artifact Support |
|---|---|---|---|---|
| ephemeral | local socket | ollama-qwen3 | ask (default) | text, images |
| daemon | SSH stdio | openai-compatible | ask / auto / off | text, images, PDFs |
| durable | SSH subsystem | external-process | ask | text, images, PDFs, STEP |
| host-only | local socket | codex-cli | restricted | text, images, STL |
| fabric | SSH + catalog | hermes-agent | ask | text, images, PDFs, STEP, STL |
| multi-user | SSH + ACL | openclaw | role-based | text, images, PDFs, STEP, STL, ycpkg |

### 4.2 Artifact Renderer Support and Format Coverage

This table has seven rows and six columns, testing wider content and varying
cell lengths. Some cells contain comma-separated lists that should not be
re-wrapped.

| Format | Extension(s) | Initial Renderer | Fallback | Inline Preview | Multimodal Attach |
|---|---|---|---|---|---|
| Plain text | `.txt`, `.md`, `.csv` | built-in | none | yes | yes |
| Structured data | `.json`, `.yaml`, `.toml` | built-in | none | yes | yes |
| Raster image | `.png`, `.jpg`, `.svg` | built-in | none | yes (terminal protocol) | yes |
| 3D mesh | `.stl`, `.obj`, `.ply` | F3D | external viewer | yes (PNG render) | yes (PNG + manifest) |
| CAD geometry | `.step`, `.iges`, `.glb` | F3D | external viewer | yes (PNG render) | yes (PNG + manifest) |
| Engineering drawing | `.dxf` | F3D (partial) | dedicated converter | yes (PNG render) | yes (PNG + manifest) |
| yapCAD package | `.ycpkg` | manifest parser | none | yes (manifest + render) | yes (manifest + PNG) |

### 4.3 Platform Service and Packaging Matrix

| Platform | Service Manager | Package Format | Signing | Secret Reference | Terminal Protocol |
|---|---|---|---|---|---|
| macOS | launchd | `.pkg` universal | codesign + notarize | Keychain | Kitty / iTerm2 |
| Ubuntu | systemd --user | `.deb` | dpkg-sig | Secret Service | Kitty / WezTerm |
| Fedora | systemd --user | `.rpm` | rpm --sign | Secret Service | Kitty / WezTerm |
| Arch | systemd --user | tarball | detached sig | keyring-cli | Kitty / foot |
| Alpine | openrc / systemd | tarball | detached sig | keyring-cli | foot |

## 5. Links

- [xshell specification](xshell-specification.md)
- [xshell implementation plan](xshell-implementation-plan.md)
- [Session fabric protocol](docs/session-fabric.md)
- [Audit design notes](docs/auditing.md)

Links should render visibly (underline or distinct color) but must never
receive inline-image or code-block treatment.

## 6. Fenced Code Blocks

### 6.1 Rust code block

```rust
pub async fn classify_input(line: &str) -> InputForm {
    if let Some(rest) = line.strip_prefix("$$") {
        InputForm::StickyShell(rest.to_owned())
    } else if let Some(rest) = line.strip_prefix('$') {
        InputForm::Shell(rest.to_owned())
    } else if let Some(rest) = line.strip_prefix("//") {
        InputForm::Control(rest.to_owned())
    } else {
        InputForm::AgentMessage(line.to_owned())
    }
}
```

### 6.2 TOML configuration block

```toml
[session_fabric]
enabled = true
socket_path = "~/.local/share/xshelld/socket"

[rendering]
markdown = "auto"
color = "auto"
# width = 100  # optional; valid range is 20..512

[audit]
enabled = true
socket = "/tmp/xshell-audit.sock"
directory = "/tmp/xshell-audit"
```

### 6.3 Plain text block (no language)

```
This is a plain fenced block with no language tag.
The renderer should NOT apply syntax highlighting
but MUST preserve exact whitespace, indentation,
and line breaks. No wrapping should occur here.
```

### 6.4 Shell session block

```bash
# Start the session daemon
cargo run -p xshell-session --bin xshelld -- --config config.example.toml

# Launch the CLI with a specific model profile
XSHELL_MODEL=qwen3:4b cargo run -p xshell-cli -- --profile local-qwen

# Connect to a remote host
//connect rich@mini.local --session cad
```

## 7. Mixed Content and Edge Cases

Here is a heading followed immediately by a table with **no intervening paragraph**, testing
whether the renderer handles adjacent blocks correctly:

| Metric | Target | Status | Priority | Owner |
|---|---|---|---|---|
| PTY shell support | Phase 3 | in progress | high | terminal-ux |
| Remote bootstrap | Phase 3 | planned | high | session-fabric |
| CAD rendering | Phase 2 | planned | medium | artifact-viewer |
| FUSE projection | Phase 5 | deferred | low | resource-sharing |

Inline code inside a table cell: `--approval ask`.
Bold inside a table cell: **in progress**.
Italic inside a table cell: _planned_.
A link inside a table cell: [docs/session-fabric.md](docs/session-fabric.md).

---

## 8. Horizontal Rule

The three dashes above should render as a visible horizontal rule.
Horizontal rules are useful visual separators between major sections
and should not be confused with list items or heading underlines.

## 9. Nested Blockquote with Code

> Here is a blockquote that contains inline code `--connect` and a reference
> to a control command. It also contains a **bold** and _italic_ word.
>
> > This is a nested blockquote inside the outer blockquote. The renderer
> > should indent or visually distinguish this from the parent quote.
>
> And this returns to the outer quote level after the nested block.

## 10. Closing Summary

This document covers:

1. ✅ Headings (h1–h3, with nested h3s)
2. ✅ Paragraphs with wrapping
3. ✅ Unordered lists with two levels of nesting
4. ✅ Ordered lists with nested ordered items
5. ✅ Blockquotes with nested paragraphs and nested quotes
6. ✅ **Three** tables with 4+ rows and 5+ columns each
7. ✅ Links
8. ✅ Inline code in prose, lists, and table cells
9. ✅ Fenced code blocks (Rust, TOML, plain, Bash)
10. ✅ Horizontal rule
11. ✅ Mixed content (tables adjacent to other blocks, styled cells)
12. ✅ Bold, italic, and strikethrough in prose

If every section renders correctly, the Markdown renderer handles all
documented rendering paths from the xshell README and specification.
