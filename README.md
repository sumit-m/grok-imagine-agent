# imagine-agent

A Buzz custom harness with no LLM behind it. Each `@imagine` mention runs the
[imagine](../grok-imagine-cli) CLI (Grok Imagine) and posts the image or video it
saves, with its `.json` sidecar, back to the channel.

## Install

```powershell
.\install.ps1 -ImagineSource ..\grok-imagine-cli\dist
```

This builds the agent, copies it next to `imagine.exe` in
`%LOCALAPPDATA%\Programs\imagine`, and writes `imagine.json` to Buzz's
`custom_harnesses` folder. Add `-App prod` for the installed Buzz. Re-run it after
`just reset`.

## The panel

Mentioning an `@imagine` agent shows a panel in the message box. Its menus come
from imagine's `models.ini` and `presets.ini` and are rebuilt whenever Buzz reads
the panel.

- **Image:** Model, Aspect ratio, Resolution, number of images (1–10), Quality,
  Preset. Attached images are all sent, in order; Auto aspect and resolution
  follow the first.
- **Video:** Model, First frame (with images attached), Aspect ratio ("From frame"
  once a frame is picked), Resolution, Duration (1–15s), Voice, Preset.
  - With two or more images, every image is also a reference; references cap the
    video at 720p.
  - With a video attached: Extend (by 2–10s) or Edit, on Video 1.0.
  - Images and a video can't be sent together.
- **Repo:** saves the result and its sidecar to a channel repository.

In the prompt, `@image3` names the third attachment and becomes `<IMAGE_2>`;
`@eve` names a voice and becomes `<AUDIO_n>` with `--voice eve`.

## Typed commands

Without the panel, the message is an imagine command line:

```
@imagine gen a red fox --aspect 16:9 --repo assets/
```

Every imagine flag except `--output` passes through; `--repo` is the agent's own.
Attached images go to `generate` as `--image`, the first to `video` as its frame,
and an attached video to `video-extend` / `video-edit` as `--src`.

## Environment

| Variable | Default |
|---|---|
| `IMAGINE_CLI` | `imagine.exe` beside the agent, else `imagine` on `PATH` |
| `BUZZ_CLI` | `buzz` on `PATH` |
| `IMAGINE_HARNESS_FILE` | the harness file naming this exe |
