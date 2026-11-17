//! Replay recorded Jellyfin invocations through the rrffmpeg binary.
//!
//! Each fixture is one JSON line: `program`, `argv`, plus the optional
//! `stderr_as_stdout`, `exit_code` and `label` fields that `scripts/record.sh`
//! writes. The stand-in binary records the argv it receives, so a passthrough
//! regression fails here before a viewer sees it.
//!
//! `@SRC@` in an argument is replaced with a short synthetic file built with
//! `lavfi`. The whole module skips when ffmpeg is not installed.

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const SRC: &str = "@SRC@";

struct Fixture {
    label: String,
    program: String,
    argv: Vec<String>,
    stderr_as_stdout: bool,
    exit_code: i32,
}

/// Records its argv as a JSON array, marks stdout and stderr, then exits with
/// `FAKE_EXIT`. The dump path comes from `FAKE_ARGV_OUT`.
const FAKE: &str = r#"#!/bin/sh
{
    printf '['
    first=1
    for arg in "$@"; do
        if [ "$first" -eq 1 ]; then first=0; else printf ','; fi
        escaped=$(printf '%s' "$arg" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')
        printf '"%s"' "$escaped"
    done
    printf ']\n'
} > "$FAKE_ARGV_OUT"
printf 'fake-stdout\n'
printf 'fake-stderr\n' >&2
exit "${FAKE_EXIT:-0}"
"#;

fn work_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("rrffmpeg-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    })
}

/// Stand-in binary, created once per test process, mode 755.
fn fake_binary() -> &'static Path {
    static FAKE_PATH: OnceLock<PathBuf> = OnceLock::new();
    FAKE_PATH.get_or_init(|| {
        let path = work_dir().join("fake-ffmpeg");
        std::fs::write(&path, FAKE).expect("write fake binary");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake binary");
        path
    })
}

/// Synthetic media, or `None` when ffmpeg is missing. Callers then skip.
fn source() -> Option<&'static Path> {
    static SRC_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    SRC_PATH
        .get_or_init(|| {
            let src = work_dir().join("source.mkv");
            let status = Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=160x120:rate=5",
                    "-t",
                    "0.4",
                    "-c:v",
                    "mpeg4",
                ])
                .arg(&src)
                .status()
                .ok()?;
            status.success().then_some(src)
        })
        .as_deref()
}

fn load_fixtures() -> Vec<Fixture> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jellyfin");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();

    let mut fixtures = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read fixture");
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let where_ = format!("{}:{}", file.display(), index + 1);
            let value: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{where_}: not JSON: {e}"));
            fixtures.push(Fixture::parse(&value, &where_));
        }
    }
    fixtures
}

impl Fixture {
    fn parse(value: &Value, where_: &str) -> Fixture {
        let argv: Vec<String> = value["argv"]
            .as_array()
            .unwrap_or_else(|| panic!("{where_}: missing argv array"))
            .iter()
            .map(|a| {
                a.as_str()
                    .unwrap_or_else(|| panic!("{where_}: argv is not text"))
                    .to_string()
            })
            .collect();
        assert!(!argv.is_empty(), "{where_}: empty argv");

        Fixture {
            label: value["label"].as_str().unwrap_or(where_).to_string(),
            program: value["program"].as_str().unwrap_or("ffmpeg").to_string(),
            argv,
            stderr_as_stdout: value["stderr_as_stdout"].as_bool().unwrap_or(false),
            exit_code: value["exit_code"].as_i64().unwrap_or(0) as i32,
        }
    }
}

struct Replayed {
    argv: Vec<String>,
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// Run one fixture. With `via_env` the stand-in comes from `RRFFMPEG_BIN`,
/// otherwise from a generated config, which leaves `RRFFMPEG_BIN` unset.
fn replay(fixture: &Fixture, src: &Path, via_env: bool) -> Replayed {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dump = work_dir().join(format!(
        "argv-{}-{}.json",
        COUNTER.fetch_add(1, Ordering::Relaxed),
        fixture.exit_code
    ));

    let expected: Vec<String> = fixture
        .argv
        .iter()
        .map(|arg| arg.replace(SRC, &src.to_string_lossy()))
        .collect();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.args(&expected)
        .current_dir(work_dir())
        .env("FAKE_ARGV_OUT", &dump)
        .env("FAKE_EXIT", fixture.exit_code.to_string())
        .env("RRFFMPEG_PROGRAM", &fixture.program)
        .env("HOME", work_dir())
        .env("XDG_CONFIG_HOME", work_dir())
        .env("XDG_STATE_HOME", work_dir());

    if via_env {
        cmd.env("RRFFMPEG_BIN", fake_binary());
        // A config that does not exist forces local-only mode.
        cmd.env("RRFFMPEG_CONFIG", work_dir().join("absent.toml"));
    } else {
        cmd.env_remove("RRFFMPEG_BIN");
        let config = work_dir().join(format!("fake-{}.toml", std::process::id()));
        std::fs::write(
            &config,
            format!(
                "local_ffmpeg = \"{}\"\nlocal_ffprobe = \"{}\"\n",
                fake_binary().display(),
                fake_binary().display()
            ),
        )
        .expect("write test config");
        cmd.env("RRFFMPEG_CONFIG", &config);
    }

    if fixture.stderr_as_stdout {
        cmd.env("RRFFMPEG_STDERR_AS_STDOUT", "1");
    }

    let out = cmd.output().expect("rrffmpeg should start");
    let dump_text = std::fs::read_to_string(&dump)
        .unwrap_or_else(|e| panic!("{}: stand-in did not run: {e}", fixture.label));
    let received: Vec<String> = serde_json::from_str(&dump_text)
        .unwrap_or_else(|e| panic!("{}: bad argv dump: {e}", fixture.label));

    assert_eq!(
        received, expected,
        "{}: argv changed in flight",
        fixture.label
    );

    Replayed {
        argv: received,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    }
}

fn check(fixture: &Fixture, src: &Path, via_env: bool) {
    let replayed = replay(fixture, src, via_env);

    assert_eq!(
        replayed.code,
        Some(fixture.exit_code),
        "{}: exit code changed",
        fixture.label
    );

    if fixture.stderr_as_stdout {
        assert_eq!(
            replayed.stdout, "fake-stdout\nfake-stderr\n",
            "{}: stderr should join stdout",
            fixture.label
        );
        assert!(
            replayed.stderr.is_empty(),
            "{}: stderr should be empty, got {:?}",
            fixture.label,
            replayed.stderr
        );
    } else {
        assert_eq!(
            replayed.stdout, "fake-stdout\n",
            "{}: stdout changed",
            fixture.label
        );
        assert_eq!(
            replayed.stderr, "fake-stderr\n",
            "{}: stderr changed",
            fixture.label
        );
    }
    assert!(!replayed.argv.is_empty());
}

#[test]
fn fixtures_are_well_formed() {
    let fixtures = load_fixtures();
    assert!(
        fixtures.len() >= 7,
        "expected the recorded set, found {}",
        fixtures.len()
    );
    let labels: Vec<&str> = fixtures.iter().map(|f| f.label.as_str()).collect();
    assert!(
        labels.contains(&"probe-json"),
        "missing ffprobe fixture: {labels:?}"
    );
    assert!(
        labels.contains(&"hls-transcode"),
        "missing HLS fixture: {labels:?}"
    );
    assert!(
        labels.contains(&"trickplay-sprite"),
        "missing thumbnail fixture: {labels:?}"
    );
    assert!(
        labels.contains(&"remux-copy"),
        "missing remux fixture: {labels:?}"
    );
    assert!(
        labels.contains(&"4k-hdr-tonemap"),
        "missing 4K HDR fixture: {labels:?}"
    );
    assert!(
        labels.contains(&"seek-transcode"),
        "missing seek fixture: {labels:?}"
    );
}

#[test]
fn recorded_invocations_replay_byte_identically() {
    let Some(src) = source() else {
        eprintln!("skipping replay: ffmpeg is not installed");
        return;
    };
    let fixtures = load_fixtures();
    for fixture in &fixtures {
        check(fixture, src, true);
    }
}

#[test]
fn replay_works_without_the_bin_override() {
    let Some(src) = source() else {
        eprintln!("skipping replay: ffmpeg is not installed");
        return;
    };
    let fixtures = load_fixtures();
    for fixture in &fixtures {
        check(fixture, src, false);
    }
}
