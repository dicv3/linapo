//! Talks to PipeWire on a background thread so the UI never blocks on `pw-cli`.
//!
//! The UI sends [`Job`]s; consecutive `Apply` jobs are coalesced so dragging a slider
//! sends only the latest value instead of a backlog.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub use linapo_pw::{Applied, Sink, Status};

use linapo_core::pipewire_conf::Options;
use linapo_core::profiles::ProfileStore;
use linapo_core::Setup;

pub trait Backend: Send + 'static {
    fn status(&mut self, setup: &Setup) -> Result<Status, String>;
    fn apply(&mut self, setup: &Setup) -> Result<Applied, String>;
    /// Write the config and restart PipeWire. Returns warnings.
    fn install(&mut self, setup: &Setup) -> Result<Vec<String>, String>;
    fn uninstall(&mut self) -> Result<bool, String>;
    fn list_sinks(&mut self) -> Result<Vec<Sink>, String>;
    /// The real device LinAPO is playing to right now.
    fn current_output(&mut self) -> Result<Option<Sink>, String>;
}

/// The real thing: shells out through `linapo-pw`.
pub struct PwBackend {
    pub opts: Options,
    /// When set, installing sizes the graph for every stored profile, so switching stays live.
    pub store: Option<ProfileStore>,
}

impl Default for PwBackend {
    fn default() -> Self {
        PwBackend { opts: Options::default(), store: None }
    }
}

fn e(err: anyhow::Error) -> String {
    format!("{err:#}")
}

impl Backend for PwBackend {
    fn status(&mut self, setup: &Setup) -> Result<Status, String> {
        linapo_pw::status(setup, &self.opts).map_err(e)
    }
    fn apply(&mut self, setup: &Setup) -> Result<Applied, String> {
        linapo_pw::apply_live(setup, &self.opts).map_err(e)
    }
    fn install(&mut self, setup: &Setup) -> Result<Vec<String>, String> {
        match &self.store {
            Some(store) => linapo_pw::install_sized(setup, &self.opts, store, true).map_err(e),
            None => linapo_pw::install(setup, &self.opts, true).map_err(e),
        }
    }
    fn uninstall(&mut self) -> Result<bool, String> {
        linapo_pw::uninstall(true).map_err(e)
    }
    fn list_sinks(&mut self) -> Result<Vec<Sink>, String> {
        linapo_pw::list_sinks().map_err(e)
    }
    fn current_output(&mut self) -> Result<Option<Sink>, String> {
        linapo_pw::current_output(&self.opts).map_err(e)
    }
}

#[derive(Debug, Clone)]
pub enum Job {
    /// Push values live (never restarts anything).
    Apply(Setup),
    /// Re-check status without changing anything.
    Refresh(Setup),
    /// Write config and restart PipeWire.
    Install(Setup),
    Uninstall,
    ListSinks,
    /// Ask which real device LinAPO is playing to.
    PollDevice,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Status(Status),
    Message(String),
    Error(String),
    Sinks(Vec<Sink>),
    /// The device LinAPO is playing to (`None`: unknown or not running).
    Device(Option<Sink>),
    /// A slow job (restart) started / finished.
    Busy(bool),
}

pub struct Worker {
    tx: Sender<Job>,
    rx: Receiver<Event>,
}

/// Collapse runs of identical job kinds: only the last of consecutive `Apply`s (or `Refresh`es) matters.
fn coalesce(jobs: Vec<Job>) -> Vec<Job> {
    let mut out: Vec<Job> = Vec::new();
    for j in jobs {
        let same_kind = matches!(
            (out.last(), &j),
            (Some(Job::Apply(_)), Job::Apply(_)) | (Some(Job::Refresh(_)), Job::Refresh(_))
        );
        if same_kind {
            out.pop();
        }
        out.push(j);
    }
    out
}

impl Worker {
    /// `wake` is called after every event so the UI can repaint.
    pub fn spawn(mut backend: impl Backend, wake: impl Fn() + Send + 'static) -> Worker {
        let (tx, jobs) = channel::<Job>();
        let (events, rx) = channel::<Event>();
        thread::spawn(move || {
            let send = |ev: Event| {
                let _ = events.send(ev);
                wake();
            };
            while let Ok(first) = jobs.recv() {
                let mut batch = vec![first];
                while let Ok(more) = jobs.try_recv() {
                    batch.push(more);
                }
                for job in coalesce(batch) {
                    run_job(&mut backend, job, &send);
                }
            }
        });
        Worker { tx, rx }
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }

    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }
}

fn status_event(r: Result<Status, String>) -> Event {
    match r {
        Ok(s) => Event::Status(s),
        Err(m) => Event::Error(m),
    }
}

fn run_job(b: &mut impl Backend, job: Job, send: &impl Fn(Event)) {
    match job {
        Job::Apply(setup) => match b.apply(&setup) {
            Ok(Applied::Live { .. }) => send(Event::Status(Status::Live)),
            Ok(Applied::NeedsRestart) => send(Event::Status(Status::NeedsRestart)),
            Ok(Applied::NotInstalled) => send(Event::Status(Status::NotInstalled)),
            Err(m) => {
                // Most often "node not found": installed but PipeWire has not picked it up.
                match b.status(&setup) {
                    Ok(Status::NotRunning) => send(Event::Status(Status::NotRunning)),
                    _ => send(Event::Error(m)),
                }
            }
        },
        Job::Refresh(setup) => send(status_event(b.status(&setup))),
        Job::Install(setup) => {
            send(Event::Busy(true));
            match b.install(&setup) {
                Ok(warnings) => {
                    warnings.into_iter().for_each(|w| send(Event::Message(format!("warning: {w}"))));
                    // PipeWire needs a moment to bring the sink back.
                    let deadline = Instant::now() + Duration::from_secs(8);
                    loop {
                        let st = b.status(&setup);
                        let done = matches!(st, Ok(Status::Live)) || Instant::now() >= deadline;
                        if done {
                            send(status_event(st));
                            break;
                        }
                        thread::sleep(Duration::from_millis(400));
                    }
                    send(Event::Message("Audio engine restarted".into()));
                }
                Err(m) => send(Event::Error(m)),
            }
            send(Event::Busy(false));
        }
        Job::Uninstall => {
            send(Event::Busy(true));
            match b.uninstall() {
                Ok(_) => {
                    send(Event::Status(Status::NotInstalled));
                    send(Event::Message("Removed from PipeWire".into()));
                }
                Err(m) => send(Event::Error(m)),
            }
            send(Event::Busy(false));
        }
        Job::ListSinks => match b.list_sinks() {
            Ok(s) => send(Event::Sinks(s)),
            // Not an error worth shouting about: usually PipeWire is simply not running.
            Err(m) => send(Event::Message(format!("Could not list output devices: {m}"))),
        },
        Job::PollDevice => send(Event::Device(b.current_output().ok().flatten())),
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records calls; `apply_result` / `status_result` decide what it answers.
    #[derive(Clone)]
    pub struct Fake {
        pub calls: Arc<Mutex<Vec<String>>>,
        pub apply_result: Arc<Mutex<Result<Applied, String>>>,
        pub status_result: Arc<Mutex<Result<Status, String>>>,
        /// If set, `apply` waits for a message on this channel before answering.
        pub gate: Arc<Mutex<Option<Receiver<()>>>>,
        /// What `current_output` answers.
        pub device: Arc<Mutex<Option<Sink>>>,
    }

    impl Fake {
        pub fn new() -> Fake {
            Fake {
                calls: Default::default(),
                apply_result: Arc::new(Mutex::new(Ok(Applied::Live { values: 1 }))),
                status_result: Arc::new(Mutex::new(Ok(Status::Live))),
                gate: Default::default(),
                device: Default::default(),
            }
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Backend for Fake {
        fn status(&mut self, _: &Setup) -> Result<Status, String> {
            self.calls.lock().unwrap().push("status".into());
            self.status_result.lock().unwrap().clone()
        }
        fn apply(&mut self, s: &Setup) -> Result<Applied, String> {
            if let Some(g) = self.gate.lock().unwrap().as_ref() {
                let _ = g.recv_timeout(Duration::from_secs(5));
            }
            self.calls.lock().unwrap().push(format!("apply master={}", s.master_gain_db));
            self.apply_result.lock().unwrap().clone()
        }
        fn install(&mut self, _: &Setup) -> Result<Vec<String>, String> {
            self.calls.lock().unwrap().push("install".into());
            Ok(vec![])
        }
        fn uninstall(&mut self) -> Result<bool, String> {
            self.calls.lock().unwrap().push("uninstall".into());
            Ok(true)
        }
        fn current_output(&mut self) -> Result<Option<Sink>, String> {
            Ok(self.device.lock().unwrap().clone())
        }
        fn list_sinks(&mut self) -> Result<Vec<Sink>, String> {
            self.calls.lock().unwrap().push("list_sinks".into());
            Ok(vec![Sink { node_name: "dev".into(), description: "Device".into() }])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;
    use linapo_core::apo::layout_for;
    use std::sync::mpsc::channel;

    fn setup(master: f64) -> Setup {
        let mut s = Setup::passthrough(&layout_for(2));
        s.master_gain_db = master;
        s
    }

    fn wait_for(w: &Worker, want: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut all = Vec::new();
        for _ in 0..200 {
            all.extend(w.poll());
            if all.iter().any(&want) {
                return all;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("event never arrived: {all:?}");
    }

    #[test]
    fn coalesce_keeps_last_of_consecutive_applies_only() {
        let jobs = vec![
            Job::Apply(setup(1.0)),
            Job::Apply(setup(2.0)),
            Job::Install(setup(3.0)),
            Job::Apply(setup(4.0)),
            Job::Apply(setup(5.0)),
            Job::Refresh(setup(6.0)),
            Job::Refresh(setup(7.0)),
        ];
        let out = coalesce(jobs);
        let kinds: Vec<String> = out
            .iter()
            .map(|j| match j {
                Job::Apply(s) => format!("apply{}", s.master_gain_db),
                Job::Install(_) => "install".into(),
                Job::Refresh(s) => format!("refresh{}", s.master_gain_db),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kinds, ["apply2", "install", "apply5", "refresh7"]);
    }

    #[test]
    fn a_burst_of_applies_while_busy_sends_only_the_first_and_last() {
        let fake = Fake::new();
        let (gate_tx, gate_rx) = channel();
        *fake.gate.lock().unwrap() = Some(gate_rx);
        let w = Worker::spawn(fake.clone(), || {});
        w.send(Job::Apply(setup(1.0)));
        thread::sleep(Duration::from_millis(100)); // worker is now blocked inside apply #1
        for i in 2..=20 {
            w.send(Job::Apply(setup(i as f64)));
        }
        for _ in 0..3 {
            let _ = gate_tx.send(());
        }
        wait_for(&w, |_| false.then_some(()).is_some() || fake.calls().len() >= 2);
        thread::sleep(Duration::from_millis(150));
        assert_eq!(fake.calls(), ["apply master=1", "apply master=20"]);
    }

    #[test]
    fn apply_outcomes_become_status_events() {
        let fake = Fake::new();
        let w = Worker::spawn(fake.clone(), || {});
        *fake.apply_result.lock().unwrap() = Ok(Applied::NeedsRestart);
        w.send(Job::Apply(setup(0.0)));
        wait_for(&w, |e| *e == Event::Status(Status::NeedsRestart));
        *fake.apply_result.lock().unwrap() = Ok(Applied::NotInstalled);
        w.send(Job::Apply(setup(0.0)));
        wait_for(&w, |e| *e == Event::Status(Status::NotInstalled));
        *fake.apply_result.lock().unwrap() = Ok(Applied::Live { values: 5 });
        w.send(Job::Apply(setup(0.0)));
        wait_for(&w, |e| *e == Event::Status(Status::Live));
    }

    #[test]
    fn a_failed_apply_reports_not_running_when_the_config_is_installed_but_the_node_is_missing() {
        let fake = Fake::new();
        *fake.apply_result.lock().unwrap() = Err("node not found".into());
        *fake.status_result.lock().unwrap() = Ok(Status::NotRunning);
        let w = Worker::spawn(fake.clone(), || {});
        w.send(Job::Apply(setup(0.0)));
        wait_for(&w, |e| *e == Event::Status(Status::NotRunning));

        *fake.status_result.lock().unwrap() = Ok(Status::Live);
        w.send(Job::Apply(setup(0.0)));
        wait_for(&w, |e| matches!(e, Event::Error(m) if m == "node not found"));
    }

    #[test]
    fn install_is_busy_then_reports_the_final_status() {
        let fake = Fake::new();
        let w = Worker::spawn(fake.clone(), || {});
        w.send(Job::Install(setup(0.0)));
        let ev = wait_for(&w, |e| *e == Event::Busy(false));
        assert_eq!(ev.first(), Some(&Event::Busy(true)));
        assert!(ev.contains(&Event::Status(Status::Live)));
        assert!(fake.calls().starts_with(&["install".to_string()]));
    }

    #[test]
    fn uninstall_and_sinks() {
        let fake = Fake::new();
        let w = Worker::spawn(fake.clone(), || {});
        w.send(Job::Uninstall);
        wait_for(&w, |e| *e == Event::Status(Status::NotInstalled));
        w.send(Job::ListSinks);
        let ev = wait_for(&w, |e| matches!(e, Event::Sinks(_)));
        assert!(ev.iter().any(|e| matches!(e, Event::Sinks(s) if s[0].node_name == "dev")));
    }

    #[test]
    fn polling_reports_the_current_device() {
        let fake = Fake::new();
        let w = Worker::spawn(fake.clone(), || {});
        w.send(Job::PollDevice);
        wait_for(&w, |e| *e == Event::Device(None));
        *fake.device.lock().unwrap() = Some(Sink { node_name: "usb".into(), description: "USB DAC".into() });
        w.send(Job::PollDevice);
        wait_for(&w, |e| matches!(e, Event::Device(Some(d)) if d.node_name == "usb"));
    }

    #[test]
    fn wake_is_called_for_events() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let w = Worker::spawn(Fake::new(), move || {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        w.send(Job::Refresh(setup(0.0)));
        wait_for(&w, |_| true);
        assert!(n.load(Ordering::SeqCst) >= 1);
    }
}
