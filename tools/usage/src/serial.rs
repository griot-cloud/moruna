//! What a node's serial log says besides the programs' dumps: each Linux process's end, with
//! the calls it was refused (`kernel/linux`'s line, which B2's personality keeps).

use std::collections::BTreeMap;

/// One Linux process's end, as the Linux server prints it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinuxEnd {
    pub name: String,
    pub served: u64,
    /// Call number to the times it was refused.
    pub refused: BTreeMap<u64, u64>,
}

/// `linux: NAME (pid N) HOW; S calls served; refused: name(n) xC, n xC` or `refused: none`.
pub fn linux_end(line: &str) -> Option<LinuxEnd> {
    let at = line.find("linux: ")?;
    let line = &line[at + "linux: ".len()..];
    let (head, refused) = line.split_once(" calls served; refused: ")?;
    let (head, served) = head.rsplit_once("; ")?;
    let served = served.trim().parse().ok()?;
    let name = head.split(" (pid ").next()?.to_string();
    let mut counts = BTreeMap::new();
    let refused = refused.trim();
    if refused != "none" {
        for item in refused.split(", ") {
            let (call, times) = item.rsplit_once(" x")?;
            let number = match call.rsplit_once('(') {
                Some((_, n)) => n.strip_suffix(')')?,
                None => call,
            };
            *counts.entry(number.parse().ok()?).or_insert(0) += times.parse::<u64>().ok()?;
        }
    }
    Some(LinuxEnd {
        name,
        served,
        refused: counts,
    })
}

/// Every Linux process end in a log.
pub fn linux_ends(log: &str) -> Vec<LinuxEnd> {
    log.lines().filter_map(linux_end).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_processs_end_is_read() {
        let log = "kk: boot\n\
                   linux: moruna-engine (pid 3) exited with code 0; 5012 calls served; refused: rseq(334) x1, socket(41) x2, 9999 x1\n\
                   linux: hello (pid 4) ended by signal 9; 7 calls served; refused: none\r\n\
                   linux: broken (pid 5) exited; x calls served; refused: none\n\
                   linux: odd (pid 6) exited; 1 calls served; refused: what\n";
        let ends = linux_ends(log);
        assert_eq!(ends.len(), 2);
        assert_eq!(ends[0].name, "moruna-engine");
        assert_eq!(ends[0].served, 5012);
        assert_eq!(
            ends[0].refused,
            BTreeMap::from([(41, 2), (334, 1), (9999, 1)])
        );
        assert_eq!(
            ends[1],
            LinuxEnd {
                name: "hello".into(),
                served: 7,
                refused: BTreeMap::new()
            }
        );
        assert_eq!(linux_end("node: service 3 started"), None);
    }
}
