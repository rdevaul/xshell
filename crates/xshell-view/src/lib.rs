use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::Deserialize;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::mem;
use std::path::Path;
use terminal_size::{Width, terminal_size};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DEFAULT_WIDTH: usize = 80;
const MIN_WIDTH: usize = 20;
const MAX_WIDTH: usize = 512;
const MAX_PENDING_BYTES: usize = 64 * 1024;
const MAX_BLOCK_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RenderingConfig {
    pub markdown: OutputMode,
    pub color: OutputMode,
    pub width: Option<usize>,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            markdown: OutputMode::Auto,
            color: OutputMode::Auto,
            width: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    markdown: bool,
    ansi: bool,
    width: usize,
}

impl RenderOptions {
    pub fn resolve(
        config: &RenderingConfig,
        markdown_override: Option<OutputMode>,
        color_override: Option<OutputMode>,
    ) -> Result<Self> {
        let stdout_is_terminal = io::stdout().is_terminal();
        Self::resolve_for(
            config,
            markdown_override,
            color_override,
            stdout_is_terminal,
            env::var_os("NO_COLOR").is_some(),
            detected_width(),
        )
    }

    fn resolve_for(
        config: &RenderingConfig,
        markdown_override: Option<OutputMode>,
        color_override: Option<OutputMode>,
        stdout_is_terminal: bool,
        no_color: bool,
        detected_width: usize,
    ) -> Result<Self> {
        let width = config.width.unwrap_or(detected_width);
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) {
            bail!("rendering width must be between {MIN_WIDTH} and {MAX_WIDTH} columns");
        }
        let markdown = enabled(
            markdown_override.unwrap_or(config.markdown),
            stdout_is_terminal,
        );
        let ansi = markdown
            && !no_color
            && enabled(color_override.unwrap_or(config.color), stdout_is_terminal);
        Ok(Self {
            markdown,
            ansi,
            width,
        })
    }
}

fn enabled(mode: OutputMode, is_terminal: bool) -> bool {
    match mode {
        OutputMode::Auto => is_terminal,
        OutputMode::Always => true,
        OutputMode::Never => false,
    }
}

fn detected_width() -> usize {
    env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .or_else(|| terminal_size().map(|(Width(width), _)| usize::from(width)))
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

/// A presentation-only renderer for one agent response. Input is sanitized
/// before either Markdown parsing or plain streaming, and the caller retains
/// the original response for history and auditing.
pub struct AgentRenderer {
    options: RenderOptions,
    sanitizer: TerminalSanitizer,
    pending: String,
    block: Vec<String>,
    block_bytes: usize,
    fence: Option<Fence>,
    received_delta: bool,
    wrote_any: bool,
    ends_with_newline: bool,
}

impl AgentRenderer {
    pub fn new(options: RenderOptions) -> Self {
        Self {
            options,
            sanitizer: TerminalSanitizer::default(),
            pending: String::new(),
            block: Vec::new(),
            block_bytes: 0,
            fence: None,
            received_delta: false,
            wrote_any: false,
            ends_with_newline: true,
        }
    }

    pub fn received_delta(&self) -> bool {
        self.received_delta
    }

    pub fn push<W: Write>(&mut self, input: &str, output: &mut W) -> io::Result<()> {
        self.received_delta = true;
        let sanitized = self.sanitizer.push(input);
        if sanitized.is_empty() {
            return Ok(());
        }
        if !self.options.markdown {
            output.write_all(sanitized.as_bytes())?;
            self.note_output(&sanitized);
            output.flush()?;
            return Ok(());
        }

        self.pending.push_str(&sanitized);
        while let Some(newline) = self.pending.find('\n') {
            let mut line = self.pending.drain(..=newline).collect::<String>();
            line.pop();
            self.process_line(line, output)?;
        }
        let streaming_threshold = self.options.width.saturating_mul(4).min(MAX_PENDING_BYTES);
        if self.pending.len() >= streaming_threshold {
            let line = mem::take(&mut self.pending);
            self.process_line(line, output)?;
            self.flush_block(output)?;
        }
        output.flush()
    }

    pub fn finish<W: Write>(&mut self, output: &mut W) -> io::Result<()> {
        self.sanitizer.finish();
        if self.options.markdown {
            if !self.pending.is_empty() {
                let line = mem::take(&mut self.pending);
                self.process_line(line, output)?;
            }
            self.flush_block(output)?;
            self.fence = None;
        } else if self.wrote_any && !self.ends_with_newline {
            writeln!(output)?;
            self.ends_with_newline = true;
        }
        output.flush()
    }

    fn process_line<W: Write>(&mut self, line: String, output: &mut W) -> io::Result<()> {
        if let Some(fence) = &self.fence {
            if fence.closes(&line) {
                self.fence = None;
            } else {
                self.write_code_line(&line, output)?;
            }
            return Ok(());
        }

        if let Some(fence) = Fence::opens(&line) {
            self.flush_block(output)?;
            if !fence.language.is_empty() {
                self.write_line(
                    &[Span::new(
                        format!("code: {}", fence.language),
                        Style::default().dim().cyan(),
                    )],
                    "┌ ",
                    "  ",
                    Style::default().dim().cyan(),
                    output,
                )?;
            }
            self.fence = Some(fence);
            return Ok(());
        }

        if line.trim().is_empty() {
            self.flush_block(output)?;
            writeln!(output)?;
            self.wrote_any = true;
            self.ends_with_newline = true;
            return Ok(());
        }

        self.block_bytes += line.len();
        self.block.push(line);
        if self.block_bytes >= MAX_BLOCK_BYTES {
            self.flush_block(output)?;
        }
        Ok(())
    }

    fn flush_block<W: Write>(&mut self, output: &mut W) -> io::Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let lines = mem::take(&mut self.block);
        self.block_bytes = 0;
        let mut paragraph = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            if let Some(end) = table_extent(&lines, index) {
                self.flush_paragraph(&mut paragraph, output)?;
                self.write_table(&lines[index..end], output)?;
                index = end;
                continue;
            }

            let line = &lines[index];
            if let Some((level, content)) = heading(line) {
                self.flush_paragraph(&mut paragraph, output)?;
                let marker = if level == 1 { "▌ " } else { "▸ " };
                self.write_line(
                    &inline_spans(content),
                    marker,
                    "  ",
                    Style::default().bold().cyan(),
                    output,
                )?;
            } else if let Some((marker, content)) = list_item(line) {
                self.flush_paragraph(&mut paragraph, output)?;
                let continuation = " ".repeat(UnicodeWidthStr::width(marker.as_str()));
                self.write_line(
                    &inline_spans(content),
                    &marker,
                    &continuation,
                    Style::default().green(),
                    output,
                )?;
            } else if let Some(content) = line.trim_start().strip_prefix('>') {
                self.flush_paragraph(&mut paragraph, output)?;
                self.write_line(
                    &inline_spans(content.trim_start()),
                    "│ ",
                    "│ ",
                    Style::default().dim(),
                    output,
                )?;
            } else if is_rule(line) {
                self.flush_paragraph(&mut paragraph, output)?;
                let length = self.options.width.min(40);
                writeln!(output, "{}", "─".repeat(length))?;
                self.wrote_any = true;
                self.ends_with_newline = true;
            } else {
                paragraph.push(line.clone());
            }
            index += 1;
        }
        self.flush_paragraph(&mut paragraph, output)
    }

    fn flush_paragraph<W: Write>(
        &mut self,
        paragraph: &mut Vec<String>,
        output: &mut W,
    ) -> io::Result<()> {
        if paragraph.is_empty() {
            return Ok(());
        }
        let text = paragraph
            .drain(..)
            .map(|line| line.trim().to_owned())
            .collect::<Vec<_>>()
            .join(" ");
        self.write_line(&inline_spans(&text), "", "", Style::default(), output)
    }

    fn write_code_line<W: Write>(&mut self, line: &str, output: &mut W) -> io::Result<()> {
        self.write_styled("│ ", Style::default().dim().cyan(), output)?;
        self.write_styled(line, Style::default().cyan(), output)?;
        writeln!(output)?;
        self.wrote_any = true;
        self.ends_with_newline = true;
        Ok(())
    }

    fn write_line<W: Write>(
        &mut self,
        spans: &[Span],
        first_prefix: &str,
        continuation_prefix: &str,
        prefix_style: Style,
        output: &mut W,
    ) -> io::Result<()> {
        let words = words(spans);
        self.write_styled(first_prefix, prefix_style, output)?;
        let mut column = UnicodeWidthStr::width(first_prefix);
        let continuation_width = UnicodeWidthStr::width(continuation_prefix);
        for (index, word) in words.iter().enumerate() {
            let word_width = word
                .iter()
                .map(|span| visible_width(&span.text))
                .sum::<usize>();
            let separator = usize::from(index > 0 && column > continuation_width);
            if column > continuation_width && column + separator + word_width > self.options.width {
                writeln!(output)?;
                self.write_styled(continuation_prefix, prefix_style, output)?;
                column = continuation_width;
            } else if separator == 1 {
                write!(output, " ")?;
                column += 1;
            }
            for span in word {
                self.write_styled(&span.text, span.style, output)?;
            }
            column += word_width;
        }
        writeln!(output)?;
        self.wrote_any = true;
        self.ends_with_newline = true;
        Ok(())
    }

    fn write_table<W: Write>(&mut self, lines: &[String], output: &mut W) -> io::Result<()> {
        let rows = lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 1)
            .map(|(_, line)| split_table_row(line))
            .collect::<Vec<_>>();
        let columns = rows.first().map(Vec::len).unwrap_or(0);
        if columns == 0 {
            return Ok(());
        }
        if columns.saturating_mul(4).saturating_add(1) > self.options.width {
            for (index, row) in rows.iter().enumerate() {
                let text = row
                    .iter()
                    .map(|cell| plain_inline(cell))
                    .collect::<Vec<_>>()
                    .join(" │ ");
                let style = if index == 0 {
                    Style::default().bold()
                } else {
                    Style::default()
                };
                self.write_line(&[Span::new(text, style)], "│ ", "│ ", style, output)?;
            }
            return Ok(());
        }
        let available = self.options.width.saturating_sub(columns * 3 + 1);
        let per_column = (available / columns).max(3);
        let mut widths = vec![3; columns];
        for row in &rows {
            for (index, cell) in row.iter().take(columns).enumerate() {
                widths[index] = widths[index]
                    .max(visible_width(&plain_inline(cell)))
                    .min(per_column);
            }
        }

        for (row_index, row) in rows.iter().enumerate() {
            let wrapped = widths
                .iter()
                .enumerate()
                .map(|(column, width)| {
                    let cell = row.get(column).map(String::as_str).unwrap_or("");
                    wrap_plain(&plain_inline(cell), *width)
                })
                .collect::<Vec<_>>();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            let style = if row_index == 0 {
                Style::default().bold()
            } else {
                Style::default()
            };
            for physical_line in 0..height {
                self.write_styled("│", Style::default().dim(), output)?;
                for (column, width) in widths.iter().enumerate() {
                    let cell = wrapped[column]
                        .get(physical_line)
                        .map(String::as_str)
                        .unwrap_or("");
                    let padding = width.saturating_sub(visible_width(cell));
                    write!(output, " ")?;
                    self.write_styled(cell, style, output)?;
                    write!(output, "{} ", " ".repeat(padding))?;
                    self.write_styled("│", Style::default().dim(), output)?;
                }
                writeln!(output)?;
            }
            if row_index == 0 {
                self.write_styled("├", Style::default().dim(), output)?;
                for (index, width) in widths.iter().enumerate() {
                    self.write_styled(&"─".repeat(width + 2), Style::default().dim(), output)?;
                    self.write_styled(
                        if index + 1 == columns { "┤" } else { "┼" },
                        Style::default().dim(),
                        output,
                    )?;
                }
                writeln!(output)?;
            }
        }
        self.wrote_any = true;
        self.ends_with_newline = true;
        Ok(())
    }

    fn write_styled<W: Write>(&self, text: &str, style: Style, output: &mut W) -> io::Result<()> {
        if text.is_empty() || !self.options.ansi || style.is_plain() {
            return write!(output, "{text}");
        }
        write!(output, "\x1b[{}m{text}\x1b[0m", style.ansi_codes())
    }

    fn note_output(&mut self, text: &str) {
        self.wrote_any = true;
        self.ends_with_newline = text.ends_with('\n');
    }
}

#[derive(Debug, Clone)]
struct Fence {
    marker: char,
    length: usize,
    language: String,
}

impl Fence {
    fn opens(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next()?;
        if marker != '`' && marker != '~' {
            return None;
        }
        let length = trimmed
            .chars()
            .take_while(|character| *character == marker)
            .count();
        if length < 3 {
            return None;
        }
        Some(Self {
            marker,
            length,
            language: trimmed[length..].trim().to_owned(),
        })
    }

    fn closes(&self, line: &str) -> bool {
        let trimmed = line.trim();
        trimmed
            .chars()
            .take_while(|character| *character == self.marker)
            .count()
            >= self.length
            && trimmed.chars().all(|character| character == self.marker)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    dim: bool,
    color: Option<u8>,
}

impl Style {
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    fn strike(mut self) -> Self {
        self.strike = true;
        self
    }

    fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    fn cyan(mut self) -> Self {
        self.color = Some(36);
        self
    }

    fn green(mut self) -> Self {
        self.color = Some(32);
        self
    }

    fn blue(mut self) -> Self {
        self.color = Some(34);
        self
    }

    fn is_plain(self) -> bool {
        self == Self::default()
    }

    fn ansi_codes(self) -> String {
        let mut codes = Vec::new();
        if self.bold {
            codes.push("1");
        }
        if self.dim {
            codes.push("2");
        }
        if self.italic {
            codes.push("3");
        }
        if self.underline {
            codes.push("4");
        }
        if self.strike {
            codes.push("9");
        }
        let color;
        if let Some(value) = self.color {
            color = value.to_string();
            codes.push(&color);
        }
        codes.join(";")
    }
}

#[derive(Debug, Clone)]
struct Span {
    text: String,
    style: Style,
}

impl Span {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

fn inline_spans(markdown: &str) -> Vec<Span> {
    let parser = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH);
    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut links = Vec::new();
    for event in parser {
        match event {
            Event::Start(Tag::Emphasis) => style = style.italic(),
            Event::End(TagEnd::Emphasis) => style.italic = false,
            Event::Start(Tag::Strong) => style = style.bold(),
            Event::End(TagEnd::Strong) => style.bold = false,
            Event::Start(Tag::Strikethrough) => style = style.strike(),
            Event::End(TagEnd::Strikethrough) => style.strike = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                links.push(dest_url.into_string());
                style = style.underline().blue();
            }
            Event::End(TagEnd::Link) => {
                style.underline = false;
                style.color = None;
                if let Some(destination) = links.pop()
                    && !destination.is_empty()
                {
                    spans.push(Span::new(
                        format!(" ({destination})"),
                        Style::default().dim().blue(),
                    ));
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                spans.push(Span::new(text.into_string(), style));
            }
            Event::Code(code) => {
                spans.push(Span::new(code.into_string(), Style::default().cyan()));
            }
            Event::SoftBreak | Event::HardBreak => {
                spans.push(Span::new(" ", style));
            }
            Event::TaskListMarker(checked) => {
                spans.push(Span::new(if checked { "[x] " } else { "[ ] " }, style))
            }
            Event::FootnoteReference(reference) => {
                spans.push(Span::new(format!("[{reference}]"), style));
            }
            _ => {}
        }
    }
    spans
}

fn plain_inline(markdown: &str) -> String {
    inline_spans(markdown)
        .into_iter()
        .map(|span| span.text)
        .collect()
}

fn words(spans: &[Span]) -> Vec<Vec<Span>> {
    let mut words = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    for span in spans {
        for character in span.text.chars() {
            if character.is_whitespace() {
                if !current.is_empty() {
                    words.push(mem::take(&mut current));
                }
            } else if let Some(last) = current.last_mut().filter(|last| last.style == span.style) {
                last.text.push(character);
            } else {
                current.push(Span::new(character.to_string(), span.style));
            }
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn visible_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (1..=6).contains(&level).then(|| {
        trimmed[level..]
            .strip_prefix(' ')
            .map(|content| (level, content))
    })?
}

fn list_item(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(content) = trimmed.strip_prefix(marker) {
            return Some(("• ".into(), content));
        }
    }
    let separator = trimmed.find(". ")?;
    let number = &trimmed[..separator];
    if !number.is_empty() && number.chars().all(|character| character.is_ascii_digit()) {
        return Some((format!("{number}. "), &trimmed[separator + 2..]));
    }
    None
}

fn is_rule(line: &str) -> bool {
    let compact = line
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let Some(marker) = compact.chars().next() else {
        return false;
    };
    compact.len() >= 3
        && ['-', '*', '_'].contains(&marker)
        && compact.chars().all(|character| character == marker)
}

fn is_table(lines: &[String]) -> bool {
    if lines.len() < 2 {
        return false;
    }
    let header = split_table_row(&lines[0]);
    let delimiter = split_table_row(&lines[1]);
    !header.is_empty()
        && header.len() == delimiter.len()
        && lines[0].contains('|')
        && delimiter.iter().all(|cell| {
            let cell = cell.trim().trim_matches(':');
            cell.len() >= 3 && cell.chars().all(|character| character == '-')
        })
}

fn table_extent(lines: &[String], start: usize) -> Option<usize> {
    let remaining = lines.get(start..)?;
    if !is_table(remaining) {
        return None;
    }
    let columns = split_table_row(&remaining[0]).len();
    let mut end = start + 2;
    while let Some(line) = lines.get(end) {
        if !line.contains('|') || split_table_row(line).len() != columns {
            break;
        }
        end += 1;
    }
    Some(end)
}

fn split_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let word_width = visible_width(word);
        if word_width > width {
            if !current.is_empty() {
                lines.push(mem::take(&mut current));
            }
            lines.extend(split_at_width(word, width));
        } else if current.is_empty() {
            current.push_str(word);
        } else if visible_width(&current) + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(mem::replace(&mut current, word.to_owned()));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_at_width(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for character in word.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if !current.is_empty() && current_width + character_width > width {
            chunks.push(mem::take(&mut current));
            current_width = 0;
        }
        current.push(character);
        current_width += character_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Debug, Clone, Copy, Default)]
enum SanitizeState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

#[derive(Debug, Default)]
struct TerminalSanitizer {
    state: SanitizeState,
}

/// Remove terminal control sequences from a complete string so it can be
/// printed to a terminal without altering display state. Newlines and tabs are
/// preserved; all other control characters, C1 controls, and ESC/CSI/OSC/DCS
/// sequences are dropped.
///
/// Use this for any agent-derived text that bypasses [`AgentRenderer`], such
/// as tool arguments and tool results.
pub fn sanitize_terminal_text(input: &str) -> String {
    let mut sanitizer = TerminalSanitizer::default();
    let clean = sanitizer.push(input);
    sanitizer.finish();
    clean
}

/// Render agent-derived text for a security-sensitive single-line prompt.
///
/// Control sequences are stripped as in [`sanitize_terminal_text`]; in
/// addition, newlines, tabs, and any remaining non-printable characters are
/// shown as visible escapes (`\n`, `\t`, `\u{..}`) so that the displayed text
/// occupies exactly one line and a reviewer sees every byte that will be acted
/// on. Bidirectional-override and zero-width formatting characters are also
/// escaped because they can visually reorder or hide text.
pub fn escape_for_prompt(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for character in sanitize_terminal_text(input).chars() {
        match character {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            value if value.is_control() || is_invisible_format(value) => {
                out.push_str(&format!("\\u{{{:x}}}", u32::from(value)));
            }
            value => out.push(value),
        }
    }
    out
}

fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{200b}'..='\u{200f}' // zero-width space/joiners, LRM/RLM
            | '\u{202a}'..='\u{202e}' // bidi embeddings and overrides
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{feff}' // BOM / zero-width no-break space
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerDescriptor {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub media_types: &'static [&'static str],
    pub extensions: &'static [&'static str],
}

pub struct ViewInput<'a> {
    pub name: &'a str,
    pub media_type: &'a str,
    pub text: &'a str,
}

pub enum ViewerContent {
    TerminalMarkdown(String),
}

pub trait ViewerPlugin: Send + Sync {
    fn descriptor(&self) -> ViewerDescriptor;
    fn render(&self, input: &ViewInput<'_>) -> Result<ViewerContent>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedView {
    pub viewer_id: String,
    pub bytes: Vec<u8>,
}

pub struct ViewerRegistry {
    viewers: Vec<Box<dyn ViewerPlugin>>,
}

impl ViewerRegistry {
    pub fn with_builtins() -> Self {
        Self {
            viewers: vec![Box::new(MarkdownViewer), Box::new(RestructuredTextViewer)],
        }
    }

    pub fn register(&mut self, viewer: Box<dyn ViewerPlugin>) {
        self.viewers.push(viewer);
    }

    pub fn render(
        &self,
        input: &ViewInput<'_>,
        requested_viewer: Option<&str>,
        options: RenderOptions,
    ) -> Result<RenderedView> {
        let viewer = match requested_viewer {
            Some(requested) => self
                .viewers
                .iter()
                .find(|viewer| viewer_matches_id(viewer.descriptor(), requested))
                .with_context(|| format!("unknown viewer {requested:?}"))?,
            None => self
                .viewers
                .iter()
                .find(|viewer| viewer_supports(viewer.descriptor(), input))
                .with_context(|| {
                    format!(
                        "no viewer supports {} ({})",
                        input.name,
                        if input.media_type.is_empty() {
                            "unknown media type"
                        } else {
                            input.media_type
                        }
                    )
                })?,
        };
        let descriptor = viewer.descriptor();
        let content = viewer.render(input)?;
        let mut bytes = Vec::new();
        match content {
            ViewerContent::TerminalMarkdown(markdown) => {
                let mut renderer = AgentRenderer::new(options);
                renderer.push(&markdown, &mut bytes)?;
                renderer.finish(&mut bytes)?;
            }
        }
        Ok(RenderedView {
            viewer_id: descriptor.id.into(),
            bytes,
        })
    }

    pub fn descriptors(&self) -> Vec<ViewerDescriptor> {
        self.viewers
            .iter()
            .map(|viewer| viewer.descriptor())
            .collect()
    }
}

impl Default for ViewerRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

fn viewer_matches_id(descriptor: ViewerDescriptor, requested: &str) -> bool {
    descriptor.id.eq_ignore_ascii_case(requested)
        || descriptor
            .aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(requested))
}

fn viewer_supports(descriptor: ViewerDescriptor, input: &ViewInput<'_>) -> bool {
    if descriptor
        .media_types
        .iter()
        .any(|media_type| media_type.eq_ignore_ascii_case(input.media_type))
    {
        return true;
    }
    let extension = Path::new(input.name)
        .extension()
        .and_then(|extension| extension.to_str());
    extension.is_some_and(|extension| {
        descriptor
            .extensions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(extension))
    })
}

struct MarkdownViewer;

impl ViewerPlugin for MarkdownViewer {
    fn descriptor(&self) -> ViewerDescriptor {
        ViewerDescriptor {
            id: "markdown",
            aliases: &["md"],
            media_types: &["text/markdown", "text/x-markdown"],
            extensions: &["md", "markdown", "mdown", "mkd"],
        }
    }

    fn render(&self, input: &ViewInput<'_>) -> Result<ViewerContent> {
        Ok(ViewerContent::TerminalMarkdown(input.text.to_owned()))
    }
}

struct RestructuredTextViewer;

impl ViewerPlugin for RestructuredTextViewer {
    fn descriptor(&self) -> ViewerDescriptor {
        ViewerDescriptor {
            id: "rst",
            aliases: &["restructuredtext", "restructured-text"],
            media_types: &["text/x-rst", "text/prs.fallenstein.rst"],
            extensions: &["rst", "rest"],
        }
    }

    fn render(&self, input: &ViewInput<'_>) -> Result<ViewerContent> {
        Ok(ViewerContent::TerminalMarkdown(rst_to_markdown(input.text)))
    }
}

fn rst_to_markdown(source: &str) -> String {
    let lines = source.lines().collect::<Vec<_>>();
    let mut markdown = String::with_capacity(source.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(level) = lines
            .get(index + 1)
            .and_then(|underline| rst_heading_level(line, underline))
        {
            markdown.push_str(&"#".repeat(level));
            markdown.push(' ');
            markdown.push_str(&rst_inline(line.trim()));
            markdown.push_str("\n\n");
            index += 2;
            continue;
        }

        if let Some(language) = line
            .trim_start()
            .strip_prefix(".. code-block::")
            .or_else(|| line.trim_start().strip_prefix(".. code::"))
        {
            index += 1;
            while lines.get(index).is_some_and(|line| line.trim().is_empty()) {
                index += 1;
            }
            let (code, next) = take_indented_block(&lines, index);
            let fence = markdown_fence(&code);
            markdown.push_str(&fence);
            markdown.push_str(language.trim());
            markdown.push('\n');
            markdown.push_str(&code);
            if !code.ends_with('\n') {
                markdown.push('\n');
            }
            markdown.push_str(&fence);
            markdown.push_str("\n\n");
            index = next;
            continue;
        }

        if line.trim_end().ends_with("::") {
            let mut content_start = index + 1;
            while lines
                .get(content_start)
                .is_some_and(|line| line.trim().is_empty())
            {
                content_start += 1;
            }
            if lines
                .get(content_start)
                .is_some_and(|line| is_indented(line))
            {
                let introduction = line.trim_end().trim_end_matches(':');
                if !introduction.is_empty() {
                    markdown.push_str(&rst_inline(introduction));
                    markdown.push_str("\n\n");
                }
                let (code, next) = take_indented_block(&lines, content_start);
                let fence = markdown_fence(&code);
                markdown.push_str(&fence);
                markdown.push('\n');
                markdown.push_str(&code);
                if !code.ends_with('\n') {
                    markdown.push('\n');
                }
                markdown.push_str(&fence);
                markdown.push_str("\n\n");
                index = next;
                continue;
            }
        }

        markdown.push_str(&rst_inline(line));
        markdown.push('\n');
        index += 1;
    }
    markdown
}

fn rst_heading_level(title: &str, underline: &str) -> Option<usize> {
    if title.trim().is_empty() || visible_width(underline.trim()) < visible_width(title.trim()) {
        return None;
    }
    let marker = underline.trim().chars().next()?;
    if !underline
        .trim()
        .chars()
        .all(|character| character == marker)
    {
        return None;
    }
    match marker {
        '=' => Some(1),
        '-' => Some(2),
        '~' | '^' | '"' => Some(3),
        _ => None,
    }
}

fn rst_inline(line: &str) -> String {
    let mut converted = line.replace("``", "`");
    while let Some(label_start) = converted.find('`') {
        let rest = &converted[label_start + 1..];
        let Some(target_start) = rest.find(" <") else {
            break;
        };
        let Some(end) = rest[target_start + 2..].find(">`_") else {
            break;
        };
        let label = &rest[..target_start];
        let target_end = target_start + 2 + end;
        let target = &rest[target_start + 2..target_end];
        let matched_end = label_start + 1 + target_end + 3;
        converted.replace_range(label_start..matched_end, &format!("[{label}]({target})"));
    }
    converted
}

fn is_indented(line: &str) -> bool {
    line.starts_with("   ") || line.starts_with('\t')
}

fn take_indented_block(lines: &[&str], start: usize) -> (String, usize) {
    let mut content = String::new();
    let mut index = start;
    while let Some(line) = lines.get(index) {
        if line.trim().is_empty() {
            content.push('\n');
            index += 1;
            continue;
        }
        if !is_indented(line) {
            break;
        }
        content.push_str(
            line.strip_prefix("   ")
                .unwrap_or_else(|| line.trim_start_matches('\t')),
        );
        content.push('\n');
        index += 1;
    }
    (content, index)
}

fn markdown_fence(code: &str) -> String {
    let longest = code
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.saturating_add(1).max(3))
}

impl TerminalSanitizer {
    fn push(&mut self, input: &str) -> String {
        let mut clean = String::with_capacity(input.len());
        for character in input.chars() {
            match self.state {
                SanitizeState::Ground => match character {
                    '\n' | '\t' => clean.push(character),
                    '\u{1b}' => self.state = SanitizeState::Escape,
                    '\u{9b}' => self.state = SanitizeState::Csi,
                    '\u{9d}' => self.state = SanitizeState::Osc,
                    '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
                        self.state = SanitizeState::ControlString;
                    }
                    value if !value.is_control() => clean.push(value),
                    _ => {}
                },
                SanitizeState::Escape => {
                    self.state = match character {
                        '[' => SanitizeState::Csi,
                        ']' => SanitizeState::Osc,
                        'P' | 'X' | '^' | '_' => SanitizeState::ControlString,
                        _ => SanitizeState::Ground,
                    };
                }
                SanitizeState::Csi => {
                    if ('@'..='~').contains(&character) {
                        self.state = SanitizeState::Ground;
                    }
                }
                SanitizeState::Osc => match character {
                    '\u{7}' => self.state = SanitizeState::Ground,
                    '\u{1b}' => self.state = SanitizeState::OscEscape,
                    _ => {}
                },
                SanitizeState::OscEscape => {
                    self.state = if character == '\\' {
                        SanitizeState::Ground
                    } else {
                        SanitizeState::Osc
                    };
                }
                SanitizeState::ControlString => {
                    if character == '\u{1b}' {
                        self.state = SanitizeState::ControlStringEscape;
                    }
                }
                SanitizeState::ControlStringEscape => {
                    self.state = if character == '\\' {
                        SanitizeState::Ground
                    } else {
                        SanitizeState::ControlString
                    };
                }
            }
        }
        clean
    }

    fn finish(&mut self) {
        self.state = SanitizeState::Ground;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(markdown: bool, ansi: bool, width: usize) -> RenderOptions {
        RenderOptions {
            markdown,
            ansi,
            width,
        }
    }

    #[test]
    fn resolves_auto_modes_and_honors_no_color() {
        let config = RenderingConfig::default();
        let interactive = RenderOptions::resolve_for(&config, None, None, true, false, 90).unwrap();
        assert_eq!(interactive, options(true, true, 90));

        let redirected = RenderOptions::resolve_for(&config, None, None, false, false, 90).unwrap();
        assert_eq!(redirected, options(false, false, 90));

        let no_color = RenderOptions::resolve_for(
            &config,
            Some(OutputMode::Always),
            Some(OutputMode::Always),
            true,
            true,
            90,
        )
        .unwrap();
        assert_eq!(no_color, options(true, false, 90));
    }

    #[test]
    fn renders_fragmented_markdown_blocks_at_a_fixed_width() {
        let mut renderer = AgentRenderer::new(options(true, false, 24));
        let mut output = Vec::new();
        renderer.push("# Res", &mut output).unwrap();
        assert!(output.is_empty());
        renderer
            .push(
                "ult\n\nA paragraph with **bold words** that wraps.\n\n- one\n- two\n",
                &mut output,
            )
            .unwrap();
        renderer.finish(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "▌ Result\n\nA paragraph with bold\nwords that wraps.\n\n• one\n• two\n"
        );
    }

    #[test]
    fn long_unterminated_paragraphs_stream_before_response_end() {
        let mut renderer = AgentRenderer::new(options(true, false, 20));
        let mut output = Vec::new();
        renderer.push(&"word ".repeat(20), &mut output).unwrap();
        assert!(!output.is_empty());
        renderer.finish(&mut output).unwrap();
    }

    #[test]
    fn renders_code_and_tables_without_rewriting_raw_content() {
        let mut renderer = AgentRenderer::new(options(true, false, 60));
        let mut output = Vec::new();
        renderer
            .push(
                "```rust\nfn main() {}\n```\n\n| Name | Value |\n| --- | --- |\n| bee | 3 |",
                &mut output,
            )
            .unwrap();
        renderer.finish(&mut output).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("┌ code: rust\n│ fn main() {}\n"));
        assert!(rendered.contains("│ Name"));
        assert!(rendered.contains("│ bee"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn recognizes_a_table_immediately_after_a_heading_and_wraps_cells() {
        let mut renderer = AgentRenderer::new(options(true, false, 72));
        let mut output = Vec::new();
        renderer
            .push(
                "### 📋 Not Yet Implemented (Future Phases)\n| Feature | Target Phase | Notes |\n|---|---|---|\n| CAD/Artifact Viewer | Phase 2 | F3D renderer plugin, content-addressed staging, multimodal attachment |\n| Remote Bootstrap | Phase 3 | Signed releases, launchd/systemd packaging, reconnect supervision |",
                &mut output,
            )
            .unwrap();
        renderer.finish(&mut output).unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.starts_with("▸ 📋 Not Yet Implemented (Future Phases)\n│"));
        assert!(rendered.contains("├"));
        assert!(!rendered.contains("|---|"));
        for expected in [
            "CAD/Artifact",
            "Viewer",
            "content-addressed",
            "multimodal",
            "attachment",
            "launchd/systemd",
            "supervision",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
        assert!(rendered.lines().all(|line| visible_width(line) <= 72));
    }

    #[test]
    fn very_wide_tables_fall_back_without_exceeding_terminal_width() {
        let mut renderer = AgentRenderer::new(options(true, false, 20));
        let mut output = Vec::new();
        renderer
            .push(
                "| A | B | C | D | E |\n| --- | --- | --- | --- | --- |\n| 1 | 2 | 3 | 4 | 5 |",
                &mut output,
            )
            .unwrap();
        renderer.finish(&mut output).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.lines().all(|line| visible_width(line) <= 20));
    }

    #[test]
    fn strips_fragmented_terminal_control_sequences_in_all_modes() {
        for markdown in [false, true] {
            let mut renderer = AgentRenderer::new(options(markdown, false, 80));
            let mut output = Vec::new();
            renderer.push("safe\x1b]0;stolen", &mut output).unwrap();
            renderer
                .push(" title\x1b\\ text\x1b[31", &mut output)
                .unwrap();
            renderer.push("m red\u{7}\n", &mut output).unwrap();
            renderer.finish(&mut output).unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), "safe text red\n");
        }
    }

    #[test]
    fn sanitize_terminal_text_strips_sequences_and_keeps_layout() {
        let input = "echo ok\x1b[2K\rrm -rf /\n\ttab\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(sanitize_terminal_text(input), "echo okrm -rf /\n\ttablink");
    }

    #[test]
    fn escape_for_prompt_makes_every_byte_visible_on_one_line() {
        let input = "echo ok\x1b[2K\r\nrm -rf /\t\u{202e}gnp.exe\\";
        let escaped = escape_for_prompt(input);
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains('\r'));
        assert!(!escaped.contains('\u{1b}'));
        assert_eq!(escaped, "echo ok\\nrm -rf /\\t\\u{202e}gnp.exe\\\\");
        assert_eq!(escape_for_prompt("plain text"), "plain text");
    }

    #[test]
    fn rejects_unreasonable_configured_widths() {
        let config = RenderingConfig {
            width: Some(10),
            ..RenderingConfig::default()
        };
        assert!(RenderOptions::resolve_for(&config, None, None, true, false, 80).is_err());
    }

    #[test]
    fn registry_selects_markdown_by_extension_or_explicit_override() {
        let registry = ViewerRegistry::with_builtins();
        let markdown = "# Viewer\n\n- one\n- two";
        let detected = registry
            .render(
                &ViewInput {
                    name: "README.md",
                    media_type: "text/markdown",
                    text: markdown,
                },
                None,
                options(true, false, 60),
            )
            .unwrap();
        assert_eq!(detected.viewer_id, "markdown");
        assert!(
            String::from_utf8(detected.bytes)
                .unwrap()
                .contains("▌ Viewer")
        );

        let explicit = registry
            .render(
                &ViewInput {
                    name: "README.unknown",
                    media_type: "text/plain",
                    text: markdown,
                },
                Some("md"),
                options(true, false, 60),
            )
            .unwrap();
        assert_eq!(explicit.viewer_id, "markdown");
    }

    #[test]
    fn rst_viewer_converts_headings_links_and_code_blocks() {
        let source = "Title\n=====\n\nSee `the docs <https://example.test/docs>`_.\n\n.. code-block:: python\n\n   print('bee')\n";
        let rendered = ViewerRegistry::with_builtins()
            .render(
                &ViewInput {
                    name: "guide.rst",
                    media_type: "text/x-rst",
                    text: source,
                },
                None,
                options(true, false, 72),
            )
            .unwrap();
        let text = String::from_utf8(rendered.bytes).unwrap();
        assert_eq!(rendered.viewer_id, "rst");
        assert!(text.contains("▌ Title"));
        assert!(text.contains("the docs (https://example.test/docs)"));
        assert!(text.contains("┌ code: python"));
        assert!(text.contains("│ print('bee')"));
    }
}
