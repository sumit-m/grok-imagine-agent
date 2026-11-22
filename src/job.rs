//! One `@imagine` request: run the CLI, post its output and files, save to a repo.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::command::{self, Attach, Request};
use crate::composer;
use crate::prompt::Event;
use crate::{proc, repo, strip, terminal};

/// A video can take several minutes; imagine waits for it itself.
const IMAGINE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const BUZZ_TIMEOUT: Duration = Duration::from_secs(180);
const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];
const VIDEO_EXTENSIONS: [&str; 3] = ["mp4", "webm", "mov"];
/// Grok's reference images for a video.
const MAX_REFERENCES: usize = 14;
/// Far longer than any job (see `IMAGINE_TIMEOUT`), so a folder this old was
/// left by an agent stopped mid-job, not one still running elsewhere.
const STALE_WORK_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Removes job folders under `work_root` last changed more than `max_age`
/// ago. Returns how many were removed.
pub fn sweep_stale_work(work_root: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(work_root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > max_age)
        })
        .filter(|entry| std::fs::remove_dir_all(entry.path()).is_ok())
        .count()
}

/// [`sweep_stale_work`] with the standard age, logging what it removed.
pub fn sweep_stale(work_root: &Path) {
    let removed = sweep_stale_work(work_root, STALE_WORK_AGE);
    if removed > 0 {
        eprintln!(
            "imagine-agent: removed {removed} leftover job folder(s) from {}",
            work_root.display()
        );
    }
}

pub struct Config {
    pub imagine: PathBuf,
    pub buzz: PathBuf,
    pub display_name: Option<String>,
    pub work_root: PathBuf,
}

impl Config {
    /// imagine's install folder, where its ini files live; `None` for an
    /// imagine found only on PATH.
    pub fn imagine_dir(&self) -> Option<&Path> {
        self.imagine
            .parent()
            .filter(|dir| dir.is_absolute() && dir.is_dir())
    }
}

struct Attachment {
    url: String,
    mime: String,
    /// The imeta fields Buzz needs to attach the same upload again.
    imeta: Value,
}

impl Attachment {
    fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    fn is_video(&self) -> bool {
        self.mime.starts_with("video/")
    }
}

pub fn run(config: &Config, event: &Event, reply_to: Option<&str>) {
    let work = config.work_root.join(&event.id[..16.min(event.id.len())]);
    let _ = std::fs::remove_dir_all(&work);
    let message = match std::fs::create_dir_all(work.join("out")) {
        Ok(()) => handle(config, event, &work),
        Err(e) => Reply::text(format!(
            "imagine-agent could not create {}: {e}",
            work.display()
        )),
    };
    post(config, event, reply_to, message);
    let _ = std::fs::remove_dir_all(&work);
}

struct Reply {
    content: String,
    files: Vec<PathBuf>,
}

impl Reply {
    fn text(content: String) -> Self {
        Reply {
            content,
            files: Vec::new(),
        }
    }
}

/// How the attachments reach imagine: the flags to add, the uploads used (for
/// the sidecar), and notes for the reply.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    args: Vec<String>,
    sources: Vec<usize>,
    /// A video's first frame among `sources` (1-based), or `none`; for
    /// Regenerate to pick it again.
    frame: Option<String>,
    notes: Vec<String>,
}

/// The highest `<IMAGE_n>` the prompt names, plus one.
fn images_named(args: &[String]) -> usize {
    let Some(prompt) = flag_value(args, "--prompt") else {
        return 0;
    };
    let mut highest = 0;
    let mut rest = prompt;
    while let Some(at) = rest.find("<IMAGE_") {
        let tail = &rest[at + 7..];
        if let Some(end) = tail.find('>') {
            if let Ok(n) = tail[..end].parse::<usize>() {
                highest = highest.max(n + 1);
            }
        }
        rest = tail;
    }
    highest
}

/// Which attached files go where, as flags naming each one's local path
/// (`paths`, in attachment order: images, then videos).
fn plan(request: &Request, images: &[PathBuf], videos: &[PathBuf]) -> Result<Plan, String> {
    let path = |p: &PathBuf| p.to_string_lossy().into_owned();
    let named = images_named(&request.args);
    let takes_images = matches!(request.attach, Attach::Images | Attach::Video { .. });
    if takes_images && named > images.len() {
        let attached = match images.len() {
            1 => "1 image is".to_string(),
            n => format!("{n} images are"),
        };
        return Err(format!(
            "The prompt names @image{named}, but {attached} attached."
        ));
    }
    let mut plan = Plan::default();
    match request.attach {
        Attach::None => {}
        Attach::Images => {
            for (index, image) in images.iter().enumerate() {
                plan.args.extend(["--image".into(), path(image)]);
                plan.sources.push(index);
            }
        }
        Attach::Video { frame } => {
            let frame = frame.filter(|n| *n <= images.len());
            if let Some(n) = frame {
                plan.args.extend(["--image".into(), path(&images[n - 1])]);
            }
            // With more than the frame, or names in the prompt, every image is
            // a reference, so `<IMAGE_n>` is attachment n + 1 as in Image mode.
            let references = images.len() > 1 || frame.is_none() || named > 0;
            if references && !images.is_empty() {
                if images.len() > MAX_REFERENCES {
                    return Err(format!(
                        "Grok takes up to {MAX_REFERENCES} reference images; remove {}.",
                        images.len() - MAX_REFERENCES
                    ));
                }
                for image in images {
                    plan.args.extend(["--ref".into(), path(image)]);
                }
                plan.sources = (0..images.len()).collect();
                plan.frame = Some(frame.map_or("none".into(), |n| n.to_string()));
            } else if let Some(n) = frame {
                plan.sources.push(n - 1);
                plan.frame = Some("1".into());
            }
        }
        Attach::Source => {
            let Some(video) = videos.first() else {
                return Err("Attach the video to change.".into());
            };
            plan.args.extend(["--src".into(), path(video)]);
            plan.sources.push(images.len());
        }
    }
    Ok(plan)
}

/// References cap a video at 720p; a request asking for more gets 720p.
fn capped_for_references(args: &mut [String]) -> Option<String> {
    if !args.iter().any(|a| a == "--ref") {
        return None;
    }
    let index = args.iter().position(|a| a == "--res")? + 1;
    let res = args.get(index)?.to_ascii_lowercase();
    let height: u32 = res.trim_end_matches('p').parse().ok()?;
    (height > 720).then(|| {
        args[index] = "720p".into();
        format!("Made at 720p: reference images cap a video there (asked for {res}).")
    })
}

fn handle(config: &Config, event: &Event, work: &Path) -> Reply {
    let attachments = attachments(&event.tags);
    let urls: Vec<String> = attachments.iter().map(|a| a.url.clone()).collect();
    let display_name = config.display_name.as_deref();
    let parsed = match composer::payload(&event.tags) {
        Some(json) => composer::request(&event.content, &urls, display_name, json),
        None => command::parse_request(&event.content, &urls, display_name),
    };
    let mut request = match parsed {
        Ok(request) => request,
        Err(message) => return Reply::text(message),
    };

    let image_attachments: Vec<&Attachment> = attachments.iter().filter(|a| a.is_image()).collect();
    let video_attachments: Vec<&Attachment> = attachments.iter().filter(|a| a.is_video()).collect();
    let wants = |kind: Attach| match kind {
        Attach::None => (false, false),
        Attach::Source => (false, true),
        _ => (true, false),
    };
    let (want_images, want_videos) = wants(request.attach);
    let mut images = Vec::new();
    if want_images {
        for (index, image) in image_attachments.iter().enumerate() {
            match download(config, image, work, &format!("image{}", index + 1)) {
                Ok(path) => images.push(path),
                Err(e) => {
                    return Reply::text(format!(
                        "Could not download attached image {}: {e}",
                        index + 1
                    ))
                }
            }
        }
    }
    let mut videos = Vec::new();
    if want_videos {
        if let Some(video) = video_attachments.first() {
            match download(config, video, work, "video1") {
                Ok(path) => videos.push(path),
                Err(e) => {
                    return Reply::text(format!("Could not download the attached video: {e}"))
                }
            }
        }
    }
    let plan = match plan(&request, &images, &videos) {
        Ok(plan) => plan,
        Err(message) => return Reply::text(message),
    };
    let mut args = request.args.clone();
    args.extend(plan.args);
    let mut notes = plan.notes;
    notes.extend(capped_for_references(&mut args));

    let uploads: Vec<&Attachment> = image_attachments
        .iter()
        .chain(video_attachments.iter())
        .copied()
        .collect();
    let sources: Vec<Value> = plan
        .sources
        .iter()
        .filter_map(|&i| uploads.get(i).map(|a| a.imeta.clone()))
        .collect();
    let mode = match request.subcommand() {
        "generate" | "gen" if sources.is_empty() => "generate",
        "generate" | "gen" => "edit",
        other => other,
    };
    let mut origin = json!({ "mode": mode });
    if let Some(frame) = &plan.frame {
        origin["frame"] = frame.clone().into();
    }
    if let Some(first) = sources.first() {
        // Desktops before multi-image sources read only `source`.
        origin["source"] = first.clone();
        origin["sources"] = sources.into();
    }
    if let Err(e) = localize_repo_images(config, event, &mut args, work) {
        return Reply::text(format!("Could not get the `--image` image: {e}"));
    }
    let out_dir = work.join("out");
    if request.produces_files() {
        args.push("--output".into());
        args.push(format!(
            "{}{}",
            out_dir.to_string_lossy(),
            std::path::MAIN_SEPARATOR
        ));
    }

    let mut cmd = proc::command(&config.imagine);
    cmd.args(&args)
        .env_remove("BUZZ_PRIVATE_KEY")
        .env_remove("BUZZ_AUTH_TAG");
    if let Some(dir) = config
        .imagine
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        cmd.current_dir(dir);
    }
    let output = match proc::run(cmd, None, IMAGINE_TIMEOUT) {
        Ok(output) => output,
        Err(e) => {
            return Reply::text(format!(
                "Could not start `{}`: {e}",
                config.imagine.display()
            ))
        }
    };

    let printed = without_lines_mentioning(&terminal::clean(&output.combined()), work);
    if output.timed_out {
        notes.push(format!(
            "Stopped waiting after {} minutes.",
            IMAGINE_TIMEOUT.as_secs() / 60
        ));
    } else if !output.success {
        notes.push(format!(
            "imagine exited with code {}.",
            output.code.unwrap_or(-1)
        ));
    }

    let (media, sidecars) = produced_files(&out_dir);
    if let Some(voices) = &request.panel_voices {
        restore_buzz_references(&sidecars, voices);
    }
    annotate_sidecars(&sidecars, &origin);
    let mut files = attachable(&media, work);
    files.extend(sidecars.iter().cloned());

    if media.is_empty() {
        if output.success && request.produces_files() {
            notes.push("imagine reported success but saved no file.".into());
        }
        let content = std::iter::once(printed.trim().to_string())
            .chain(notes)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let content = if content.is_empty() {
            "imagine finished without output.".to_string()
        } else {
            content
        };
        return Reply { content, files };
    }

    // Buzz Desktop shows an image's or video's prompt, settings, sources and
    // repo location in a sidebar built from the sidecar, so the reply carries
    // only notes (a failed save among them). The commit message is built from
    // the output.
    if let Some(spec) = request.repo.take() {
        let message = commit_message(&printed, &event.content);
        let save = Save {
            media: &media,
            sidecars: &sidecars,
            message: &message,
            origin: &origin,
        };
        if let Err(e) = save_to_repo(config, event, work, &spec, &save) {
            notes.insert(0, format!("Repo save failed: {e}"));
        }
    }
    Reply {
        content: notes.join("\n"),
        files,
    }
}

/// The commit message is the channel reply without its `url:` line (Grok's
/// link is temporary, while the commit lasts), as one line with its lines
/// separated by commas. Falls back to the request text when imagine printed
/// nothing else.
fn commit_message(reply: &str, request: &str) -> String {
    let kept: Vec<&str> = reply
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            !line.is_empty() && !lower.starts_with("url:") && !lower.starts_with("saved:")
        })
        .collect();
    let message = kept.join(", ");
    if message.is_empty() {
        command::strip_formatting(request).trim().to_string()
    } else {
        message
    }
}

struct Save<'a> {
    media: &'a [PathBuf],
    sidecars: &'a [PathBuf],
    message: &'a str,
    origin: &'a Value,
}

/// Commits the job's files, with the sidecars naming the repo they were saved
/// to (`<owner>/<repo>`; Buzz finds the file's folder in it by name, so a
/// later move is still found). A failed save takes that back out again.
fn save_to_repo(
    config: &Config,
    event: &Event,
    work: &Path,
    spec: &str,
    save: &Save,
) -> Result<(), String> {
    let target = repo::resolve_for(&config.buzz, &event.channel, &event.sender, spec)?;
    let mut saved_origin = save.origin.clone();
    saved_origin["repo"] = repo_address(&target.repo).into();
    let sources = source_files(config, work, &mut saved_origin);
    annotate_sidecars(save.sidecars, &saved_origin);
    repo::save(
        work,
        &target,
        save.media,
        save.sidecars,
        &sources,
        save.message,
    )
    .map(|_| ())
    .inspect_err(|_| annotate_sidecars(save.sidecars, save.origin))
}

/// Each source upload, fetched again to be committed once per repo at
/// `.sources/<sha256>.<ext>`; its entry in `origin` names that `path`, so the
/// saved file can be made again after the upload is gone. A source that
/// can't be fetched keeps only its upload.
fn source_files(config: &Config, work: &Path, origin: &mut Value) -> Vec<(PathBuf, String)> {
    let Some(sources) = origin.get_mut("sources").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let dir = work.join("sources");
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for source in sources.iter_mut() {
        let (Some(url), Some(sha)) = (source["url"].as_str(), source["x"].as_str()) else {
            continue;
        };
        if !crate::prompt::is_hex64(sha) {
            continue;
        }
        let sha = sha.to_ascii_lowercase();
        let ext = source_extension(source["m"].as_str().unwrap_or_default());
        let path = format!(".sources/{sha}.{ext}");
        let fetched = files.iter().any(|(_, known)| *known == path)
            || fetch_source(config, url, &dir.join(format!("{sha}.{ext}")))
                .map(|file| files.push((file, path.clone())))
                .is_ok();
        if fetched {
            source["path"] = path.into();
        }
    }
    if let Some(first) = origin["sources"].get(0).cloned() {
        origin["source"] = first;
    }
    files
}

/// The extension a source is committed under. Buzz Desktop derives the same
/// one for a source whose entry names no `path`.
fn source_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        m if m.starts_with("video/") => "mp4",
        _ => "jpg",
    }
}

fn fetch_source(config: &Config, url: &str, path: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut cmd = proc::command(&config.buzz);
    cmd.args(["media", "get", url, "--output"]).arg(path);
    let out = proc::run(cmd, None, BUZZ_TIMEOUT).map_err(|e| e.to_string())?;
    if out.success {
        Ok(path.to_path_buf())
    } else {
        Err(out.failure_text())
    }
}

fn repo_address(repo: &repo::RepoInfo) -> String {
    match repo.owner.as_deref().filter(|owner| !owner.is_empty()) {
        Some(owner) => format!("{owner}/{}", repo.id),
        None => repo.id.clone(),
    }
}

/// Adds `origin` to each sidecar as `buzz`: how the file was made (`mode`,
/// the `sources` uploads in order) and where it was saved (`repo`). A sidecar
/// in another shape is left as it is.
fn annotate_sidecars(sidecars: &[PathBuf], origin: &Value) {
    for path in sidecars {
        let Some(mut sidecar) = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')).ok())
            .filter(Value::is_object)
        else {
            continue;
        };
        sidecar["buzz"] = origin.clone();
        if let Ok(out) = serde_json::to_string_pretty(&sidecar) {
            let _ = std::fs::write(path, out);
        }
    }
}

/// Writes each sidecar's prompt with Buzz's `@image2` and `@eve` in place of
/// Grok's `<IMAGE_1>` and `<AUDIO_0>`, so Copy, Edit and Regenerate start from
/// the prompt as it was written.
fn restore_buzz_references(sidecars: &[PathBuf], voices: &[String]) {
    for path in sidecars {
        let Some(mut sidecar) = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')).ok())
        else {
            continue;
        };
        let Some(prompt) = sidecar["prompt"].as_str() else {
            continue;
        };
        let restored = composer::with_buzz_references(prompt, voices);
        if restored == prompt {
            continue;
        }
        sidecar["prompt"] = restored.into();
        if let Ok(out) = serde_json::to_string_pretty(&sidecar) {
            let _ = std::fs::write(path, out);
        }
    }
}

/// Replaces each `--image <repo>:<path>` value with a local copy of that file.
fn localize_repo_images(
    config: &Config,
    event: &Event,
    args: &mut [String],
    work: &Path,
) -> Result<(), String> {
    for (count, (index, prefix, value)) in image_values(args).into_iter().enumerate() {
        if !is_repo_ref(&value) {
            continue;
        }
        let target = repo::resolve_for(&config.buzz, &event.channel, &event.sender, &value)?;
        let dest_dir = work.join(format!("src{}", count + 1));
        std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
        let local = repo::fetch_file(work, &target, &dest_dir)?;
        args[index] = format!("{prefix}{}", local.to_string_lossy());
    }
    Ok(())
}

/// The value of `--flag value` or `--flag=value`.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let with_value = format!("{flag}=");
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == flag {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix(&with_value)
        }
    })
}

/// Each `--image` value: its argument's index, the text before it in that
/// argument, and the value.
fn image_values(args: &[String]) -> Vec<(usize, &'static str, String)> {
    args.iter()
        .enumerate()
        .filter_map(|(i, arg)| {
            if arg == "--image" {
                args.get(i + 1).map(|v| (i + 1, "", v.clone()))
            } else {
                arg.strip_prefix("--image=")
                    .map(|v| (i, "--image=", v.to_string()))
            }
        })
        .collect()
}

/// `<repo>:<path>`, as opposed to a local path such as `C:\pics\a.png`.
fn is_repo_ref(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive_path = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes.get(2), None | Some(b'\\') | Some(b'/'));
    value.contains(':') && !drive_path && !value.contains("://")
}

/// Drops lines that only point into imagine-agent's temp folder, which is
/// deleted once the reply is posted.
fn without_lines_mentioning(text: &str, dir: &Path) -> String {
    let needle = dir.to_string_lossy().replace('\\', "/").to_lowercase();
    text.lines()
        .filter(|line| !line.replace('\\', "/").to_lowercase().contains(&needle))
        .collect::<Vec<_>>()
        .join("\n")
}

fn attachments(tags: &[Vec<String>]) -> Vec<Attachment> {
    tags.iter()
        .filter(|tag| tag.first().map(String::as_str) == Some("imeta"))
        .filter_map(|tag| {
            let field = |name: &str| {
                tag.iter()
                    .find_map(|f| f.strip_prefix(name).map(str::to_string))
            };
            let url = field("url ")?;
            let mut imeta = json!({ "url": url });
            for name in ["m", "x", "size", "dim"] {
                if let Some(value) = field(&format!("{name} ")) {
                    imeta[name] = value.into();
                }
            }
            Some(Attachment {
                mime: field("m ").unwrap_or_default(),
                url,
                imeta,
            })
        })
        .collect()
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        m if m.starts_with("video/") => "mp4",
        _ => "jpg",
    }
}

fn download(
    config: &Config,
    file: &Attachment,
    work: &Path,
    stem: &str,
) -> Result<PathBuf, String> {
    let path = work.join(format!("{stem}.{}", extension_for(&file.mime)));
    let mut cmd = proc::command(&config.buzz);
    cmd.args(["media", "get", &file.url, "--output"]).arg(&path);
    let out = proc::run(cmd, None, BUZZ_TIMEOUT).map_err(|e| e.to_string())?;
    if out.success {
        Ok(path)
    } else {
        Err(out.failure_text())
    }
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|ext| extensions.contains(&ext.as_str()))
}

/// The images or videos imagine saved, and their sidecars.
fn produced_files(out_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut media = Vec::new();
    let mut sidecars = Vec::new();
    let Ok(entries) = std::fs::read_dir(out_dir) else {
        return (media, sidecars);
    };
    for path in entries.flatten().map(|e| e.path()).filter(|p| p.is_file()) {
        if has_extension(&path, &["json"]) {
            sidecars.push(path);
        } else if has_extension(&path, &IMAGE_EXTENSIONS) || has_extension(&path, &VIDEO_EXTENSIONS)
        {
            media.push(path);
        }
    }
    media.sort();
    sidecars.sort();
    (media, sidecars)
}

/// Metadata-free copies of the images and videos for posting; the originals
/// go to the repo untouched.
fn attachable(media: &[PathBuf], work: &Path) -> Vec<PathBuf> {
    let dir = work.join("attach");
    let _ = std::fs::create_dir_all(&dir);
    media
        .iter()
        .filter_map(|file| {
            let bytes = std::fs::read(file).ok()?;
            let dest = dir.join(file.file_name()?);
            match strip::strip_metadata(&bytes) {
                Some(clean) => std::fs::write(&dest, clean).ok().map(|_| dest),
                None => {
                    eprintln!(
                        "imagine-agent: cannot strip {}, attaching as is",
                        file.display()
                    );
                    Some(file.clone())
                }
            }
        })
        .collect()
}

/// The reply opens with the requester's name, as a text agent's does.
/// `messages send --file` puts each file on the next line.
fn addressed(mention: &str, content: &str) -> String {
    let text = content.trim();
    match (mention.is_empty(), text.is_empty()) {
        (true, _) => text.to_string(),
        (false, true) => mention.to_string(),
        (false, false) => format!("{mention} {text}"),
    }
}

fn post(config: &Config, event: &Event, reply_to: Option<&str>, reply: Reply) {
    let send = |content: &str, files: &[PathBuf]| {
        let content = addressed(&event.sender_mention, content);
        let mut cmd = proc::command(&config.buzz);
        cmd.args([
            "messages",
            "send",
            "--channel",
            &event.channel,
            "--mention",
            &event.sender,
            "--content",
            "-",
        ]);
        if let Some(id) = reply_to {
            cmd.args(["--reply-to", id]);
        }
        for file in files {
            cmd.arg("--file").arg(file);
        }
        proc::run(cmd, Some(&content), BUZZ_TIMEOUT)
            .map_err(|e| e.to_string())
            .and_then(|out| {
                if out.success {
                    Ok(())
                } else {
                    Err(out.failure_text())
                }
            })
    };

    let Err(error) = send(&reply.content, &reply.files) else {
        return;
    };
    eprintln!("imagine-agent: send failed: {error}");
    if reply.files.is_empty() {
        return;
    }
    let fallback = format!("{}\nCould not attach the file: {error}", reply.content);
    if let Err(e) = send(&fallback, &[]) {
        eprintln!("imagine-agent: fallback send failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str], attach: Attach) -> Request {
        Request {
            args: args.iter().map(|s| s.to_string()).collect(),
            repo: None,
            attach,
            panel_voices: None,
        }
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn images_all_go_to_generate() {
        let r = request(&["generate", "--prompt", "x <IMAGE_2>"], Attach::Images);
        let plan = plan(&r, &paths(&["a", "b", "c"]), &[]).unwrap();
        assert_eq!(plan.args, ["--image", "a", "--image", "b", "--image", "c"]);
        assert_eq!(plan.sources, [0, 1, 2]);
        assert_eq!(
            super::plan(&r, &paths(&["a", "b"]), &[]).unwrap_err(),
            "The prompt names @image3, but 2 images are attached."
        );
    }

    #[test]
    fn a_video_frame_and_its_references() {
        let frame = |n| Attach::Video { frame: n };
        let plain = request(&["video", "--prompt", "a wave"], frame(Some(1)));
        let named = request(&["video", "--prompt", "<IMAGE_0> waves"], frame(Some(1)));
        let one = paths(&["a"]);
        let three = paths(&["a", "b", "c"]);

        let p = plan(&plain, &one, &[]).unwrap();
        assert_eq!(p.args, ["--image", "a"]);
        assert_eq!(p.sources, [0]);
        assert_eq!(p.frame.as_deref(), Some("1"));

        let p = plan(&named, &one, &[]).unwrap();
        assert_eq!(p.args, ["--image", "a", "--ref", "a"]);

        let p = plan(&request(&["video"], frame(Some(2))), &three, &[]).unwrap();
        assert_eq!(
            p.args,
            ["--image", "b", "--ref", "a", "--ref", "b", "--ref", "c"]
        );
        assert_eq!(p.sources, [0, 1, 2]);
        assert_eq!(p.frame.as_deref(), Some("2"));

        let p = plan(&request(&["video"], frame(None)), &one, &[]).unwrap();
        assert_eq!(p.args, ["--ref", "a"]);
        assert_eq!(p.frame.as_deref(), Some("none"));

        assert!(plan(&plain, &[], &[]).unwrap().args.is_empty());
        let many: Vec<PathBuf> = (0..15).map(|i| PathBuf::from(format!("{i}"))).collect();
        assert!(plan(&plain, &many, &[]).is_err());
    }

    #[test]
    fn a_change_takes_the_first_video() {
        let r = request(&["video-extend"], Attach::Source);
        let p = plan(&r, &paths(&["a"]), &paths(&["v1", "v2"])).unwrap();
        assert_eq!(p.args, ["--src", "v1"]);
        assert_eq!(p.sources, [1]);
        assert_eq!(
            plan(&r, &[], &[]).unwrap_err(),
            "Attach the video to change."
        );
    }

    #[test]
    fn references_cap_the_resolution() {
        let mut args: Vec<String> = ["video", "--res", "1080p", "--ref", "a"]
            .map(String::from)
            .to_vec();
        assert!(capped_for_references(&mut args).is_some());
        assert_eq!(args[2], "720p");
        let mut plain: Vec<String> = ["video", "--res", "1080p"].map(String::from).to_vec();
        assert!(capped_for_references(&mut plain).is_none());
        let mut low: Vec<String> = ["video", "--res", "480p", "--ref", "a"]
            .map(String::from)
            .to_vec();
        assert!(capped_for_references(&mut low).is_none());
    }

    #[test]
    fn imeta_fields() {
        let tags = vec![
            vec!["h".to_string(), "x".to_string()],
            vec![
                "imeta".to_string(),
                "url https://r/media/a.mp4".to_string(),
                "m video/mp4".to_string(),
                "x abc123".to_string(),
                "blurhash LKO2".to_string(),
            ],
        ];
        let found = attachments(&tags);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_video() && !found[0].is_image());
        assert_eq!(
            found[0].imeta,
            json!({ "url": "https://r/media/a.mp4", "m": "video/mp4", "x": "abc123" })
        );
        assert_eq!(extension_for("video/mp4"), "mp4");
        assert_eq!(extension_for("image/png"), "png");
    }

    #[test]
    fn sidecars_get_the_origin_and_others_are_left_alone() {
        let dir = std::env::temp_dir().join(format!("imagine-annotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("h1.json");
        let other = dir.join("h2.json");
        std::fs::write(&sidecar, "\u{feff}{\"prompt\":\"a fox\"}").unwrap();
        std::fs::write(&other, "[1, 2]").unwrap();
        let origin = json!({ "mode": "edit", "repo": "owner/site" });
        annotate_sidecars(&[sidecar.clone(), other.clone()], &origin);
        let read = |path: &Path| std::fs::read_to_string(path).unwrap();
        let written: Value = serde_json::from_str(&read(&sidecar)).unwrap();
        assert_eq!(written["prompt"], "a fox");
        assert_eq!(written["buzz"], origin);
        assert_eq!(read(&other), "[1, 2]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_prompts_get_buzz_references_back() {
        let dir = std::env::temp_dir().join(format!("imagine-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("a.json");
        std::fs::write(
            &sidecar,
            r#"{"prompt":"<IMAGE_0> meets <AUDIO_0>, logo style"}"#,
        )
        .unwrap();
        restore_buzz_references(std::slice::from_ref(&sidecar), &["eve".to_string()]);
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(written["prompt"], "@image1 meets @eve, logo style");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn produced_files_keep_images_and_videos() {
        let dir = std::env::temp_dir().join(format!("imagine-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.jpg", "a.json", "b.mp4", "b.json", "notes.txt"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let (media, sidecars) = produced_files(&dir);
        assert_eq!(media, [dir.join("a.jpg"), dir.join("b.mp4")]);
        assert_eq!(sidecars, [dir.join("a.json"), dir.join("b.json")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_refs_versus_local_paths() {
        assert!(is_repo_ref("site:img/fox.jpg"));
        assert!(!is_repo_ref(r"C:\pics\a.png"));
        assert!(!is_repo_ref("d:/pics/a.png"));
        assert!(!is_repo_ref("pics/a.png"));
        assert!(!is_repo_ref("https://cdn/x.png"));
    }

    #[test]
    fn commit_message_drops_url_and_saved_lines() {
        let reply = "model: grok-imagine-image-2.0, 1:1, 2K (2048x2048), 23s\n\
                     url: https://imgen.x.ai/x.png\n\
                     saved: C:\\out\\x.png";
        assert_eq!(
            commit_message(reply, "@Imagine a fox"),
            "model: grok-imagine-image-2.0, 1:1, 2K (2048x2048), 23s"
        );
        assert_eq!(
            commit_message("url: https://x\n", "@Imagine **a** fox"),
            "@Imagine a fox"
        );
    }

    #[test]
    fn sweep_removes_only_job_folders_past_the_age() {
        let root = std::env::temp_dir().join(format!("imagine-sweep-{}", std::process::id()));
        std::fs::create_dir_all(root.join("aaaa/out")).unwrap();
        std::fs::write(root.join("note.txt"), "x").unwrap();
        assert_eq!(sweep_stale_work(&root, Duration::from_secs(3600)), 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(sweep_stale_work(&root, Duration::ZERO), 1);
        assert!(root.join("note.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replies_open_with_the_requester() {
        assert_eq!(addressed("@Alex", ""), "@Alex");
        assert_eq!(addressed("", "done"), "done");
        assert_eq!(
            addressed("@Alex", "Repo save failed: x\n"),
            "@Alex Repo save failed: x"
        );
    }
}
