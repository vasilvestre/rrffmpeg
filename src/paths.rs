//! Client to worker path rewriting.
//!
//! The worker mounts media at a different root than the client, and HLS
//! segments land in a second mount there.
//!
//! Rewriting works on `Path` components, never on `String`, so a media file
//! whose name is not valid UTF-8 survives unchanged. Every rule is a plain
//! prefix replacement, the longest matching prefix wins, and a path that
//! matches no rule comes back as it went in.

use crate::Host;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// Jellyfin prefixes local media inputs with `file:` (EncodingUtils), so a rule
/// that only matches a leading `/` misses every real input path.
const FILE_PREFIX: &[u8] = b"file:";

/// Rewrite a client path for the worker.
pub fn to_worker(host: &Host, path: &OsString) -> OsString {
    map_arg(host, path)
}

/// Maps a whole argument: a bare path, or a `file:` token with the path inside.
/// Other schemes (`http://`, `pipe:`, `-vf`) are left alone.
fn map_arg(host: &Host, arg: &OsString) -> OsString {
    let bytes = arg.as_bytes();
    let Some(rest) = bytes.strip_prefix(FILE_PREFIX) else {
        return rewrite(host, arg);
    };
    let rest = OsString::from_vec(rest.to_vec());
    let mut out = OsString::from("file:");
    // .NET strips the quotes before exec, but keep them if they are there.
    let quoted = rest.as_bytes().len() >= 2
        && rest.as_bytes().first() == Some(&b'"')
        && rest.as_bytes().last() == Some(&b'"');
    if quoted {
        let inner = OsString::from_vec(rest.as_bytes()[1..rest.as_bytes().len() - 1].to_vec());
        out.push("\"");
        out.push(rewrite(host, &inner));
        out.push("\"");
    } else {
        out.push(rewrite(host, &rest));
    }
    out
}

/// Rewrites `from` -> `to` at the longest matching rule.
fn rewrite(host: &Host, path: &OsString) -> OsString {
    let path = Path::new(path);
    let mut best: Option<(&Path, &Path)> = None;
    let mut best_key = (0usize, 0usize);

    for rule in &host.paths {
        let (from, to) = (rule.from.as_path(), rule.to.as_path());
        if from.as_os_str().is_empty() || !path.starts_with(from) {
            continue;
        }
        // Longest prefix wins: more components first, then more bytes. The
        // byte count breaks a tie between rules of equal depth. A strict
        // comparison keeps the first rule when two are identical.
        let key = (from.components().count(), from.as_os_str().len());
        if key > best_key {
            best_key = key;
            best = Some((from, to));
        }
    }

    match best {
        Some((from, to)) => match path.strip_prefix(from) {
            // An exact match must not gain or keep a trailing slash.
            Ok(rest) if rest.as_os_str().is_empty() => to
                .components()
                .collect::<std::path::PathBuf>()
                .into_os_string(),
            Ok(rest) => to.join(rest).into_os_string(),
            Err(_) => path.as_os_str().to_os_string(),
        },
        None => path.as_os_str().to_os_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PathMap;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    fn host(paths: Vec<(&str, &str)>) -> Host {
        Host {
            name: "worker".into(),
            concurrency: 1,
            capabilities: vec![],
            ffmpeg_bin: None,
            ffprobe_bin: None,
            agent_addr: None,
            ssh_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            paths: paths
                .into_iter()
                .map(|(from, to)| PathMap {
                    from: PathBuf::from(from),
                    to: PathBuf::from(to),
                })
                .collect(),
        }
    }

    fn production() -> Host {
        host(vec![
            ("/home/alice", "/home/bob/media"),
            ("/dev/shm/jellyfin-transcode", "/home/bob/transcode"),
        ])
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn maps_media_to_the_worker_root() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("/home/alice/media/film.mkv")),
            os("/home/bob/media/media/film.mkv")
        );
    }

    #[test]
    fn maps_hls_output_to_the_worker_transcode_root() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("/dev/shm/jellyfin-transcode/seg0001.ts")),
            os("/home/bob/transcode/seg0001.ts")
        );
    }

    #[test]
    fn prefix_must_end_on_a_component_boundary() {
        let h = host(vec![("/home/a", "/mnt/a")]);
        assert_eq!(to_worker(&h, &os("/home/ab/x")), os("/home/ab/x"));
        assert_eq!(to_worker(&h, &os("/home/a/x")), os("/mnt/a/x"));
    }

    #[test]
    fn a_near_miss_parent_does_not_match() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("/home/alicex/film.mkv")),
            os("/home/alicex/film.mkv")
        );
    }

    #[test]
    fn longest_matching_prefix_wins() {
        let h = host(vec![
            ("/home/alice", "/home/bob/media"),
            ("/home/alice/media", "/mnt/media"),
        ]);
        assert_eq!(
            to_worker(&h, &os("/home/alice/media/film.mkv")),
            os("/mnt/media/film.mkv")
        );
        assert_eq!(
            to_worker(&h, &os("/home/alice/other/film.mkv")),
            os("/home/bob/media/other/film.mkv")
        );
    }

    #[test]
    fn unmapped_paths_pass_through_unchanged() {
        let h = production();
        for p in ["-c:v", "libx264", "pipe:0", "/etc/passwd", "/home/mallory"] {
            assert_eq!(to_worker(&h, &os(p)), os(p));
        }
    }

    #[test]
    fn an_exact_match_has_no_trailing_slash() {
        let h = production();
        assert_eq!(to_worker(&h, &os("/home/alice")), os("/home/bob/media"));
    }

    #[test]
    fn trailing_slashes_on_either_side_are_tolerated() {
        let h = host(vec![("/home/alice/", "/home/bob/media/")]);
        assert_eq!(
            to_worker(&h, &os("/home/alice/media/film.mkv")),
            os("/home/bob/media/media/film.mkv")
        );
        assert_eq!(to_worker(&h, &os("/home/alice/")), os("/home/bob/media"));
    }

    #[test]
    fn a_non_utf8_name_survives_byte_for_byte() {
        let h = production();
        let client = OsString::from_vec(b"/home/alice/f\xff\xfe.mkv".to_vec());
        let worker = to_worker(&h, &client);
        assert_eq!(worker.as_bytes(), b"/home/bob/media/f\xff\xfe.mkv");
    }

    #[test]
    fn a_host_without_rules_rewrites_nothing() {
        let h = host(vec![]);
        assert_eq!(
            to_worker(&h, &os("/home/alice/film.mkv")),
            os("/home/alice/film.mkv")
        );
    }

    #[test]
    fn a_relative_argument_is_never_treated_as_absolute() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("home/alice/film.mkv")),
            os("home/alice/film.mkv")
        );
    }

    #[test]
    fn a_file_prefixed_input_is_mapped_inside_the_prefix() {
        // Jellyfin sends `file:/abs/path`, never a bare path.
        let h = production();
        assert_eq!(
            to_worker(&h, &os("file:/home/alice/media/film.mkv")),
            os("file:/home/bob/media/media/film.mkv")
        );
    }

    #[test]
    fn file_prefix_mapping_survives_dotnet_style_quotes_and_spaces() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("file:\"/home/alice/my film.mkv\"")),
            os("file:\"/home/bob/media/my film.mkv\"")
        );
        assert_eq!(
            to_worker(&h, &os("file:/home/alice/my film.mkv")),
            os("file:/home/bob/media/my film.mkv")
        );
    }

    #[test]
    fn other_schemes_and_plain_filters_are_untouched() {
        let h = production();
        for arg in [
            "http://example.invalid/live.m3u8",
            "pipe:0",
            "-vf",
            "tonemap_opencl=tonemap=hable",
            "file:relative.mkv",
        ] {
            assert_eq!(to_worker(&h, &os(arg)), os(arg), "{arg} must not change");
        }
    }

    #[test]
    fn a_bare_file_colon_is_left_alone() {
        let h = production();
        assert_eq!(to_worker(&h, &os("file:")), os("file:"));
    }

    #[test]
    fn two_maps_of_equal_length_pick_the_first_declared() {
        // Same `from` under two `to`: the depth and byte keys tie, so the
        // strict comparison keeps the rule declared first.
        let h = host(vec![
            ("/home/alice", "/mnt/first"),
            ("/home/alice", "/mnt/second"),
        ]);
        assert_eq!(
            to_worker(&h, &os("/home/alice/film.mkv")),
            os("/mnt/first/film.mkv")
        );
    }

    #[test]
    fn a_map_to_the_same_root_is_identity() {
        let h = host(vec![("/home/alice", "/home/alice")]);
        assert_eq!(
            to_worker(&h, &os("/home/alice/media/film.mkv")),
            os("/home/alice/media/film.mkv")
        );
        assert_eq!(to_worker(&h, &os("/home/alice")), os("/home/alice"));
    }
}
