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
//view --paginate long-report.md
//view --no-paginate short-notes.md
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

## Display policy and pagination

Resource acquisition and rendering finish before controller-local display
policy is applied. This means a document acquired from a remote host is paged
by the controller and never needs to grant the remote host access to the local
terminal. The policy precedence is:

1. `--paginate` or `--no-paginate` on the current command;
2. the canonical viewer ID under `view.viewers`;
3. the rendered media class under `view.classes`;
4. the top-level `view.pagination` default.

```toml
[view]
pagination = "auto" # auto, always, never
pager = ["less", "-R", "-X"]

[view.classes.text]
pagination = "auto"

[view.viewers.markdown]
pagination = "always"
```

`auto` starts the pager only when stdin/stdout are terminals and rendered output
exceeds the detected terminal height. `always` pages whenever the controller is
interactive; `never` writes directly. Redirected output is never sent through
an interactive pager. If an automatically selected pager cannot start, xshell
warns and safely falls back to direct output; an explicit `always` request
reports the error instead. An error after a pager has started never triggers a
second copy of potentially partially displayed output.

The pager setting is an argv vector: xshell invokes its executable directly and
does not interpret shell syntax. The default `less` invocation receives only
already-sanitized renderer output. xshell removes inherited less option, key,
and preprocessor settings and sets `LESSSECURE=1`, preventing preprocessors and
less shell/file commands. A user-configured replacement pager is trusted local
configuration.

The policy maps are the extension point for future typed options such as video
autoplay and 3D camera layout. Options are added only alongside a renderer that
implements them, so accepted configuration is never silently ignored.

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

Binary viewer transport, content-addressed staging, external renderer processes,
inline image protocols, multimodal attachment, and plugin installation remain
unimplemented.
