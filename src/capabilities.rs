//! What a worker can actually do, proved by one frame per encoder.
//!
//! Neither libva nor FFmpeg answers the only question placement needs. libva
//! advertises `VAProfileHEVCMain10` on a Skylake iGPU that then fails to
//! initialise the decode context Jellyfin asks for, and
//! `avcodec_get_hw_config` only names the device types an encoder accepts. A
//! one-frame encode is the only statement that cannot lie, so the probe runs
//! that on the worker and prints one token per success.

use std::ffi::OsString;

/// One encoder test: the token it prints, whether it needs the VAAPI device,
/// and the arguments after the shared source. The tone-map test is separate:
/// it builds an HDR clip first.
const CASES: [(&str, bool, &str); 9] = [
    ("vaapi", true, "-vf format=nv12,hwupload -c:v h264_vaapi"),
    (
        "vaapi_hevc",
        true,
        "-vf format=nv12,hwupload -c:v hevc_vaapi -profile:v main",
    ),
    (
        "vaapi_hevc10",
        true,
        "-vf format=p010,hwupload -c:v hevc_vaapi -profile:v main10",
    ),
    ("vaapi_av1", true, "-vf format=nv12,hwupload -c:v av1_vaapi"),
    (
        "vaapi_vpp",
        true,
        "-vf format=nv12,hwupload,scale_vaapi=w=160:h=90 -c:v h264_vaapi",
    ),
    ("qsv", false, "-c:v h264_qsv"),
    ("qsv_av1", false, "-c:v av1_qsv"),
    ("nvenc", false, "-c:v h264_nvenc"),
    ("nvenc_av1", false, "-c:v av1_nvenc"),
];

/// Every token the probe can report. A host advertises a subset in
/// `capabilities`.
pub fn tokens() -> Vec<&'static str> {
    CASES
        .iter()
        .map(|(token, _, _)| *token)
        .chain(["vaapi_tonemap"])
        .collect()
}

/// The one-line shell command the probe runs on the worker.
///
/// It must stay a single line: a seedbox login shell may be a restricted
/// wrapper that accepts a command but not a script on stdin. Every test is
/// bounded, so a wedged driver cannot hang the caller.
pub fn script(ffmpeg: &str) -> String {
    let f = crate::transport::shell_quote(ffmpeg);
    let run = format!("timeout 30 {f} -hide_banner -loglevel error -nostdin -y");
    let src = "-f lavfi -i color=c=black:s=320x180:r=5:d=0.2";
    let dev = "-vaapi_device /dev/dri/renderD128";
    let null = "-frames:v 1 -f null -";
    // Each entry is a complete statement: it prints its own token, so a
    // failing test cannot swallow the next one.
    let mut out = String::new();
    for (token, needs_dev, tail) in CASES {
        let dev = if needs_dev {
            format!(" {dev}")
        } else {
            String::new()
        };
        let cmd = format!("{run}{dev} {src} {tail} {null}");
        out.push_str(&format!("{cmd} >/dev/null 2>&1 && echo {token}; "));
    }
    // tonemap_vaapi refuses a frame with no mastering display, so this test
    // builds a two-frame HDR clip first. Without libx265 the token is absent.
    out.push_str(&format!(
        "h=$(mktemp -d)/h.mkv; ( {run} {src} -vf setparams=color_trc=smpte2084:colorspace=bt2020nc:color_primaries=bt2020,format=p010le -c:v libx265 -preset ultrafast -x265-params 'log-level=error:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400' $h && {run} {dev} -i $h -vf format=p010,hwupload,tonemap_vaapi=format=nv12 -c:v h264_vaapi {null} ) >/dev/null 2>&1 && echo vaapi_tonemap; rm -rf ${{h%/*}}; "
    ));
    out.push_str("true");
    out
}

/// The tokens the probe printed. Anything else on stdout is ignored, so a
/// chatty shell cannot invent a capability.
pub fn parse(output: &str) -> Vec<String> {
    tokens()
        .into_iter()
        .filter(|token| output.lines().any(|line| line.trim() == *token))
        .map(str::to_string)
        .collect()
}

/// The capabilities a job needs, read from the arguments Jellyfin passes.
///
/// The encoder names the accelerator and the codec. A 10-bit profile and the
/// tone-mapping filter name the sub-capabilities that split one GPU generation
/// from the next. No token comes from the input file: reading it would cost a
/// probe on the dispatch path, and Jellyfin already states its choice in the
/// arguments.
pub fn required(args: &[OsString]) -> Vec<String> {
    let mut accelerator = None;
    let mut av1 = false;
    let mut ten_bit = false;
    let mut tonemap = false;
    for arg in args {
        let text = arg.to_string_lossy();
        // `_vaapi` also ends with `vaapi`, so one suffix test covers both the
        // bare and the prefixed encoder names.
        for cap in ["vaapi", "nvenc", "qsv", "amf", "videotoolbox"] {
            if text.ends_with(cap) {
                accelerator = Some(cap);
            }
        }
        av1 = av1 || text.starts_with("av1_");
        ten_bit = ten_bit || text.contains("main10") || text.contains("p010");
        tonemap = tonemap || text.contains("tonemap_vaapi");
    }
    let Some(accelerator) = accelerator else {
        return Vec::new();
    };
    let mut need = vec![accelerator.to_string()];
    // AV1 is a codec, not a profile: an AV1 encoder needs its own token, and
    // the H.264/HEVC tokens say nothing about it.
    let av1_token = format!("{accelerator}_av1");
    if av1 && tokens().iter().any(|token| *token == av1_token) {
        need.push(av1_token);
    } else if accelerator == "vaapi" {
        if ten_bit {
            need.push("vaapi_hevc10".to_string());
        }
        if tonemap {
            need.push("vaapi_tonemap".to_string());
        }
    }
    need
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn an_encoder_names_the_accelerator() {
        assert_eq!(required(&args(&["-c:v", "h264_vaapi"])), vec!["vaapi"]);
        assert_eq!(required(&args(&["-c:v", "hevc_nvenc"])), vec!["nvenc"]);
        assert_eq!(required(&args(&["-c:v", "h264_qsv"])), vec!["qsv"]);
        assert!(required(&args(&["-c:v", "libx264"])).is_empty());
        assert!(required(&args(&["-c", "copy"])).is_empty());
        // Software AV1 names no accelerator, so any host can run it.
        assert!(required(&args(&["-c:v", "libsvtav1"])).is_empty());
    }

    #[test]
    fn an_av1_encoder_asks_for_its_own_token() {
        assert_eq!(
            required(&args(&["-c:v", "av1_vaapi"])),
            vec!["vaapi", "vaapi_av1"]
        );
        assert_eq!(
            required(&args(&["-c:v", "av1_qsv"])),
            vec!["qsv", "qsv_av1"]
        );
        assert_eq!(
            required(&args(&["-c:v", "av1_nvenc"])),
            vec!["nvenc", "nvenc_av1"]
        );
        // A 10-bit hint does not turn an AV1 job into an HEVC one.
        assert_eq!(
            required(&args(&["-c:v", "av1_vaapi", "-profile:v", "main10"])),
            vec!["vaapi", "vaapi_av1"]
        );
    }

    #[test]
    fn a_ten_bit_profile_and_a_tone_map_add_a_token() {
        assert_eq!(
            required(&args(&["-c:v", "hevc_vaapi", "-profile:v", "main10"])),
            vec!["vaapi", "vaapi_hevc10"]
        );
        assert_eq!(
            required(&args(&[
                "-vf",
                "format=p010,hwupload",
                "-c:v",
                "hevc_vaapi"
            ])),
            vec!["vaapi", "vaapi_hevc10"]
        );
        assert_eq!(
            required(&args(&[
                "-vf",
                "tonemap_vaapi=format=nv12",
                "-c:v",
                "h264_vaapi"
            ])),
            vec!["vaapi", "vaapi_tonemap"]
        );
        // A ten-bit hint on a non-vaapi encoder adds nothing.
        assert_eq!(
            required(&args(&["-c:v", "hevc_nvenc", "-profile:v", "main10"])),
            vec!["nvenc"]
        );
    }

    #[test]
    fn the_probe_reads_only_known_tokens() {
        let out = "vaapi\nvaapi_hevc\nbanana\nqsv\n";
        assert_eq!(parse(out), vec!["vaapi", "vaapi_hevc", "qsv"]);
        assert_eq!(parse("vaapi_hevc10 extra"), Vec::<String>::new());
    }

    #[test]
    fn the_script_runs_one_command_per_token() {
        let s = script("ffmpeg");
        assert!(s.matches("timeout 30").count() > tokens().len());
        assert!(s.ends_with("true"));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn copy_does_not_request_a_codec_token() {
        assert!(required(&args(&["-c:v", "copy"])).is_empty());
        assert!(required(&args(&["-c:v", "copy", "-c:a", "copy"])).is_empty());
    }

    #[test]
    fn duplicate_requirements_are_deduplicated() {
        // Two encoders naming the same accelerator, plus repeated hints, still
        // yield each token once.
        let need = required(&args(&[
            "-c:v",
            "h264_vaapi",
            "-profile:v",
            "main10",
            "-c:v",
            "hevc_vaapi",
            "-vf",
            "format=p010,hwupload",
            "-vf",
            "tonemap_vaapi=format=nv12",
        ]));
        assert_eq!(need, vec!["vaapi", "vaapi_hevc10", "vaapi_tonemap"]);
    }

    #[test]
    fn token_matching_is_case_sensitive() {
        assert!(required(&args(&["-c:v", "h264_VAAPI"])).is_empty());
        assert!(parse("VAAPI\nVaapi\n").is_empty());
        assert_eq!(parse("vaapi\n"), vec!["vaapi"]);
    }
}
