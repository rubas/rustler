use crate::sys::{c_char, c_int, c_uint, ErlNifEnv, ErlNifFunc, ErlNifTerm};

pub struct Nif {
    pub name: *const c_char,
    pub arity: c_uint,
    pub flags: c_uint,
    // pub func: DEF_NIF_FUNC,
    pub raw_func: unsafe extern "C" fn(
        nif_env: *mut ErlNifEnv,
        argc: c_int,
        argv: *const ErlNifTerm,
    ) -> ErlNifTerm,
}

impl Nif {
    pub fn get_def(&self) -> ErlNifFunc {
        ErlNifFunc {
            arity: self.arity,
            flags: self.flags,
            function: self.raw_func,
            name: self.name,
        }
    }
}

unsafe impl Sync for Nif {}

inventory::collect!(Nif);

/// The module that a library registers its NIFs for, as given to `init!`.
/// A test runner reads it to load the library into a module of that name.
pub struct NifModule(pub &'static str);

inventory::collect!(NifModule);
