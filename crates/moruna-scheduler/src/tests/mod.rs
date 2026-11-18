//! The SC tests of section k, one module per test id.
//!
//! They live inside the crate rather than under `tests/` because SC-T19 names a seam compiled
//! under `cfg(test)` (`Scheduler::test_kill_worker`) and SC-T2 asserts on the pick rule, which
//! is private to the crate.

pub(crate) mod common;

mod sc_behaviour;

mod sc_cpu_limit;
mod sc_io_times;
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
mod sc_t20_manifest_on_completion;
mod sc_t21_checkpoint_on_demand;
mod sc_t22_copy_behind_a_slow_sink;
mod sc_t23_refusal_follows_the_policy;
mod sc_t24_zero_row_splits;
mod sc_t25_any_plan_finishes;
mod sc_t26_a_drive_that_panics_ends_the_run;
mod sc_t27_a_kernels_finish;
mod sc_t2_admission_rule;
mod sc_t34_policy_port;
mod sc_t35_liveness;
mod sc_t36_residency_greedy;
mod sc_t37_lookahead;
mod sc_t38_learning;
mod sc_t39_a_finish_gives_downstream;
mod sc_t3_source_admission;
mod sc_t43_a_refused_output_waits;
mod sc_t44_a_refused_output_asks_for_room;
mod sc_t45_an_immovable_wall_ends_the_run;
mod sc_t46_a_stuck_sink_is_flushed;
// Item 6.3a: a budget bounds how far reading runs ahead, not how many workers decode.
mod sc_t47_a_budget_bounds_reads_not_decoders;
mod sc_t47_b_parquet_groups_cut_by_their_pages;
// Item 6.3b: no morsel is larger than the smallest budget of the tiers it must be placed in.
mod sc_t48_a_morsel_fits_its_tiers;
// Item 6.3c: a read in flight is counted once by the arena's admission.
mod sc_t49_a_read_in_flight_is_counted_once;
mod sc_t4_one_record_per_morsel_stage;
mod sc_t50_a_waiting_read_is_never_dropped;
// Item 6.3e: a function runs one instance per CPU of the job unless it declares fewer.
mod sc_p_a_function_runs_per_cpu;
// stream P: a native stage's instances fed in turn.
mod sc_p_dealt_instances;
mod sc_p_instances_wait_for_a_warm_up;
// Item 3.1: a native stage of eight ring tasks under a memory bound.
mod sc_p_native_stage_memory;
mod sc_t5_knobs_immediate;
mod sc_t6_instance_affinity;
mod sc_t7_error_policies;
mod sc_t8_completion_exact;
mod sc_t9_draining_order;
