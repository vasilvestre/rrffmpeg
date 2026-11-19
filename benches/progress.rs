//! Progress cost: parse 1 MiB of ffmpeg stderr in chunks, then take.
//!
//! Every stderr chunk of a job passes through `feed`, so this is the cost a
//! chatty transcode adds on the client while ffmpeg runs.

#[path = "harness/mod.rs"]
mod harness;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use rrffmpeg::progress;

fn bench(c: &mut Criterion) {
    let stream = build_stream(1024 * 1024);
    let mut group = c.benchmark_group("progress_feed");
    group.throughput(Throughput::Bytes(1 << 20));
    group.bench_function("1mib", |b| {
        b.iter(|| {
            for chunk in stream.chunks(4_096) {
                progress::feed(chunk);
            }
        });
    });
    group.finish();

    // `take` runs once per job, after ffmpeg exits.
    c.bench_function("progress_take", |b| {
        b.iter(|| {
            progress::feed(b"frame=  100 fps= 24 q=-0.0 speed=1.02x\r");
            std::hint::black_box(progress::take())
        });
    });
}

/// Real-looking stderr: input and output headers, then a stats line.
fn build_stream(size: usize) -> Vec<u8> {
    let block = concat!(
        "Input #0, matroska,webm, from 'file:/home/alice/media/film.mkv':\r\n",
        "  Stream #0:0: Video: hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/smpte2084), ",
        "3840x2160 [SAR 1:1 DAR 16:9], 23.98 fps\r\n",
        "Output #0, hls, to '/dev/shm/jellyfin-transcode/out.m3u8':\r\n",
        "  Stream #0:0: Video: h264 (High), vaapi(tv, bt709, progressive), 1920x1080, ",
        "4000 kb/s, 23.98 fps\r\n",
        "frame=  100 fps= 24 q=-0.0 size=  512kB time=00:00:04.17 ",
        "bitrate=1006.2kbits/s speed=1.02x\r",
    );
    let mut out = Vec::with_capacity(size + 1);
    while out.len() < size {
        out.extend_from_slice(block.as_bytes());
    }
    out.truncate(size);
    // End on a line break so the last chunk is never a half line.
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out
}

criterion_group!(benches, bench);
criterion_main!(benches);
