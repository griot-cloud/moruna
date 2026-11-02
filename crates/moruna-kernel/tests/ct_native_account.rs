//! Light host tests compile the exact shipping account sampler core. The real
//! syscall factory remains native-only in Runtime; no authority is simulated.
#[path = "../../moruna-runtime/src/native_account.rs"]
mod native_account;
