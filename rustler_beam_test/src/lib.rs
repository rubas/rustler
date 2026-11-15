#![doc = include_str!("../README.md")]

mod guest;
mod host;

pub use guest::{receive, Outcome, Receive};
pub use rustler_beam_test_macros::test;

/// Used by the code that `#[rustler_beam_test::test]` generates.
#[doc(hidden)]
pub mod __private {
    pub use crate::guest::{nif, step};
    pub use crate::host::{run, Test};
    pub use rustler::codegen_runtime::{c_int, inventory, ErlNifEnv, ErlNifTerm};
}

/// Registers a test as a NIF of the library, as `#[rustler::nif]` does for a
/// function. The NIF has the path of the test as its name.
#[doc(hidden)]
#[macro_export]
macro_rules! __nif {
    ($name:ident, $test:expr) => {
        $crate::__private::inventory::submit!($crate::__private::nif(
            concat!(module_path!(), "::", stringify!($name), "\0"),
            {
                unsafe extern "C" fn nif(
                    env: *mut $crate::__private::ErlNifEnv,
                    _argc: $crate::__private::c_int,
                    argv: *const $crate::__private::ErlNifTerm,
                ) -> $crate::__private::ErlNifTerm {
                    unsafe { $crate::__private::step(env, argv, $test) }
                }
                nif
            }
        ));
    };
}
