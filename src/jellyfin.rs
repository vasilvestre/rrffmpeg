//! Compare Jellyfin's transcode settings with what the workers proved.
//!
//! Every Jellyfin knob only chooses the arguments it emits, and rrffmpeg passes
//! those through unchanged. A setting therefore works when every argument set it
//! can produce needs tokens at least one host advertises. `capabilities::required`
//! reads those argument sets, so this table cannot drift from placement.
//!
//! The audit is stricter than placement in one place: an HEVC row also asks for
//! `<family>_hevc` when that token exists, even though `required()` returns only
//! the family for an 8-bit profile (ADR 0024). A host that lists `vaapi` and
//! fails `hevc_vaapi` at run time is exactly the case worth catching.

use crate::capabilities;
use crate::config::{Config, PathMap};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The scalar children of `EncodingOptions`, key to text.
///
/// Jellyfin writes a flat list of scalar tags. Nested blocks (`CodecProfile`,
/// ...) are skipped by the parser and never read here.
#[derive(Debug, Clone, Default)]
pub struct Settings(BTreeMap<String, String>);

impl Settings {
    /// The text of one tag, or `None` when it is absent or empty.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// A `true`/`false` tag. `default` is Jellyfin's own default, which applies
    /// when the key is absent: `AllowHevcEncoding` is false by default,
    /// `EnableDecodingColorDepth10Hevc` is true.
    pub fn flag(&self, key: &str, default: bool) -> bool {
        match self.text(key) {
            Some(value) => value.eq_ignore_ascii_case("true"),
            None => default,
        }
    }
}

/// Reads the scalar tags of `encoding.xml`.
///
/// No XML dependency: the file is flat, and a tag this code does not know is
/// kept without being understood, so a newer Jellyfin key never fails the run.
pub fn parse(xml: &str) -> Settings {
    let mut out = BTreeMap::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        if let Some(body) = rest.strip_prefix("!--") {
            match body.find("-->") {
                Some(i) => rest = &body[i + 3..],
                None => break,
            }
            continue;
        }
        if rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('!') {
            match rest.find('>') {
                Some(i) => rest = &rest[i + 1..],
                None => break,
            }
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let raw_head = &rest[..gt];
        let name = raw_head.split_whitespace().next().unwrap_or("");
        if name.is_empty() {
            rest = &rest[gt + 1..];
            continue;
        }
        if raw_head.trim_end().ends_with('/') {
            out.insert(name.to_string(), String::new());
            rest = &rest[gt + 1..];
            continue;
        }
        let after = &rest[gt + 1..];
        let close = format!("</{name}>");
        let Some(end) = after.find(&close) else { break };
        let body = &after[..end];
        if body.contains('<') {
            // A container such as `EncodingOptions` or `CodecProfiles`: read
            // its children instead of treating the whole block as text.
            rest = after;
            continue;
        }
        out.insert(name.to_string(), unescape(body.trim()));
        rest = &after[end + close.len()..];
    }
    Settings(out)
}

/// The five entities Jellyfin can write into a path or a device name.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// One worker as the scheduler sees it: the tokens it advertises, its slot
/// count, and its path maps.
#[derive(Debug, Clone)]
pub struct Worker {
    pub name: String,
    pub tokens: Vec<String>,
    pub concurrency: usize,
    pub paths: Vec<PathMap>,
}

impl Worker {
    /// The scheduler's rule: an empty list is a host saying it runs anything.
    fn accepts(&self, need: &[String]) -> bool {
        self.tokens.is_empty() || need.iter().all(|token| self.tokens.contains(token))
    }
}

/// The declared capabilities of every host, which is what placement reads.
/// `--probe` replaces `tokens` with what the worker proves.
pub fn workers(config: &Config) -> Vec<Worker> {
    config
        .hosts
        .iter()
        .map(|host| Worker {
            name: host.name.clone(),
            tokens: host.capabilities.clone(),
            concurrency: host.concurrency,
            paths: host.paths.clone(),
        })
        .collect()
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Row {
    pub setting: String,
    pub value: String,
    /// The tokens the setting needs, or `-`.
    pub needs: String,
    /// `ok`, `partial`, `broken`, `unknown` or `warn`.
    pub verdict: &'static str,
    pub detail: String,
}

/// Builds the report. `probe_device` is the device the one-frame probe ran on,
/// so a `VaapiDevice` mismatch is visible instead of silent.
pub fn rows(settings: &Settings, workers: &[Worker], probe_device: &str) -> Vec<Row> {
    let mut out = Vec::new();
    let accel = settings
        .text("HardwareAccelerationType")
        .map(str::trim)
        .unwrap_or("none");
    let family = match accel {
        "vaapi" | "qsv" | "nvenc" | "amf" | "videotoolbox" => accel,
        _ => "none",
    };

    let hardware = settings.flag("EnableHardwareEncoding", true);
    out.push(Row {
        setting: "EnableHardwareEncoding".into(),
        value: hardware.to_string(),
        needs: "-".into(),
        verdict: "ok",
        detail: if hardware {
            "Jellyfin emits -c:v <family>; the rows below check where it lands".into()
        } else {
            "Jellyfin encodes in software on the client, so the rows below do not apply".into()
        },
    });

    if family == "none" {
        out.push(Row {
            setting: "HardwareAccelerationType".into(),
            value: accel.into(),
            needs: "-".into(),
            verdict: "ok",
            detail: "software only: no worker token is needed".into(),
        });
    } else if family == "amf" || family == "videotoolbox" {
        out.push(Row {
            setting: "HardwareAccelerationType".into(),
            value: accel.into(),
            needs: "-".into(),
            verdict: "unknown",
            detail: format!(
                "rrffmpeg has no {family} token, so placement cannot tell: any worker takes the job and a wrong one fails after taking a slot"
            ),
        });
    } else {
        let h264 = format!("h264_{family}");
        out.push(row(
            workers,
            "HardwareAccelerationType",
            accel,
            need(&["-c:v", &h264]),
        ));

        if settings.flag("AllowHevcEncoding", false) {
            let hevc = format!("hevc_{family}");
            let mut tokens = need(&["-c:v", &hevc]);
            // `required()` returns only the family for an 8-bit profile, and
            // ADR 0024 exists because a host can advertise that family and
            // still fail the encoder. Ask for the proof when one exists.
            let proof = format!("{family}_hevc");
            if capabilities::tokens().contains(&proof.as_str()) && !tokens.contains(&proof) {
                tokens.push(proof);
            }
            out.push(row(workers, "AllowHevcEncoding", "true", tokens));

            if settings.flag("EnableDecodingColorDepth10Hevc", true) {
                if family == "vaapi" {
                    let tokens = need(&[
                        "-c:v",
                        "hevc_vaapi",
                        "-profile:v",
                        "main10",
                        "-vf",
                        "format=p010,hwupload",
                    ]);
                    out.push(row(
                        workers,
                        "EnableDecodingColorDepth10Hevc",
                        "true",
                        tokens,
                    ));
                } else {
                    out.push(unknown(
                        "EnableDecodingColorDepth10Hevc",
                        "true",
                        "the one-frame probe tests 10-bit HEVC on vaapi only",
                    ));
                }
            }
        }

        if settings.flag("AllowAv1Encoding", false) {
            let av1 = format!("av1_{family}");
            out.push(row(
                workers,
                "AllowAv1Encoding",
                "true",
                need(&["-c:v", &av1]),
            ));
        }

        if settings.flag("EnableTonemapping", false) {
            if family == "vaapi" {
                let mut tokens = need(&["-vf", "tonemap_vaapi=format=nv12", "-c:v", "h264_vaapi"]);
                if settings.flag("EnableVppTonemapping", false) {
                    // `required()` cannot see `scale_vaapi`; the probe has a
                    // token for the VPP filter itself.
                    tokens.push("vaapi_vpp".into());
                }
                out.push(row(workers, "EnableTonemapping", "true", tokens));
            } else {
                out.push(unknown(
                    "EnableTonemapping",
                    "true",
                    "Jellyfin builds tonemap_opencl for this family, and the probe tests tonemap_vaapi only",
                ));
            }
        }
    }

    // Segments go to the client's tmpfs. Without a map the worker writes to a
    // path it does not mount, and every HLS job fails after the encode starts.
    if let Some(temp) = settings.text("TranscodingTempPath") {
        out.push(temp_row(temp, workers));
    }

    // `-1` means every core, once per job. On a host that runs several jobs at
    // once that is N times the machine, and the last one to grow loses.
    if settings.text("EncodingThreadCount") == Some("-1") {
        let busy: Vec<&str> = workers
            .iter()
            .filter(|worker| worker.concurrency > 1)
            .map(|worker| worker.name.as_str())
            .collect();
        if !busy.is_empty() {
            out.push(Row {
                setting: "EncodingThreadCount".into(),
                value: "-1".into(),
                needs: "-".into(),
                verdict: "warn",
                detail: format!(
                    "ffmpeg takes every core, and {} runs more than one job at once",
                    busy.join(", ")
                ),
            });
        }
    }

    if family == "vaapi"
        && let Some(device) = settings.text("VaapiDevice")
    {
        let (verdict, detail) = if device == probe_device {
            ("ok", format!("the probe ran on {probe_device}"))
        } else {
            (
                "unknown",
                format!(
                    "the probe ran on {probe_device} and Jellyfin uses {device}: re-run with --device {device}"
                ),
            )
        };
        out.push(Row {
            setting: "VaapiDevice".into(),
            value: device.into(),
            needs: "-".into(),
            verdict,
            detail,
        });
    }

    out
}

fn row(workers: &[Worker], setting: &str, value: &str, tokens: Vec<String>) -> Row {
    let (verdict, detail) = judge(workers, &tokens);
    Row {
        setting: setting.into(),
        value: value.into(),
        needs: tokens.join(", "),
        verdict,
        detail,
    }
}

fn unknown(setting: &str, value: &str, detail: &str) -> Row {
    Row {
        setting: setting.into(),
        value: value.into(),
        needs: "-".into(),
        verdict: "unknown",
        detail: detail.into(),
    }
}

/// The tokens for one argument set. This is the same function placement uses,
/// so a rule added to `capabilities.rs` reaches the audit for free.
fn need(args: &[&str]) -> Vec<String> {
    capabilities::required(
        &args
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect::<Vec<_>>(),
    )
}

fn judge(workers: &[Worker], tokens: &[String]) -> (&'static str, String) {
    if workers.is_empty() {
        return (
            "broken",
            "no worker declared, so a hardware job has nowhere to run".into(),
        );
    }
    let ok: Vec<&str> = workers
        .iter()
        .filter(|worker| worker.accepts(tokens))
        .map(|worker| worker.name.as_str())
        .collect();
    let missing: Vec<&str> = workers
        .iter()
        .filter(|worker| !worker.accepts(tokens))
        .map(|worker| worker.name.as_str())
        .collect();
    if ok.is_empty() {
        return ("broken", format!("no worker proves {}", tokens.join(" + ")));
    }
    if missing.is_empty() {
        return ("ok", format!("all workers: {}", ok.join(", ")));
    }
    (
        "partial",
        format!("only {}; missing on {}", ok.join(", "), missing.join(", ")),
    )
}

fn temp_row(temp: &str, workers: &[Worker]) -> Row {
    let path = Path::new(temp);
    let covered: Vec<&str> = workers
        .iter()
        .filter(|worker| {
            worker
                .paths
                .iter()
                .any(|map| !map.from.as_os_str().is_empty() && path.starts_with(map.from.as_path()))
        })
        .map(|worker| worker.name.as_str())
        .collect();
    let (verdict, detail) = if workers.is_empty() {
        ("broken", "no worker declared".to_string())
    } else if covered.is_empty() {
        (
            "broken",
            format!("no path map covers {temp}: segments land on a path the worker does not mount"),
        )
    } else if covered.len() == workers.len() {
        ("ok", format!("covered on {}", covered.join(", ")))
    } else {
        (
            "partial",
            format!(
                "covered on {}; another worker writes to a path it does not have",
                covered.join(", ")
            ),
        )
    };
    Row {
        setting: "TranscodingTempPath".into(),
        value: temp.into(),
        needs: "path map".into(),
        verdict,
        detail,
    }
}

/// The first existing `encoding.xml`: the explicit path, Jellyfin's
/// `JELLYFIN_CONFIG_DIR`, the package default, then a user install.
pub fn find(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::Config(format!("{} is not a file", path.display())))
        };
    }
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("JELLYFIN_CONFIG_DIR") {
        candidates.push(PathBuf::from(dir).join("encoding.xml"));
    }
    candidates.push(PathBuf::from("/etc/jellyfin/encoding.xml"));
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    if let Some(base) = base {
        candidates.push(base.join("jellyfin/config/encoding.xml"));
        candidates.push(base.join("jellyfin/encoding.xml"));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            Error::Config("no encoding.xml found: pass --path, or set JELLYFIN_CONFIG_DIR".into())
        })
}

pub fn print(path: &Path, rows: &[Row]) {
    println!("encoding.xml: {}", path.display());
    println!();
    println!(
        "{:<30} {:<9} {:<36} {:<8} detail",
        "setting", "value", "needs", "verdict"
    );
    for row in rows {
        println!(
            "{:<30} {:<9} {:<36} {:<8} {}",
            row.setting, row.value, row.needs, row.verdict, row.detail
        );
    }
    let proved: Vec<&str> = rows
        .iter()
        .filter(|row| row.verdict == "ok" && row.needs != "-")
        .map(|row| row.setting.as_str())
        .collect();
    let missing: Vec<&str> = rows
        .iter()
        .filter(|row| matches!(row.verdict, "broken" | "unknown"))
        .map(|row| row.setting.as_str())
        .collect();
    println!();
    if !proved.is_empty() {
        println!("proved: {}", proved.join(", "));
    }
    if !missing.is_empty() {
        println!("not proved: {}", missing.join(", "));
    }
}

pub fn json(path: &Path, rows: &[Row]) -> String {
    serde_json::json!({
        "encoding_xml": path.display().to_string(),
        "rows": rows,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn worker(name: &str, tokens: &[&str]) -> Worker {
        Worker {
            name: name.into(),
            tokens: tokens.iter().map(|t| t.to_string()).collect(),
            concurrency: 1,
            paths: vec![PathMap {
                from: PathBuf::from("/dev/shm/jellyfin-transcode"),
                to: PathBuf::from("/home/bob/transcode"),
            }],
        }
    }

    fn settings(xml: &str) -> Settings {
        parse(xml)
    }

    fn vaapi_xml(extra: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<EncodingOptions>
  <!-- a comment with a > inside -->
  <HardwareAccelerationType>vaapi</HardwareAccelerationType>
  <EnableHardwareEncoding>true</EnableHardwareEncoding>
  {extra}
</EncodingOptions>"#
        )
    }

    #[test]
    fn parse_reads_scalar_children_and_skips_nested_blocks() {
        let xml = vaapi_xml(
            "<VaapiDevice>/dev/dri/renderD128</VaapiDevice>\n  \
             <EncodingThreadCount>-1</EncodingThreadCount>\n  \
             <AllowHevcEncoding>false</AllowHevcEncoding>\n  \
             <CodecProfiles><CodecProfile><Type>Video</Type></CodecProfile></CodecProfiles>",
        );
        let s = settings(&xml);
        assert_eq!(s.text("HardwareAccelerationType"), Some("vaapi"));
        assert_eq!(s.text("VaapiDevice"), Some("/dev/dri/renderD128"));
        assert!(!s.flag("AllowHevcEncoding", true));
        assert_eq!(s.text("Missing"), None);
    }

    #[test]
    fn a_self_closing_tag_reads_as_empty_and_entities_are_decoded() {
        let s = settings(
            "<EncodingOptions><VaapiDevice /><MediaPath>/a&amp;b</MediaPath></EncodingOptions>",
        );
        assert_eq!(s.text("VaapiDevice"), None);
        assert_eq!(s.text("MediaPath"), Some("/a&b"));
    }

    #[test]
    fn absent_keys_take_jellyfins_default() {
        let s = settings("<EncodingOptions />");
        assert!(s.flag("EnableDecodingColorDepth10Hevc", true));
        assert!(!s.flag("AllowHevcEncoding", false));
    }

    #[test]
    fn an_empty_host_list_accepts_any_job_like_the_scheduler() {
        let s = settings(&vaapi_xml(""));
        let rows = super::rows(
            &s,
            &[worker("open", &[])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "HardwareAccelerationType")
            .unwrap();
        assert_eq!(row.verdict, "ok");
    }

    #[test]
    fn hevc_asks_for_the_proof_token_even_though_placement_does_not() {
        let s = settings(&vaapi_xml("<AllowHevcEncoding>true</AllowHevcEncoding>"));
        // `required()` for `-c:v hevc_vaapi` returns only `vaapi`, so a host
        // with that alone would be routed an HEVC job it may fail.
        let rows = super::rows(
            &s,
            &[
                worker("plain", &["vaapi"]),
                worker("proved", &["vaapi", "vaapi_hevc"]),
            ],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "AllowHevcEncoding")
            .unwrap();
        assert_eq!(row.verdict, "partial");
        assert!(row.needs.contains("vaapi_hevc"));
        assert!(row.detail.contains("missing on plain"));
    }

    #[test]
    fn a_ten_bit_hdr_job_needs_the_ten_bit_and_tonemap_tokens() {
        let s = settings(&vaapi_xml(
            "<AllowHevcEncoding>true</AllowHevcEncoding>\
             <EnableDecodingColorDepth10Hevc>true</EnableDecodingColorDepth10Hevc>\
             <EnableTonemapping>true</EnableTonemapping>",
        ));
        let rows = super::rows(
            &s,
            &[worker(
                "n100",
                &["vaapi", "vaapi_hevc", "vaapi_hevc10", "vaapi_tonemap"],
            )],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let ten = rows
            .iter()
            .find(|r| r.setting == "EnableDecodingColorDepth10Hevc")
            .unwrap();
        assert_eq!(ten.verdict, "ok");
        assert!(ten.needs.contains("vaapi_hevc10"));
        let tone = rows
            .iter()
            .find(|r| r.setting == "EnableTonemapping")
            .unwrap();
        assert!(tone.needs.contains("vaapi_tonemap"));
    }

    #[test]
    fn av1_on_a_host_without_an_av1_encoder_is_broken() {
        let s = settings(&vaapi_xml("<AllowAv1Encoding>true</AllowAv1Encoding>"));
        let rows = super::rows(
            &s,
            &[worker("n100", &["vaapi", "vaapi_hevc"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "AllowAv1Encoding")
            .unwrap();
        assert_eq!(row.verdict, "broken");
        assert!(row.needs.contains("vaapi_av1"));
    }

    #[test]
    fn vpp_tonemapping_adds_the_vpp_token() {
        let s = settings(&vaapi_xml(
            "<EnableTonemapping>true</EnableTonemapping>\
             <EnableVppTonemapping>true</EnableVppTonemapping>",
        ));
        let rows = super::rows(
            &s,
            &[
                worker("vppless", &["vaapi", "vaapi_tonemap"]),
                worker("vpp", &["vaapi", "vaapi_tonemap", "vaapi_vpp"]),
            ],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "EnableTonemapping")
            .unwrap();
        assert!(row.needs.contains("vaapi_vpp"));
        assert_eq!(row.verdict, "partial");
    }

    #[test]
    fn a_temp_path_without_a_map_is_broken() {
        let s = settings(&vaapi_xml(
            "<TranscodingTempPath>/dev/shm/other</TranscodingTempPath>",
        ));
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "TranscodingTempPath")
            .unwrap();
        assert_eq!(row.verdict, "broken");
    }

    #[test]
    fn a_temp_path_inside_a_map_is_ok() {
        let s = settings(&vaapi_xml(
            "<TranscodingTempPath>/dev/shm/jellyfin-transcode/abc</TranscodingTempPath>",
        ));
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "TranscodingTempPath")
            .unwrap();
        assert_eq!(row.verdict, "ok");
    }

    #[test]
    fn thread_count_minus_one_warns_when_a_host_runs_several_jobs() {
        let s = settings(&vaapi_xml("<EncodingThreadCount>-1</EncodingThreadCount>"));
        let mut busy = worker("w", &["vaapi"]);
        busy.concurrency = 2;
        let rows = super::rows(&s, &[busy], capabilities::DEFAULT_VAAPI_DEVICE);
        let row = rows
            .iter()
            .find(|r| r.setting == "EncodingThreadCount")
            .unwrap();
        assert_eq!(row.verdict, "warn");
    }

    #[test]
    fn a_vaapi_device_the_probe_did_not_test_is_unknown() {
        let s = settings(&vaapi_xml("<VaapiDevice>/dev/dri/renderD129</VaapiDevice>"));
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows.iter().find(|r| r.setting == "VaapiDevice").unwrap();
        assert_eq!(row.verdict, "unknown");
        assert!(row.detail.contains("renderD129"));
    }

    #[test]
    fn software_only_needs_nothing() {
        let s = settings(
            "<EncodingOptions><HardwareAccelerationType>none</HardwareAccelerationType></EncodingOptions>",
        );
        let rows = super::rows(&s, &[worker("w", &[])], capabilities::DEFAULT_VAAPI_DEVICE);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.verdict == "ok"));
    }

    #[test]
    fn an_unknown_accelerator_is_reported_not_guessed() {
        let s = settings(
            "<EncodingOptions><HardwareAccelerationType>amf</HardwareAccelerationType></EncodingOptions>",
        );
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows
            .iter()
            .find(|r| r.setting == "HardwareAccelerationType")
            .unwrap();
        assert_eq!(row.verdict, "unknown");
    }

    #[test]
    fn workers_come_from_the_declared_config() {
        let config = Config::from_toml(
            "[[hosts]]\nname = \"w\"\nconcurrency = 3\ncapabilities = [\"vaapi\"]\n",
        )
        .unwrap();
        let workers = super::workers(&config);
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].name, "w");
        assert_eq!(workers[0].concurrency, 3);
        assert!(workers[0].accepts(&["vaapi".to_string()]));
        assert!(!workers[0].accepts(&["qsv".to_string()]));
    }

    #[test]
    fn find_takes_an_explicit_file_and_refuses_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("rrffmpeg-jellyfin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("encoding.xml");
        std::fs::write(&file, "<EncodingOptions/>").unwrap();
        assert_eq!(find(Some(file.clone())).unwrap(), file);
        assert!(matches!(
            find(Some(dir.join("absent.xml"))),
            Err(Error::Config(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_file_stops_instead_of_looping() {
        // An unterminated comment, declaration, closer or empty tag name must
        // end the scan instead of spinning on the same offset.
        assert!(parse("<!-- no end").0.is_empty());
        assert!(parse("<EncodingOptions><!-- no end").0.is_empty());
        assert!(parse("<!DOCTYPE").0.is_empty());
        assert!(parse("</EncodingOptions").0.is_empty());
        assert!(parse("<>").0.is_empty());
        let s = settings("<!-- a > inside --><EncodingThreadCount>-1</EncodingThreadCount>");
        assert_eq!(s.text("EncodingThreadCount"), Some("-1"));
    }

    #[test]
    fn software_only_needs_no_token_and_disabled_hardware_encoding_is_said() {
        let none = settings(
            "<EncodingOptions><HardwareAccelerationType>none</HardwareAccelerationType></EncodingOptions>",
        );
        let rows = super::rows(
            &none,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let accel = rows
            .iter()
            .find(|r| r.setting == "HardwareAccelerationType")
            .unwrap();
        assert_eq!(accel.verdict, "ok");
        assert!(accel.detail.contains("software only"), "{}", accel.detail);

        let off = settings(
            "<EncodingOptions><HardwareAccelerationType>vaapi</HardwareAccelerationType>\
             <EnableHardwareEncoding>false</EnableHardwareEncoding></EncodingOptions>",
        );
        let rows = super::rows(
            &off,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        assert_eq!(rows[0].verdict, "ok");
        assert!(rows[0].detail.contains("software on the client"));
        assert!(rows[0].detail.contains("do not apply"));
    }

    #[test]
    fn a_family_rrffmpeg_has_no_token_for_reads_unknown() {
        let amf = settings(
            "<EncodingOptions><HardwareAccelerationType>amf</HardwareAccelerationType></EncodingOptions>",
        );
        let rows = super::rows(
            &amf,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let accel = rows
            .iter()
            .find(|r| r.setting == "HardwareAccelerationType")
            .unwrap();
        assert_eq!(accel.verdict, "unknown");
        assert_eq!(accel.value, "amf");
        assert!(accel.detail.contains("no amf token"), "{}", accel.detail);
    }

    #[test]
    fn a_family_the_probe_does_not_test_reads_unknown() {
        let qsv = settings(
            "<EncodingOptions><HardwareAccelerationType>qsv</HardwareAccelerationType>\
             <AllowHevcEncoding>true</AllowHevcEncoding>\
             <EnableDecodingColorDepth10Hevc>true</EnableDecodingColorDepth10Hevc>\
             <EnableTonemapping>true</EnableTonemapping></EncodingOptions>",
        );
        let rows = super::rows(
            &qsv,
            &[worker("q", &["qsv", "qsv_av1"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        // 8-bit HEVC on qsv needs only the family, and the row is `ok`.
        let hevc = rows
            .iter()
            .find(|r| r.setting == "AllowHevcEncoding")
            .unwrap();
        assert_eq!(hevc.verdict, "ok");
        assert_eq!(hevc.needs, "qsv");
        let depth = rows
            .iter()
            .find(|r| r.setting == "EnableDecodingColorDepth10Hevc")
            .unwrap();
        assert_eq!(depth.verdict, "unknown");
        assert_eq!(depth.needs, "-");
        let tone = rows
            .iter()
            .find(|r| r.setting == "EnableTonemapping")
            .unwrap();
        assert_eq!(tone.verdict, "unknown");
        assert!(tone.detail.contains("tonemap_opencl"), "{}", tone.detail);
    }

    #[test]
    fn no_declared_worker_makes_every_hardware_row_broken() {
        let s = settings(&vaapi_xml(
            "<TranscodingTempPath>/dev/shm/jellyfin-transcode</TranscodingTempPath>",
        ));
        let rows = super::rows(&s, &[], capabilities::DEFAULT_VAAPI_DEVICE);
        let accel = rows
            .iter()
            .find(|r| r.setting == "HardwareAccelerationType")
            .unwrap();
        assert_eq!(accel.verdict, "broken");
        assert!(accel.detail.contains("no worker declared"));
        let temp = rows
            .iter()
            .find(|r| r.setting == "TranscodingTempPath")
            .unwrap();
        assert_eq!(temp.verdict, "broken");
        assert_eq!(temp.detail, "no worker declared");
    }

    #[test]
    fn a_temp_path_no_map_covers_reads_broken() {
        let s = settings(&vaapi_xml(
            "<TranscodingTempPath>/var/tmp/elsewhere</TranscodingTempPath>",
        ));
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let temp = rows
            .iter()
            .find(|r| r.setting == "TranscodingTempPath")
            .unwrap();
        assert_eq!(temp.verdict, "broken");
        assert!(
            temp.detail.contains("no path map covers"),
            "{}",
            temp.detail
        );
        assert_eq!(temp.value, "/var/tmp/elsewhere");
        assert_eq!(temp.needs, "path map");
    }

    #[test]
    fn a_temp_path_one_worker_does_not_map_reads_partial() {
        let s = settings(&vaapi_xml(
            "<TranscodingTempPath>/dev/shm/jellyfin-transcode</TranscodingTempPath>",
        ));
        let mut bare = worker("bare", &["vaapi"]);
        bare.paths.clear();
        let rows = super::rows(
            &s,
            &[worker("mapped", &["vaapi"]), bare],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let temp = rows
            .iter()
            .find(|r| r.setting == "TranscodingTempPath")
            .unwrap();
        assert_eq!(temp.verdict, "partial");
        assert!(temp.detail.contains("covered on mapped"), "{}", temp.detail);
        assert!(temp.detail.contains("does not have"), "{}", temp.detail);
    }

    #[test]
    fn a_vaapi_device_the_probe_did_not_test_reads_unknown() {
        let s = settings(&vaapi_xml("<VaapiDevice>/dev/dri/renderD129</VaapiDevice>"));
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let row = rows.iter().find(|r| r.setting == "VaapiDevice").unwrap();
        assert_eq!(row.verdict, "unknown");
        assert!(
            row.detail.contains("--device /dev/dri/renderD129"),
            "{}",
            row.detail
        );
    }

    #[test]
    fn a_thread_count_of_minus_one_is_fine_on_a_single_slot_host() {
        let s = settings(&vaapi_xml("<EncodingThreadCount>-1</EncodingThreadCount>"));
        let rows = super::rows(
            &s,
            &[worker("one", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        assert!(!rows.iter().any(|r| r.setting == "EncodingThreadCount"));
    }

    #[test]
    fn json_carries_the_path_and_every_row() {
        let s = settings(&vaapi_xml(""));
        let rows = super::rows(
            &s,
            &[worker("w", &["vaapi"])],
            capabilities::DEFAULT_VAAPI_DEVICE,
        );
        let text = json(Path::new("/etc/jellyfin/encoding.xml"), &rows);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["encoding_xml"], "/etc/jellyfin/encoding.xml");
        assert_eq!(value["rows"].as_array().unwrap().len(), rows.len());
    }
}
