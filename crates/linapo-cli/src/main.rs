use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use linapo_core::matrix::lin_to_db;
use linapo_core::matrix::db_to_lin;
use linapo_core::pipewire_conf::{generate, live_controls, master_control, matrix_control, Options};
use linapo_core::profiles::Workspace;
use linapo_core::{layout_for, parse_file, response_db, Channel, Engine, Setup};

#[derive(Parser)]
#[command(name = "linapo", version, about = "Equalizer APO-style channel matrix + EQ for PipeWire")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Where a setup comes from: a `.json` setup file, or an Equalizer APO profile.
#[derive(Args)]
struct Source {
    /// A setup (.json) or an Equalizer APO profile (any other extension).
    file: PathBuf,
    /// Channel layout for APO profiles: mono, stereo, quad, 5.1, 7.1 or a number. Ignored for .json.
    #[arg(long, default_value = "stereo")]
    layout: String,
}

#[derive(Args)]
struct PwOpts {
    /// Name shown in sound settings.
    #[arg(long, default_value = "LinAPO EQ")]
    name: String,
    /// Sample rate used for the few coefficients PipeWire cannot compute itself.
    #[arg(long, default_value_t = 48000.0)]
    rate: f64,
    /// Send the processed audio to this PipeWire node name instead of the default output.
    #[arg(long)]
    target: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what a setup or profile contains (and what could not be understood).
    Info(Source),

    /// Print the frequency response of one output channel as CSV (freq_hz,gain_db).
    Response {
        #[command(flatten)]
        src: Source,
        #[arg(short, long, default_value = "L")]
        channel: String,
        #[arg(long, default_value_t = 48000.0)]
        rate: f64,
        #[arg(long, default_value_t = 100)]
        points: usize,
    },

    /// Render a WAV file through the full engine (matrix, EQ, master, limiter) and write a 32-bit float WAV.
    Process {
        #[command(flatten)]
        src: Source,
        input: PathBuf,
        output: PathBuf,
    },

    /// Create or convert setup files.
    #[command(subcommand)]
    Setup(SetupCmd),

    /// Print or write the PipeWire filter-chain config.
    GenConfig {
        #[command(flatten)]
        src: Source,
        #[command(flatten)]
        pw: PwOpts,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Install as a PipeWire virtual sink and restart PipeWire (briefly interrupts all audio).
    Install {
        #[command(flatten)]
        src: Source,
        #[command(flatten)]
        pw: PwOpts,
        /// Write the config but do not restart PipeWire.
        #[arg(long)]
        no_restart: bool,
    },

    /// Remove the installed config and restart PipeWire.
    Uninstall {
        #[arg(long)]
        no_restart: bool,
    },

    /// Change values of the running virtual sink without restarting PipeWire.
    #[command(subcommand)]
    Live(LiveCmd),

    /// Named profiles: save, switch, copy, delete.
    #[command(subcommand)]
    Profile(ProfileCmd),

    /// Which profile goes with which output device, and automatic switching.
    #[command(subcommand)]
    Bindings(BindingsCmd),

    /// Show the output device LinAPO is currently playing to.
    Device,

    /// Switch profiles automatically when the output device changes (runs until stopped).
    Watch {
        /// Seconds between checks.
        #[arg(long, default_value_t = 0.5)]
        interval: f64,
    },

    /// Start `linapo watch` automatically at login (systemd user service).
    #[command(subcommand)]
    Autostart(AutostartCmd),
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List profiles; the active one is marked with *.
    List,
    /// Print the name of the active profile.
    Active,
    /// Print the file a profile is stored in.
    Path { name: String },
    /// Create a pass-through profile.
    New {
        name: String,
        #[arg(long, default_value = "stereo")]
        layout: String,
    },
    /// Store a setup (.json) or an Equalizer APO profile (.txt) as a profile, overwriting it.
    Save {
        name: String,
        #[command(flatten)]
        src: Source,
    },
    /// Make a profile the active one and apply it live. Fails if that needs a restart, unless --install.
    Use {
        name: String,
        /// Reinstall (restarting PipeWire) if the profile needs more filter slots or other channels.
        #[arg(long)]
        install: bool,
    },
    Copy { from: String, to: String },
    Rename { from: String, to: String },
    Delete { name: String },
}

#[derive(Subcommand)]
enum BindingsCmd {
    /// Show bindings, the default profile, and whether automatic switching is on.
    List,
    /// Use a profile whenever this output device (PipeWire node name) is in use.
    Set { device: String, profile: String },
    /// Bind the output device that is in use right now.
    Here { profile: String },
    Remove { device: String },
    /// Profile for devices without a binding ("none" to leave the profile alone).
    Default { profile: String },
    /// Turn automatic switching "on" or "off".
    Auto { state: String },
}

#[derive(Subcommand)]
enum AutostartCmd {
    /// Install and start the user service.
    Enable,
    Disable,
    Status,
}

#[derive(Subcommand)]
enum LiveCmd {
    /// Push every adjustable value (matrix, preamps, filter parameters, master) of a
    /// setup to the running sink. Fails if the structure differs from what is installed.
    Apply {
        #[command(flatten)]
        src: Source,
        #[command(flatten)]
        pw: PwOpts,
        /// Print the values that would be sent instead of sending them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Set the master gain in dB.
    Master {
        /// Setup file (.json)
        setup: PathBuf,
        #[arg(allow_negative_numbers = true)]
        db: f64,
        /// Also write the new value into the setup file.
        #[arg(long)]
        save: bool,
    },
    /// Switch every EQ off (A/B comparison) or back on. The matrix and master gain still apply.
    Bypass {
        /// Setup file (.json)
        setup: PathBuf,
        /// "on" bypasses the EQs, "off" restores them
        state: String,
        #[command(flatten)]
        pw: PwOpts,
        /// Also write the new state into the setup file.
        #[arg(long)]
        save: bool,
    },
    /// Set one matrix route: gain in dB, or "off".
    Route {
        /// Setup file (.json)
        setup: PathBuf,
        /// Input channel, e.g. L
        input: String,
        /// Output channel, e.g. SUB
        output: String,
        #[arg(allow_hyphen_values = true)]
        gain: String,
        /// Also write the new value into the setup file.
        #[arg(long)]
        save: bool,
    },
}

#[derive(Subcommand)]
enum SetupCmd {
    /// Write a pass-through setup (identity matrix, no EQ).
    New {
        #[arg(long, default_value = "stereo")]
        layout: String,
        /// Output layout if different from the input layout (e.g. --layout stereo --out-layout 5.1).
        #[arg(long)]
        out_layout: Option<String>,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Convert an Equalizer APO profile (Copy: -> matrix, Channel: -> EQ assignment) into a setup file.
    Import {
        profile: PathBuf,
        #[arg(long, default_value = "stereo")]
        layout: String,
        /// Name used for the imported EQ presets (default: the file name).
        #[arg(long)]
        name: Option<String>,
        #[arg(short, long)]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    // Behave like other command line tools when piped into `head` and friends.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match Cli::parse().cmd {
        Cmd::Info(src) => info(&src),
        Cmd::Response { src, channel, rate, points } => response(&src, &channel, rate, points),
        Cmd::Process { src, input, output } => process(&src, &input, &output),
        Cmd::Setup(SetupCmd::New { layout, out_layout, output }) => {
            let ins = parse_layout(&layout)?;
            let outs = match out_layout {
                Some(l) => parse_layout(&l)?,
                None => ins.clone(),
            };
            let mut s = Setup::passthrough(&ins);
            s.output_channels = outs;
            s.matrix = linapo_core::Matrix::zeros(ins.len(), s.output_channels.len());
            for (i, c) in ins.iter().enumerate() {
                if let Some(o) = s.output_channels.iter().position(|x| x == c) {
                    s.matrix.set(i, o, 1.0); // route same-named channels straight through
                }
            }
            s.save(&output)?;
            println!("wrote {}", output.display());
            Ok(())
        }
        Cmd::Setup(SetupCmd::Import { profile, layout, name, output }) => {
            let (s, warnings) = setup_from_apo(&profile, &layout, name.as_deref())?;
            warnings.iter().for_each(|w| eprintln!("warning: {w}"));
            s.save(&output)?;
            println!("wrote {}", output.display());
            Ok(())
        }
        Cmd::GenConfig { src, pw, output } => {
            let text = config_text(&src, &pw)?;
            match output {
                Some(p) => std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?,
                None => print!("{text}"),
            }
            Ok(())
        }
        Cmd::Install { src, pw, no_restart } => install(&src, &pw, no_restart),
        Cmd::Uninstall { no_restart } => uninstall(no_restart),
        Cmd::Live(l) => live(l),
        Cmd::Profile(c) => profile(c),
        Cmd::Bindings(c) => bindings(c),
        Cmd::Device => device(),
        Cmd::Watch { interval } => watch(interval),
        Cmd::Autostart(c) => autostart(c),
    }
}

fn parse_layout(s: &str) -> Result<Vec<Channel>> {
    let n = match s.to_ascii_lowercase().as_str() {
        "mono" => 1,
        "stereo" | "2.0" => 2,
        "quad" => 4,
        "5.1" => 6,
        "7.1" => 8,
        other => other.parse::<usize>().with_context(|| format!("unknown layout '{s}' (try stereo, 5.1, 7.1 or a number)"))?,
    };
    if n == 0 {
        bail!("layout needs at least one channel");
    }
    Ok(layout_for(n))
}

fn setup_from_apo(path: &Path, layout: &str, name: Option<&str>) -> Result<(Setup, Vec<String>)> {
    let profile = parse_file(path).with_context(|| format!("reading {}", path.display()))?;
    for d in &profile.diagnostics {
        eprintln!("warning: {d}");
    }
    let default_name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "profile".into());
    Ok(Setup::from_profile(&profile, &parse_layout(layout)?, name.unwrap_or(&default_name)))
}

fn load(src: &Source) -> Result<Setup> {
    if src.file.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        return Ok(Setup::load(&src.file).with_context(|| format!("loading {}", src.file.display()))?);
    }
    let (s, warnings) = setup_from_apo(&src.file, &src.layout, None)?;
    warnings.iter().for_each(|w| eprintln!("warning: {w}"));
    Ok(s)
}

fn parse_channel(s: &str) -> Result<Channel> {
    Channel::parse(s).with_context(|| format!("unknown channel '{s}'"))
}

fn info(src: &Source) -> Result<()> {
    let s = load(src)?;
    let names = |l: &[Channel]| l.iter().map(|c| c.name()).collect::<Vec<_>>().join(" ");
    println!("inputs:  {}", names(&s.input_channels));
    println!("outputs: {}", names(&s.output_channels));
    println!(
        "master gain: {:+.2} dB, limiter: {}{}",
        s.master_gain_db,
        if s.limiter { "on" } else { "off" },
        if s.bypass_eq { ", EQ BYPASSED" } else { "" }
    );

    println!("\nmatrix (rows = inputs, columns = outputs, gain in dB, '.' = not routed)");
    print!("{:>6}", "");
    for o in &s.output_channels {
        print!("{:>8}", o.name());
    }
    println!();
    for (i, ic) in s.input_channels.iter().enumerate() {
        print!("{:>6}", ic.name());
        for o in 0..s.output_channels.len() {
            let g = s.matrix.get(i, o);
            match lin_to_db(g) {
                None => print!("{:>8}", "."),
                Some(db) => print!("{:>8}", format!("{}{:.1}", if g < 0.0 { "-" } else { "" }, db)),
            }
        }
        println!();
    }

    println!("\nEQs");
    if s.eqs.is_empty() {
        println!("  (none)");
    }
    for eq in &s.eqs {
        println!(
            "  {}{}: channels [{}], preamp {:+.2} dB, {} filter(s)",
            eq.name,
            if eq.enabled { "" } else { " (disabled)" },
            names(&eq.channels),
            eq.preamp_db,
            eq.filters.len()
        );
        const SHOWN: usize = 12;
        for (i, f) in eq.filters.iter().enumerate().take(SHOWN) {
            println!("      {:>2}. {f}", i + 1);
        }
        if eq.filters.len() > SHOWN {
            println!("      ... and {} more", eq.filters.len() - SHOWN);
        }
    }
    Ok(())
}

fn response(src: &Source, channel: &str, rate: f64, points: usize) -> Result<()> {
    if points < 2 {
        bail!("--points must be at least 2");
    }
    let s = load(src)?;
    let ch = parse_channel(channel)?;
    if !s.output_channels.contains(&ch) {
        bail!("channel {ch} is not an output channel of this setup");
    }
    let chain = s.chain_for_output(&ch);
    println!("freq_hz,gain_db");
    let (lo, hi) = (20f64.ln(), 20_000f64.ln());
    for i in 0..points {
        let f = (lo + (hi - lo) * i as f64 / (points - 1) as f64).exp();
        println!("{f:.2},{:.3}", response_db(&chain, rate, f));
    }
    Ok(())
}

fn process(src: &Source, input: &Path, output: &Path) -> Result<()> {
    let mut reader = hound::WavReader::open(input).with_context(|| format!("opening {}", input.display()))?;
    let spec = reader.spec();
    let n_ch = spec.channels as usize;
    let setup = if src.file.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        load(src)?
    } else {
        // For APO profiles the layout follows the WAV file.
        load(&Source { file: src.file.clone(), layout: n_ch.to_string() })?
    };
    if setup.input_channels.len() != n_ch {
        bail!("setup expects {} input channels but {} has {}", setup.input_channels.len(), input.display(), n_ch);
    }

    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => reader.samples::<f32>().collect::<Result<_, _>>()?,
        (hound::SampleFormat::Int, bits @ 8..=32) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
        (fmt, bits) => bail!("unsupported WAV format: {fmt:?} {bits}-bit"),
    };
    let frames = samples.len() / n_ch;
    let planar_in: Vec<Vec<f32>> = (0..n_ch).map(|c| (0..frames).map(|f| samples[f * n_ch + c]).collect()).collect();

    let (mut engine, warnings) = Engine::new(&setup, spec.sample_rate as f64).map_err(anyhow::Error::msg)?;
    warnings.iter().for_each(|w| eprintln!("warning: {w}"));
    let n_out = engine.n_out();
    let mut planar_out = vec![vec![0f32; frames]; n_out];
    const BLOCK: usize = 1024; // like a real-time callback
    let mut pos = 0;
    while pos < frames {
        let end = (pos + BLOCK).min(frames);
        let ins: Vec<&[f32]> = planar_in.iter().map(|v| &v[pos..end]).collect();
        let mut outs: Vec<&mut [f32]> = planar_out.iter_mut().map(|v| &mut v[pos..end]).collect();
        engine.process(&ins, &mut outs);
        pos = end;
    }

    let out_spec = hound::WavSpec {
        channels: n_out as u16,
        sample_rate: spec.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(output, out_spec)?;
    let mut peak = 0f32;
    for f in 0..frames {
        for ch in &planar_out {
            peak = peak.max(ch[f].abs());
            writer.write_sample(ch[f])?;
        }
    }
    writer.finalize()?;
    eprintln!("wrote {} ({} frames, {} channels, peak {:.3})", output.display(), frames, n_out, peak);
    if peak > 1.0 {
        eprintln!("warning: output exceeds 0 dBFS; enable the limiter or lower the gain");
    }
    Ok(())
}

fn config_text(src: &Source, pw: &PwOpts) -> Result<String> {
    let setup = load(src)?;
    let (text, warnings) = generate(&setup, &pw_options(pw)).map_err(anyhow::Error::msg)?;
    warnings.iter().for_each(|w| eprintln!("warning: {w}"));
    Ok(text)
}

fn pw_options(pw: &PwOpts) -> Options {
    Options { description: pw.name.clone(), sample_rate: pw.rate, target: pw.target.clone(), ..Options::default() }
}

fn install(src: &Source, pw: &PwOpts, no_restart: bool) -> Result<()> {
    let setup = load(src)?;
    // Size the graph for every stored profile, so switching between them stays live.
    let store = linapo_core::profiles::ProfileStore::new(linapo_pw::config_root()?.join("profiles"));
    let warnings = linapo_pw::install_sized(&setup, &pw_options(pw), &store, !no_restart)?;
    warnings.iter().for_each(|w| eprintln!("warning: {w}"));
    println!("wrote {}", linapo_pw::conf_path()?.display());
    if no_restart {
        println!("restart PipeWire to activate: systemctl --user restart pipewire wireplumber pipewire-pulse");
    } else {
        println!("done. Select \"{}\" as the output device in your sound settings.", pw.name);
    }
    Ok(())
}

fn uninstall(no_restart: bool) -> Result<()> {
    if linapo_pw::uninstall(!no_restart)? {
        println!("removed {}", linapo_pw::conf_path()?.display());
    } else {
        println!("nothing to remove ({} does not exist)", linapo_pw::conf_path()?.display());
    }
    Ok(())
}

// ------------------------------------------------------------------ live

fn send(controls: &[(linapo_core::pipewire_conf::ControlKey, f64)]) -> Result<()> {
    let n = linapo_pw::send_controls(controls)?;
    println!("updated {n} value(s)");
    Ok(())
}

fn live(cmd: LiveCmd) -> Result<()> {
    match cmd {
        LiveCmd::Apply { src, pw, dry_run } => {
            let setup = load(&src)?;
            let opts = pw_options(&pw);
            if dry_run {
                // Same structure check as a real apply, but nothing is sent.
                match linapo_pw::structure_matches(&setup, &opts)? {
                    None => bail!("not installed; run `linapo install` first"),
                    Some(false) => bail!(
                        "the structure changed (channel layout, output device, or more filters than the installed \
                         graph has slots for), so a restart is needed: run `linapo install`"
                    ),
                    Some(true) => {}
                }
                let opts = Options { slots: linapo_pw::installed_slot_count(), ..opts };
                for (k, v) in live_controls(&setup, &opts).map_err(anyhow::Error::msg)? {
                    println!("{k} = {v:?}");
                }
                return Ok(());
            }
            match linapo_pw::apply_live(&setup, &opts)? {
                linapo_pw::Applied::Live { values } => {
                    println!("updated {values} value(s)");
                    Ok(())
                }
                linapo_pw::Applied::NotInstalled => bail!("not installed; run `linapo install` first"),
                linapo_pw::Applied::NeedsRestart => bail!(
                    "the structure changed (channel layout, output device, or more filters than the installed \
                     graph has slots for), so a restart is needed: run `linapo install`"
                ),
            }
        }
        LiveCmd::Master { setup, db, save } => {
            let mut s = Setup::load(&setup).with_context(|| format!("loading {}", setup.display()))?;
            s.master_gain_db = db;
            let lin = db_to_lin(db);
            let controls: Vec<_> = s.output_channels.iter().map(|c| (master_control(c), lin)).collect();
            send(&controls)?;
            if save {
                s.save(&setup)?;
            }
            Ok(())
        }
        LiveCmd::Bypass { setup, state, pw, save } => {
            let mut s = Setup::load(&setup).with_context(|| format!("loading {}", setup.display()))?;
            s.bypass_eq = match state.to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" => true,
                "off" | "false" | "no" => false,
                other => bail!("expected 'on' or 'off', got '{other}'"),
            };
            match linapo_pw::apply_live(&s, &pw_options(&pw))? {
                linapo_pw::Applied::Live { values } => println!("EQ bypass {}: updated {values} value(s)", if s.bypass_eq { "on" } else { "off" }),
                linapo_pw::Applied::NotInstalled => bail!("not installed; run `linapo install` first"),
                linapo_pw::Applied::NeedsRestart => bail!("the installed structure differs from {}; run `linapo install` first", setup.display()),
            }
            if save {
                s.save(&setup)?;
            }
            Ok(())
        }
        LiveCmd::Route { setup, input, output, gain, save } => {
            let mut s = Setup::load(&setup).with_context(|| format!("loading {}", setup.display()))?;
            let (inp, outp) = (parse_channel(&input)?, parse_channel(&output)?);
            let i = s.input_channels.iter().position(|c| *c == inp).with_context(|| format!("{inp} is not an input channel"))?;
            let o = s.output_channels.iter().position(|c| *c == outp).with_context(|| format!("{outp} is not an output channel"))?;
            let lin = if gain.eq_ignore_ascii_case("off") {
                0.0
            } else {
                db_to_lin(gain.parse::<f64>().with_context(|| format!("gain must be a number of dB or 'off', got '{gain}'"))?)
            };
            s.matrix.set(i, o, lin);
            send(&[(matrix_control(&outp, i), lin)])?;
            if save {
                s.save(&setup)?;
            }
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ profiles

fn workspace() -> Result<Workspace> {
    Workspace::open(&linapo_pw::config_root()?).context("opening the profile store")
}

fn profile(cmd: ProfileCmd) -> Result<()> {
    let mut ws = workspace()?;
    match cmd {
        ProfileCmd::List => {
            let active = ws.active_name().to_string();
            for n in ws.store.list()? {
                println!("{} {n}", if n == active { "*" } else { " " });
            }
        }
        ProfileCmd::Active => println!("{}", ws.active_name()),
        ProfileCmd::Path { name } => println!("{}", ws.store.path_of(&name).display()),
        ProfileCmd::New { name, layout } => {
            let name = linapo_core::profiles::valid_name(&name).map_err(anyhow::Error::msg)?;
            if ws.store.exists(&name) {
                bail!("a profile named '{name}' already exists");
            }
            ws.store.save(&name, &Setup::passthrough(&parse_layout(&layout)?))?;
            println!("created profile '{name}'");
        }
        ProfileCmd::Save { name, src } => {
            let setup = load(&src)?;
            ws.store.save(&name, &setup)?;
            println!("saved profile '{name}'");
        }
        ProfileCmd::Use { name, install } => {
            let opts = Options::default();
            match linapo_pw::apply_profile(&mut ws, &name, &opts, install) {
                Ok(linapo_pw::ProfileSwitch::Live) => println!("switched to '{name}' (live)"),
                Ok(linapo_pw::ProfileSwitch::Reinstalled) => println!("switched to '{name}' (reinstalled, PipeWire restarted)"),
                Ok(linapo_pw::ProfileSwitch::NotInstalled) => {
                    println!("'{name}' is now the active profile, but LinAPO is not installed in PipeWire yet: run `linapo install`")
                }
                Ok(linapo_pw::ProfileSwitch::NeedsRestart) => {
                    bail!("'{name}' needs a restart (more filter slots or other channels than installed): run again with --install")
                }
                Err(e) => bail!("'{name}' is now the active profile, but applying it failed: {e:#}"),
            }
        }
        ProfileCmd::Copy { from, to } => {
            ws.store.duplicate(&from, &to).map_err(anyhow::Error::msg)?;
            println!("copied '{from}' to '{to}'");
        }
        ProfileCmd::Rename { from, to } => {
            ws.rename(&from, &to).map_err(anyhow::Error::msg)?;
            println!("renamed '{from}' to '{to}'");
        }
        ProfileCmd::Delete { name } => {
            let now = ws.delete(&name).map_err(anyhow::Error::msg)?;
            println!("deleted '{name}'; active profile is '{now}'");
        }
    }
    Ok(())
}

fn bindings(cmd: BindingsCmd) -> Result<()> {
    let mut ws = workspace()?;
    match cmd {
        BindingsCmd::List => {
            println!("automatic switching: {}", if ws.state.auto_switch { "on" } else { "off" });
            println!("default profile:     {}", ws.state.default_profile.as_deref().unwrap_or("(leave the profile alone)"));
            if ws.state.bindings.is_empty() {
                println!("no devices are bound to profiles");
            }
            for (device, profile) in &ws.state.bindings {
                println!("{device} -> {profile}");
            }
            if let Ok(Some(d)) = linapo_pw::current_output(&Options::default()) {
                println!("\noutput device now: {} ({})", d.node_name, d.description);
            }
        }
        BindingsCmd::Set { device, profile } => {
            ws.bind(&device, &profile).map_err(anyhow::Error::msg)?;
            println!("{device} -> {profile}");
        }
        BindingsCmd::Here { profile } => {
            let d = linapo_pw::current_output(&Options::default())?.context("LinAPO is not playing to any device right now")?;
            ws.bind(&d.node_name, &profile).map_err(anyhow::Error::msg)?;
            println!("{} ({}) -> {profile}", d.node_name, d.description);
        }
        BindingsCmd::Remove { device } => {
            if ws.unbind(&device).map_err(anyhow::Error::msg)? {
                println!("removed the binding for {device}");
            } else {
                println!("{device} had no binding");
            }
        }
        BindingsCmd::Default { profile } => {
            let p = (!profile.eq_ignore_ascii_case("none")).then_some(profile.as_str());
            ws.set_default_profile(p).map_err(anyhow::Error::msg)?;
            println!("default profile: {}", p.unwrap_or("(none)"));
        }
        BindingsCmd::Auto { state } => {
            let on = match state.to_ascii_lowercase().as_str() {
                "on" | "true" | "yes" => true,
                "off" | "false" | "no" => false,
                other => bail!("expected 'on' or 'off', got '{other}'"),
            };
            ws.set_auto_switch(on).map_err(anyhow::Error::msg)?;
            println!("automatic switching is {}", if on { "on" } else { "off" });
        }
    }
    Ok(())
}

fn device() -> Result<()> {
    match linapo_pw::current_output(&Options::default())? {
        Some(d) => println!("{} ({})", d.node_name, d.description),
        None => bail!("LinAPO is not playing to any device right now (is it installed and running?)"),
    }
    Ok(())
}

fn watch(interval: f64) -> Result<()> {
    if !(interval >= 0.1) {
        bail!("--interval must be at least 0.1 seconds");
    }
    // Only one process switches profiles. If a GUI window holds the lock, wait for it to close
    // (so a systemd service started at login does not fail and restart in a loop).
    let _lock = loop {
        if let Some(l) = linapo_pw::WatchLock::try_acquire() {
            break l;
        }
        eprintln!("another linapo watcher (a GUI window, probably) is switching profiles; waiting for it to finish");
        std::thread::sleep(std::time::Duration::from_secs(5));
    };
    let mut w = linapo_pw::Watcher::new(workspace()?, Options::default());
    eprintln!("watching the output device (every {interval} s); Ctrl+C to stop");
    let mut last_error = String::new();
    loop {
        match w.tick() {
            Ok(Some(note)) => println!("{note}"),
            Ok(None) => last_error.clear(),
            Err(e) => {
                // Report a given problem once instead of every second.
                let msg = format!("{e:#}");
                if msg != last_error {
                    eprintln!("warning: {msg}");
                    last_error = msg;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(interval));
    }
}

fn autostart(cmd: AutostartCmd) -> Result<()> {
    let unit_path = linapo_pw::systemd_unit_path()?;
    let systemctl = |args: &[&str]| -> Result<()> {
        let st = std::process::Command::new("systemctl").arg("--user").args(args).status().context("running systemctl")?;
        if !st.success() {
            bail!("systemctl --user {} failed ({st})", args.join(" "));
        }
        Ok(())
    };
    match cmd {
        AutostartCmd::Enable => {
            let exe = std::env::current_exe().context("finding this program")?;
            if let Some(dir) = unit_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&unit_path, linapo_pw::systemd_unit(&exe))?;
            println!("wrote {}", unit_path.display());
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", "--now", "linapo-watch.service"])?;
            println!("enabled: `linapo watch` now starts at login");
        }
        AutostartCmd::Disable => {
            let _ = systemctl(&["disable", "--now", "linapo-watch.service"]);
            if unit_path.exists() {
                std::fs::remove_file(&unit_path)?;
                let _ = systemctl(&["daemon-reload"]);
                println!("removed {}", unit_path.display());
            } else {
                println!("it was not enabled");
            }
        }
        AutostartCmd::Status => {
            println!("unit file: {} ({})", unit_path.display(), if unit_path.exists() { "present" } else { "absent" });
            let active = std::process::Command::new("systemctl").args(["--user", "is-active", "linapo-watch.service"]).output();
            match active {
                Ok(o) => println!("service: {}", String::from_utf8_lossy(&o.stdout).trim()),
                Err(_) => println!("service: systemctl is not available"),
            }
        }
    }
    Ok(())
}
