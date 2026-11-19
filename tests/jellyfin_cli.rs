//! `rrffmpeg jellyfin` compares a Jellyfin config with the declared
//! capabilities and never edits the file.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-jellyfin-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn setup(root: &Path, capabilities: &str, xml: &str) -> (PathBuf, PathBuf) {
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "state_dir = \"{}\"\n\n[[hosts]]\nname = \"worker\"\ncapabilities = [{capabilities}]\n\n\
             [[hosts.paths]]\nfrom = \"/dev/shm/jellyfin-transcode\"\nto = \"/home/bob/transcode\"\n",
            root.join("state").display()
        ),
    )
    .expect("config");
    let encoding = root.join("encoding.xml");
    fs::write(&encoding, xml).expect("encoding.xml");
    (config, encoding)
}

const VAAPI_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<EncodingOptions>
  <EnableHardwareEncoding>true</EnableHardwareEncoding>
  <HardwareAccelerationType>vaapi</HardwareAccelerationType>
  <VaapiDevice>/dev/dri/renderD128</VaapiDevice>
  <AllowHevcEncoding>true</AllowHevcEncoding>
  <AllowAv1Encoding>true</AllowAv1Encoding>
  <TranscodingTempPath>/dev/shm/jellyfin-transcode</TranscodingTempPath>
</EncodingOptions>
"#;

#[test]
fn a_setting_no_worker_proves_exits_one_and_says_so_in_json() {
    let root = dir("broken");
    let (config, encoding) = setup(&root, "\"vaapi\", \"vaapi_hevc\"", VAAPI_XML);

    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["jellyfin", "--json", "--path"])
        .arg(&encoding)
        .env("RRFFMPEG_CONFIG", &config)
        .output()
        .expect("rrffmpeg starts");
    assert_eq!(out.status.code(), Some(1));

    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    let rows = value["rows"].as_array().expect("rows");
    let av1 = rows
        .iter()
        .find(|row| row["setting"] == "AllowAv1Encoding")
        .expect("an AV1 row");
    assert_eq!(av1["verdict"], "broken");
    let hevc = rows
        .iter()
        .find(|row| row["setting"] == "AllowHevcEncoding")
        .expect("a HEVC row");
    assert_eq!(hevc["verdict"], "ok");
    let temp = rows
        .iter()
        .find(|row| row["setting"] == "TranscodingTempPath")
        .expect("a temp row");
    assert_eq!(temp["verdict"], "ok");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_table_names_what_is_proved_and_what_is_not() {
    let root = dir("table");
    let (config, encoding) = setup(&root, "\"vaapi\", \"vaapi_hevc\"", VAAPI_XML);

    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["jellyfin", "--path"])
        .arg(&encoding)
        .env("RRFFMPEG_CONFIG", &config)
        .output()
        .expect("rrffmpeg starts");
    assert_eq!(out.status.code(), Some(1));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("encoding.xml: "), "{stdout}");
    assert!(stdout.contains("setting"), "{stdout}");
    assert!(stdout.contains("proved: "), "{stdout}");
    assert!(stdout.contains("AllowHevcEncoding"), "{stdout}");
    assert!(stdout.contains("not proved: "), "{stdout}");
    assert!(stdout.contains("AllowAv1Encoding"), "{stdout}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn jellyfins_config_dir_is_searched_without_a_path() {
    let root = dir("env");
    let (config, encoding) = setup(&root, "\"vaapi\", \"vaapi_hevc\"", VAAPI_XML);

    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["jellyfin", "--json"])
        .env("RRFFMPEG_CONFIG", &config)
        .env("JELLYFIN_CONFIG_DIR", &root)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("rrffmpeg starts");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    assert_eq!(
        value["encoding_xml"],
        encoding.display().to_string(),
        "{stdout}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_unknown_argument_is_refused() {
    let root = dir("arg");
    let (config, encoding) = setup(&root, "\"vaapi\"", VAAPI_XML);

    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["jellyfin", "--path"])
        .arg(&encoding)
        .arg("--nonsense")
        .env("RRFFMPEG_CONFIG", &config)
        .output()
        .expect("rrffmpeg starts");
    assert_eq!(out.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown argument"), "{stderr}");

    let _ = fs::remove_dir_all(&root);
}
