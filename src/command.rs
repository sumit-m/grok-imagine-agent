//! Turns a typed chat message into imagine CLI arguments plus the Buzz-only
//! `--repo` flag, and says which attachments the command takes.

/// Which of the message's attachments `job` adds to the command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Attach {
    None,
    /// Every image as `--image`, in order (`generate`).
    Images,
    /// Image `frame` (1-based) as the first frame, and the images as
    /// references when they're named or more than the frame (`video`).
    Video {
        frame: Option<usize>,
    },
    /// The first video as `--src` (`video-extend`, `video-edit`).
    Source,
}

#[derive(Debug, PartialEq)]
pub struct Request {
    pub args: Vec<String>,
    pub repo: Option<String>,
    pub attach: Attach,
    /// For a panel request, the voices its prompt named, in `<AUDIO_n>`
    /// order: its sidecars get Buzz's `@` references back.
    pub panel_voices: Option<Vec<String>>,
}

/// imagine commands that save a file with `--output`.
const PRODUCING: [&str; 5] = ["generate", "gen", "video", "video-extend", "video-edit"];

impl Request {
    pub fn subcommand(&self) -> &str {
        self.args.first().map_or("", String::as_str)
    }

    pub fn produces_files(&self) -> bool {
        PRODUCING.contains(&self.subcommand())
    }

    pub fn has_flag(&self, flag: &str) -> bool {
        let with_value = format!("{flag}=");
        self.args
            .iter()
            .any(|a| a == flag || a.starts_with(&with_value))
    }
}

pub const USAGE: &str = "Usage: `@imagine <imagine command and flags> [--repo [<repo>:]<path>]`\n\
e.g. `@imagine gen a red fox --aspect 16:9 --repo assets/` — every imagine flag except `--output` is passed through.";

pub fn parse_request(
    content: &str,
    attachment_urls: &[String],
    display_name: Option<&str>,
) -> Result<Request, String> {
    let text = without_attachment_lines(content, attachment_urls);
    let text = unwrap_links(&strip_formatting(&text));
    let text = normalize_typography(&text);
    let text = strip_display_name(text.trim(), display_name);
    let mut tokens = split_words(text);
    let leading_mentions = tokens
        .iter()
        .take_while(|t| t.starts_with('@') || t.starts_with("nostr:"))
        .count();
    tokens.drain(..leading_mentions);

    let mut args = Vec::new();
    let mut repo = None;
    let mut iter = tokens.into_iter();
    while let Some(token) = iter.next() {
        if token == "--output" || token.starts_with("--output=") {
            return Err("`--output` is set by imagine-agent; leave it out.".into());
        }
        if token == "--repo" {
            repo = Some(iter.next().ok_or("`--repo` needs a path")?);
        } else if let Some(value) = token.strip_prefix("--repo=") {
            repo = Some(value.to_string());
        } else {
            args.push(token);
        }
    }

    let mut request = Request {
        args,
        repo,
        attach: Attach::None,
        panel_voices: None,
    };
    if request.args.is_empty() {
        return Err(USAGE.into());
    }
    if request.repo.is_some() && !request.produces_files() {
        return Err(format!(
            "`--repo` only works with a command that makes an image or video, not `{}`.",
            request.subcommand()
        ));
    }
    request.attach = match request.subcommand() {
        "generate" | "gen" if !request.has_flag("--image") => Attach::Images,
        "video" if !request.has_flag("--image") => Attach::Video { frame: Some(1) },
        "video-extend" | "video-edit" if !request.has_flag("--src") => Attach::Source,
        _ => Attach::None,
    };
    Ok(request)
}

pub(crate) fn without_attachment_lines(content: &str, attachment_urls: &[String]) -> String {
    content
        .lines()
        .filter(|line| {
            !attachment_urls
                .iter()
                .any(|url| !url.is_empty() && line.contains(url.as_str()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Bold, italic, strikethrough and code formatting reach the content as `**`,
/// `*`, `~~` and backticks (pasted text often carries bold); none of them
/// belong in imagine arguments.
pub fn strip_formatting(text: &str) -> String {
    text.replace("~~", "").replace(['*', '`'], "")
}

/// Buzz's composer sends a typed URL as `<url>`, or `[text](url)` when the
/// link text differs; imagine needs the bare URL.
fn unwrap_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['<', '[']) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        match link_at(tail) {
            Some((url, consumed)) => {
                out.push_str(&url);
                rest = &tail[consumed..];
            }
            None => {
                out.push_str(&tail[..1]);
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn link_at(tail: &str) -> Option<(String, usize)> {
    let is_url = |s: &str| s.contains("://") && !s.contains(char::is_whitespace);
    if let Some(body) = tail.strip_prefix('<') {
        let end = body.find('>')?;
        let url = &body[..end];
        return is_url(url).then(|| (url.to_string(), end + 2));
    }
    let label_end = tail.find("](")?;
    if tail[1..label_end].contains(['[', ']', '\n']) {
        return None;
    }
    let href_start = label_end + 2;
    let mut url = String::new();
    let mut chars = tail[href_start..].char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => url.push(chars.next()?.1),
            ')' => return is_url(&url).then(|| (url, href_start + i + 1)),
            _ => url.push(c),
        }
    }
    None
}

fn normalize_typography(text: &str) -> String {
    text.replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2018}', '\u{2019}'], "'")
        .split_whitespace()
        .map(|word| match word.strip_prefix(['\u{2014}', '\u{2013}']) {
            Some(rest) => format!("--{rest}"),
            None => word.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn strip_display_name<'a>(text: &'a str, display_name: Option<&str>) -> &'a str {
    let Some(name) = display_name.map(str::trim).filter(|n| !n.is_empty()) else {
        return text;
    };
    let mention = format!("@{name}");
    match text.get(..mention.len()) {
        Some(head) if head.eq_ignore_ascii_case(&mention) => text[mention.len()..].trim_start(),
        _ => text,
    }
}

/// Splits on whitespace, grouping `"…"` and `'…'`. Backslashes are ordinary
/// characters so Windows paths survive unquoted. A lone apostrophe
/// (`a cat's hat`) leaves a quote unbalanced; that falls back to plain words.
fn split_words(text: &str) -> Vec<String> {
    split_quoted(text).unwrap_or_else(|| text.split_whitespace().map(String::from).collect())
}

fn split_quoted(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Result<Request, String> {
        parse_request(content, &[], Some("imagine"))
    }

    #[test]
    fn passes_args_through_and_extracts_repo() {
        let r = parse("@imagine gen \"a red fox\" --aspect 16:9 --repo art:assets/").unwrap();
        assert_eq!(r.args, ["gen", "a red fox", "--aspect", "16:9"]);
        assert_eq!(r.repo.as_deref(), Some("art:assets/"));
        assert!(r.produces_files());
        assert_eq!(r.attach, Attach::Images);
        let r = parse("@imagine gen fox --repo=img/fox.jpg").unwrap();
        assert_eq!(r.repo.as_deref(), Some("img/fox.jpg"));
    }

    #[test]
    fn each_command_takes_its_attachments() {
        let attach = |text: &str| parse(text).unwrap().attach;
        assert_eq!(
            attach("@imagine video a wave"),
            Attach::Video { frame: Some(1) }
        );
        assert_eq!(attach("@imagine video-extend"), Attach::Source);
        assert_eq!(attach("@imagine video-edit add snow"), Attach::Source);
        assert_eq!(attach("@imagine gen x --image a.png"), Attach::None);
        assert_eq!(attach("@imagine video-edit --src c.mp4 x"), Attach::None);
        assert_eq!(attach("@imagine quota"), Attach::None);
    }

    #[test]
    fn rejects_output_and_misplaced_repo() {
        assert!(parse("@imagine gen fox --output x").is_err());
        assert!(parse("@imagine gen fox --output=x").is_err());
        assert!(parse("@imagine quota --repo x").is_err());
        assert!(parse("@imagine gen fox --repo").is_err());
        assert_eq!(parse("@imagine").unwrap_err(), USAGE);
    }

    #[test]
    fn mentions_quotes_paths_and_links() {
        let r = parse_request("@Imagine Bot gen fox", &[], Some("Imagine Bot")).unwrap();
        assert_eq!(r.args, ["gen", "fox"]);
        let r = parse("@imagine gen a cat\u{2019}s hat").unwrap();
        assert_eq!(r.args, ["gen", "a", "cat's", "hat"]);
        let r = parse(r"@imagine gen --image C:\pics\cat.png add a hat").unwrap();
        assert_eq!(r.args[2], r"C:\pics\cat.png");
        let r = parse("@imagine gen --image <https://cdn.example/a.png?x=1> hat").unwrap();
        assert_eq!(r.args[2], "https://cdn.example/a.png?x=1");
        let r = parse("@imagine gen `a fox` ~~old~~ *soft*").unwrap();
        assert_eq!(r.args, ["gen", "a", "fox", "old", "soft"]);
    }

    #[test]
    fn attachment_markdown_is_dropped() {
        let url = "https://relay.example/media/abc.png".to_string();
        let content = format!("@imagine gen make it blue\n![photo.png]({url})");
        let r = parse_request(&content, &[url], Some("imagine")).unwrap();
        assert_eq!(r.args, ["gen", "make", "it", "blue"]);
    }
}
