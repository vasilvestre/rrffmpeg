//! Client to worker path rewriting.
//!
//! The worker mounts media at a different root than the client, and HLS
//! segments come back through a second mount. Both directions matter.
//!
//! Rewriting works on `Path` components, never on `String`, so a media file
//! whose name is not valid UTF-8 survives unchanged. Every rule is a plain
//! prefix replacement, the longest matching prefix wins, and a path that
//! matches no rule comes back as it went in.

use crate::Host;
use std::ffi::OsString;
use std::path::Path;

/// Rewrite a client path for the worker.
pub fn to_worker(host: &Host, path: &OsString) -> OsString {
    rewrite(host, path, true)
}

/// Rewrite a worker path back to the client root.
pub fn to_client(host: &Host, path: &OsString) -> OsString {
    rewrite(host, path, false)
}

/// `forward` selects the rule direction: `from` -> `to` for the worker,
/// `to` -> `from` on the way back.
fn rewrite(host: &Host, path: &OsString, forward: bool) -> OsString {
    let path = Path::new(path);
    let mut best: Option<(&Path, &Path)> = None;
    let mut best_key = (0usize, 0usize);

    for rule in &host.paths {
        let (from, to) = if forward {
            (rule.from.as_path(), rule.to.as_path())
        } else {
            (rule.to.as_path(), rule.from.as_path())
        };
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
            name: "virmire".into(),
            concurrency: 1,
            capabilities: vec![],
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
            ("/home/vasilves", "/home/my666/le4"),
            ("/dev/shm/jellyfin-transcode", "/home/my666/transcode"),
        ])
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn maps_media_to_the_worker_root() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("/home/vasilves/media/film.mkv")),
            os("/home/my666/le4/media/film.mkv")
        );
    }

    #[test]
    fn maps_hls_output_back_to_the_client_root() {
        let h = production();
        assert_eq!(
            to_client(&h, &os("/home/my666/transcode/seg0001.ts")),
            os("/dev/shm/jellyfin-transcode/seg0001.ts")
        );
    }

    #[test]
    fn mapping_is_reversible() {
        let h = production();
        let original = os("/home/vasilves/media/film.mkv");
        let there = to_worker(&h, &original);
        assert_eq!(to_client(&h, &there), original);
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
            to_worker(&h, &os("/home/vasilvesx/film.mkv")),
            os("/home/vasilvesx/film.mkv")
        );
    }

    #[test]
    fn longest_matching_prefix_wins() {
        let h = host(vec![
            ("/home/vasilves", "/home/my666/le4"),
            ("/home/vasilves/media", "/mnt/media"),
        ]);
        assert_eq!(
            to_worker(&h, &os("/home/vasilves/media/film.mkv")),
            os("/mnt/media/film.mkv")
        );
        assert_eq!(
            to_worker(&h, &os("/home/vasilves/other/film.mkv")),
            os("/home/my666/le4/other/film.mkv")
        );
    }

    #[test]
    fn nested_rules_work_in_both_directions() {
        let h = host(vec![
            ("/home/vasilves", "/home/my666/le4"),
            ("/home/vasilves/media", "/mnt/media"),
        ]);
        assert_eq!(
            to_client(&h, &os("/mnt/media/film.mkv")),
            os("/home/vasilves/media/film.mkv")
        );
    }

    #[test]
    fn unmapped_paths_pass_through_unchanged() {
        let h = production();
        for p in ["-c:v", "libx264", "pipe:0", "/etc/passwd", "/home/vasilvs"] {
            assert_eq!(to_worker(&h, &os(p)), os(p));
            assert_eq!(to_client(&h, &os(p)), os(p));
        }
    }

    #[test]
    fn an_exact_match_has_no_trailing_slash() {
        let h = production();
        assert_eq!(to_worker(&h, &os("/home/vasilves")), os("/home/my666/le4"));
        assert_eq!(to_client(&h, &os("/home/my666/le4")), os("/home/vasilves"));
    }

    #[test]
    fn trailing_slashes_on_either_side_are_tolerated() {
        let h = host(vec![("/home/vasilves/", "/home/my666/le4/")]);
        assert_eq!(
            to_worker(&h, &os("/home/vasilves/media/film.mkv")),
            os("/home/my666/le4/media/film.mkv")
        );
        assert_eq!(to_worker(&h, &os("/home/vasilves/")), os("/home/my666/le4"));
        assert_eq!(
            to_client(&h, &os("/home/my666/le4/media/film.mkv")),
            os("/home/vasilves/media/film.mkv")
        );
    }

    #[test]
    fn a_non_utf8_name_survives_byte_for_byte() {
        let h = production();
        let client = OsString::from_vec(b"/home/vasilves/f\xff\xfe.mkv".to_vec());
        let worker = to_worker(&h, &client);
        assert_eq!(worker.as_bytes(), b"/home/my666/le4/f\xff\xfe.mkv");
        assert_eq!(to_client(&h, &worker).as_bytes(), client.as_bytes());
    }

    #[test]
    fn a_host_without_rules_rewrites_nothing() {
        let h = host(vec![]);
        assert_eq!(
            to_worker(&h, &os("/home/vasilves/film.mkv")),
            os("/home/vasilves/film.mkv")
        );
    }

    #[test]
    fn a_relative_argument_is_never_treated_as_absolute() {
        let h = production();
        assert_eq!(
            to_worker(&h, &os("home/vasilves/film.mkv")),
            os("home/vasilves/film.mkv")
        );
    }
}
