//! Small bench harness: shared helpers for the bench targets.
//!
//! Every target includes this module, so each helper is allowed to be unused
//! in some of them.
#![allow(dead_code)]

use std::path::PathBuf;

/// A fresh temp dir under `/tmp/opencode`, named for this process and tag.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        PathBuf::from("/tmp/opencode").join(format!("rrffmpeg-bench-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bench temp dir");
    dir
}

/// A worker with the given concurrency and no path rules.
pub fn host(name: &str, concurrency: usize) -> rrffmpeg::Host {
    rrffmpeg::Host {
        name: name.to_string(),
        concurrency,
        light_concurrency: 2,
        capabilities: Vec::new(),
        local: false,
        ffmpeg_bin: None,
        ffprobe_bin: None,
        agent_addr: None,
        ssh_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        paths: Vec::new(),
        reads: Vec::new(),
        prefer: 0,
    }
}
