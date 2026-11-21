//! Removes metadata the Buzz relay refuses, mirroring its structural
//! allowlist in `buzz-media/src/validation.rs`: from images EXIF, XMP, ICC,
//! comments, text and private chunks; from MP4s every box off the list
//! (Grok's `uuid` and `udta`/`meta`). Pixel and sample data are copied, never
//! re-encoded.

pub fn strip_metadata(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        strip_jpeg(bytes)
    } else if bytes.starts_with(PNG_SIGNATURE) {
        strip_png(bytes)
    } else if bytes.get(4..8) == Some(b"ftyp") {
        strip_mp4(bytes)
    } else {
        None
    }
}

/// Boxes the relay walks into.
const MP4_CONTAINERS: [&[u8; 4]; 9] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"dinf", b"sinf", b"schi",
];

/// Boxes the relay accepts (`udta` only as ffmpeg's empty one, so never here).
const MP4_ALLOWED: [&[u8; 4]; 35] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"trak", b"mdia", b"minf", b"stbl",
    b"edts", b"dinf", b"sinf", b"schi", b"mvhd", b"tkhd", b"mdhd", b"hdlr", b"vmhd", b"smhd",
    b"dref", b"url ", b"urn ", b"stsd", b"stts", b"stss", b"ctts", b"stsc", b"stsz", b"stco",
    b"co64", b"sgpd", b"sbgp", b"sdtp", b"elst",
];

/// Turns each box the relay refuses into a `free` box of the same size with
/// its contents zeroed, so every offset (the chunk tables into `mdat`) stays
/// right. `None` when the box structure doesn't parse.
fn strip_mp4(bytes: &[u8]) -> Option<Vec<u8>> {
    fn walk(out: &mut [u8], start: usize, end: usize, depth: usize) -> Option<()> {
        if depth > 32 {
            return None;
        }
        let mut at = start;
        while at < end {
            let compact = u32::from_be_bytes(out.get(at..at + 4)?.try_into().ok()?) as usize;
            let (size, header) = match compact {
                0 => (end - at, 8),
                1 => (
                    usize::try_from(u64::from_be_bytes(
                        out.get(at + 8..at + 16)?.try_into().ok()?,
                    ))
                    .ok()?,
                    16,
                ),
                n => (n, 8),
            };
            if size < header || at.checked_add(size)? > end {
                return None;
            }
            let kind: [u8; 4] = out[at + 4..at + 8].try_into().ok()?;
            if !MP4_ALLOWED.contains(&&kind) {
                out[at + 4..at + 8].copy_from_slice(b"free");
                out[at + header..at + size].fill(0);
            } else if MP4_CONTAINERS.contains(&&kind) {
                walk(out, at + header, at + size, depth + 1)?;
            }
            at += size;
        }
        Some(())
    }
    let mut out = bytes.to_vec();
    walk(&mut out, 0, bytes.len(), 0)?;
    Some(out)
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

const PNG_RENDERING_CHUNKS: [&[u8; 4]; 11] = [
    b"cHRM", b"gAMA", b"sBIT", b"sRGB", b"bKGD", b"hIST", b"tRNS", b"sPLT", b"acTL", b"fcTL",
    b"fdAT",
];

fn strip_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = PNG_SIGNATURE.to_vec();
    let mut i = PNG_SIGNATURE.len();
    while i + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[i..i + 4].try_into().ok()?) as usize;
        let kind: &[u8; 4] = bytes[i + 4..i + 8].try_into().ok()?;
        let end = i.checked_add(12)?.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        let critical = kind[0] & 0x20 == 0;
        if critical || PNG_RENDERING_CHUNKS.contains(&kind) {
            out.extend_from_slice(&bytes[i..end]);
        }
        if kind == b"IEND" {
            return Some(out);
        }
        i = end;
    }
    None
}

fn strip_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0xff, 0xd8];
    let mut i = 2;
    loop {
        if *bytes.get(i)? != 0xff {
            return None;
        }
        while bytes.get(i) == Some(&0xff) {
            i += 1;
        }
        let marker = *bytes.get(i)?;
        i += 1;
        match marker {
            0xd9 => {
                out.extend_from_slice(&[0xff, 0xd9]);
                return Some(out);
            }
            0x01 | 0xd0..=0xd7 => out.extend_from_slice(&[0xff, marker]),
            0xd8 => return None,
            _ => {
                let len = u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]) as usize;
                let end = i
                    .checked_add(len)
                    .filter(|&e| len >= 2 && e <= bytes.len())?;
                let payload = &bytes[i + 2..end];
                if keep_jpeg_segment(marker, payload) {
                    out.extend_from_slice(&[0xff, marker]);
                    out.extend_from_slice(&bytes[i..end]);
                }
                i = end;
                if marker == 0xda {
                    let scan_end = entropy_end(bytes, i)?;
                    out.extend_from_slice(&bytes[i..scan_end]);
                    i = scan_end;
                }
            }
        }
    }
}

fn keep_jpeg_segment(marker: u8, payload: &[u8]) -> bool {
    match marker {
        0xe0 => {
            payload.len() >= 14
                && &payload[..5] == b"JFIF\0"
                && payload.len() == 14 + 3 * payload[12] as usize * payload[13] as usize
        }
        0xee => payload.len() == 12 && &payload[..5] == b"Adobe",
        0xe1..=0xed | 0xef | 0xfe => false,
        _ => true,
    }
}

/// Index of the first marker after entropy-coded data that is not a stuffed
/// zero or a restart marker.
fn entropy_end(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i + 1 < bytes.len() {
        if bytes[i] == 0xff {
            let next = bytes[i + 1];
            if next != 0x00 && next != 0xff && !(0xd0..=0xd7).contains(&next) {
                return Some(i);
            }
            if next == 0xff {
                i += 1;
                continue;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn mp4_metadata_boxes_become_free_space_of_the_same_size() {
        let ilst = mp4_box(b"ilst", b"Grok prompt");
        let meta = mp4_box(b"meta", &[&[0, 0, 0, 0][..], &ilst].concat());
        let trak = mp4_box(b"trak", &mp4_box(b"tkhd", &[1; 12]));
        let moov = mp4_box(
            b"moov",
            &[mp4_box(b"mvhd", &[2; 8]), trak, mp4_box(b"udta", &meta)].concat(),
        );
        let file = [
            mp4_box(b"ftyp", b"isom"),
            mp4_box(b"uuid", b"c2pa manifest"),
            moov,
            mp4_box(b"mdat", &[7; 16]),
        ]
        .concat();
        let clean = strip_metadata(&file).unwrap();
        assert_eq!(clean.len(), file.len());
        for gone in [&b"uuid"[..], b"udta", b"meta", b"ilst", b"Grok", b"c2pa"] {
            assert!(!clean.windows(4).any(|w| w == &gone[..4]), "{gone:?} left");
        }
        for kept in [&b"tkhd"[..], b"mvhd", b"mdat", &[7; 16]] {
            assert!(clean.windows(kept.len()).any(|w| w == kept));
        }
        assert_eq!(strip_metadata(&file[..file.len() - 3]), None);
    }

    fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xff, marker];
        s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    fn jfif() -> Vec<u8> {
        let mut p = b"JFIF\0".to_vec();
        p.extend_from_slice(&[1, 1, 0, 0, 1, 0, 1, 0, 0]);
        p
    }

    #[test]
    fn jpeg_drops_exif_icc_comment_and_trailer() {
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend(segment(0xe0, &jfif()));
        jpeg.extend(segment(0xe1, b"Exif\0\0secret"));
        jpeg.extend(segment(0xe2, b"ICC_PROFILE\0..."));
        jpeg.extend(segment(0xfe, b"prompt: a fox"));
        jpeg.extend(segment(0xdb, &[0; 65]));
        jpeg.extend(segment(0xda, &[1, 1, 0, 0, 63, 0]));
        jpeg.extend_from_slice(&[0x12, 0xff, 0x00, 0x34, 0xff, 0xd0, 0x56]);
        jpeg.extend_from_slice(&[0xff, 0xd9, 0xde, 0xad]);

        let out = strip_metadata(&jpeg).unwrap();
        let mut expected = vec![0xff, 0xd8];
        expected.extend(segment(0xe0, &jfif()));
        expected.extend(segment(0xdb, &[0; 65]));
        expected.extend(segment(0xda, &[1, 1, 0, 0, 63, 0]));
        expected.extend_from_slice(&[0x12, 0xff, 0x00, 0x34, 0xff, 0xd0, 0x56, 0xff, 0xd9]);
        assert_eq!(out, expected);
    }

    #[test]
    fn jpeg_keeps_canonical_adobe_and_drops_custom_app0() {
        let mut adobe = b"Adobe".to_vec();
        adobe.extend_from_slice(&[0, 100, 0, 0, 0, 0, 1]);
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend(segment(0xe0, b"JFXX\0thumb"));
        jpeg.extend(segment(0xee, &adobe));
        jpeg.extend(segment(0xda, &[0]));
        jpeg.extend_from_slice(&[0x01, 0xff, 0xd9]);
        let out = strip_metadata(&jpeg).unwrap();
        let mut expected = vec![0xff, 0xd8];
        expected.extend(segment(0xee, &adobe));
        expected.extend(segment(0xda, &[0]));
        expected.extend_from_slice(&[0x01, 0xff, 0xd9]);
        assert_eq!(out, expected);
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0, 0, 0, 0]);
        c
    }

    #[test]
    fn png_keeps_only_critical_and_rendering_chunks() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend(chunk(b"IHDR", &[0; 13]));
        png.extend(chunk(b"tEXt", b"parameters\0a fox"));
        png.extend(chunk(b"iCCP", b"icc"));
        png.extend(chunk(b"pHYs", &[0; 9]));
        png.extend(chunk(b"gAMA", &[0; 4]));
        png.extend(chunk(b"IDAT", b"pixels"));
        png.extend(chunk(b"IEND", b""));
        png.extend_from_slice(b"trailing");

        let mut expected = PNG_SIGNATURE.to_vec();
        expected.extend(chunk(b"IHDR", &[0; 13]));
        expected.extend(chunk(b"gAMA", &[0; 4]));
        expected.extend(chunk(b"IDAT", b"pixels"));
        expected.extend(chunk(b"IEND", b""));
        assert_eq!(strip_metadata(&png).unwrap(), expected);
    }

    /// `IMAGINE_AGENT_SAMPLE_JPEG=<clean jpeg> cargo test -- --ignored`
    #[test]
    #[ignore]
    fn real_jpeg_round_trips_and_loses_injected_exif() {
        let path = std::env::var("IMAGINE_AGENT_SAMPLE_JPEG").expect("IMAGINE_AGENT_SAMPLE_JPEG");
        let clean = std::fs::read(path).unwrap();
        assert_eq!(strip_metadata(&clean).unwrap(), clean);

        let mut dirty = clean[..2].to_vec();
        dirty.extend(segment(0xe1, b"Exif\0\0GPS"));
        dirty.extend_from_slice(&clean[2..]);
        dirty.extend(segment(0xfe, b"after"));
        assert_eq!(strip_metadata(&dirty).unwrap(), clean);
    }

    #[test]
    fn unknown_or_truncated_input_is_none() {
        assert_eq!(strip_metadata(b"RIFF....WEBP"), None);
        assert_eq!(strip_metadata(&[0xff, 0xd8, 0xff, 0xe1, 0x00]), None);
    }
}
