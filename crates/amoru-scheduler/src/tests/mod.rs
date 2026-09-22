//! The SC tests of section k, one module per test id.
//!
//! They live inside the crate rather than under `tests/` because SC-T19 names a seam compiled
//! under `cfg(test)` (`Scheduler::test_kill_worker`) and SC-T2 asserts on the pick rule, which
//! is private to the crate.

mod common;

mod sc_behaviour;

mod sc_t10_probe_protocol;
mod sc_t11_cancel;
mod sc_t12_utilisation;
mod sc_t13_evicted_replay;
mod sc_t14_watermark_and_skips;
mod sc_t15_checkpoint_tick;
mod sc_t16_resume_equivalence;
mod sc_t17_single_row_larger_than_max;
mod sc_t18_stateful_init_fails;
mod sc_t19_worker_heartbeat;
mod sc_t1_workers_only_apply;
mod sc_t2_admission_rule;
mod sc_t3_source_admission;
mod sc_t4_one_record_per_morsel_stage;
mod sc_t5_knobs_immediate;
mod sc_t6_instance_affinity;
mod sc_t7_error_policies;
mod sc_t8_completion_exact;
mod sc_t9_draining_order;
