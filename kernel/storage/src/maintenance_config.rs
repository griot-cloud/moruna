//! Explicit readonly private housekeeping policy input, shared by server and client.
use crate::memory::Loan;
use moruna_storage_format::{
    config::{MAX_CONFIG, MaintenanceConfig},
    wire::Refusal,
};
use moruna_svc::{Handle, Memory, ObjectType, PrivateMemory, Rights, start};
/// Consume only the explicit optional boot asset; absent never synthesizes maintenance
/// authority or policy. Both server and resident client use the same canonical parser.
pub fn load(account: &Handle) -> Result<Option<MaintenanceConfig>, Refusal> {
    let Some(handle) = start::take("storage.maintenance-config") else {
        return Ok(None);
    };
    let memory = Memory(handle);
    let (kind, rights) = memory.0.info().map_err(|_| Refusal::Invalid)?;
    if kind != ObjectType::Memory
        || !rights.contains(Rights::READ | Rights::MAP)
        || rights.contains(Rights::WRITE)
    {
        return Err(Refusal::Denied);
    }
    let source = Loan::new(memory, 0, MAX_CONFIG as u64, false).map_err(|_| Refusal::Invalid)?;
    let mut private =
        PrivateMemory::create_in(MAX_CONFIG as u64, account).map_err(|_| Refusal::Capacity)?;
    let policy = private
        .with_bytes(|bytes| {
            source
                .read(0, &mut bytes[..MAX_CONFIG])
                .map_err(|_| Refusal::Invalid)?;
            MaintenanceConfig::decode_page(&bytes[..MAX_CONFIG]).map_err(|_| Refusal::Invalid)
        })
        .map_err(|_| Refusal::Unavailable)??;
    Ok(Some(policy))
}
