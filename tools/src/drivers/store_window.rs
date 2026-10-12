// SPDX-License-Identifier: MIT
//! The actual Linux Store webview with offline content, without accounts.
//! The test program renders the production purchase host, or with `--health`
//! the window check page, in a real window; this driver captures that window
//! and requires a rendered page and a cancellation that reaches the native
//! event loop. The fixture forbids form submissions and network navigation
//! and opens no real Store. `--legacy` demonstrates the previous
//! foreign-window attachment for comparison and may fail to render.
//!
//! It needs the native build's Cargo environment, a desktop display and
//! ImageMagick's `import`, or Hyprland and grim for a native Wayland window.
use super::common::{self, Owned, exit_code, truthy};
use crate::util::{Failure, Result, pretty, require};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const PROGRAM: &str = include_str!("programs/store_window.rs.txt");
const NAME: &str = "flightdeck-store-window-test";
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
/// More than any screen; bounds what a damaged header can make this allocate.
const PIXELS_MAX: usize = 1 << 26;

fn paeth(left: u8, up: u8, corner: u8) -> u8 {
    let distance =
        |value: u8| (i16::from(left) + i16::from(up) - i16::from(corner) - i16::from(value)).abs();
    if distance(left) <= distance(up) && distance(left) <= distance(corner) {
        left
    } else if distance(up) <= distance(corner) {
        up
    } else {
        corner
    }
}

/// The pixels of a screenshot as 8-bit RGB. Alpha is dropped and 16-bit
/// samples keep their high byte. Interlaced files, which neither capture
/// tool writes, are refused.
fn pixels(data: &[u8]) -> Result<Vec<[u8; 3]>> {
    let invalid = || Failure("The screenshot is not a readable PNG".into());
    let mut rest = data.strip_prefix(&SIGNATURE).ok_or_else(invalid)?;
    let (mut header, mut palette, mut packed) = (None, &[][..], Vec::new());
    loop {
        let length = rest
            .get(..4)
            .map(|length| u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize)
            .filter(|length| rest.len() >= 12 && rest.len() - 12 >= *length)
            .ok_or_else(invalid)?;
        let (kind, body) = (&rest[4..8], &rest[8..8 + length]);
        let mut sum = flate2::Crc::new();
        sum.update(&rest[4..8 + length]);
        require(
            sum.sum().to_be_bytes() == rest[8 + length..12 + length],
            "The screenshot is a damaged PNG",
        )?;
        match kind {
            b"IHDR" => header = Some(body),
            b"PLTE" => palette = body,
            b"IDAT" => packed.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        rest = &rest[12 + length..];
    }
    let header = header
        .filter(|header| header.len() == 13)
        .ok_or_else(invalid)?;
    let size = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]) as usize
    };
    let (width, height, depth, color) = (size(0), size(4), usize::from(header[8]), header[9]);
    let channels = match (color, depth) {
        (0, 1 | 2 | 4 | 8 | 16) | (3, 1 | 2 | 4 | 8) => 1,
        (2, 8 | 16) => 3,
        (4, 8 | 16) => 2,
        (6, 8 | 16) => 4,
        _ => return Err(invalid()),
    };
    require(
        header[10] == 0 && header[11] == 0 && header[12] == 0,
        "The screenshot is an interlaced PNG, which is not supported",
    )?;
    require(
        width > 0 && height > 0 && width.saturating_mul(height) <= PIXELS_MAX,
        "The screenshot has no usable size",
    )?;
    let bits = channels * depth;
    let (step, stride) = (bits.div_ceil(8), (width * bits).div_ceil(8));
    let expected = height * (stride + 1);
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&packed[..])
        .take(expected as u64)
        .read_to_end(&mut raw)
        .map_err(|_| invalid())?;
    require(raw.len() == expected, "The screenshot is a truncated PNG")?;
    // One sample of a row, where a sample narrower than a byte is packed
    // from the high bits down.
    let sample = |line: &[u8], index: usize| match depth {
        8 => line[index],
        16 => line[index * 2],
        _ => {
            let bit = index * depth;
            (line[bit / 8] >> (8 - depth - bit % 8)) & ((1 << depth) - 1)
        }
    };
    let full = if depth < 8 {
        255 / ((1 << depth) - 1)
    } else {
        1
    };
    let mut pixels = Vec::with_capacity(width * height);
    let mut previous = vec![0u8; stride];
    for row in raw.chunks_exact(stride + 1) {
        let mut line = row[1..].to_vec();
        for index in 0..stride {
            let before = |line: &[u8]| if index >= step { line[index - step] } else { 0 };
            let predicted = match row[0] {
                0 => 0,
                1 => before(&line),
                2 => previous[index],
                3 => ((u16::from(before(&line)) + u16::from(previous[index])) / 2) as u8,
                4 => paeth(before(&line), previous[index], before(&previous)),
                _ => return Err(invalid()),
            };
            line[index] = line[index].wrapping_add(predicted);
        }
        for x in 0..width {
            pixels.push(match color {
                0 | 4 => [sample(&line, x * channels) * full; 3],
                2 | 6 => [0, 1, 2].map(|channel| sample(&line, x * channels + channel)),
                _ => {
                    let entry = usize::from(sample(&line, x)) * 3;
                    let entry = palette.get(entry..entry + 3).ok_or_else(invalid)?;
                    [entry[0], entry[1], entry[2]]
                }
            });
        }
        previous = line;
    }
    Ok(pixels)
}

/// The share of nearly white pixels and the number of distinct colors. A
/// rendered page has a light surface and many colors; an unpainted or
/// black window has neither.
fn statistics(screenshot: &[u8]) -> Result<(f64, usize)> {
    let pixels = pixels(screenshot)?;
    let white = pixels
        .iter()
        .filter(|pixel| pixel.iter().all(|channel| *channel > 235))
        .count();
    let colors: HashSet<_> = pixels.iter().collect();
    Ok((white as f64 / pixels.len() as f64, colors.len()))
}

/// Whether the page reported a real layout with its controls and the capture
/// shows it. The health page has no checkout frame.
fn verdict(render: &Value, health: bool, white: f64, colors: usize) -> Result<bool> {
    let field = |name: &str| {
        render
            .get(name)
            .ok_or_else(|| Failure(format!("The render event has no {name}")))
    };
    let size = |name: &str| {
        field(name)?
            .as_f64()
            .ok_or_else(|| Failure(format!("The render event has no numeric {name}")))
    };
    Ok(size("width")? > 100.0
        && size("height")? > 100.0
        && truthy(field("button")?)
        && (health || truthy(field("frame")?))
        && white > 0.1
        && colors > 10)
}

fn arguments(legacy: bool, health: bool) -> Vec<&'static str> {
    legacy
        .then_some("--legacy")
        .into_iter()
        .chain(health.then_some("--health"))
        .collect()
}

/// Runs the test program and captures its window when the page reports that
/// it has rendered. Returns every event the program printed.
fn observe(
    binary: &Path,
    arguments: &[&str],
    backend: &str,
    output: &Path,
    desktop: &mut dyn FnMut(&str) -> Result<Value>,
) -> Result<Vec<Value>> {
    let errors = fs::File::create(output.join("stderr.log"))?;
    let mut command = Command::new(binary);
    command
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(errors)
        .env("GDK_BACKEND", backend);
    let mut process = Owned::terminated(common::spawn(&mut command)?);
    let pid = process.child.id();
    let lines = process
        .child
        .stdout
        .take()
        .ok_or_else(|| Failure("The test program has no output".into()))?;
    let (mut results, mut window) = (Vec::new(), None);
    for line in BufReader::new(lines).lines() {
        let value = common::event(&line?)?;
        if let Some(id) = value.get("window") {
            window = Some(id.clone());
        }
        let rendered = value["probe"] == "render";
        results.push(value);
        if rendered {
            common::capture(&common::screenshot(
                window.as_ref(),
                pid,
                &output.join("window.png"),
                true,
                desktop,
            )?)?;
        }
    }
    let status = common::wait_until(&mut process.child, Instant::now() + Duration::from_secs(20))?
        .ok_or_else(|| Failure("The test program did not exit".into()))?;
    require(
        status.success(),
        format!(
            "The test program failed with exit code {}",
            exit_code(status)
        ),
    )?;
    Ok(results)
}

pub fn run(stage: &Path, legacy: bool, health: bool, backend: &str) -> Result<u8> {
    let stage = common::stage(stage)?;
    let output = tempfile::Builder::new()
        .prefix("flightdeck-store-window-")
        .tempdir()?
        .keep();
    let binary = common::built(
        &stage,
        NAME,
        PROGRAM,
        "Refusing to replace an existing probe",
        false,
        None,
    )?;
    let results = observe(
        &binary,
        &arguments(legacy, health),
        backend,
        &output,
        &mut common::hyprctl,
    )?;
    // No render event means no capture, and that is a failed run.
    let screenshot = output.join("window.png");
    let screenshot = fs::read(&screenshot)
        .map_err(|error| Failure(format!("{}: {error}", screenshot.display())))?;
    let (white, colors) = statistics(&screenshot)?;
    let render = results
        .iter()
        .find(|value| value["probe"] == "render")
        .ok_or_else(|| Failure("The page reported no render".into()))?;
    let passed = verdict(render, health, white, colors)?;
    let report = json!({"passed": passed, "legacy_attachment": legacy, "backend": backend,
        "health_check": health, "white_fraction": white, "colors": colors, "events": results,
        "artifacts": output.display().to_string()});
    fs::write(output.join("result.json"), pretty(&report))?;
    println!("{report}");
    require(
        legacy || passed,
        "The real Linux webview did not render the offline fixture",
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::fail;
    use std::io::Write;

    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
        chunk.extend(kind);
        chunk.extend(body);
        let mut sum = flate2::Crc::new();
        sum.update(&chunk[4..]);
        chunk.extend(sum.sum().to_be_bytes());
        chunk
    }

    /// A PNG from unfiltered rows, each stored with the given filter type.
    fn png(
        width: u32,
        depth: u8,
        color: u8,
        palette: &[u8],
        rows: &[Vec<u8>],
        filter: u8,
    ) -> Vec<u8> {
        let step = match color {
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 1,
        } * usize::from(depth).div_ceil(8);
        let mut raw = Vec::new();
        let mut previous = vec![0u8; rows[0].len()];
        for row in rows {
            raw.push(filter);
            for (index, value) in row.iter().enumerate() {
                let left = if index >= step { row[index - step] } else { 0 };
                let corner = if index >= step {
                    previous[index - step]
                } else {
                    0
                };
                let up = previous[index];
                raw.push(value.wrapping_sub(match filter {
                    0 => 0,
                    1 => left,
                    2 => up,
                    3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                    _ => paeth(left, up, corner),
                }));
            }
            previous = row.clone();
        }
        let mut packed =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        packed.write_all(&raw).expect("compress");
        let packed = packed.finish().expect("compress");
        let mut header = width.to_be_bytes().to_vec();
        header.extend((rows.len() as u32).to_be_bytes());
        header.extend([depth, color, 0, 0, 0]);
        let mut file = SIGNATURE.to_vec();
        file.extend(chunk(b"IHDR", &header));
        if !palette.is_empty() {
            file.extend(chunk(b"PLTE", palette));
        }
        file.extend(chunk(b"tEXt", b"Software\0synthetic"));
        // The data may be split anywhere between chunks.
        let (first, second) = packed.split_at(packed.len() / 2);
        file.extend(chunk(b"IDAT", first));
        file.extend(chunk(b"IDAT", second));
        file.extend(chunk(b"IEND", b""));
        file
    }

    #[test]
    fn screenshots_decode_through_every_filter_and_pixel_format() {
        let colors: [[u8; 3]; 6] = [
            [255, 255, 255],
            [250, 0, 3],
            [17, 200, 99],
            [0, 0, 0],
            [236, 240, 255],
            [90, 91, 92],
        ];
        // Three rows of four pixels, so that every filter has neighbors.
        let image: Vec<[u8; 3]> = (0..12).map(|index| colors[(index * 5 + 1) % 6]).collect();
        for filter in 0..=4 {
            let rgb: Vec<Vec<u8>> = image.chunks(4).map(|row| row.concat()).collect();
            assert_eq!(
                pixels(&png(4, 8, 2, &[], &rgb, filter)).expect("rgb"),
                image,
                "{filter}"
            );
            let rgba: Vec<Vec<u8>> = image
                .chunks(4)
                .map(|row| row.iter().flat_map(|p| [p[0], p[1], p[2], 7]).collect())
                .collect();
            assert_eq!(
                pixels(&png(4, 8, 6, &[], &rgba, filter)).expect("rgba"),
                image,
                "{filter}"
            );
            let deep: Vec<Vec<u8>> = image
                .chunks(4)
                .map(|row| row.iter().flatten().flat_map(|v| [*v, 0x5a]).collect())
                .collect();
            assert_eq!(
                pixels(&png(4, 16, 2, &[], &deep, filter)).expect("deep"),
                image,
                "{filter}"
            );
        }
        let palette = colors.concat();
        let indexed = vec![vec![0, 1, 2], vec![3, 4, 5]];
        assert_eq!(
            pixels(&png(3, 8, 3, &palette, &indexed, 4)).expect("palette"),
            colors
        );
        // Four-bit indexes share bytes; the last row byte is padded.
        let packed = vec![vec![0x01, 0x20], vec![0x34, 0x50]];
        assert_eq!(
            pixels(&png(3, 4, 3, &palette, &packed, 0)).expect("packed"),
            colors
        );
        let gray = |values: &[u8]| values.iter().map(|v| [*v; 3]).collect::<Vec<_>>();
        assert_eq!(
            pixels(&png(2, 8, 4, &[], &[vec![9, 255, 240, 0]], 1)).expect("gray alpha"),
            gray(&[9, 240])
        );
        assert_eq!(
            pixels(&png(10, 1, 0, &[], &[vec![0b1010_0000, 0b0100_0000]], 0)).expect("bits"),
            gray(&[255, 0, 255, 0, 0, 0, 0, 0, 0, 255])
        );
        assert_eq!(
            pixels(&png(3, 2, 0, &[], &[vec![0b0001_1011]], 0)).expect("two bits"),
            gray(&[0, 85, 170])
        );
        assert_eq!(
            pixels(&png(2, 16, 0, &[], &[vec![0x80, 0xff, 0x01, 0x00]], 2)).expect("deep gray"),
            gray(&[0x80, 0x01])
        );
    }

    #[test]
    fn damaged_and_unsupported_screenshots_are_refused() {
        let rows = [vec![1, 2, 3, 4, 5, 6]];
        let valid = png(2, 8, 2, &[], &rows, 0);
        assert!(pixels(&valid).is_ok());
        for cut in [0, 7, 8, 20, 33, valid.len() - 13, valid.len() - 1] {
            assert!(pixels(&valid[..cut]).is_err(), "{cut}");
        }
        let mut flipped = valid.clone();
        flipped[20] ^= 1;
        assert_eq!(
            pixels(&flipped).expect_err("checksum").0,
            "The screenshot is a damaged PNG"
        );
        // Declared taller than the data, an index beyond the palette, an
        // unknown filter, an invalid format and an interlaced file.
        let mut tall = SIGNATURE.to_vec();
        let mut header = 2u32.to_be_bytes().to_vec();
        header.extend(2u32.to_be_bytes());
        header.extend([8, 2, 0, 0, 0]);
        tall.extend(chunk(b"IHDR", &header));
        tall.extend(&valid[33..]);
        assert_eq!(
            pixels(&tall).expect_err("short").0,
            "The screenshot is a truncated PNG"
        );
        assert!(pixels(&png(1, 8, 3, &[0, 0, 0], &[vec![1]], 0)).is_err());
        assert!(pixels(&png(2, 8, 2, &[], &rows, 5)).is_err());
        assert!(pixels(&png(2, 4, 2, &[], &rows, 0)).is_err());
        let mut interlaced = SIGNATURE.to_vec();
        header.truncate(8);
        header.extend([8, 2, 0, 0, 1]);
        interlaced.extend(chunk(b"IHDR", &header));
        interlaced.extend(&valid[33..]);
        assert!(
            pixels(&interlaced)
                .expect_err("interlaced")
                .0
                .contains("interlaced")
        );
        assert!(pixels(b"not an image").is_err());
    }

    #[test]
    fn statistics_count_nearly_white_pixels_and_distinct_colors() {
        // Eight pixels: three nearly white, of which two are the same color.
        let row = [
            [255, 255, 255],
            [255, 255, 255],
            [236, 236, 236],
            [235, 255, 255],
            [0, 0, 0],
            [0, 0, 1],
            [200, 100, 50],
            [0, 0, 0],
        ]
        .concat();
        let (white, colors) = statistics(&png(8, 8, 2, &[], &[row], 1)).expect("statistics");
        assert_eq!((white, colors), (0.375, 6));
    }

    #[test]
    fn verdict_requires_layout_controls_and_a_painted_capture() {
        let render =
            json!({"probe": "render", "width": 720, "height": 820, "button": true, "frame": true});
        assert!(verdict(&render, false, 0.6, 400).expect("verdict"));
        for (name, value) in [
            ("width", json!(100)),
            ("height", json!(64.5)),
            ("button", json!(false)),
            ("frame", json!(false)),
        ] {
            let mut changed = render.clone();
            changed[name] = value;
            assert!(
                !verdict(&changed, false, 0.6, 400).expect("verdict"),
                "{name}"
            );
        }
        assert!(!verdict(&render, false, 0.1, 400).expect("verdict"));
        assert!(!verdict(&render, false, 0.6, 10).expect("verdict"));
        // The health page has no checkout frame, and none is asked for.
        let health = json!({"probe": "render", "width": 720, "height": 820, "button": true,
            "frame": false});
        assert!(verdict(&health, true, 0.6, 400).expect("verdict"));
        assert!(!verdict(&health, false, 0.6, 400).expect("verdict"));
        assert!(verdict(&json!({"probe": "render"}), false, 0.6, 400).is_err());
        assert!(verdict(&json!({"width": "wide"}), false, 0.6, 400).is_err());
    }

    #[test]
    fn program_is_the_offline_store_window_fixture() {
        assert_eq!(PROGRAM.len(), 3718);
        assert!(PROGRAM.starts_with("\n// SPDX-License-Identifier: MIT\n"));
        assert!(PROGRAM.ends_with("\"payment_requests\":0}));\n}\n"));
        for marker in [
            "#[path = \"../store_purchase.rs\"]",
            "#[path = \"../store_window_check.rs\"]",
            "Flightdeck offline Store test",
            "store_purchase::attach_webview(&window, builder)",
            "--legacy",
            "--health",
            "form-action 'none'",
            "Cancellation did not reach the native event loop",
        ] {
            assert!(PROGRAM.contains(marker), "{marker}");
        }
        assert_eq!(arguments(false, false), [""; 0]);
        assert_eq!(arguments(true, false), ["--legacy"]);
        assert_eq!(arguments(false, true), ["--health"]);
        assert_eq!(arguments(true, true), ["--legacy", "--health"]);
    }

    /// A stand-in test program in a new directory.
    fn program(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("program");
        common::fixtures::script(&path, text);
        (temp, path)
    }

    #[test]
    fn window_is_captured_when_the_page_renders_and_the_program_must_succeed() {
        let mut asked = Vec::new();
        let mut desktop = |list: &str| -> Result<Value> {
            asked.push(list.to_string());
            fail("No desktop in this test")
        };
        // Without a render event nothing is captured and nobody is asked.
        let (temp, path) = program(
            "echo \"{\\\"window\\\":7,\\\"backend\\\":\\\"$GDK_BACKEND\\\",\\\"arguments\\\":\\\"$*\\\"}\"\n\
             echo problem >&2\necho '{\"cancelled\":true}'",
        );
        let events = observe(
            &path,
            &["--legacy", "--health"],
            "x11",
            temp.path(),
            &mut desktop,
        )
        .expect("events");
        assert_eq!(
            events,
            [
                json!({"window": 7, "backend": "x11", "arguments": "--legacy --health"}),
                json!({"cancelled": true})
            ]
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("stderr.log")).expect("log"),
            "problem\n"
        );
        let (temp, path) = program("echo '{\"window\":7}'; exit 101");
        assert_eq!(
            observe(&path, &[], "x11", temp.path(), &mut desktop)
                .expect_err("failure")
                .0,
            "The test program failed with exit code 101"
        );
        // A native Wayland window is looked up on the desktop. That fails
        // here, and the program, which would stay for half a minute, is
        // stopped instead of being left behind.
        let (temp, path) =
            program("echo '{\"window\":null}'; echo '{\"probe\":\"render\"}'; exec sleep 30");
        let started = Instant::now();
        assert_eq!(
            observe(&path, &[], "wayland", temp.path(), &mut desktop)
                .expect_err("desktop")
                .0,
            "No desktop in this test"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        let (temp, path) = program("echo 'thread main panicked'; exec sleep 30");
        assert!(observe(&path, &[], "x11", temp.path(), &mut desktop).is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
        drop(desktop);
        assert_eq!(asked, ["clients"]);
    }
}
