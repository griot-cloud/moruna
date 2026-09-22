//! PL-T17 kill_and_resume (PL-I13, S17). Tagged "(integration, closes in wave 4)": it drives
//! a three-stage run through the scheduler and `FakeSource`, kills the process with SIGKILL
//! at ten random points including mid-checkpoint, and resumes from the manifest. The
//! scheduler is component 10 and does not exist yet, and this crate depends on neither it nor
//! `amoru-sources` (d.2), so the test exists here, is ignored with its reason, and closes in
//! wave 4 with SC-T16.

mod common;

#[test]
#[ignore = "integration, closes in wave 4: it needs the scheduler (component 10) to drive the resume path"]
fn pl_t17_kill_and_resume() {
    // What wave 4 will assert, once `Scheduler::run_resumed` exists:
    //   - a 3-stage run with a throttled sink and a source ten times the size of RAM;
    //   - the process is killed with SIGKILL at ten random points, one of them while the
    //     manifest's `.tmp` file exists, so PL-I12's rename is what a reader sees;
    //   - a fresh process resumes through `find_manifest`, `read_manifest_header`,
    //     `Placement::restore` and `Scheduler::apply_resume_point`;
    //   - the output is byte-equal to an uninterrupted run for an ordered sink and
    //     row-set-equal for an unordered one;
    //   - the number of source rows re-read equals the recomputed lineage and never more,
    //     counted through `Source::read` calls (PL-I13: recovery is by lineage, never by
    //     replication).
    // The manifest round trip those steps rest on is PL-T16, which passes here.
    let _ = common::PAGE;
}
