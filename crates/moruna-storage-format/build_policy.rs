// Shared build-policy predicate, tested without invoking a compiler or device.
/// True only for the upstream backend selector in Cargo's dependency-wide flag list.
pub fn allows(flags: &str) -> bool {
    let mut flags = flags.split('\x1f');
    while let Some(flag) = flags.next() {
        if flag == "--cfg=polyval_force_soft" {
            return true;
        }
        if flag == "--cfg" && flags.next() == Some("polyval_force_soft") {
            return true;
        }
    }
    false
}
