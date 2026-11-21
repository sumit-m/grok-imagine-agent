//! Buzz's panel for imagine, built from imagine's `models.ini` (next to
//! imagine.exe): the Image / Video toggle and every value control, plus the
//! Voice menu. The Preset menu comes from `presets.ini` (`presets.rs`) and the
//! repo picker from `install.ps1`.
//!
//! Each control or option says which mode it belongs to (`modes`) and which
//! models offer it (`when`). In Video mode the attachments decide the rest:
//! with images, a First frame pick (and Aspect shows "From frame"); with
//! references (images past the frame, or a frame of None) no 1080p; with a
//! video, Extend / Edit on the models that take a video.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::presets;

const MAX_OPTIONS: usize = 50;
const MAX_TEXT: usize = 64;
const IMAGE_DEFAULT: &str = "grok-imagine-image-2.0";
const VIDEO_DEFAULT: &str = "grok-imagine-video-1.5";
/// Grok's reference images for a video.
const MAX_VIDEO_REFERENCES: u32 = 14;
/// The preset voices Grok narrates with (`--voice`), as prompt handles.
pub const VOICES: [&str; 5] = ["ara", "eve", "leo", "rex", "sal"];
/// The resolution references cap a video at.
const REFERENCE_MAX_RES: &str = "720p";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Media {
    Image,
    Video,
}

#[derive(Debug, PartialEq)]
pub struct Model {
    pub id: String,
    display: String,
    media: Media,
    aspects: Vec<String>,
    aspect_default: Option<String>,
    /// As the panel offers them: `1K` upper-case, `720p` lower-case.
    resolutions: Vec<String>,
    res_default: Option<String>,
    speed: Option<String>,
    max_refs: Option<u32>,
    inputs: Vec<String>,
}

impl Model {
    fn takes(&self, input: &str) -> bool {
        self.inputs.iter().any(|i| i == input)
    }
}

/// `1K` and `2K` upper-case, `480p` and `720p` lower-case, as imagine and the
/// panel spell them.
pub fn panel_resolution(value: &str) -> String {
    if value.to_ascii_lowercase().ends_with('k') {
        value.to_ascii_uppercase()
    } else {
        value.to_ascii_lowercase()
    }
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// The models in `models.ini`, in file order. A section without a media
/// type, aspects or resolutions, or with a name Buzz can't send, is left out.
pub fn parse(text: &str) -> Vec<Model> {
    presets::parse(text)
        .into_iter()
        .filter_map(|(id, fields)| {
            let get = |key: &str| {
                fields
                    .iter()
                    .find(|(k, v)| k.eq_ignore_ascii_case(key) && !v.is_empty())
                    .map(|(_, v)| v.as_str())
            };
            let media = match get("media")? {
                "image" => Media::Image,
                "video" => Media::Video,
                _ => return None,
            };
            let aspects: Vec<String> = list(get("aspects")?)
                .into_iter()
                .filter(|a| a != "auto" && presets::usable_name(a))
                .collect();
            let resolutions: Vec<String> = list(get("resolutions")?)
                .iter()
                .filter(|r| presets::usable_name(r))
                .map(|r| panel_resolution(r))
                .collect();
            if !presets::usable_name(&id) || aspects.is_empty() || resolutions.is_empty() {
                eprintln!("imagine-agent: model {id:?} left out of the panel");
                return None;
            }
            let display = get("display")
                .filter(|d| d.chars().count() <= MAX_TEXT)
                .unwrap_or(&id)
                .to_string();
            Some(Model {
                aspect_default: get("aspect_default")
                    .filter(|a| aspects.iter().any(|x| x == a))
                    .map(str::to_string),
                res_default: get("res_default")
                    .map(panel_resolution)
                    .filter(|r| resolutions.contains(r)),
                speed: get("speed").map(capitalized),
                max_refs: get("max_refs")
                    .and_then(|n| n.trim().parse().ok())
                    .filter(|n| *n > 0),
                inputs: get("inputs").map(list).unwrap_or_default(),
                id,
                display,
                media,
                aspects,
                resolutions,
            })
        })
        .take(MAX_OPTIONS)
        .collect()
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Values in first-seen order across `lists`.
fn union<'a>(lists: impl Iterator<Item = &'a Vec<String>>) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for item in lists.flatten() {
        if !all.contains(item) {
            all.push(item.clone());
        }
    }
    all
}

/// `{"model": [ids]}` merged into `when`, unless every model in `of` qualifies.
fn with_models(mut when: Map<String, Value>, ids: &[&str], of: &[&str]) -> Map<String, Value> {
    if ids.len() < of.len() {
        when.insert("model".into(), json!(ids));
    }
    when
}

fn with_when(mut option: Value, when: Map<String, Value>) -> Value {
    if !when.is_empty() {
        option["when"] = when.into();
    }
    option
}

/// The frame values an `imagePick` can hold: Image 1 to the reference cap.
fn frame_values() -> Vec<String> {
    (1..=MAX_VIDEO_REFERENCES).map(|n| n.to_string()).collect()
}

struct Catalog<'a> {
    images: Vec<&'a Model>,
    videos: Vec<&'a Model>,
}

impl<'a> Catalog<'a> {
    fn new(models: &'a [Model]) -> Self {
        Catalog {
            images: models.iter().filter(|m| m.media == Media::Image).collect(),
            videos: models.iter().filter(|m| m.media == Media::Video).collect(),
        }
    }

    fn default_of(group: &[&'a Model], id: &str) -> Option<&'a Model> {
        group
            .iter()
            .find(|m| m.id == id)
            .or_else(|| group.first())
            .copied()
    }
}

fn ids<'a>(group: &[&'a Model]) -> Vec<&'a str> {
    group.iter().map(|m| m.id.as_str()).collect()
}

fn model_option(model: &Model, mode: &str) -> Value {
    let mut option = json!({ "value": model.id, "label": model.display, "modes": [mode] });
    if let Some(speed) = &model.speed {
        option["hint"] = speed.clone().into();
    }
    let mut defaults = Map::new();
    if let Some(aspect) = &model.aspect_default {
        defaults.insert("aspect".into(), aspect.clone().into());
    }
    if let Some(res) = &model.res_default {
        defaults.insert("res".into(), res.clone().into());
    }
    if !defaults.is_empty() {
        option["defaults"] = defaults.into();
    }
    let max = match model.media {
        Media::Image => model.max_refs,
        Media::Video => model.takes("image").then_some(MAX_VIDEO_REFERENCES),
    };
    if let Some(max) = max {
        option["maxImages"] = max.into();
        if max > 1 {
            option["imageRefs"] = true.into();
        }
    }
    if model.media == Media::Video && !model.takes("video") {
        option = with_when(
            option,
            Map::from_iter([("@videos".into(), json!(["none"]))]),
        );
    }
    option
}

/// The value controls in panel order, then the Voice menu; `None` without
/// image models.
pub fn controls(models: &[Model]) -> Option<(Vec<Value>, Option<Value>)> {
    let catalog = Catalog::new(models);
    let image = Catalog::default_of(&catalog.images, IMAGE_DEFAULT)?;
    let video = Catalog::default_of(&catalog.videos, VIDEO_DEFAULT);
    let image_ids = ids(&catalog.images);
    let video_ids = ids(&catalog.videos);
    let mut controls = Vec::new();

    let mut model = json!({
        "id": "model", "kind": "list", "label": "Model", "icon": "sparkles",
        "default": image.id, "sidecarKey": "model_id",
        "options": catalog.images.iter().map(|m| model_option(m, "image"))
            .chain(catalog.videos.iter().map(|m| model_option(m, "video")))
            .collect::<Vec<_>>()
    });
    if let Some(video) = video {
        model["defaultByMode"] = json!({ "video": video.id });
    }
    controls.push(model);

    let changes_video = catalog.videos.iter().any(|m| m.takes("video"));
    if changes_video {
        controls.push(json!({
            "id": "action", "kind": "segmented", "label": "Action", "modes": ["video"],
            "when": { "@videos": ["attached"] }, "default": "extend",
            "options": [{ "value": "extend", "label": "Extend" }, { "value": "edit", "label": "Edit" }]
        }));
    }
    if video.is_some() {
        controls.push(json!({
            "id": "frame", "kind": "imagePick", "label": "Frame", "title": "First frame",
            "icon": "image-play", "modes": ["video"], "when": { "@videos": ["none"] },
            "default": "1", "options": []
        }));
    }

    let mut aspect_options = vec![
        json!({ "value": "auto", "label": "Auto", "modes": ["image"],
                      "when": { "@images": ["attached"] } }),
    ];
    for (mode, group, group_ids) in [
        ("image", &catalog.images, &image_ids),
        ("video", &catalog.videos, &video_ids),
    ] {
        if mode == "video" && !group.is_empty() {
            aspect_options.push(json!({
                "value": "frame", "label": "From frame", "modes": ["video"],
                "when": { "@images": ["attached"], "frame": frame_values() }
            }));
        }
        for aspect in union(group.iter().map(|m| &m.aspects)) {
            let offering: Vec<&str> = group
                .iter()
                .filter(|m| m.aspects.contains(&aspect))
                .map(|m| m.id.as_str())
                .collect();
            let mut when = Map::new();
            if mode == "video" {
                when.insert("frame".into(), json!(["none"]));
            }
            let option = json!({ "value": aspect, "label": aspect, "modes": [mode] });
            aspect_options.push(with_when(option, with_models(when, &offering, group_ids)));
        }
    }
    let mut aspect = json!({
        "id": "aspect", "kind": "shapes", "label": "Aspect ratio",
        "default": image.aspect_default.as_ref().unwrap_or(&image.aspects[0]),
        "defaultWithImages": "auto", "sidecarKey": "aspect_ratio",
        "when": { "@videos": ["none"] },
        "options": aspect_options.into_iter().take(MAX_OPTIONS).collect::<Vec<_>>()
    });
    if let Some(video) = video {
        aspect["defaultByMode"] =
            json!({ "video": video.aspect_default.as_ref().unwrap_or(&video.aspects[0]) });
    }
    controls.push(aspect);

    let mut res_options = vec![
        json!({ "value": "auto", "label": "Auto", "modes": ["image"],
                      "when": { "@images": ["attached"] } }),
    ];
    for (mode, group, group_ids) in [
        ("image", &catalog.images, &image_ids),
        ("video", &catalog.videos, &video_ids),
    ] {
        let mut resolutions = union(group.iter().map(|m| &m.resolutions));
        resolutions.sort_by_key(|res| {
            res.trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
        let cap = resolutions
            .iter()
            .position(|r| r == REFERENCE_MAX_RES)
            .unwrap_or(usize::MAX);
        for (index, res) in resolutions.iter().enumerate() {
            let offering: Vec<&str> = group
                .iter()
                .filter(|m| m.resolutions.contains(res))
                .map(|m| m.id.as_str())
                .collect();
            let mut when = Map::new();
            if mode == "video" && index > cap {
                when.insert("@images".into(), json!(["none", "one"]));
                when.insert("frame".into(), json!(["1"]));
            }
            let option = json!({ "value": res, "label": res, "modes": [mode] });
            res_options.push(with_when(option, with_models(when, &offering, group_ids)));
        }
    }
    let mut res = json!({
        "id": "res", "kind": "segmented", "label": "Resolution",
        "default": image.res_default.as_ref().unwrap_or(&image.resolutions[0]),
        "defaultWithImages": "auto", "sidecarKey": "resolution",
        "when": { "@videos": ["none"] },
        "options": res_options.into_iter().take(MAX_OPTIONS).collect::<Vec<_>>()
    });
    if let Some(video) = video {
        res["defaultByMode"] =
            json!({ "video": video.res_default.as_ref().unwrap_or(&video.resolutions[0]) });
    }
    controls.push(res);

    controls.push(json!({
        "id": "count", "kind": "number", "label": "Images", "icon": "layers",
        "modes": ["image"], "min": 1, "max": 10, "default": "1", "options": []
    }));
    controls.push(json!({
        "id": "quality", "kind": "list", "label": "Quality", "icon": "photo",
        "modes": ["image"], "default": "auto", "sidecarKey": "quality",
        "options": [
            { "value": "auto", "label": "Auto", "hint": "Low for text, medium with images" },
            { "value": "low", "label": "Low" },
            { "value": "medium", "label": "Medium" }
        ]
    }));
    if video.is_some() {
        controls.push(json!({
            "id": "duration", "kind": "number", "label": "Duration", "icon": "clock",
            "unit": "s", "modes": ["video"], "when": { "@videos": ["none"] },
            "min": 1, "max": 15, "default": "8", "sidecarKey": "duration", "options": []
        }));
    }
    if changes_video {
        controls.push(json!({
            "id": "extend", "kind": "number", "label": "Extend by", "icon": "clock-plus",
            "unit": "s", "modes": ["video"],
            "when": { "@videos": ["attached"], "action": ["extend"] },
            "min": 2, "max": 10, "default": "6", "sidecarKey": "extend_by", "options": []
        }));
    }

    let voiced: Vec<&Model> = catalog
        .videos
        .iter()
        .filter(|m| m.takes("audio"))
        .copied()
        .collect();
    let voice = (!voiced.is_empty()).then(|| {
        let when = with_models(
            Map::from_iter([("@videos".into(), json!(["none"]))]),
            &ids(&voiced),
            &video_ids,
        );
        json!({
            "id": "voice", "kind": "assets", "label": "Voice", "title": "Choose a voice",
            "icon": "speech", "modes": ["video"], "when": when,
            "options": VOICES.iter().map(|v| json!({ "value": v, "label": capitalized(v) }))
                .collect::<Vec<_>>()
        })
    });
    Some((controls, voice))
}

/// The panel's modes: Video only when `models.ini` lists a video model.
fn modes(models: &[Model]) -> Value {
    let mut modes = vec![json!({ "id": "image", "label": "Image" })];
    if models.iter().any(|m| m.media == Media::Video) {
        modes.push(json!({ "id": "video", "label": "Video" }));
    }
    modes.into()
}

/// Ids this module writes; other controls (Preset, repo) are left in place.
const OWN_IDS: [&str; 10] = [
    "model", "action", "frame", "aspect", "res", "count", "quality", "duration", "extend", "voice",
];

/// Puts the value controls first, in order, then the controls this module
/// doesn't write (Preset), then the Voice menu, then the repo picker.
fn arrange(existing: &[Value], values: Vec<Value>, voice: Option<Value>) -> Vec<Value> {
    let others: Vec<&Value> = existing
        .iter()
        .filter(|c| !OWN_IDS.iter().any(|id| c["id"] == *id))
        .collect();
    let (repo, rest): (Vec<&Value>, Vec<&Value>) =
        others.into_iter().partition(|c| c["kind"] == "repoFolder");
    values
        .into_iter()
        .chain(rest.into_iter().cloned())
        .chain(voice)
        .chain(repo.into_iter().cloned())
        .collect()
}

fn read(models_ini: &Path) -> Result<Vec<Model>, String> {
    match std::fs::read(models_ini) {
        Ok(bytes) => Ok(parse(&presets::decode(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", models_ini.display())),
    }
}

/// Rewrites the harness file's modes and its controls from `models_ini`.
/// Without image models the file is left as it is. Returns whether it changed.
pub fn refresh(harness_file: &Path, models_ini: &Path) -> Result<bool, String> {
    let models = read(models_ini)?;
    let Some((values, voice)) = controls(&models) else {
        return Ok(false);
    };
    let modes = modes(&models);
    presets::update_composer(harness_file, |composer| {
        let existing = composer
            .get("controls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let arranged = arrange(&existing, values, voice);
        if composer.get("modes") == Some(&modes) && existing == arranged {
            return false;
        }
        composer["modes"] = modes;
        composer["controls"] = arranged.into();
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "; catalog\n\
        [grok-imagine-image-2.0]\n\
        display=Quality 2.0\n\
        media=image\n\
        aspects=1:1,16:9,9:19.5,auto\n\
        aspect_default=1:1\n\
        resolutions=1K,2K\n\
        res_default=2K\n\
        speed=~13s at 1K\n\
        max_refs=5\n\
        [grok-imagine-image]\n\
        display=Speed\n\
        media=image\n\
        aspects=1:1,16:9\n\
        resolutions=1K\n\
        max_refs=5\n\
        [grok-imagine-video-1.5]\n\
        display=Video 1.5\n\
        media=video\n\
        aspects=16:9,9:16\n\
        aspect_default=16:9\n\
        resolutions=480p,720p,1080p\n\
        res_default=720p\n\
        inputs=text,image,audio\n\
        [grok-imagine-video]\n\
        display=Video 1.0\n\
        media=video\n\
        aspects=16:9,9:16\n\
        resolutions=480p,720p\n\
        inputs=text,image,video\n\
        [broken]\n\
        display=No media\n";

    fn find<'a>(controls: &'a [Value], id: &str) -> &'a Value {
        controls.iter().find(|c| c["id"] == id).unwrap()
    }

    fn values(options: &Value) -> Vec<&str> {
        options
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn parse_keeps_models_with_what_they_need() {
        let models = parse(INI);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "grok-imagine-image-2.0",
                "grok-imagine-image",
                "grok-imagine-video-1.5",
                "grok-imagine-video"
            ]
        );
        assert_eq!(models[0].aspects, ["1:1", "16:9", "9:19.5"]);
        assert_eq!(models[2].resolutions, ["480p", "720p", "1080p"]);
        assert!(models[3].takes("video") && !models[2].takes("video"));
        assert_eq!(panel_resolution("2k"), "2K");
        assert_eq!(panel_resolution("720P"), "720p");
    }

    #[test]
    fn controls_split_by_mode_and_follow_the_attachments() {
        let (controls, voice) = controls(&parse(INI)).unwrap();
        let order: Vec<&str> = controls.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(
            order,
            [
                "model", "action", "frame", "aspect", "res", "count", "quality", "duration",
                "extend"
            ]
        );
        let model = find(&controls, "model");
        assert_eq!(model["default"], "grok-imagine-image-2.0");
        assert_eq!(
            model["defaultByMode"],
            json!({ "video": "grok-imagine-video-1.5" })
        );
        assert_eq!(model["options"][0]["maxImages"], 5);
        assert_eq!(model["options"][0]["imageRefs"], true);
        assert_eq!(model["options"][2]["when"], json!({ "@videos": ["none"] }));
        assert_eq!(model["options"][2]["maxImages"], 14);
        assert!(model["options"][3].get("when").is_none());

        let aspect = find(&controls, "aspect");
        assert_eq!(
            values(&aspect["options"]),
            ["auto", "1:1", "16:9", "9:19.5", "frame", "16:9", "9:16"]
        );
        assert_eq!(
            aspect["options"][3]["when"],
            json!({ "model": ["grok-imagine-image-2.0"] })
        );
        assert_eq!(aspect["options"][5]["when"], json!({ "frame": ["none"] }));
        assert_eq!(aspect["defaultByMode"], json!({ "video": "16:9" }));

        let res = find(&controls, "res");
        assert_eq!(
            values(&res["options"]),
            ["auto", "1K", "2K", "480p", "720p", "1080p"]
        );
        assert_eq!(
            res["options"][5]["when"],
            json!({ "@images": ["none", "one"], "frame": ["1"],
                    "model": ["grok-imagine-video-1.5"] })
        );
        assert!(res["options"][4].get("when").is_none());

        let voice = voice.unwrap();
        assert_eq!(
            voice["when"],
            json!({ "@videos": ["none"], "model": ["grok-imagine-video-1.5"] })
        );
        assert_eq!(
            voice["options"][1],
            json!({ "value": "eve", "label": "Eve" })
        );
    }

    #[test]
    fn image_only_catalogs_have_no_video_controls() {
        let image_only: String = INI.split("[grok-imagine-video-1.5]").next().unwrap().into();
        let models = parse(&image_only);
        let (controls, voice) = controls(&models).unwrap();
        let order: Vec<&str> = controls.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(order, ["model", "aspect", "res", "count", "quality"]);
        assert!(voice.is_none());
        assert_eq!(modes(&models), json!([{ "id": "image", "label": "Image" }]));
        assert!(super::controls(&[]).is_none());
    }

    #[test]
    fn refresh_arranges_the_panel_around_preset_and_repo() {
        let dir = std::env::temp_dir().join(format!("imagine-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("imagine.json");
        let ini = dir.join("models.ini");
        std::fs::write(
            &file,
            json!({ "composer": { "modes": [], "controls": [
                { "id": "repo", "kind": "repoFolder" },
                { "id": "preset", "kind": "preset" },
                { "id": "model", "kind": "list", "options": [] }
            ] } })
            .to_string(),
        )
        .unwrap();
        assert!(!refresh(&file, &ini).unwrap());
        std::fs::write(&ini, INI).unwrap();
        assert!(refresh(&file, &ini).unwrap());
        let harness: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let order: Vec<&str> = harness["composer"]["controls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            order,
            [
                "model", "action", "frame", "aspect", "res", "count", "quality", "duration",
                "extend", "preset", "voice", "repo"
            ]
        );
        assert_eq!(harness["composer"]["modes"][1]["id"], "video");
        assert!(!refresh(&file, &ini).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
