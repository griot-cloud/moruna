//! moruna: random bytes from the CPU's random source, through the core's `random_read`.

pub fn fill_bytes(bytes: &mut [u8]) {
    if let Err(error) = moruna_sys::random(bytes) {
        panic!("no random bytes from the core: {}", error.name());
    }
}
