//! End-to-end tests of the `linapo` binary. None of them need PipeWire.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_linapo"))
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("linapo-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("failed to run linapo")
}

fn ok(cmd: &mut Command) -> String {
    let o = run(cmd);
    assert!(o.status.success(), "failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn wav(path: &Path, channels: u16, frames: usize, f: impl Fn(usize, u16) -> f32) {
    let spec = hound::WavSpec { channels, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..frames {
        for c in 0..channels {
            w.write_sample(f(i, c)).unwrap();
        }
    }
    w.finalize().unwrap();
}

fn read(path: &Path) -> (u16, Vec<f32>) {
    let mut r = hound::WavReader::open(path).unwrap();
    (r.spec().channels, r.samples::<f32>().map(Result::unwrap).collect())
}

const PROFILE: &str = "Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 4 dB Q 1\n\
                       Channel: R\nFilter: ON HSC Fc 8000 Hz Gain -3 dB Q 0.7\n\
                       Channel: all\nCopy: L=0.5*L+0.5*R\n";

#[test]
fn import_creates_a_valid_setup_and_info_shows_it() {
    let d = tmp("import");
    let apo = write(&d, "p.txt", PROFILE);
    let json = d.join("s.json");
    ok(bin().args(["setup", "import"]).arg(&apo).args(["--layout", "stereo", "-o"]).arg(&json));
    let info = ok(bin().arg("info").arg(&json));
    assert!(info.contains("inputs:  L R"));
    assert!(info.contains("-6.0"), "Copy: L=0.5*L+0.5*R should show as -6 dB routes:\n{info}");
    assert!(info.contains("p (all)") && info.contains("p (R)"), "{info}");
    // info also works directly on the APO file
    assert!(ok(bin().arg("info").arg(&apo)).contains("outputs: L R"));
}

#[test]
fn setup_new_builds_identity_and_upmix_layouts() {
    let d = tmp("new");
    let json = d.join("s.json");
    ok(bin().args(["setup", "new", "--layout", "stereo", "--out-layout", "5.1", "-o"]).arg(&json));
    let info = ok(bin().arg("info").arg(&json));
    assert!(info.contains("outputs: L R C SUB RL RR"), "{info}");
    // same-named channels are routed straight through
    let text = std::fs::read_to_string(&json).unwrap();
    assert!(text.contains("\"matrix\""));
}

#[test]
fn bad_input_gives_a_clear_error_not_a_panic() {
    let d = tmp("bad");
    let bad = write(&d, "bad.json", "{ not json");
    let o = run(bin().arg("info").arg(&bad));
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("invalid setup file") && !err.contains("panicked"), "{err}");
    assert!(!run(bin().arg("info").arg(d.join("missing.txt"))).status.success());
    assert!(!run(bin().args(["response"]).arg(write(&d, "e.txt", "")).args(["-c", "SUB"])).status.success());
}

#[test]
fn process_renders_matrix_and_gain() {
    let d = tmp("process");
    let setup = write(
        &d,
        "s.json",
        r#"{ "input_channels": ["L","R"], "output_channels": ["L","R"],
             "matrix": [[0.0, 0.5], [1.0, 0.0]], "master_gain_db": 0.0, "limiter": false }"#,
    );
    let (input, output) = (d.join("in.wav"), d.join("out.wav"));
    wav(&input, 2, 4800, |_, c| if c == 0 { 0.4 } else { 0.2 });
    ok(bin().arg("process").arg(&setup).arg(&input).arg(&output));
    let (ch, s) = read(&output);
    assert_eq!(ch, 2);
    // out L = in R * 1.0 ; out R = in L * 0.5
    assert!((s[200] - 0.2).abs() < 1e-6 && (s[201] - 0.2).abs() < 1e-6, "{} {}", s[200], s[201]);
}

#[test]
fn process_can_upmix_stereo_to_5_1() {
    let d = tmp("upmix");
    let json = d.join("s.json");
    ok(bin().args(["setup", "new", "--layout", "stereo", "--out-layout", "5.1", "-o"]).arg(&json));
    let (input, output) = (d.join("in.wav"), d.join("out.wav"));
    wav(&input, 2, 2400, |_, c| 0.1 * (c as f32 + 1.0));
    ok(bin().arg("process").arg(&json).arg(&input).arg(&output));
    let (ch, s) = read(&output);
    assert_eq!(ch, 6);
    let frame = &s[600 * 6..601 * 6];
    assert!((frame[0] - 0.1).abs() < 1e-6 && (frame[1] - 0.2).abs() < 1e-6);
    assert!(frame[2..].iter().all(|v| *v == 0.0), "unrouted channels stay silent: {frame:?}");
}

#[test]
fn process_rejects_a_channel_count_mismatch() {
    let d = tmp("mismatch");
    let json = d.join("s.json");
    ok(bin().args(["setup", "new", "--layout", "5.1", "-o"]).arg(&json));
    let input = d.join("in.wav");
    wav(&input, 2, 100, |_, _| 0.0);
    let o = run(bin().arg("process").arg(&json).arg(&input).arg(d.join("o.wav")));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("expects 6 input channels"));
}

#[test]
fn gen_config_and_live_dry_run_agree() {
    let d = tmp("conf");
    let apo = write(&d, "p.txt", PROFILE);
    let cfg = ok(bin().arg("gen-config").arg(&apo).args(["--name", "Test EQ"]));
    assert!(cfg.contains("libpipewire-module-filter-chain") && cfg.contains("Test EQ"));
    assert!(cfg.contains("name = mix_l label = mixer"), "Copy: must become a matrix:\n{cfg}");

    // install into a scratch config dir without touching PipeWire, then diff live values
    let home = d.join("cfg");
    let install = |apo: &Path| {
        ok(bin().env("XDG_CONFIG_HOME", &home).arg("install").arg(apo).arg("--no-restart"));
    };
    install(&apo);
    let dry = |file: &Path| {
        let o = run(bin().env("XDG_CONFIG_HOME", &home).args(["live", "apply"]).arg(file).arg("--dry-run"));
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let (good, out, _) = dry(&apo);
    assert!(good);
    assert!(out.contains("mix_l:Gain 1 = 0.5") && out.contains("eq_l_1:b0 = "), "{out}");
    assert!(!out.contains("eq_l_1:b0 = 1.0\n"), "slot 1 carries the peaking filter: {out}");

    // value-only change: accepted
    let tweaked = write(&d, "p2.txt", &PROFILE.replace("Gain 4 dB", "Gain 2 dB"));
    let (good, out, _) = dry(&tweaked);
    assert!(good && !out.contains("eq_l_1:b0 = 1.0\n"));

    // an extra filter is still a live edit: there are spare slots
    let more = write(&d, "p3.txt", &format!("{PROFILE}Filter: ON PK Fc 200 Hz Gain 1 dB Q 1\n"));
    let (good, out, _) = dry(&more);
    assert!(good && !out.contains("eq_l_2:b0 = 1.0\n"), "{out}");

    // a different channel layout is structural: refused with a clear message
    let o = run(bin().env("XDG_CONFIG_HOME", &home).args(["live", "apply"]).arg(&tweaked).args(["--layout", "5.1", "--dry-run"]));
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("linapo install"));
}

#[test]
fn live_commands_fail_cleanly_without_a_running_pipewire() {
    let d = tmp("nolive");
    let json = d.join("s.json");
    ok(bin().args(["setup", "new", "-o"]).arg(&json));
    let o = run(bin().env("XDG_RUNTIME_DIR", d.join("nonexistent")).args(["live", "master"]).arg(&json).arg("-6"));
    assert!(!o.status.success());
    assert!(!String::from_utf8_lossy(&o.stderr).contains("panicked"));
}

// ---------------------------------------------------------------- profiles

fn with_config(dir: &Path) -> Command {
    let mut c = bin();
    c.env("XDG_CONFIG_HOME", dir.join("cfg")).env("XDG_RUNTIME_DIR", dir.join("run-missing"));
    c
}

#[test]
fn profile_lifecycle_from_the_command_line() {
    let d = tmp("profiles");
    let list = |d: &Path| ok(&mut { let mut c = with_config(d); c.args(["profile", "list"]); c });
    assert_eq!(list(&d).trim(), "* Default", "first run creates and activates a Default profile");

    let apo = write(&d, "cans.txt", "Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 4 dB Q 1\n");
    ok(with_config(&d).args(["profile", "save", "Headphones"]).arg(&apo));
    ok(with_config(&d).args(["profile", "new", "Room", "--layout", "5.1"]));
    assert!(!run(with_config(&d).args(["profile", "new", "Room"])).status.success(), "no silent overwrite");
    assert_eq!(list(&d).lines().count(), 3);

    ok(with_config(&d).args(["profile", "copy", "Headphones", "Cans copy"]));
    ok(with_config(&d).args(["profile", "rename", "Cans copy", "Cans"]));
    assert!(list(&d).contains("Cans"));
    assert!(!run(with_config(&d).args(["profile", "rename", "Cans", "Room"])).status.success());
    assert!(!run(with_config(&d).args(["profile", "new", "bad/name"])).status.success());

    let path = ok(with_config(&d).args(["profile", "path", "Headphones"]));
    let info = ok(bin().arg("info").arg(path.trim()));
    assert!(info.contains("1 filter(s)") && info.contains("-3.00 dB"), "{info}");

    ok(with_config(&d).args(["profile", "delete", "Cans"]));
    assert_eq!(list(&d).lines().count(), 3);
    assert!(!run(with_config(&d).args(["profile", "delete", "ghost"])).status.success());
}

#[test]
fn using_a_profile_when_nothing_is_installed_activates_it_and_says_so() {
    let d = tmp("use");
    ok(with_config(&d).args(["profile", "new", "Other"]));
    let out = ok(with_config(&d).args(["profile", "use", "Other"]));
    assert!(out.contains("now the active profile") && out.contains("not installed"), "{out}");
    assert_eq!(ok(with_config(&d).args(["profile", "active"])).trim(), "Other");
    assert!(!run(with_config(&d).args(["profile", "use", "ghost"])).status.success());
    assert_eq!(ok(with_config(&d).args(["profile", "active"])).trim(), "Other", "a failed switch changes nothing");
}

#[test]
fn bindings_commands_edit_the_state() {
    let d = tmp("bindings");
    ok(with_config(&d).args(["profile", "new", "Cans"]));
    ok(with_config(&d).args(["bindings", "set", "alsa_output.usb", "Cans"]));
    assert!(!run(with_config(&d).args(["bindings", "set", "dev", "ghost"])).status.success());
    ok(with_config(&d).args(["bindings", "default", "Default"]));
    ok(with_config(&d).args(["bindings", "auto", "on"]));
    let l = ok(with_config(&d).args(["bindings", "list"]));
    assert!(l.contains("automatic switching: on") && l.contains("alsa_output.usb -> Cans") && l.contains("default profile:     Default"), "{l}");
    assert!(!run(with_config(&d).args(["bindings", "auto", "maybe"])).status.success());
    ok(with_config(&d).args(["bindings", "remove", "alsa_output.usb"]));
    ok(with_config(&d).args(["bindings", "default", "none"]));
    let l = ok(with_config(&d).args(["bindings", "list"]));
    assert!(l.contains("no devices are bound") && l.contains("(leave the profile alone)"), "{l}");
    // deleting a profile removes its bindings
    ok(with_config(&d).args(["bindings", "set", "x", "Cans"]));
    ok(with_config(&d).args(["profile", "delete", "Cans"]));
    assert!(ok(with_config(&d).args(["bindings", "list"])).contains("no devices are bound"));
}

#[test]
fn device_and_watch_fail_cleanly_without_pipewire() {
    let d = tmp("nodev");
    let o = run(with_config(&d).arg("device"));
    assert!(!o.status.success() && !String::from_utf8_lossy(&o.stderr).contains("panicked"));
    assert!(!run(with_config(&d).args(["watch", "--interval", "0"])).status.success());
    let s = ok(with_config(&d).args(["autostart", "status"]));
    assert!(s.contains("unit file:") && s.contains("absent"), "{s}");
}

#[test]
fn a_legacy_setup_json_becomes_the_default_profile() {
    let d = tmp("migrate");
    std::fs::create_dir_all(d.join("cfg/linapo")).unwrap();
    std::fs::write(d.join("cfg/linapo/setup.json"), r#"{"input_channels":["L","R"],"output_channels":["L","R"],"matrix":[[1,0],[0,1]],"master_gain_db":-6}"#).unwrap();
    let path = ok(with_config(&d).args(["profile", "path", "Default"]));
    let info = ok(bin().arg("info").arg(path.trim()));
    assert!(info.contains("master gain: -6.00 dB"), "{info}");
}
