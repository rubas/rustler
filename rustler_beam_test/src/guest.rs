//! The half that runs inside the BEAM, in the NIF library.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};

use rustler::codegen_runtime::{c_char, c_int, ErlNifEnv, ErlNifTerm};
use rustler::{Encoder, Env, Term};

mod atoms {
    rustler::atoms! { ok, error, next_message }
}

/// What a test function returns: `()`, a [`Receive`], or a `Result` of one of
/// them, whose `Err` fails the test.
pub trait Outcome {
    #[doc(hidden)]
    fn into_step(self) -> Step;
}

#[doc(hidden)]
pub enum Step {
    Done(Result<(), String>),
    Receive(Receive),
}

/// The rest of a test, which runs with the next message that the process of
/// the test receives. See [`receive`].
pub struct Receive(Box<dyn for<'a> FnOnce(Env<'a>, Term<'a>) -> Step + Send>);

/// Continues the test with the next message that its process receives.
///
/// A NIF cannot read the mailbox of its process, so the test returns, the
/// process receives one message, and `then` runs in a second NIF call of the
/// same process. No term of the first call is valid in the second one.
/// The test fails when no message arrives within 5 seconds.
pub fn receive<F, O>(then: F) -> Receive
where
    F: for<'a> FnOnce(Env<'a>, Term<'a>) -> O + Send + 'static,
    O: Outcome,
{
    Receive(Box::new(move |env, message| then(env, message).into_step()))
}

impl Outcome for () {
    fn into_step(self) -> Step {
        Step::Done(Ok(()))
    }
}

impl Outcome for Receive {
    fn into_step(self) -> Step {
        Step::Receive(self)
    }
}

impl<O: Outcome, E: Debug> Outcome for Result<O, E> {
    fn into_step(self) -> Step {
        match self {
            Ok(outcome) => outcome.into_step(),
            Err(error) => Step::Done(Err(format!("returned an error: {error:?}"))),
        }
    }
}

/// The tests that wait for a message, by the number that the driver passes
/// back with it.
static WAITING: Mutex<BTreeMap<u64, Receive>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

pub const fn nif(
    name: &'static str,
    raw_func: unsafe extern "C" fn(*mut ErlNifEnv, c_int, *const ErlNifTerm) -> ErlNifTerm,
) -> rustler::Nif {
    rustler::Nif {
        name: name.as_ptr() as *const c_char,
        arity: 1,
        flags: 0,
        raw_func,
    }
}

/// One call of a test NIF. Its argument is `start`, or `{Continuation,
/// Message}` after it returned `{next_message, Continuation}`. It returns that,
/// `ok`, or `{error, Report}`.
///
/// # Safety
///
/// `env` and `argv` are the arguments of a NIF call with one argument.
pub unsafe fn step<O: Outcome>(
    env: *mut ErlNifEnv,
    argv: *const ErlNifTerm,
    test: impl for<'a> FnOnce(Env<'a>) -> O,
) -> ErlNifTerm {
    let lifetime = ();
    let env = Env::new(&lifetime, env);
    let argument = Term::new(env, *argv);

    let step = catch_panic(|| match argument.decode::<(u64, Term)>() {
        Ok((continuation, message)) => {
            let Receive(then) = WAITING
                .lock()
                .unwrap()
                .remove(&continuation)
                .expect("the driver passes each continuation back once");
            then(env, message)
        }
        Err(_) => test(env).into_step(),
    });

    match step {
        Step::Done(Ok(())) => atoms::ok().encode(env),
        Step::Done(Err(report)) => (atoms::error(), report).encode(env),
        Step::Receive(receive) => {
            let continuation = NEXT.fetch_add(1, Ordering::Relaxed);
            WAITING.lock().unwrap().insert(continuation, receive);
            (atoms::next_message(), continuation).encode(env)
        }
    }
    .as_c_arg()
}

thread_local! {
    /// Whether this thread runs a test, so that the panic hook keeps the report.
    static IN_TEST: Cell<bool> = const { Cell::new(false) };
    static REPORT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs a part of a test and turns a panic into a failure with the message and
/// location that `cargo test` shows for a panic.
fn catch_panic(part: impl FnOnce() -> Step) -> Step {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !IN_TEST.get() {
                return default(info);
            }
            let location = info
                .location()
                .map(|at| format!(" at {at}"))
                .unwrap_or_default();
            let message = info.payload_as_str().unwrap_or("Box<dyn Any>");
            let mut report = format!("panicked in the BEAM{location}:\n{message}");
            let backtrace = Backtrace::capture();
            if backtrace.status() == BacktraceStatus::Captured {
                report.push_str(&format!("\nstack backtrace:\n{backtrace}"));
            }
            REPORT.set(Some(report));
        }));
    });

    IN_TEST.set(true);
    let step = panic::catch_unwind(AssertUnwindSafe(part));
    IN_TEST.set(false);
    step.unwrap_or_else(|_| {
        // `resume_unwind` skips the hook.
        Step::Done(Err(REPORT
            .take()
            .unwrap_or_else(|| "panicked in the BEAM".into())))
    })
}
