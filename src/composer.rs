//! Requests from Buzz's message-box panel. The panel sends the prompt as the
//! message text and its settings in a hidden `["buzz-composer", "imagine",
//! json]` tag; this module turns them into an imagine command. Flags are never
//! shown in Buzz, so every setting is passed explicitly rather than left to
//! imagine's defaults. `auto` is the exception: leaving `--aspect` or `--res`
//! out is how `generate --image` follows the primary image.
//!
//! Prompt references become Grok's own: `@image3` is `<IMAGE_2>` (attachments
//! are sent in order, so the numbers match), and a voice handle (`@eve`) is
//! `<AUDIO_n>` in the order the prompt names them, each sent as `--voice`. The
//! attached files themselves are added by `job`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::command::{self, Attach, Request};
use crate::models::VOICES;

pub const TAG: &str = "buzz-composer";
pub const HARNESS_ID: &str = "imagine";

#[derive(Deserialize)]
struct Payload {
    v: u32,
    mode: String,
    #[serde(default)]
    controls: BTreeMap<String, String>,
}

/// The panel payload carried by this event, if any.
pub fn payload(tags: &[Vec<String>]) -> Option<&str> {
    tags.iter().find_map(|tag| match tag.as_slice() {
        [name, harness, json, ..] if name == TAG && harness == HARNESS_ID => Some(json.as_str()),
        _ => None,
    })
}

pub fn request(
    content: &str,
    attachment_urls: &[String],
    display_name: Option<&str>,
    json: &str,
) -> Result<Request, String> {
    let payload: Payload =
        serde_json::from_str(json).map_err(|e| format!("Unreadable panel settings: {e}"))?;
    if payload.v != 1 {
        return Err(format!(
            "Panel settings version {} is newer than this imagine-agent; update it.",
            payload.v
        ));
    }
    let controls = &payload.controls;
    let setting = |id: &str| -> Result<Option<String>, String> {
        let Some(value) = controls.get(id) else {
            return Ok(None);
        };
        if value.is_empty() || value.starts_with('-') || value.contains(char::is_whitespace) {
            return Err(format!("Invalid panel value for {id}: `{value}`"));
        }
        Ok(Some(value.clone()))
    };
    let (text, voices) = with_grok_references(&prompt_text(content, attachment_urls, display_name));
    let action = setting("action")?;
    if text.is_empty() && action.as_deref() != Some("extend") {
        return Err("A prompt is required to continue.".into());
    }

    let mut args: Vec<String> = Vec::new();
    let flag = |args: &mut Vec<String>, name: &str, id: &str| -> Result<(), String> {
        if let Some(value) = setting(id)?.filter(|v| v != "auto") {
            args.extend([name.to_string(), value]);
        }
        Ok(())
    };
    let attach = match (payload.mode.as_str(), action.as_deref()) {
        ("image", _) => {
            args.push("generate".into());
            flag(&mut args, "--model", "model")?;
            flag(&mut args, "--aspect", "aspect")?;
            flag(&mut args, "--res", "res")?;
            flag(&mut args, "--count", "count")?;
            flag(&mut args, "--quality", "quality")?;
            Attach::Images
        }
        ("video", None) => {
            args.push("video".into());
            flag(&mut args, "--model", "model")?;
            if setting("aspect")?.is_some_and(|a| a != "frame") {
                flag(&mut args, "--aspect", "aspect")?;
            }
            flag(&mut args, "--res", "res")?;
            flag(&mut args, "--duration", "duration")?;
            for voice in &voices {
                args.extend(["--voice".into(), voice.clone()]);
            }
            let frame = match setting("frame")?.as_deref() {
                None | Some("none") => None,
                Some(n) => Some(
                    n.parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("Invalid panel value for frame: `{n}`"))?,
                ),
            };
            Attach::Video { frame }
        }
        ("video", Some(action @ ("extend" | "edit"))) => {
            args.push(format!("video-{action}"));
            flag(&mut args, "--model", "model")?;
            if action == "extend" {
                flag(&mut args, "--duration", "extend")?;
            }
            Attach::Source
        }
        ("video", Some(other)) => return Err(format!("Unknown video action `{other}`.")),
        (other, _) => return Err(format!("Unknown panel mode `{other}`.")),
    };
    if !voices.is_empty() && !matches!(attach, Attach::Video { .. }) {
        return Err("Voices work only when making a new video.".into());
    }
    if !text.is_empty() {
        args.insert(1, "--prompt".into());
        args.insert(2, text);
    }
    if let Some(preset) = setting("preset")? {
        args.extend(["--preset".into(), preset]);
    }
    let repo = controls
        .get("repo")
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    Ok(Request {
        args,
        repo,
        attach,
        panel_voices: Some(voices),
    })
}

/// [`with_grok_references`] undone: `<IMAGE_n>` as `@image{n+1}` and
/// `<AUDIO_n>` as the voice it stands for, so a sidecar's prompt reads (and
/// is sent again) as it was written in Buzz.
pub fn with_buzz_references(text: &str, voices: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        let token = tail.find('>').map(|end| &tail[1..end]);
        let reference = token.and_then(|token| {
            let (kind, n) = token.split_once('_')?;
            let n: usize = n.parse().ok()?;
            match kind {
                "IMAGE" => Some(format!("@image{}", n + 1)),
                "AUDIO" => voices.get(n).map(|voice| format!("@{voice}")),
                _ => None,
            }
        });
        match (reference, token) {
            (Some(reference), Some(token)) => {
                out.push_str(&reference);
                rest = &tail[token.len() + 2..];
            }
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The message text minus attachment lines, markdown markers and Imagine's
/// own leading mention, as one line.
fn prompt_text(content: &str, attachment_urls: &[String], display_name: Option<&str>) -> String {
    let text =
        command::strip_formatting(&command::without_attachment_lines(content, attachment_urls));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = command::strip_display_name(&text, display_name);
    let text = match text.split_once(' ') {
        Some((first, rest)) if first.starts_with("nostr:") => rest,
        None if text.starts_with("nostr:") => "",
        _ => text,
    };
    text.trim().to_string()
}

/// `text` with `@imageN` as `<IMAGE_{N-1}>` and voice handles as `<AUDIO_n>`,
/// and the voices in the order they're first named. Other `@words` are kept.
pub fn with_grok_references(text: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(text.len());
    let mut voices: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        let starts_word = out
            .chars()
            .last()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let word_len = tail[1..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .map_or(tail.len() - 1, |end| end);
        let word = &tail[1..1 + word_len];
        let lower = word.to_ascii_lowercase();
        let image = lower
            .strip_prefix("image")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n > 0 && !lower[5..].starts_with('0'));
        match (starts_word, image) {
            (true, Some(n)) => out.push_str(&format!("<IMAGE_{}>", n - 1)),
            (true, None) if VOICES.contains(&lower.as_str()) => {
                let index = voices.iter().position(|v| *v == lower).unwrap_or_else(|| {
                    voices.push(lower.clone());
                    voices.len() - 1
                });
                out.push_str(&format!("<AUDIO_{index}>"));
            }
            _ => out.push_str(&tail[..1 + word_len]),
        }
        rest = &tail[1 + word_len..];
    }
    out.push_str(rest);
    (out, voices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(content: &str, json: &str) -> Result<Request, String> {
        request(content, &[], Some("Imagine"), json)
    }

    #[test]
    fn finds_only_this_harness_payload() {
        let tags = vec![
            vec!["h".to_string(), "c".to_string()],
            vec![TAG.to_string(), "mage".to_string(), "{}".to_string()],
            vec![
                TAG.to_string(),
                HARNESS_ID.to_string(),
                "{\"v\":1}".to_string(),
            ],
        ];
        assert_eq!(payload(&tags), Some("{\"v\":1}"));
        assert_eq!(payload(&tags[..2]), None);
    }

    #[test]
    fn image_mode_passes_every_setting_and_leaves_auto_out() {
        let r = run(
            "@Imagine the fox from @image1 on @image3 --aspect 1:1",
            r#"{"v":1,"mode":"image","controls":{"model":"grok-imagine-image-2.0","aspect":"auto",
                "res":"2K","count":"3","quality":"medium","preset":"logo","repo":"art:img/"}}"#,
        )
        .unwrap();
        assert_eq!(
            r.args,
            [
                "generate",
                "--prompt",
                "the fox from <IMAGE_0> on <IMAGE_2> --aspect 1:1",
                "--model",
                "grok-imagine-image-2.0",
                "--res",
                "2K",
                "--count",
                "3",
                "--quality",
                "medium",
                "--preset",
                "logo"
            ]
        );
        assert_eq!(r.attach, Attach::Images);
        assert_eq!(r.repo.as_deref(), Some("art:img/"));
    }

    #[test]
    fn video_mode_with_frame_voices_and_from_frame_aspect() {
        let r = run(
            "@Imagine @eve and @Rex talk; @eve laughs",
            r#"{"v":1,"mode":"video","controls":{"model":"grok-imagine-video-1.5","aspect":"frame",
                "res":"720p","duration":"8","frame":"2"}}"#,
        )
        .unwrap();
        assert_eq!(
            r.args,
            [
                "video",
                "--prompt",
                "<AUDIO_0> and <AUDIO_1> talk; <AUDIO_0> laughs",
                "--model",
                "grok-imagine-video-1.5",
                "--res",
                "720p",
                "--duration",
                "8",
                "--voice",
                "eve",
                "--voice",
                "rex"
            ]
        );
        assert_eq!(r.attach, Attach::Video { frame: Some(2) });
        let r = run(
            "@Imagine a wave",
            r#"{"v":1,"mode":"video","controls":{"aspect":"9:16","frame":"none"}}"#,
        )
        .unwrap();
        assert_eq!(r.args, ["video", "--prompt", "a wave", "--aspect", "9:16"]);
        assert_eq!(r.attach, Attach::Video { frame: None });
    }

    #[test]
    fn a_video_attached_extends_or_edits_it() {
        let r = run(
            "@Imagine",
            r#"{"v":1,"mode":"video","controls":{"model":"grok-imagine-video","action":"extend","extend":"4"}}"#,
        )
        .unwrap();
        assert_eq!(
            r.args,
            [
                "video-extend",
                "--model",
                "grok-imagine-video",
                "--duration",
                "4"
            ]
        );
        assert_eq!(r.attach, Attach::Source);
        let r = run(
            "@Imagine add snow",
            r#"{"v":1,"mode":"video","controls":{"action":"edit","extend":"4"}}"#,
        )
        .unwrap();
        assert_eq!(r.args, ["video-edit", "--prompt", "add snow"]);
        assert!(run(
            "@Imagine",
            r#"{"v":1,"mode":"video","controls":{"action":"edit"}}"#
        )
        .is_err());
        assert!(run(
            "@Imagine @eve hi",
            r#"{"v":1,"mode":"video","controls":{"action":"edit"}}"#
        )
        .is_err());
    }

    #[test]
    fn rejects_bad_payloads() {
        assert!(run("@Imagine x", "not json").is_err());
        assert!(run("@Imagine x", r#"{"v":2,"mode":"image"}"#).is_err());
        assert!(run("@Imagine x", r#"{"v":1,"mode":"remix"}"#).is_err());
        assert_eq!(
            run("@Imagine", r#"{"v":1,"mode":"image"}"#).unwrap_err(),
            "A prompt is required to continue."
        );
        assert!(run(
            "@Imagine x",
            r#"{"v":1,"mode":"image","controls":{"model":"--output"}}"#
        )
        .is_err());
        assert!(run(
            "@Imagine x",
            r#"{"v":1,"mode":"video","controls":{"frame":"0"}}"#
        )
        .is_err());
    }

    #[test]
    fn references_follow_words_only() {
        let (text, voices) =
            with_grok_references("@image12, me@image2 @image0 @images @Image2. @leo! @hero");
        assert_eq!(
            text,
            "<IMAGE_11>, me@image2 @image0 @images <IMAGE_1>. <AUDIO_0>! @hero"
        );
        assert_eq!(voices, ["leo"]);
    }

    #[test]
    fn buzz_references_come_back_for_the_sidecar() {
        let written = "the fox in @image2 with @Eve, then @rex; <b> x<y";
        let (grok, voices) = with_grok_references(written);
        assert_eq!(
            with_buzz_references(&grok, &voices),
            "the fox in @image2 with @eve, then @rex; <b> x<y"
        );
        assert_eq!(
            with_buzz_references("<AUDIO_3> <IMAGE_x>", &[]),
            "<AUDIO_3> <IMAGE_x>"
        );
    }

    #[test]
    fn prompt_drops_mention_formatting_and_attachment_lines() {
        let url = "https://relay/media/a.png".to_string();
        let r = request(
            "@Imagine make **the sky** purple\n![a.png](https://relay/media/a.png)",
            &[url],
            Some("Imagine"),
            r#"{"v":1,"mode":"image"}"#,
        )
        .unwrap();
        assert_eq!(r.args[2], "make the sky purple");
    }
}
