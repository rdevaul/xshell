use crate::config::{OutputMode, RenderingConfig};
use anyhow::{Result, bail};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::env;
use std::io::{self, IsTerminal, Write};
use std::mem;
use terminal_size::{Width, terminal_size};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DEFAULT_WIDTH: usize = 80;
const MIN_WIDTH: usize = 20;
const MAX_WIDTH: usize = 512;
const MAX_PENDING_BYTES: usize = 64 * 1024;
const MAX_BLOCK_BYTES: usize = 256 * 1024;

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
        if is_table(&lines) {
            return self.write_table(&lines, output);
        }

        let mut paragraph = Vec::new();
        for line in lines {
            if let Some((level, content)) = heading(&line) {
                self.flush_paragraph(&mut paragraph, output)?;
                let marker = if level == 1 { "▌ " } else { "▸ " };
                self.write_line(
                    &inline_spans(content),
                    marker,
                    "  ",
                    Style::default().bold().cyan(),
                    output,
                )?;
            } else if let Some((marker, content)) = list_item(&line) {
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
            } else if is_rule(&line) {
                self.flush_paragraph(&mut paragraph, output)?;
                let length = self.options.width.min(40);
                writeln!(output, "{}", "─".repeat(length))?;
                self.wrote_any = true;
                self.ends_with_newline = true;
            } else {
                paragraph.push(line);
            }
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
            self.write_styled("│", Style::default().dim(), output)?;
            for (column, width) in widths.iter().enumerate() {
                let cell = row.get(column).map(String::as_str).unwrap_or("");
                let cell = truncate_width(&plain_inline(cell), *width);
                let padding = width.saturating_sub(visible_width(&cell));
                write!(output, " ")?;
                let style = if row_index == 0 {
                    Style::default().bold()
                } else {
                    Style::default()
                };
                self.write_styled(&cell, style, output)?;
                write!(output, "{} ", " ".repeat(padding))?;
                self.write_styled("│", Style::default().dim(), output)?;
            }
            writeln!(output)?;
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

fn split_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

fn truncate_width(text: &str, width: usize) -> String {
    if visible_width(text) <= width {
        return text.to_owned();
    }
    let target = width.saturating_sub(1);
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > target {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
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
    fn rejects_unreasonable_configured_widths() {
        let config = RenderingConfig {
            width: Some(10),
            ..RenderingConfig::default()
        };
        assert!(RenderOptions::resolve_for(&config, None, None, true, false, 80).is_err());
    }
}
