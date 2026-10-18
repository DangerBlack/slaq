//! Parser for Slack's "mrkdwn".
//!
//! It is not Markdown: Slack uses `*bold*` where Markdown wants `**`, `_` for
//! italic, `~` for strikethrough, and encloses links and mentions in angle
//! brackets — `<https://x|text>`, `<@U123>`, `<#C1|general>`. Handing this text
//! to a Markdown parser gives silently wrong results, which is why this exists.
//!
//! It lives in `app/` and not in the front end because the TUI can colour text
//! too: the parsing is the same, only the drawing changes.

/// A piece of text with its formatting.
#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    Text(String),
    /// Emphasis contains other spans, not raw text: Slack puts anything
    /// inside it, and a `*<url|text>*` without nesting left the link's markup
    /// on screen instead of the link.
    Bold(Vec<Span>),
    Italic(Vec<Span>),
    Strike(Vec<Span>),
    /// Code stays verbatim: nothing inside it is interpreted.
    Code(String),
    /// `<url|text>`; without a bar, text and url coincide.
    Link {
        url: String,
        text: String,
    },
    /// `<@U123>`: the id, which the front end resolves into a name.
    User(String),
    /// `<#C123|general>`: the id selects the channel, the label is shown. The id
    /// is kept (not just the name) so a click can find the channel exactly.
    Channel {
        id: String,
        label: String,
    },
    /// `<!here>`, `<!channel>`, `<!everyone>`
    Broadcast(String),
    /// `:name:` — a standard emoji or a workspace custom one.
    Emoji(String),
}

/// A logical line of the message.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(Vec<Span>),
    /// `> quote`
    Quote(Vec<Span>),
    /// ```` ```…``` ````, kept verbatim.
    CodeBlock(String),
}

/// Resolves the HTML entities Slack puts in the text.
fn unescape(s: &str) -> String {
    // Order matters: `&amp;` goes last, otherwise `&amp;lt;` becomes `<`.
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The text to load into the editor when a message is edited.
///
/// Slack writes a linkified URL as `<url>` (or `<url|label>`), but that
/// bracketed markup, sent back through `chat.update`, is taken as literal
/// text: the link breaks. So a **bare** link goes back to its plain URL —
/// what the user originally typed — which Slack linkifies again on save.
/// HTML entities are decoded as in the renderer.
pub fn to_editable(text: &str) -> String {
    let text = unescape(text);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == '>') {
                let inner: String = chars[i + 1..close].iter().collect();
                let bare_url = !inner.contains('|')
                    && (inner.starts_with("http://") || inner.starts_with("https://"))
                    && !inner.contains(char::is_whitespace);
                if bare_url {
                    out.push_str(&inner);
                } else {
                    out.push('<');
                    out.push_str(&inner);
                    out.push('>');
                }
                i = close + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Splits the text into blocks, isolating the code blocks.
pub fn parse(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        let (before, after) = rest.split_at(start);
        push_lines(before, &mut blocks);

        let after = &after[3..];
        match after.find("```") {
            Some(end) => {
                blocks.push(Block::CodeBlock(unescape(after[..end].trim_matches('\n'))));
                rest = &after[end + 3..];
            }
            None => {
                // An unclosed block: Slack shows it as code anyway.
                blocks.push(Block::CodeBlock(unescape(after.trim_matches('\n'))));
                return blocks;
            }
        }
    }
    push_lines(rest, &mut blocks);
    blocks
}

fn push_lines(text: &str, blocks: &mut Vec<Block>) {
    // The newlines around a code block are separators, not intended empty
    // lines: keeping them would produce ghost paragraphs.
    let text = text.trim_matches('\n');
    if text.is_empty() {
        return;
    }
    // Consecutive `>` lines are one quote, not one per line: kept apart they
    // draw a separate bar and background per line, with gaps between them.
    let mut quote: Vec<Span> = Vec::new();
    for line in text.split('\n') {
        match line
            .strip_prefix("&gt; ")
            .or_else(|| line.strip_prefix("> "))
        {
            Some(q) => {
                if !quote.is_empty() {
                    quote.push(Span::Text("\n".to_string()));
                }
                quote.extend(parse_spans(q));
            }
            None => {
                if !quote.is_empty() {
                    blocks.push(Block::Quote(std::mem::take(&mut quote)));
                }
                blocks.push(Block::Paragraph(parse_spans(line)));
            }
        }
    }
    if !quote.is_empty() {
        blocks.push(Block::Quote(quote));
    }
}

/// Parses a line into formatted spans.
pub fn parse_spans(line: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push(Span::Text(unescape(&buf)));
                buf.clear();
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];

        // `<...>` — links, mentions, broadcasts
        if c == '<' {
            if let Some(close) = find_char(&chars, i + 1, '>') {
                let inner: String = chars[i + 1..close].iter().collect();
                flush!();
                out.push(parse_angle(&inner));
                i = close + 1;
                continue;
            }
        }

        // `:name:` — the opening must not follow an alphanumeric character,
        // otherwise a time like "10:30:00" would become an emoji.
        if c == ':' && !prev_is_alnum(&chars, i) {
            if let Some(close) = find_char(&chars, i + 1, ':') {
                let mut name: String = chars[i + 1..close].iter().collect();
                let mut end = close;
                // Slack writes the skin tone as `:+1::skin-tone-4:`.
                // Without consuming the suffix too, two emoji were produced,
                // the second of which — `skin-tone-4` — does not exist and
                // stayed on screen as text.
                if let Some(tone_end) = skin_tone_suffix(&chars, close) {
                    // We resume from the closing `:`, so the name becomes
                    // `+1::skin-tone-4`, the form Slack uses.
                    name.push_str(&chars[close..tone_end].iter().collect::<String>());
                    end = tone_end;
                }
                if is_emoji_name(name.split("::").next().unwrap_or(&name)) {
                    flush!();
                    out.push(Span::Emoji(name));
                    i = end + 1;
                    continue;
                }
            }
        }

        // Formatting delimiters
        if let Some(kind) = delimiter(c) {
            if let Some(close) = find_closing(&chars, i, c) {
                let inner: String = chars[i + 1..close].iter().collect();
                if !inner.is_empty() {
                    flush!();
                    out.push(match kind {
                        Delim::Code => Span::Code(unescape(&inner)),
                        Delim::Bold => Span::Bold(parse_spans(&inner)),
                        Delim::Italic => Span::Italic(parse_spans(&inner)),
                        Delim::Strike => Span::Strike(parse_spans(&inner)),
                    });
                    i = close + 1;
                    continue;
                }
            }
        }

        buf.push(c);
        i += 1;
    }
    flush!();
    out
}

enum Delim {
    Bold,
    Italic,
    Strike,
    Code,
}

fn delimiter(c: char) -> Option<Delim> {
    match c {
        '*' => Some(Delim::Bold),
        '_' => Some(Delim::Italic),
        '~' => Some(Delim::Strike),
        '`' => Some(Delim::Code),
        _ => None,
    }
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == target)
}

/// Looks for the closing delimiter on the same line.
///
/// It requires that it not be preceded by a space, so `a * b * c` stays text
/// and does not become an accidental bold.
///
/// It skips entities, which are atomic: a `_` inside `utm_source` in a URL
/// neither opens nor closes anything. Without this, an italic containing a link
/// closed in the middle of the address, and the link was no longer recognised —
/// which is what happened to n8n's messages. The same goes for emoji names,
/// which have plenty of underscores (`:white_check_mark:`).
///
/// Nothing is skipped for code between backticks: there the content is verbatim
/// and contains no entities to respect.
fn find_closing(chars: &[char], open: usize, delim: char) -> Option<usize> {
    let skip_entities = matches!(delim, '*' | '_' | '~');
    let mut i = open + 1;
    while i < chars.len() {
        if skip_entities {
            if chars[i] == '<' {
                if let Some(end) = find_char(chars, i + 1, '>') {
                    i = end + 1;
                    continue;
                }
            }
            if chars[i] == ':' {
                if let Some(end) = find_char(chars, i + 1, ':') {
                    let name: String = chars[i + 1..end].iter().collect();
                    if is_emoji_name(&name) {
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        if chars[i] == delim && !chars[i - 1].is_whitespace() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// If the closing `close` is followed by `:skin-tone-N:`, returns the
/// index of the final colon.
fn skin_tone_suffix(chars: &[char], close: usize) -> Option<usize> {
    let rest: String = chars.get(close + 1..)?.iter().collect();
    let tone = rest.strip_prefix(":skin-tone-")?;
    let digit = tone.chars().next()?;
    if !digit.is_ascii_digit() || tone.chars().nth(1) != Some(':') {
        return None;
    }
    Some(close + 1 + ":skin-tone-".len() + 1)
}

/// True if the character before `i` is alphanumeric.
fn prev_is_alnum(chars: &[char], i: usize) -> bool {
    i > 0 && chars[i - 1].is_alphanumeric()
}

fn is_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+')
}

fn parse_angle(inner: &str) -> Span {
    if let Some(rest) = inner.strip_prefix('@') {
        return Span::User(before_pipe(rest).to_string());
    }
    if let Some(rest) = inner.strip_prefix('#') {
        // `<#C123|general>`: keep both — the id to open the channel, the name
        // to show. Without a name the id is what Slack itself displays.
        let (id, label) = split_pipe(rest);
        let label = if label.is_empty() {
            id.to_string()
        } else {
            label.to_string()
        };
        return Span::Channel {
            id: id.to_string(),
            label,
        };
    }
    if let Some(rest) = inner.strip_prefix('!') {
        let (body, label) = split_pipe(rest);
        // `<!date^1786960800^{time}>`: Slack leaves it to the client to write
        // it in the reader's time zone. Untranslated it came out raw.
        if let Some(d) = body.strip_prefix("date^").and_then(slack_date) {
            return Span::Text(d);
        }
        // For everything else — `<!subteam^S123|@devs>` and the like — the
        // label is the rendering Slack itself provides: discarding it, as we did,
        // left the internal id on screen.
        if !label.is_empty() {
            return Span::Broadcast(label.trim_start_matches('@').to_string());
        }
        return Span::Broadcast(body.to_string());
    }
    let (url, label) = split_pipe(inner);
    Span::Link {
        url: unescape(url),
        text: unescape(if label.is_empty() { url } else { label }),
    }
}

/// Turns `TIMESTAMP^{format}` into the date written out.
///
/// The placeholders are Slack's: `{date_num}`, `{date}`, `{date_short}`,
/// `{date_long}`, the `_pretty` variants that say "today" instead of the date,
/// `{time}` and `{time_secs}`. The rest of the format is literal text and
/// passes through intact — often a dash or a space between two placeholders.
fn slack_date(rest: &str) -> Option<String> {
    let (ts, fmt) = rest.split_once('^')?;
    let secs: i64 = ts.split('.').next()?.parse().ok()?;
    let dt = chrono::DateTime::from_timestamp(secs, 0)?.with_timezone(&chrono::Local);
    Some(expand(&dt, fmt))
}

fn expand(dt: &chrono::DateTime<chrono::Local>, fmt: &str) -> String {
    use chrono::{Datelike, Timelike};
    let g = super::day_name(dt.weekday().num_days_from_monday() as usize);
    let m = super::month_name(dt.month0() as usize);
    let (d, y) = (dt.day(), dt.year());

    // "today"/"yesterday"/"tomorrow" only if it really falls there: a
    // meeting two weeks away labelled "today" would be worse than the full date.
    let pretty: Option<String> = {
        let today = chrono::Local::now().date_naive();
        match (dt.date_naive() - today).num_days() {
            0 => Some(t!("app.today").to_string()),
            -1 => Some(t!("app.yesterday").to_string()),
            1 => Some(t!("app.tomorrow").to_string()),
            _ => None,
        }
    };

    let mut out = String::with_capacity(fmt.len() + 24);
    let mut rest = fmt;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('}') else {
            break;
        };
        let token = &rest[i + 1..i + j];
        let full = format!("{d} {m} {y}");
        out.push_str(&match token {
            "date_num" => format!("{y}-{:02}-{:02}", dt.month(), d),
            "date" => full.clone(),
            "date_short" => format!("{d} {} {y}", &m[..3.min(m.len())]),
            "date_long" => format!("{g} {full}"),
            "date_pretty" => pretty.clone().unwrap_or(full.clone()),
            "date_short_pretty" => pretty
                .clone()
                .unwrap_or_else(|| format!("{d} {} {y}", &m[..3.min(m.len())])),
            "date_long_pretty" => pretty.clone().unwrap_or_else(|| format!("{g} {full}")),
            "time" => format!("{:02}:{:02}", dt.hour(), dt.minute()),
            "time_secs" => {
                format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second())
            }
            // A placeholder we do not know is left as it is: inventing a
            // rendering would be worse than showing it.
            other => format!("{{{other}}}"),
        });
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out
}

fn split_pipe(s: &str) -> (&str, &str) {
    match s.split_once('|') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    }
}

fn before_pipe(s: &str) -> &str {
    split_pipe(s).0
}

/// Renders a message as plain text, for notifications and the status bar.
pub fn to_plain(text: &str) -> String {
    let mut out = String::new();
    for b in parse(text) {
        let spans = match b {
            Block::CodeBlock(c) => {
                out.push_str(&c);
                out.push('\n');
                continue;
            }
            Block::Paragraph(s) | Block::Quote(s) => s,
        };
        out.push_str(&plain_spans(spans));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// The textual rendering of a list of spans, emphasis included.
fn plain_spans(spans: Vec<Span>) -> String {
    let mut out = String::new();
    {
        for s in spans {
            match s {
                Span::Text(t) | Span::Code(t) => out.push_str(&t),
                Span::Bold(v) | Span::Italic(v) | Span::Strike(v) => out.push_str(&plain_spans(v)),
                Span::Link { text, .. } => out.push_str(&text),
                Span::User(u) => {
                    out.push('@');
                    out.push_str(&u);
                }
                Span::Channel { label, .. } => {
                    out.push('#');
                    out.push_str(&label);
                }
                Span::Broadcast(b) => {
                    out.push('@');
                    out.push_str(&b);
                }
                Span::Emoji(e) => {
                    out.push(':');
                    out.push_str(&e);
                    out.push(':');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(s: &str) -> Vec<Span> {
        parse_spans(s)
    }

    #[test]
    fn bold_uses_one_asterisk_not_two() {
        // In Markdown `*x*` is italic; in Slack it is bold.
        assert_eq!(
            spans("*ciao*"),
            vec![Span::Bold(vec![Span::Text("ciao".into())])]
        );
    }

    #[test]
    fn italic_strike_and_code() {
        assert_eq!(
            spans("_a_"),
            vec![Span::Italic(vec![Span::Text("a".into())])]
        );
        assert_eq!(
            spans("~a~"),
            vec![Span::Strike(vec![Span::Text("a".into())])]
        );
        assert_eq!(spans("`a`"), vec![Span::Code("a".into())]);
    }

    #[test]
    fn mixed_text_and_formatting() {
        assert_eq!(
            spans("ecco *questo* e `quello`"),
            vec![
                Span::Text("ecco ".into()),
                Span::Bold(vec![Span::Text("questo".into())]),
                Span::Text(" e ".into()),
                Span::Code("quello".into()),
            ]
        );
    }

    #[test]
    fn a_lone_asterisk_stays_text() {
        assert_eq!(spans("2 * 3 * 4"), vec![Span::Text("2 * 3 * 4".into())]);
    }

    #[test]
    fn links_with_and_without_label() {
        assert_eq!(
            spans("<https://x.test|sito>"),
            vec![Span::Link {
                url: "https://x.test".into(),
                text: "sito".into()
            }]
        );
        assert_eq!(
            spans("<https://x.test>"),
            vec![Span::Link {
                url: "https://x.test".into(),
                text: "https://x.test".into()
            }]
        );
    }

    #[test]
    fn mentions_channels_and_broadcasts() {
        assert_eq!(spans("<@U123>"), vec![Span::User("U123".into())]);
        assert_eq!(spans("<@U123|mario>"), vec![Span::User("U123".into())]);
        assert_eq!(
            spans("<#C1|general>"),
            vec![Span::Channel {
                id: "C1".into(),
                label: "general".into()
            }]
        );
        assert_eq!(
            spans("<#C2>"),
            vec![Span::Channel {
                id: "C2".into(),
                label: "C2".into()
            }]
        );
        assert_eq!(spans("<!here>"), vec![Span::Broadcast("here".into())]);
    }

    #[test]
    fn emoji_is_recognised_but_a_clock_time_is_not() {
        assert_eq!(spans(":tada:"), vec![Span::Emoji("tada".into())]);
        assert_eq!(
            spans("alle 10:30:00"),
            vec![Span::Text("alle 10:30:00".into())]
        );
    }

    #[test]
    fn html_entities_are_unescaped() {
        assert_eq!(spans("a &amp; b"), vec![Span::Text("a & b".into())]);
        assert_eq!(spans("&lt;tag&gt;"), vec![Span::Text("<tag>".into())]);
    }

    #[test]
    fn ampersand_entity_is_decoded_last() {
        // `&amp;lt;` must give a literal `&lt;`, not `<`.
        assert_eq!(spans("&amp;lt;"), vec![Span::Text("&lt;".into())]);
    }

    #[test]
    fn an_escaped_link_is_restored_when_editing() {
        // Exact text Slack stored after the reported edit: a bare URL that was
        // linkified. The editor must show it plain, or saving it back as
        // `<url>` makes Slack keep it as literal text.
        let raw = "&lt;https://example.com&gt; fuck";
        assert_eq!(to_editable(raw), "https://example.com fuck");
        assert_eq!(
            to_editable("<https://example.com> fuck"),
            "https://example.com fuck"
        );
        // A labelled link keeps its markup: that IS valid Slack syntax.
        assert_eq!(to_editable("<https://x|sito>"), "<https://x|sito>");
        // Mentions and everything else are left alone.
        assert_eq!(to_editable("<@U1> ciao"), "<@U1> ciao");
    }

    #[test]
    fn code_blocks_are_kept_verbatim() {
        let b = parse("prima\n```\nlet x = *1*;\n```\ndopo");
        assert_eq!(b[0], Block::Paragraph(vec![Span::Text("prima".into())]));
        assert_eq!(b[1], Block::CodeBlock("let x = *1*;".into()));
        assert!(matches!(b.last(), Some(Block::Paragraph(_))));
    }

    #[test]
    fn unclosed_code_block_still_renders() {
        let b = parse("x\n```\nmai chiuso");
        assert_eq!(b.last(), Some(&Block::CodeBlock("mai chiuso".into())));
    }

    #[test]
    fn quotes_work_both_escaped_and_not() {
        assert_eq!(
            parse("> detto"),
            vec![Block::Quote(vec![Span::Text("detto".into())])]
        );
        assert_eq!(
            parse("&gt; detto"),
            vec![Block::Quote(vec![Span::Text("detto".into())])]
        );
    }

    #[test]
    fn consecutive_quote_lines_are_one_block() {
        assert_eq!(
            parse("> a\n> b"),
            vec![Block::Quote(vec![
                Span::Text("a".into()),
                Span::Text("\n".into()),
                Span::Text("b".into()),
            ])]
        );
    }

    #[test]
    fn plain_text_strips_the_markup() {
        assert_eq!(
            to_plain("*ciao* <@U1> guarda <https://x.test|qui> :tada:"),
            "ciao @U1 guarda qui :tada:"
        );
    }
}

/// The full shortcode -> emoji map, generated from `iamcal/emoji-data`.
///
/// It is the same source Slack's names derive from, so it also covers the
/// aliases GitHub's set lacks (`thinking_face`, `the_horns`, `mostly_sunny`)
/// and the skin tone variants already resolved (`+1::skin-tone-4` -> 👍🏽).
/// Regenerate it with `proto/../gen_emoji.py`.
const EMOJI_TSV: &str = include_str!("data/emoji.tsv");

fn emoji_map() -> &'static std::collections::HashMap<&'static str, &'static str> {
    use std::sync::OnceLock;
    static MAP: OnceLock<std::collections::HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        EMOJI_TSV
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .collect()
    })
}

/// Every known shortcode, in order: the reaction picker needs it.
pub fn all_emoji() -> impl Iterator<Item = (&'static str, &'static str)> {
    EMOJI_TSV.lines().filter_map(|l| l.split_once('\t'))
}

/// The Unicode character for a Slack shortcode, if there is one.
pub fn unicode_emoji(name: &str) -> Option<&'static str> {
    let map = emoji_map();
    // The full name includes the skin tone, which the map already
    // resolves: that is tried first.
    if let Some(e) = map.get(name) {
        return Some(renderable(e));
    }
    let base = name.split("::").next().unwrap_or(name);
    map.get(base).map(|e| renderable(e))
}

/// The codepoint sequence of an emoji, as Slack's files name it.
///
/// The **whole** sequence is needed, variation selectors and joiners included:
/// `:female-detective:` is `1f575-fe0f-200d-2640-fe0f`, and the same without
/// `fe0f` does not exist. This is why it does not go through [`unicode_emoji`],
/// which returns the reduced form meant for the font.
pub fn emoji_sequence(name: &str) -> Option<String> {
    let map = emoji_map();
    let glyph = map
        .get(name)
        .or_else(|| map.get(name.split("::").next().unwrap_or(name)))?;
    Some(
        glyph
            .chars()
            .map(|c| format!("{:x}", c as u32))
            .collect::<Vec<_>>()
            .join("-"),
    )
}

/// Reduces a ZWJ sequence to its drawable component.
///
/// `:mechanic:` is `🧑‍🔧` — person, joiner, wrench — and fonts that do not
/// compose ZWJ sequences render it as an empty rectangle. The final component
/// carries the meaning (the wrench, the laptop), so that is what we use: better
/// a partial but recognisable icon than a little box.
fn renderable(s: &'static str) -> &'static str {
    if !s.contains('\u{200d}') {
        // The variation selector only asks for the colour rendering. If the
        // font does not know it, it draws it as a box, so a second one appears
        // beside the emoji: better to always strip it.
        return s.trim_end_matches('\u{fe0f}');
    }
    s.rsplit('\u{200d}')
        .next()
        .map(|part| part.trim_end_matches('\u{fe0f}'))
        .filter(|p| !p.is_empty())
        .unwrap_or(s)
}

#[cfg(test)]
mod emoji_tests {
    use super::*;

    #[test]
    fn common_shortcodes_resolve() {
        assert_eq!(unicode_emoji("eyes"), Some("👀"));
        assert_eq!(unicode_emoji("tada"), Some("🎉"));
        assert_eq!(unicode_emoji("+1"), Some("👍"));
    }

    #[test]
    fn the_codepoint_sequence_keeps_every_part() {
        use super::emoji_sequence;
        // The whole sequence, as the file is named on Slack's CDN: dropping
        // `fe0f` or the joiner gives an address that does not exist.
        assert_eq!(
            emoji_sequence("female-detective").as_deref(),
            Some("1f575-fe0f-200d-2640-fe0f")
        );
        assert_eq!(emoji_sequence("tada").as_deref(), Some("1f389"));
        // The skin tone is part of the sequence, not a suffix.
        assert_eq!(
            emoji_sequence("+1::skin-tone-2").as_deref(),
            Some("1f44d-1f3fb")
        );
        assert_eq!(emoji_sequence("it").as_deref(), Some("1f1ee-1f1f9"));
        assert_eq!(emoji_sequence("nome-che-non-esiste"), None);
    }

    #[test]
    fn the_variation_selector_is_dropped() {
        // `:spiral_calendar_pad:` is U+1F5D3 U+FE0F: with the selector left on
        // the end you saw two boxes instead of an emoji.
        let e = unicode_emoji("spiral_calendar_pad").unwrap();
        assert!(
            !e.contains('\u{fe0f}'),
            "{:?}",
            e.chars().collect::<Vec<_>>()
        );
    }

    #[test]
    fn skin_tone_modifiers_give_the_toned_glyph() {
        let base = unicode_emoji("wave").unwrap();
        let toned = unicode_emoji("wave::skin-tone-4").unwrap();
        assert_ne!(base, toned);
        assert!(toned.starts_with(base));
    }

    #[test]
    fn slack_specific_names_resolve() {
        // Names Slack uses that GitHub's set lacks: with the authoritative map
        // hand-written aliases are no longer needed.
        for n in [
            "smiling_face_with_3_hearts",
            "male-mechanic",
            "female-technologist",
            "thinking_face",
            "unicorn_face",
            "the_horns",
            "mostly_sunny",
            "camera_with_flash",
            "face_palm",
            "i_love_you_hand_sign",
            "rolling_on_the_floor_laughing",
            "flag-eu",
            "flag-england",
            "flag-ai",
            "man-surfing",
        ] {
            assert!(unicode_emoji(n).is_some(), "{n}");
        }
    }

    #[test]
    fn skin_tones_resolve_to_the_toned_glyph_not_the_base() {
        let base = unicode_emoji("+1").unwrap();
        let toned = unicode_emoji("+1::skin-tone-4").unwrap();
        assert_ne!(base, toned, "the tone must change the glyph");
        assert!(toned.starts_with(base), "{toned} must start with {base}");
    }

    #[test]
    fn zwj_sequences_fall_back_to_their_meaningful_component() {
        // 🧑‍🔧 -> 🔧 : fonts without ZWJ composition would draw a blank.
        assert_eq!(unicode_emoji("mechanic"), Some("🔧"));
        assert_eq!(unicode_emoji("male-mechanic"), Some("🔧"));
        assert_eq!(unicode_emoji("technologist"), Some("💻"));
    }

    #[test]
    fn skin_tone_suffixes_are_consumed_with_the_emoji() {
        // `:+1::skin-tone-4:` is ONE emoji: splitting it left a textual
        // `:skin-tone-4:` on screen.
        assert_eq!(
            parse_spans(":+1::skin-tone-4:"),
            vec![Span::Emoji("+1::skin-tone-4".into())]
        );
        assert!(unicode_emoji("+1::skin-tone-4").unwrap().starts_with("👍"));
    }

    #[test]
    fn a_lone_skin_tone_is_the_swatch() {
        // On its own `:skin-tone-4:` is a real emoji (the colour swatch), as
        // Slack shows it.
        assert_eq!(
            parse_spans(":skin-tone-4:"),
            vec![Span::Emoji("skin-tone-4".into())]
        );
        assert!(unicode_emoji("skin-tone-4").is_some());
    }

    #[test]
    fn simple_emoji_are_untouched() {
        assert_eq!(unicode_emoji("hammer"), Some("🔨"));
        assert_eq!(unicode_emoji("cat"), Some("🐱"));
    }

    #[test]
    fn names_containing_digits_still_work() {
        assert_eq!(unicode_emoji("+1"), Some("👍"));
        assert!(unicode_emoji("100").is_some());
    }

    #[test]
    fn custom_workspace_emoji_have_no_unicode() {
        assert_eq!(unicode_emoji("acme-logo"), None);
    }
}

#[cfg(test)]
mod date_tests {
    use super::{Span, expand, parse_spans as spans};
    use chrono::TimeZone;

    /// Monday 17 August 2026, 10:00:30 local time.
    fn dt() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 8, 17, 10, 0, 30)
            .unwrap()
    }

    #[test]
    fn the_time_token_becomes_the_clock() {
        assert_eq!(expand(&dt(), "{time}"), "10:00");
        assert_eq!(expand(&dt(), "{time_secs}"), "10:00:30");
    }

    #[test]
    fn the_date_tokens_are_written_out() {
        rust_i18n::set_locale("it");
        assert_eq!(expand(&dt(), "{date_num}"), "2026-08-17");
        assert_eq!(expand(&dt(), "{date}"), "17 agosto 2026");
        assert_eq!(expand(&dt(), "{date_short}"), "17 ago 2026");
        assert_eq!(expand(&dt(), "{date_long}"), "lunedì 17 agosto 2026");
    }

    #[test]
    fn text_around_the_tokens_survives() {
        rust_i18n::set_locale("it");
        // The format is almost always mixed: separators between two placeholders.
        assert_eq!(
            expand(&dt(), "il {date} alle {time}"),
            "il 17 agosto 2026 alle 10:00"
        );
    }

    #[test]
    fn a_pretty_date_far_away_is_not_called_today() {
        rust_i18n::set_locale("it");
        // The real risk: "today" on an event two weeks away.
        assert_eq!(expand(&dt(), "{date_pretty}"), "17 agosto 2026");
    }

    #[test]
    fn a_pretty_date_today_says_today() {
        rust_i18n::set_locale("it");
        let now = chrono::Local::now();
        assert_eq!(expand(&now, "{date_pretty}"), "oggi");
    }

    #[test]
    fn an_unknown_token_is_left_alone() {
        assert_eq!(expand(&dt(), "{boh}"), "{boh}");
    }

    #[test]
    fn a_date_entity_is_parsed_not_shown_raw() {
        // The calendar message: no fallback after the bar.
        let v = spans("<!date^1786960800^{time}>");
        assert_eq!(v.len(), 1);
        assert!(
            matches!(&v[0], Span::Text(t) if t.contains(':') && !t.contains("date^")),
            "{v:?}"
        );
    }

    #[test]
    fn the_message_from_the_report_is_readable() {
        // The reported case: two times and a link to the calendar, with no
        // fallback after the bar.
        let raw = "<!date^1786960800^{time}>-<!date^1786962000^{time}> \
<https://www.google.com/calendar/event?eid=abc&ctz=UTC|Composer Squad - Daily standup>";
        let out = super::to_plain(raw);
        assert!(!out.contains("date^"), "remains raw: {out}");
        assert!(out.contains("Composer Squad - Daily standup"), "{out}");
        // Two times separated by a dash, not two placeholders.
        assert_eq!(out.matches(':').count(), 2, "{out}");
    }

    #[test]
    fn an_underscore_in_a_url_does_not_close_the_italic() {
        // The n8n message: the address contains `utm_source`, and the italic
        // closed there, breaking the link in half.
        let v = spans(
            "_Automated with this <https://x.test/w?utm_source=a&amp;utm_medium=b|n8n workflow>_",
        );
        assert_eq!(
            v,
            vec![Span::Italic(vec![
                Span::Text("Automated with this ".into()),
                Span::Link {
                    url: "https://x.test/w?utm_source=a&utm_medium=b".into(),
                    text: "n8n workflow".into(),
                },
            ])]
        );
    }

    #[test]
    fn an_underscore_in_an_emoji_name_does_not_close_the_italic() {
        let v = spans("_fatto :white_check_mark: tutto_");
        assert_eq!(
            v,
            vec![Span::Italic(vec![
                Span::Text("fatto ".into()),
                Span::Emoji("white_check_mark".into()),
                Span::Text(" tutto".into()),
            ])]
        );
    }

    #[test]
    fn backticks_still_close_where_they_should() {
        // Inside code the content is verbatim: nothing is skipped.
        assert_eq!(spans("`a <b> c`"), vec![Span::Code("a <b> c".into())]);
    }

    #[test]
    fn a_link_inside_bold_is_still_a_link() {
        // The reported case: Google Calendar sends the title in bold and the
        // whole `<url|text>` ended up on screen as markup.
        let v = spans("*<https://x.test|Daily standup>*");
        assert_eq!(
            v,
            vec![Span::Bold(vec![Span::Link {
                url: "https://x.test".into(),
                text: "Daily standup".into(),
            }])]
        );
    }

    #[test]
    fn emphasis_still_holds_plain_text() {
        // Nesting must not change the ordinary case.
        assert_eq!(
            spans("*solo testo*"),
            vec![Span::Bold(vec![Span::Text("solo testo".into())])]
        );
    }

    #[test]
    fn an_emoji_inside_bold_survives() {
        assert_eq!(
            spans("*ok :tada:*"),
            vec![Span::Bold(vec![
                Span::Text("ok ".into()),
                Span::Emoji("tada".into()),
            ])]
        );
    }

    #[test]
    fn code_inside_backticks_is_not_interpreted() {
        // Inside code the markup is content, not formatting.
        assert_eq!(
            spans("`*non* grassetto`"),
            vec![Span::Code("*non* grassetto".into())]
        );
    }

    #[test]
    fn the_whole_calendar_line_reads_cleanly() {
        let raw = "<!date^1790762400^{time}|9/30/26>-<!date^1790763600^{time}|9/30/26> \
*<https://x.test/e?a=1&amp;b=2|Composer Squad - Daily standup>*";
        let out = super::to_plain(raw);
        assert!(!out.contains("date^"), "{out}");
        assert!(!out.contains('<'), "markup left: {out}");
        assert!(out.contains("Composer Squad - Daily standup"), "{out}");
    }

    #[test]
    fn a_broken_timestamp_does_not_panic() {
        for bad in ["<!date^abc^{time}>", "<!date^>", "<!date^123>"] {
            let _ = spans(bad);
        }
    }

    #[test]
    fn a_user_group_shows_its_name_not_its_id() {
        assert_eq!(
            spans("<!subteam^S123|@devs>"),
            vec![Span::Broadcast("devs".into())]
        );
    }
}
