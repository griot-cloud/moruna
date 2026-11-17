//! On the kernel, ring's randomness (the node's key, its certificate's serial, every handshake)
//! comes from `moruna_svc::random`, as the network server's does (`kernel/net/stack`).
#![allow(unsafe_code)]

fn secure_random(bytes: &mut [u8]) -> Result<(), getrandom::Error> {
    moruna_svc::random(bytes).map_err(|_| getrandom::Error::UNSUPPORTED)
}

getrandom::register_custom_getrandom!(secure_random);
