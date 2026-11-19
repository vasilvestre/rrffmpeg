//! Dispatch path cost: path mapping, capability read, config parse.
//!
//! These run per job, before ffmpeg starts, so their latency is the
//! dispatcher's own share of the start.

#[path = "harness/mod.rs"]
mod harness;

use criterion::{Criterion, criterion_group, criterion_main};
use rrffmpeg::{Config, PathMap, capabilities, paths};
use std::ffi::OsString;
use std::path::PathBuf;

/// The production config text, parsed as a file would be.
const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

fn bench(c: &mut Criterion) {
    // One mapped 4K input, as Jellyfin sends it.
    let mut host = harness::host("worker", 2);
    host.paths = vec![
        PathMap {
            from: PathBuf::from("/home/alice"),
            to: PathBuf::from("/home/bob/media"),
        },
        PathMap {
            from: PathBuf::from("/dev/shm/jellyfin-transcode"),
            to: PathBuf::from("/home/bob/transcode"),
        },
    ];
    let input = OsString::from("file:/home/alice/media/film.mkv");
    c.bench_function("dispatch_paths_to_worker", |b| {
        b.iter(|| std::hint::black_box(paths::to_worker(&host, &input)));
    });

    // A real Jellyfin HLS argument vector: 4K HDR to 1080p H.264 on VAAPI.
    let argv: Vec<OsString> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-fflags",
        "+genpts",
        "-init_hw_device",
        "vaapi=va:/dev/dri/renderD128",
        "-filter_hw_device",
        "va",
        "-i",
        "file:/home/alice/media/film.mkv",
        "-map",
        "0:0",
        "-map",
        "0:1",
        "-map",
        "0:2",
        "-c:v",
        "hevc_vaapi",
        "-profile:v",
        "main10",
        "-vf",
        "format=p010,hwupload",
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        "-c:s",
        "copy",
        "-f",
        "hls",
        "-hls_time",
        "6",
        "-hls_segment_filename",
        "/dev/shm/jellyfin-transcode/seg%05d.ts",
        "/dev/shm/jellyfin-transcode/out.m3u8",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    c.bench_function("dispatch_capabilities_required", |b| {
        b.iter(|| std::hint::black_box(capabilities::required(&argv)));
    });

    c.bench_function("dispatch_config_from_toml", |b| {
        b.iter(|| {
            std::hint::black_box(Config::from_toml(EXAMPLE_CONFIG).expect("example config parses"))
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
