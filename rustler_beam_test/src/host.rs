//! The half that runs in the test binary: it builds the NIF library, starts
//! the node, and asks it to run each test.

use std::collections::HashMap;
use std::env;
use std::ffi::CStr;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

use rustler::codegen_runtime::{inventory, NifModule};

/// What the `#[test]` function of a test knows at compile time.
pub struct Test {
    /// The name of the test NIF.
    pub name: &'static str,
    pub manifest_dir: &'static str,
}

/// A test passes, or fails with a report.
type Verdict = Result<(), String>;

/// The node died, with `in_flight` tests waiting for their verdict.
#[derive(Clone)]
struct Death {
    status: String,
    in_flight: usize,
}

/// Runs a test in the node and panics when it fails.
pub fn run(test: &Test) {
    // Tests share the node. After it died with several tests in flight, each
    // of them runs again alone, to learn which one kills the node.
    static GATE: RwLock<()> = RwLock::new(());

    let mut outcome = {
        let _shared = GATE.read().unwrap();
        call(test)
    };
    if matches!(&outcome, Err(death) if death.in_flight > 1) {
        let _alone = GATE.write().unwrap();
        outcome = call(test);
    }
    let verdict = outcome.unwrap_or_else(|death| {
        Err(format!(
            "the BEAM node died while it ran this test ({})",
            death.status
        ))
    });

    if let Err(report) = verdict {
        // The report replaces the message of the panic hook, which would name
        // this function as the place of the panic.
        eprintln!("{report}");
        panic::resume_unwind(Box::new(report));
    }
}

/// Asks the node for the verdict of a test, and starts the node when none
/// runs.
fn call(test: &Test) -> Result<Verdict, Death> {
    static NODE: Mutex<Option<Result<Arc<Node>, String>>> = Mutex::new(None);

    loop {
        let node = {
            let mut node = NODE.lock().unwrap();
            let dead =
                matches!(&*node, Some(Ok(node)) if node.state.lock().unwrap().death.is_some());
            if node.is_none() || dead {
                *node = Some(library(test).and_then(|library| Node::start(&library)));
            }
            match node.as_ref().unwrap() {
                Ok(node) => node.clone(),
                Err(error) => return Ok(Err(error.clone())),
            }
        };
        // The node can die before it takes the test. The next one takes it.
        if let Some(outcome) = node.request(test.name) {
            return outcome.recv().unwrap();
        }
    }
}

/// Builds the NIF library with the tests in it, once, and returns its path.
fn library(test: &Test) -> Result<PathBuf, String> {
    static LIBRARY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    LIBRARY.get_or_init(|| build(test)).clone()
}

fn build(test: &Test) -> Result<PathBuf, String> {
    let manifest = Path::new(test.manifest_dir).join("Cargo.toml");
    // A directory of its own, next to the `deps` directory of this binary,
    // keeps the library with the tests away from the one that `cargo build`
    // writes.
    let target = env::current_exe()
        .map_err(|error| format!("cannot find the test binary: {error}"))?
        .ancestors()
        .nth(2)
        .ok_or("the test binary is not in a target directory")?
        .join("beam_test");

    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "--quiet", "--lib", "--features", "beam_test"])
        .arg("--message-format=json-render-diagnostics")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cannot run cargo: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo could not build the NIF library with the feature `beam_test` ({})",
            output.status
        ));
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact"
                && message["manifest_path"].as_str().map(Path::new) == Some(manifest.as_path())
                && message["target"]["kind"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
        })
        .flat_map(|message| message["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|file| file.as_str().map(PathBuf::from))
        .find(|file| {
            file.extension()
                .is_some_and(|extension| extension == env::consts::DLL_EXTENSION)
        })
        .ok_or_else(|| format!("{} builds no `cdylib`", manifest.display()))
}

struct Node {
    /// Writes hold the lock of `state`, so frames don't mix.
    stream: TcpStream,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_id: u32,
    waiting: HashMap<u32, Sender<Result<Verdict, Death>>>,
    death: Option<Death>,
}

/// Evaluated by `erl`: compiles and runs the driver.
const BOOT: &str = "[Source, Port] = init:get_plain_arguments(), \
    {ok, Module, Code} = compile:file(Source, [binary, report]), \
    {module, Module} = code:load_binary(Module, Source, Code), \
    Module:main(list_to_integer(Port)).";

impl Node {
    /// Starts `erl` and loads the library into it.
    fn start(library: &Path) -> Result<Arc<Node>, String> {
        let io = |error: io::Error| format!("cannot start the BEAM node: {error}");

        let driver = library.with_file_name("rustler_beam_test_driver.erl");
        fs::write(&driver, include_str!("driver.erl")).map_err(io)?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(io)?;
        let port = listener.local_addr().map_err(io)?.port();
        let mut child = Command::new("erl")
            .args(["-noshell", "-noinput", "-eval", BOOT, "-extra"])
            .arg(&driver)
            .arg(port.to_string())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot run `erl`: {error}"))?;

        // `accept` alone would wait forever for a node that failed to boot.
        listener.set_nonblocking(true).map_err(io)?;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if let Some(status) = child.try_wait().map_err(io)? {
                        return Err(format!("`erl` stopped before it connected ({status})"));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(io(error)),
            }
        };
        stream.set_nonblocking(false).map_err(io)?;
        stream.set_nodelay(true).map_err(io)?;

        let mut modules = inventory::iter::<NifModule>.into_iter();
        let (Some(NifModule(module)), None) = (modules.next(), modules.next()) else {
            return Err("the crate needs one `rustler::init!`".into());
        };
        // `erlang:load_nif/2` adds the extension itself.
        let path = library.with_extension("");
        let path = path
            .to_str()
            .ok_or("the path of the library is not UTF-8")?;
        let mut load = vec![module.len() as u8];
        load.extend_from_slice(module.as_bytes());
        load.extend_from_slice(&(path.len() as u16).to_be_bytes());
        load.extend_from_slice(path.as_bytes());
        // This binary holds the same NIFs as the library.
        for nif in inventory::iter::<rustler::Nif> {
            let name = unsafe { CStr::from_ptr(nif.name) }.to_bytes();
            load.push(name.len() as u8);
            load.extend_from_slice(name);
            load.push(nif.arity as u8);
        }
        match write_frame(&stream, &load).and_then(|()| read_frame(&mut stream)) {
            Ok(answer) if answer == [0] => {}
            Ok(answer) => {
                return Err(format!(
                    "the BEAM cannot load {}: {}",
                    library.display(),
                    String::from_utf8_lossy(&answer[1..])
                ))
            }
            Err(_) => {
                return Err(format!(
                    "the BEAM node died while it loaded {} ({})",
                    library.display(),
                    wait(&mut child)
                ))
            }
        }

        let node = Arc::new(Node {
            stream: stream.try_clone().map_err(io)?,
            state: Mutex::default(),
        });
        thread::spawn({
            let node = node.clone();
            move || node.read_verdicts(stream, child)
        });
        Ok(node)
    }

    /// Sends a test to the node. Returns `None` when the node is dead.
    fn request(&self, test: &str) -> Option<Receiver<Result<Verdict, Death>>> {
        let mut state = self.state.lock().unwrap();
        if state.death.is_some() {
            return None;
        }
        let id = state.next_id;
        state.next_id += 1;
        let (sender, receiver) = channel();
        state.waiting.insert(id, sender);
        // When the write fails, the node is dead, and `read_verdicts` answers.
        let _ = write_frame(&self.stream, &[&id.to_be_bytes(), test.as_bytes()].concat());
        Some(receiver)
    }

    /// Hands each verdict to the test that waits for it, until the node dies.
    fn read_verdicts(&self, mut stream: TcpStream, mut child: Child) {
        while let Ok(frame) = read_frame(&mut stream) {
            let (id, verdict) = frame.split_at(4);
            let verdict = match verdict {
                [0] => Ok(()),
                _ => Err(String::from_utf8_lossy(&verdict[1..]).into_owned()),
            };
            let id = u32::from_be_bytes(id.try_into().unwrap());
            if let Some(sender) = self.state.lock().unwrap().waiting.remove(&id) {
                let _ = sender.send(Ok(verdict));
            }
        }

        let status = wait(&mut child);
        let mut state = self.state.lock().unwrap();
        let death = Death {
            status,
            in_flight: state.waiting.len(),
        };
        if death.in_flight > 1 {
            // Not `eprintln!`, whose output belongs to the test that started
            // this thread.
            let _ = writeln!(
                io::stderr(),
                "rustler_beam_test: the BEAM node died ({}) with {} tests in flight, so each of them runs again alone",
                death.status, death.in_flight
            );
        }
        for (_, sender) in state.waiting.drain() {
            let _ = sender.send(Err(death.clone()));
        }
        state.death = Some(death);
    }
}

fn wait(child: &mut Child) -> String {
    match child.wait() {
        Ok(status) => status.to_string(),
        Err(error) => error.to_string(),
    }
}

fn write_frame(mut stream: &TcpStream, frame: &[u8]) -> io::Result<()> {
    stream.write_all(&[&(frame.len() as u32).to_be_bytes(), frame].concat())
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let mut frame = vec![0; u32::from_be_bytes(size) as usize];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}
