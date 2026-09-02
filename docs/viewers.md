# xshell viewer architecture

`//view` separates resource acquisition from presentation. The active session
host reads the resource; the controlling xshell client selects and runs a local
viewer. Remote viewing therefore uses the authenticated SSH stdio tunnel and
does not expose an inbound viewer service.

## Current command

```text
//view README.md
//view "docs/design notes.rst"
//view --as markdown notes.txt
```

Relative paths are resolved against the active session cwd. `~` is expanded on
the session host, not on the controller. Automatic selection uses the reported
media type and filename extension; `--as` selects a viewer explicitly.
View paths are not confined to the session cwd: absolute paths and symlinks are
allowed when the session's operating-system user can read their canonical
target. A future chroot or capability policy can narrow that authority.

Protocol v5 source acquisition accepts only regular UTF-8 files and is bounded
to 4 MiB with a three-second daemon deadline. The response contains the
canonical host path, media type, byte length, SHA-256 hash, and content. The SSH
proxy exposes only `fabric` sessions, and acquisition requires the requesting
connection to own the selected session.

## Library boundary

The `xshell-view` crate contains:

- the streaming terminal Markdown renderer used for agent responses;
- terminal width, color, `NO_COLOR`, and sanitization policy;
- `ViewerPlugin`, the compile-time viewer interface;
- `ViewerRegistry`, which performs explicit or media-based selection;
- `ViewerContent`, the extensible presentation result;
- built-in Markdown and reStructuredText viewers.

The RST viewer intentionally implements a safe subset: section underlines,
paragraphs, emphasis, lists, explicit links, literal blocks, and `code-block`
directives. It does not import Python, Sphinx, docutils, project configuration,
or arbitrary directives.

Audit records contain acquisition/render metadata and the outcome, but not a
second copy of the source content.

## External plugins

The Rust trait is an internal composition boundary, not a stable dynamic ABI.
Third-party viewers will run out of process under a versioned manifest and
message protocol. A future manifest is expected to declare:

- plugin ID and protocol version;
- supported media types and extensions;
- accepted input form and possible output forms;
- executable identity and integrity metadata;
- network, filesystem, GPU, timeout, and memory requirements;
- whether explicit user approval is required.

Inputs will be staged or passed through constrained handles. Outputs will be
bounded terminal text, inline media, external-viewer requests, or
content-addressed derived artifacts with render manifests. xshell will not load
untrusted Rust dynamic libraries into its own process. F3D and future CAD
renderers should use this process boundary.

Binary viewer transport, content-addressed staging, external processes, inline
image protocols, multimodal attachment, and plugin installation remain
unimplemented as of protocol v6.
