# rustler_beam_test

Run the Rust tests of a Rustler NIF library inside a real BEAM, with
`cargo test`.

A test binary has no BEAM, so a test that calls into the NIF API cannot run in
it. `#[rustler_beam_test::test]` compiles the test into the NIF library
instead, starts an `erl` node, and runs the test there as a NIF. `cargo test`
reports it like any other test.

```rust,ignore
#[cfg(any(test, feature = "beam_test"))]
mod tests {
    use rustler::{Encoder, Env};
    use rustler_beam_test::{receive, Receive};

    #[rustler_beam_test::test]
    fn an_integer_round_trips(env: Env) {
        assert_eq!(42.encode(env).decode::<i64>().unwrap(), 42);
    }

    #[rustler_beam_test::test]
    fn a_message_to_self_arrives(env: Env) -> Receive {
        env.send(&env.pid(), "hello").unwrap();
        receive(|_env, message| assert_eq!(message.decode::<&str>().unwrap(), "hello"))
    }

    // A test without the BEAM still works next to them.
    #[test]
    fn two_and_two() {
        assert_eq!(2 + 2, 4);
    }
}
```

## Setup

It works on Linux so far, and needs `erl` on the `PATH`.

```toml
[features]
beam_test = ["dep:rustler_beam_test"]

[dependencies]
rustler = "..."
rustler_beam_test = { version = "...", optional = true }

[dev-dependencies]
rustler = { version = "...", features = ["dynamic_symbols"] }
rustler_beam_test = "..."
```

The feature must have the name `beam_test`. The test binary still holds the
NIFs of the crate, so it needs the `dynamic_symbols` feature to link.

## Tests

A test takes no argument or a `rustler::Env`. It fails when it panics, or when
it returns an `Err`. `#[ignore]` and `#[should_panic]` work as on a `#[test]`,
and only a panic of the test itself satisfies `#[should_panic]`.

Each test runs as one NIF call in a new process, on a normal scheduler, so
the `Env` is bound to a process, as in a NIF. Tests of one test binary share
the node and run in parallel.

A NIF cannot read the mailbox of its process. To check a message, a test
returns `receive(...)`: the process then takes its next message and passes it
to the closure, in a second NIF call. The test fails when no message arrives
within 5 seconds.

`#[rustler::nif]` hides the function that it wraps, so a test cannot call it.
Test the function that the NIF calls.

## How it works

The attribute registers the test as a NIF of the library, like
`#[rustler::nif]` does, behind the `beam_test` feature. It also emits a
`#[test]` function for the test binary. The first of them to run:

1. builds the library with `cargo build --lib --features beam_test`, into a
   directory of its own in the target directory,
2. starts `erl` with a small driver, which connects back to the test binary on
   the loopback interface,
3. has the driver compile the module that `rustler::init!` names, with a stub
   for each NIF, and load the library into it.

Each `#[test]` function then asks the driver to run its NIF, and gets back
`ok`, or the panic message and its location. Set `RUST_BACKTRACE=1` to get the
backtrace too.

When a test kills the node, for example with a segmentation fault, that test
fails with the exit status of the node, and the next test starts a new node.
When several tests were running at that moment, each of them runs again
alone, so that only the one that kills the node fails.

Output that a test prints inside the node goes to the terminal directly.
`cargo test` does not capture it.
