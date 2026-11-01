//! Runs only on the actual native kernel; host modeling is not this acceptance.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]
use moruna_sys::{
    Account, Channel, Console, Error, Memory, PrivateMemory, Rights, map, println, start,
};
moruna_sys::main!(run);
fn proof() -> Result<(), Error> {
    let first = Account::carve(256 * 1024, "same")?;
    let foreign = Account::carve(256 * 1024, "same")?;
    // Names, limits and initial usage are identical; identity still differs.
    if first.matches(&foreign.0)? {
        return Err(Error::BadState);
    }
    let alias = first.0.duplicate(Rights::READ | Rights::TRANSFER)?;
    if !first.matches(&alias)? {
        return Err(Error::BadState);
    }
    let observer = Account(alias);
    if observer.0.info()?.1 != (Rights::READ | Rights::TRANSFER)
        || observer.usage()?.limit != first.usage()?.limit
        || observer.usage()?.used != first.usage()?.used
        || !matches!(
            Memory::create_in(4096, &observer.0),
            Err(Error::AccessDenied)
        )
        || !matches!(observer.0.duplicate(Rights::READ), Err(Error::AccessDenied))
    {
        return Err(Error::BadState);
    }
    let memory = Memory::create_in(4096, &first.0)?;
    let loan = memory
        .0
        .duplicate(Rights::READ | Rights::WRITE | Rights::MAP | Rights::TRANSFER)?;
    if !first.matches(&loan)? || foreign.matches(&loan)? {
        return Err(Error::BadState);
    }
    let no_read = memory
        .0
        .duplicate(Rights::WRITE | Rights::MAP | Rights::TRANSFER)?;
    if first.matches(&no_read) != Err(Error::AccessDenied) {
        return Err(Error::BadState);
    }
    let (channel, _peer) = Channel::create()?;
    if first.matches(&channel.0) != Err(Error::WrongType) {
        return Err(Error::BadState);
    }
    let before = first.usage()?.used;
    let contiguous = Memory::create_dma_contiguous_in(4096, &first.0)?;
    if !first.matches(&contiguous.0)?
        || foreign.matches(&contiguous.0)?
        || first.usage()?.used != before + moruna_sys::charge::memory(1)
    {
        return Err(Error::BadState);
    }
    let too_small = Account::carve(4096, "small")?;
    if !matches!(
        Memory::create_dma_contiguous_in(4096, &too_small.0),
        Err(Error::QuotaExceeded)
    ) || too_small.usage()?.used != 0
    {
        return Err(Error::BadState);
    }
    let mut scratch = PrivateMemory::create_in(17, &first.0)?;
    scratch.with_bytes(|bytes| {
        if bytes.len() != 17 {
            return Err(Error::BadState);
        }
        bytes.fill(7);
        Ok(())
    })??;
    let exported = scratch.into_memory();
    if !first.matches(&exported.0)? || foreign.matches(&exported.0)? {
        return Err(Error::BadState);
    }
    let mut bytes = [0; 17];
    exported.map(0, 4096, map::READ)?.read(0, &mut bytes)?;
    if bytes != [7; 17] {
        return Err(Error::BadState);
    }
    Ok(())
}
fn run() -> i64 {
    let Some(out) = start::take("console").map(Console) else {
        return 1;
    };
    match proof() {
        Ok(()) => {
            println!(
                out,
                "KE3 account-origin: ok: actual account identity, foreign memory refusal, charged contiguous DMA and private export"
            );
            0
        }
        Err(e) => {
            println!(out, "KE3 account-origin: FAIL: {}", e.name());
            1
        }
    }
}
