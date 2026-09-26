//! Snapshot tests: each fixture in `tests/fixtures/*.html` is rendered
//! headlessly and compared pixel-for-pixel with `tests/snapshots/<name>.png`.
//!
//! - No snapshot yet: the rendering is written and the test passes.
//! - `UPDATE_SNAPSHOTS=1`: snapshots are rewritten.
//! - No GPU adapter: the test is skipped with a message.
//!
//! Snapshots depend on the installed fonts, so they are only comparable on
//! the machine that produced them until a font is bundled (plan O11).

use std::path::PathBuf;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn snapshots_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

#[test]
fn fixtures_match_snapshots() {
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures found");

    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some();
    let mut mismatches = Vec::new();
    let mut gpu_missing = false;

    for fixture in &fixtures {
        let name = fixture.file_stem().unwrap().to_string_lossy().into_owned();
        let html = std::fs::read(fixture).expect("read fixture");
        let Some(pixels) = browser_paint::render_html(&html, WIDTH, HEIGHT, 1.0) else {
            gpu_missing = true;
            break;
        };
        let img = image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels).expect("buffer size");
        let snapshot = snapshots_dir().join(format!("{name}.png"));
        std::fs::create_dir_all(snapshots_dir()).expect("snapshots dir");

        if update || !snapshot.exists() {
            img.save(&snapshot).expect("write snapshot");
            eprintln!("wrote snapshot {}", snapshot.display());
            continue;
        }
        let expected = image::open(&snapshot).expect("read snapshot").into_rgba8();
        if expected.dimensions() != img.dimensions() {
            mismatches.push(format!("{name}: size differs"));
            continue;
        }
        let differing = expected
            .pixels()
            .zip(img.pixels())
            .filter(|(a, b)| a != b)
            .count();
        if differing > 0 {
            let actual = snapshots_dir().join(format!("{name}.actual.png"));
            img.save(&actual).expect("write actual");
            mismatches.push(format!("{name}: {differing} pixels differ (see {})", actual.display()));
        }
    }

    if gpu_missing {
        eprintln!("no GPU adapter available; snapshot test skipped");
        return;
    }
    assert!(mismatches.is_empty(), "snapshot mismatches:\n{}", mismatches.join("\n"));
}

/// Malformed inputs must render something or nothing, never panic.
#[test]
fn malformed_inputs_do_not_panic() {
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"<".to_vec(),
        b"<html><body><style>p { color: </style><p>x".to_vec(),
        b"<style>}}}{{{ @media { p { } </style><p>y</p>".to_vec(),
        b"<div style='width: -1e40px; height: 99999999999px; font-size: 1e30px'>big</div>".to_vec(),
        b"<p style='font-size: 0'>zero</p><p style='line-height: -5'>neg</p>".to_vec(),
        b"<table><tr><td><table><tr><td>nested".to_vec(),
        b"<img src='data:image/png;base64,AAAA'><img src='data:image/jpeg;base64,/9j/'>".to_vec(),
        vec![0xff, 0xfe, 0x00, 0x01, b'<', b'p', b'>', 0xc3, 0x28],
        b"<p style='display: flex; flex-direction: bogus'>".repeat(200),
        format!("<div>{}</div>", "<span>".repeat(2000)).into_bytes(),
    ];
    for (i, html) in cases.iter().enumerate() {
        let result = std::panic::catch_unwind(|| browser_paint::render_html(html, 200, 200, 1.0));
        assert!(result.is_ok(), "case {i} panicked");
    }
}
