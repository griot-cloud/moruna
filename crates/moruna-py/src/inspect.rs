//! `moruna.inspect_host()` (d.2, j): what the runtime would discover on this host, without
//! starting anything.

use moruna_kernel::{Guarantee, Limits, Sampler as _};
use moruna_runtime::{DiscoveryInput, Runtime};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::errors::{Attachments, to_py_err};

/// `{"limits": {...}, "host_profile": {...}, "anon_bytes": n, "notes": [...]}`: `Limits` and
/// `HostProfile` field by field, what the process is already holding, and discovery's notes
/// verbatim.
#[pyfunction]
pub fn inspect_host(py: Python<'_>) -> PyResult<Py<PyDict>> {
    let discovered = Runtime::inspect(&DiscoveryInput::default())
        .map_err(|e| to_py_err(py, &e.error, Attachments::default()))?;
    let out = PyDict::new(py);
    out.set_item("limits", limits_dict(py, &discovered.limits)?)?;
    // What the process holds now, through the same sampler a run would use (03 f.7). A budget is
    // a ceiling for the whole process and the arena is sized from what was already there, so a
    // caller choosing one needs this figure and had no way to ask for it: the interpreter, pyarrow
    // and numpy rest at 90 MB on one host and 430 MB on another, and 512 MiB means something
    // different on each (2026-09-23).
    let sampler = moruna_discovery::Sampler::new(&discovered)
        .map_err(|e| to_py_err(py, &e, Attachments::default()))?;
    out.set_item("anon_bytes", sampler.sample().anon_bytes)?;

    let p = &discovered.profile;
    let profile = PyDict::new(py);
    profile.set_item("huge_pages", guarantee(p.huge_pages))?;
    profile.set_item("memlock", guarantee(p.memlock))?;
    profile.set_item("io_uring", guarantee(p.io_uring))?;
    profile.set_item("direct_io_staging", guarantee(p.direct_io_staging))?;
    profile.set_item("gds", guarantee(p.gds))?;
    profile.set_item("rdma", guarantee(p.rdma))?;
    profile.set_item(
        "staging_dir",
        p.staging_dir
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned()),
    )?;
    profile.set_item("durable_staging", guarantee(p.durable_staging))?;
    out.set_item("host_profile", profile)?;

    out.set_item("host_tier", format!("{:?}", discovered.host_tier))?;
    out.set_item(
        "cgroup_path",
        discovered
            .cgroup_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    )?;
    out.set_item("notes", discovered.notes.clone())?;
    Ok(out.unbind())
}

fn limits_dict<'py>(py: Python<'py>, l: &Limits) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("memory_ceiling", l.memory_ceiling)?;
    d.set_item("memory_kill", l.memory_kill)?;
    d.set_item("cpu_quota", l.cpu_quota)?;
    d.set_item("page_bytes", l.page_bytes)?;
    d.set_item("source", format!("{:?}", l.source))?;
    let devices = PyList::empty(py);
    for dev in &l.devices {
        let e = PyDict::new(py);
        e.set_item("id", dev.id.0)?;
        e.set_item("name", &dev.name)?;
        e.set_item("total_bytes", dev.total_bytes)?;
        e.set_item("free_bytes", dev.free_bytes)?;
        devices.append(e)?;
    }
    d.set_item("devices", devices)?;
    Ok(d)
}

/// A `Guarantee` by name: `"present"`, `"absent"`, `"probed: true"`, `"probed: false"` or
/// `"unknown"`. Discovery replaces every `Unknown`, so the last is only ever a profile the caller
/// built itself (contracts d.12).
fn guarantee(g: Guarantee) -> String {
    match g {
        Guarantee::Present => "present".to_string(),
        Guarantee::Absent => "absent".to_string(),
        Guarantee::Probed(v) => format!("probed: {v}"),
        Guarantee::Unknown => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarantees_read_as_words() {
        assert_eq!(guarantee(Guarantee::Present), "present");
        assert_eq!(guarantee(Guarantee::Absent), "absent");
        assert_eq!(guarantee(Guarantee::Probed(true)), "probed: true");
        assert_eq!(guarantee(Guarantee::Unknown), "unknown");
    }
}
