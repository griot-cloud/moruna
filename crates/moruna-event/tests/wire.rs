//! Public FlatBuffers conformance: independent builders, verified readers and diagnostics.
//! These tests exercise the exported wire API, including omitted fields, without relying on
//! the high-level encoder to agree with its own decoder. Table diagnostics must name every
//! schema field, so a verified peer message can be diagnosed without losing optional data.
use flatbuffers::FlatBufferBuilder;
use moruna_event::fb;

fn check_union_accessors(root: &fb::Event<'_>) {
    assert_eq!(
        root.body_as_step_started().is_some(),
        root.body_type() == fb::Body::StepStarted
    );
    assert_eq!(
        root.body_as_step_ended().is_some(),
        root.body_type() == fb::Body::StepEnded
    );
    assert_eq!(
        root.body_as_task_registered().is_some(),
        root.body_type() == fb::Body::TaskRegistered
    );
    assert_eq!(
        root.body_as_task_waiting().is_some(),
        root.body_type() == fb::Body::TaskWaiting
    );
    assert_eq!(
        root.body_as_task_ready().is_some(),
        root.body_type() == fb::Body::TaskReady
    );
    assert_eq!(
        root.body_as_task_started().is_some(),
        root.body_type() == fb::Body::TaskStarted
    );
    assert_eq!(
        root.body_as_task_phase().is_some(),
        root.body_type() == fb::Body::TaskPhase
    );
    assert_eq!(
        root.body_as_task_completed().is_some(),
        root.body_type() == fb::Body::TaskCompleted
    );
    assert_eq!(
        root.body_as_task_failed().is_some(),
        root.body_type() == fb::Body::TaskFailed
    );
    assert_eq!(
        root.body_as_task_skipped().is_some(),
        root.body_type() == fb::Body::TaskSkipped
    );
    assert_eq!(
        root.body_as_cancel_acknowledged().is_some(),
        root.body_type() == fb::Body::CancelAcknowledged
    );
    assert_eq!(
        root.body_as_cancel_ended().is_some(),
        root.body_type() == fb::Body::CancelEnded
    );
    assert_eq!(
        root.body_as_logs().is_some(),
        root.body_type() == fb::Body::Logs
    );
    assert_eq!(
        root.body_as_log().is_some(),
        root.body_type() == fb::Body::Log
    );
    assert_eq!(
        root.body_as_attempt_started().is_some(),
        root.body_type() == fb::Body::AttemptStarted
    );
}

#[test]
fn attempt_started_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::AttemptStarted::create(&mut b, &fb::AttemptStartedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::AttemptStarted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::AttemptStarted);
        let t = root.body_as_attempt_started().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("AttemptStarted"));
        assert!(!t.resumed());
        assert_eq!(t.plan(), None);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("AttemptStarted"));
        assert!(diagnostic.contains("resumed:"), "{diagnostic}");
        assert!(diagnostic.contains("plan:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let plan = b.create_string("AttemptStarted.plan");
        let table = fb::AttemptStarted::create(
            &mut b,
            &fb::AttemptStartedArgs {
                resumed: true,
                plan: Some(plan),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::AttemptStarted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::AttemptStarted);
        let t = root.body_as_attempt_started().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("AttemptStarted"));
        assert!(t.resumed());
        assert_eq!(t.plan(), Some("AttemptStarted.plan"));
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("AttemptStarted"));
        assert!(diagnostic.contains("resumed:"), "{diagnostic}");
        assert!(diagnostic.contains("plan:"), "{diagnostic}");
    }
}

#[test]
fn step_started_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::StepStarted::create(&mut b, &fb::StepStartedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::StepStarted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::StepStarted);
        let t = root.body_as_step_started().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("StepStarted"));
        assert_eq!(t.step(), None);
        assert_eq!(t.plan(), None);
        assert_eq!(t.group(), 0);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("StepStarted"));
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("plan:"), "{diagnostic}");
        assert!(diagnostic.contains("group:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let step = b.create_string("StepStarted.step");
        let plan = b.create_string("StepStarted.plan");
        let table = fb::StepStarted::create(
            &mut b,
            &fb::StepStartedArgs {
                step: Some(step),
                plan: Some(plan),
                group: u32::MAX,
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::StepStarted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::StepStarted);
        let t = root.body_as_step_started().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("StepStarted"));
        assert_eq!(t.step(), Some("StepStarted.step"));
        assert_eq!(t.plan(), Some("StepStarted.plan"));
        assert_eq!(t.group(), u32::MAX);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("StepStarted"));
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("plan:"), "{diagnostic}");
        assert!(diagnostic.contains("group:"), "{diagnostic}");
    }
}

#[test]
fn step_ended_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::StepEnded::create(&mut b, &fb::StepEndedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::StepEnded,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::StepEnded);
        let t = root.body_as_step_ended().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("StepEnded"));
        assert_eq!(t.step(), None);
        assert_eq!(t.plan(), None);
        assert_eq!(t.outcome(), fb::StepOutcome::Failed);
        assert_eq!(t.duration_us(), None);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("StepEnded"));
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("plan:"), "{diagnostic}");
        assert!(diagnostic.contains("outcome:"), "{diagnostic}");
        assert!(diagnostic.contains("duration_us:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let step = b.create_string("StepEnded.step");
        let plan = b.create_string("StepEnded.plan");
        let table = fb::StepEnded::create(
            &mut b,
            &fb::StepEndedArgs {
                step: Some(step),
                plan: Some(plan),
                outcome: fb::StepOutcome::Succeeded,
                duration_us: Some(u64::MAX),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::StepEnded,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::StepEnded);
        let t = root.body_as_step_ended().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("StepEnded"));
        assert_eq!(t.step(), Some("StepEnded.step"));
        assert_eq!(t.plan(), Some("StepEnded.plan"));
        assert_eq!(t.outcome(), fb::StepOutcome::Succeeded);
        assert_eq!(t.duration_us(), Some(u64::MAX));
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("StepEnded"));
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("plan:"), "{diagnostic}");
        assert!(diagnostic.contains("outcome:"), "{diagnostic}");
        assert!(diagnostic.contains("duration_us:"), "{diagnostic}");
    }
}

#[test]
fn task_registered_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskRegistered::create(&mut b, &fb::TaskRegisteredArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskRegistered,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskRegistered);
        let t = root.body_as_task_registered().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskRegistered"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.element(), fb::Element::Source);
        assert_eq!(t.stage(), 0);
        assert!(t.upstream().is_none());
        assert!(t.members().is_none());
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskRegistered"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("element:"), "{diagnostic}");
        assert!(diagnostic.contains("stage:"), "{diagnostic}");
        assert!(diagnostic.contains("upstream:"), "{diagnostic}");
        assert!(diagnostic.contains("members:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskRegistered.task");
        let step = b.create_string("TaskRegistered.step");
        let item = b.create_string("TaskRegistered.upstream");
        let upstream = b.create_vector(&[item]);
        let item = b.create_string("TaskRegistered.members");
        let members = b.create_vector(&[item]);
        let table = fb::TaskRegistered::create(
            &mut b,
            &fb::TaskRegisteredArgs {
                task: Some(task),
                step: Some(step),
                element: fb::Element::Sink,
                stage: u16::MAX,
                upstream: Some(upstream),
                members: Some(members),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskRegistered,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskRegistered);
        let t = root.body_as_task_registered().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskRegistered"));
        assert_eq!(t.task(), Some("TaskRegistered.task"));
        assert_eq!(t.step(), Some("TaskRegistered.step"));
        assert_eq!(t.element(), fb::Element::Sink);
        assert_eq!(t.stage(), u16::MAX);
        assert_eq!(
            t.upstream().unwrap().iter().collect::<Vec<_>>(),
            ["TaskRegistered.upstream"]
        );
        assert_eq!(
            t.members().unwrap().iter().collect::<Vec<_>>(),
            ["TaskRegistered.members"]
        );
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskRegistered"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("element:"), "{diagnostic}");
        assert!(diagnostic.contains("stage:"), "{diagnostic}");
        assert!(diagnostic.contains("upstream:"), "{diagnostic}");
        assert!(diagnostic.contains("members:"), "{diagnostic}");
    }
}

#[test]
fn task_waiting_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskWaiting::create(&mut b, &fb::TaskWaitingArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskWaiting,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskWaiting);
        let t = root.body_as_task_waiting().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskWaiting"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.reason(), fb::WaitReason::EnginePreparing);
        assert!(t.blocking().is_none());
        assert_eq!(t.port(), None);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskWaiting"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("reason:"), "{diagnostic}");
        assert!(diagnostic.contains("blocking:"), "{diagnostic}");
        assert!(diagnostic.contains("port:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskWaiting.task");
        let step = b.create_string("TaskWaiting.step");
        let item = b.create_string("TaskWaiting.blocking");
        let blocking = b.create_vector(&[item]);
        let port = b.create_string("TaskWaiting.port");
        let table = fb::TaskWaiting::create(
            &mut b,
            &fb::TaskWaitingArgs {
                task: Some(task),
                step: Some(step),
                reason: fb::WaitReason::PortCall,
                blocking: Some(blocking),
                port: Some(port),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskWaiting,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskWaiting);
        let t = root.body_as_task_waiting().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskWaiting"));
        assert_eq!(t.task(), Some("TaskWaiting.task"));
        assert_eq!(t.step(), Some("TaskWaiting.step"));
        assert_eq!(t.reason(), fb::WaitReason::PortCall);
        assert_eq!(
            t.blocking().unwrap().iter().collect::<Vec<_>>(),
            ["TaskWaiting.blocking"]
        );
        assert_eq!(t.port(), Some("TaskWaiting.port"));
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskWaiting"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("reason:"), "{diagnostic}");
        assert!(diagnostic.contains("blocking:"), "{diagnostic}");
        assert!(diagnostic.contains("port:"), "{diagnostic}");
    }
}

#[test]
fn task_ready_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskReady::create(&mut b, &fb::TaskReadyArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskReady,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskReady);
        let t = root.body_as_task_ready().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskReady"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskReady"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskReady.task");
        let step = b.create_string("TaskReady.step");
        let table = fb::TaskReady::create(
            &mut b,
            &fb::TaskReadyArgs {
                task: Some(task),
                step: Some(step),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskReady,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskReady);
        let t = root.body_as_task_ready().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskReady"));
        assert_eq!(t.task(), Some("TaskReady.task"));
        assert_eq!(t.step(), Some("TaskReady.step"));
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskReady"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
    }
}

#[test]
fn task_started_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskStarted::create(&mut b, &fb::TaskStartedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskStarted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskStarted);
        let t = root.body_as_task_started().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskStarted"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.phase(), fb::Phase::Calibrating);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskStarted"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("phase:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskStarted.task");
        let step = b.create_string("TaskStarted.step");
        let table = fb::TaskStarted::create(
            &mut b,
            &fb::TaskStartedArgs {
                task: Some(task),
                step: Some(step),
                phase: fb::Phase::Committing,
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskStarted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskStarted);
        let t = root.body_as_task_started().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskStarted"));
        assert_eq!(t.task(), Some("TaskStarted.task"));
        assert_eq!(t.step(), Some("TaskStarted.step"));
        assert_eq!(t.phase(), fb::Phase::Committing);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskStarted"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("phase:"), "{diagnostic}");
    }
}

#[test]
fn task_phase_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskPhase::create(&mut b, &fb::TaskPhaseArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskPhase,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskPhase);
        let t = root.body_as_task_phase().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskPhase"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.phase(), fb::Phase::Calibrating);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskPhase"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("phase:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskPhase.task");
        let step = b.create_string("TaskPhase.step");
        let table = fb::TaskPhase::create(
            &mut b,
            &fb::TaskPhaseArgs {
                task: Some(task),
                step: Some(step),
                phase: fb::Phase::Committing,
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskPhase,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskPhase);
        let t = root.body_as_task_phase().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskPhase"));
        assert_eq!(t.task(), Some("TaskPhase.task"));
        assert_eq!(t.step(), Some("TaskPhase.step"));
        assert_eq!(t.phase(), fb::Phase::Committing);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskPhase"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("phase:"), "{diagnostic}");
    }
}

#[test]
fn task_completed_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskCompleted::create(&mut b, &fb::TaskCompletedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskCompleted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskCompleted);
        let t = root.body_as_task_completed().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskCompleted"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.outcome(), fb::TaskOutcome::Succeeded);
        assert_eq!(t.duration_us(), None);
        assert!(t.counters().is_none());
        assert_eq!(t.cause(), None);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskCompleted"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("outcome:"), "{diagnostic}");
        assert!(diagnostic.contains("duration_us:"), "{diagnostic}");
        assert!(diagnostic.contains("counters:"), "{diagnostic}");
        assert!(diagnostic.contains("cause:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskCompleted.task");
        let step = b.create_string("TaskCompleted.step");
        let counters = fb::Counters::new(1, 2, 3, 4, 5, 6, 7);
        let cause = b.create_string("TaskCompleted.cause");
        let table = fb::TaskCompleted::create(
            &mut b,
            &fb::TaskCompletedArgs {
                task: Some(task),
                step: Some(step),
                outcome: fb::TaskOutcome::Cancelled,
                duration_us: Some(u64::MAX),
                counters: Some(&counters),
                cause: Some(cause),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskCompleted,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskCompleted);
        let t = root.body_as_task_completed().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskCompleted"));
        assert_eq!(t.task(), Some("TaskCompleted.task"));
        assert_eq!(t.step(), Some("TaskCompleted.step"));
        assert_eq!(t.outcome(), fb::TaskOutcome::Cancelled);
        assert_eq!(t.duration_us(), Some(u64::MAX));
        assert_eq!(t.counters().unwrap().errors(), 7);
        assert_eq!(t.cause(), Some("TaskCompleted.cause"));
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskCompleted"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("outcome:"), "{diagnostic}");
        assert!(diagnostic.contains("duration_us:"), "{diagnostic}");
        assert!(diagnostic.contains("counters:"), "{diagnostic}");
        assert!(diagnostic.contains("cause:"), "{diagnostic}");
    }
}

#[test]
fn task_failed_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskFailed::create(&mut b, &fb::TaskFailedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskFailed,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskFailed);
        let t = root.body_as_task_failed().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskFailed"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.code(), fb::FailureCode::SpecRefused);
        assert_eq!(t.message(), None);
        assert_eq!(t.scope(), fb::FailureScope::Task);
        assert!(!t.retryable());
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskFailed"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("code:"), "{diagnostic}");
        assert!(diagnostic.contains("message:"), "{diagnostic}");
        assert!(diagnostic.contains("scope:"), "{diagnostic}");
        assert!(diagnostic.contains("retryable:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskFailed.task");
        let step = b.create_string("TaskFailed.step");
        let message = b.create_string("TaskFailed.message");
        let table = fb::TaskFailed::create(
            &mut b,
            &fb::TaskFailedArgs {
                task: Some(task),
                step: Some(step),
                code: fb::FailureCode::Internal,
                message: Some(message),
                scope: fb::FailureScope::Attempt,
                retryable: true,
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskFailed,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskFailed);
        let t = root.body_as_task_failed().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskFailed"));
        assert_eq!(t.task(), Some("TaskFailed.task"));
        assert_eq!(t.step(), Some("TaskFailed.step"));
        assert_eq!(t.code(), fb::FailureCode::Internal);
        assert_eq!(t.message(), Some("TaskFailed.message"));
        assert_eq!(t.scope(), fb::FailureScope::Attempt);
        assert!(t.retryable());
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskFailed"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("code:"), "{diagnostic}");
        assert!(diagnostic.contains("message:"), "{diagnostic}");
        assert!(diagnostic.contains("scope:"), "{diagnostic}");
        assert!(diagnostic.contains("retryable:"), "{diagnostic}");
    }
}

#[test]
fn task_skipped_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::TaskSkipped::create(&mut b, &fb::TaskSkippedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskSkipped,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskSkipped);
        let t = root.body_as_task_skipped().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskSkipped"));
        assert_eq!(t.task(), None);
        assert_eq!(t.step(), None);
        assert_eq!(t.reason(), fb::SkipReason::FailedTask);
        assert_eq!(t.blocking(), None);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskSkipped"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("reason:"), "{diagnostic}");
        assert!(diagnostic.contains("blocking:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("TaskSkipped.task");
        let step = b.create_string("TaskSkipped.step");
        let blocking = b.create_string("TaskSkipped.blocking");
        let table = fb::TaskSkipped::create(
            &mut b,
            &fb::TaskSkippedArgs {
                task: Some(task),
                step: Some(step),
                reason: fb::SkipReason::Cancelled,
                blocking: Some(blocking),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::TaskSkipped,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::TaskSkipped);
        let t = root.body_as_task_skipped().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("TaskSkipped"));
        assert_eq!(t.task(), Some("TaskSkipped.task"));
        assert_eq!(t.step(), Some("TaskSkipped.step"));
        assert_eq!(t.reason(), fb::SkipReason::Cancelled);
        assert_eq!(t.blocking(), Some("TaskSkipped.blocking"));
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("TaskSkipped"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("step:"), "{diagnostic}");
        assert!(diagnostic.contains("reason:"), "{diagnostic}");
        assert!(diagnostic.contains("blocking:"), "{diagnostic}");
    }
}

#[test]
fn cancel_acknowledged_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::CancelAcknowledged::create(&mut b, &fb::CancelAcknowledgedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::CancelAcknowledged,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::CancelAcknowledged);
        let t = root.body_as_cancel_acknowledged().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("CancelAcknowledged"));

        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("CancelAcknowledged"));
    }
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::CancelAcknowledged::create(&mut b, &fb::CancelAcknowledgedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::CancelAcknowledged,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::CancelAcknowledged);
        let t = root.body_as_cancel_acknowledged().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("CancelAcknowledged"));

        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("CancelAcknowledged"));
    }
}

#[test]
fn cancel_ended_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::CancelEnded::create(&mut b, &fb::CancelEndedArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::CancelEnded,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::CancelEnded);
        let t = root.body_as_cancel_ended().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("CancelEnded"));
        assert_eq!(t.disposition(), fb::CancelDisposition::Cancelled);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("CancelEnded"));
        assert!(diagnostic.contains("disposition:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::CancelEnded::create(
            &mut b,
            &fb::CancelEndedArgs {
                disposition: fb::CancelDisposition::Failed,
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::CancelEnded,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::CancelEnded);
        let t = root.body_as_cancel_ended().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("CancelEnded"));
        assert_eq!(t.disposition(), fb::CancelDisposition::Failed);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("CancelEnded"));
        assert!(diagnostic.contains("disposition:"), "{diagnostic}");
    }
}

#[test]
fn log_reach_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::LogReach::create(&mut b, &fb::LogReachArgs::default());
        b.finish(table, None);
        let t = flatbuffers::root::<fb::LogReach>(b.finished_data()).unwrap();
        assert_eq!(t.task(), None);
        assert_eq!(t.records(), 0);
        assert_eq!(t.bytes(), 0);
        assert_eq!(t.truncated(), 0);
        assert_eq!(t.dropped(), 0);
        assert_eq!(t.dropped_bytes(), 0);
        assert!(!t.drained());
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("LogReach"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("records:"), "{diagnostic}");
        assert!(diagnostic.contains("bytes:"), "{diagnostic}");
        assert!(diagnostic.contains("truncated:"), "{diagnostic}");
        assert!(diagnostic.contains("dropped:"), "{diagnostic}");
        assert!(diagnostic.contains("dropped_bytes:"), "{diagnostic}");
        assert!(diagnostic.contains("drained:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("LogReach.task");
        let table = fb::LogReach::create(
            &mut b,
            &fb::LogReachArgs {
                task: Some(task),
                records: u64::MAX,
                bytes: u64::MAX,
                truncated: u64::MAX,
                dropped: u64::MAX,
                dropped_bytes: u64::MAX,
                drained: true,
            },
        );
        b.finish(table, None);
        let t = flatbuffers::root::<fb::LogReach>(b.finished_data()).unwrap();
        assert_eq!(t.task(), Some("LogReach.task"));
        assert_eq!(t.records(), u64::MAX);
        assert_eq!(t.bytes(), u64::MAX);
        assert_eq!(t.truncated(), u64::MAX);
        assert_eq!(t.dropped(), u64::MAX);
        assert_eq!(t.dropped_bytes(), u64::MAX);
        assert!(t.drained());
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("LogReach"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("records:"), "{diagnostic}");
        assert!(diagnostic.contains("bytes:"), "{diagnostic}");
        assert!(diagnostic.contains("truncated:"), "{diagnostic}");
        assert!(diagnostic.contains("dropped:"), "{diagnostic}");
        assert!(diagnostic.contains("dropped_bytes:"), "{diagnostic}");
        assert!(diagnostic.contains("drained:"), "{diagnostic}");
    }
}

#[test]
fn logs_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::Logs::create(&mut b, &fb::LogsArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::Logs,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::Logs);
        let t = root.body_as_logs().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("Logs"));
        assert!(t.producers().is_none());
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("Logs"));
        assert!(diagnostic.contains("producers:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let item = fb::LogReach::create(
            &mut b,
            &fb::LogReachArgs {
                records: 17,
                ..Default::default()
            },
        );
        let producers = b.create_vector(&[item]);
        let table = fb::Logs::create(
            &mut b,
            &fb::LogsArgs {
                producers: Some(producers),
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::Logs,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::Logs);
        let t = root.body_as_logs().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("Logs"));
        assert_eq!(t.producers().unwrap().get(0).records(), 17);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("Logs"));
        assert!(diagnostic.contains("producers:"), "{diagnostic}");
    }
}

#[test]
fn log_fields_and_defaults() {
    {
        let mut b = FlatBufferBuilder::new();
        let table = fb::Log::create(&mut b, &fb::LogArgs::default());
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::Log,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::Log);
        let t = root.body_as_log().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("Log"));
        assert_eq!(t.task(), None);
        assert_eq!(t.stream(), fb::LogStream::Stdout);
        assert_eq!(t.level(), None);
        assert_eq!(t.bytes(), 0);
        assert_eq!(t.text(), None);
        assert!(!t.partial());
        assert!(!t.continued());
        assert_eq!(t.truncated(), 0);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("Log"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("stream:"), "{diagnostic}");
        assert!(diagnostic.contains("level:"), "{diagnostic}");
        assert!(diagnostic.contains("bytes:"), "{diagnostic}");
        assert!(diagnostic.contains("text:"), "{diagnostic}");
        assert!(diagnostic.contains("partial:"), "{diagnostic}");
        assert!(diagnostic.contains("continued:"), "{diagnostic}");
        assert!(diagnostic.contains("truncated:"), "{diagnostic}");
    }
    {
        let mut b = FlatBufferBuilder::new();
        let task = b.create_string("Log.task");
        let level = b.create_string("Log.level");
        let text = b.create_string("Log.text");
        let table = fb::Log::create(
            &mut b,
            &fb::LogArgs {
                task: Some(task),
                stream: fb::LogStream::Platform,
                level: Some(level),
                bytes: u64::MAX,
                text: Some(text),
                partial: true,
                continued: true,
                truncated: u64::MAX,
            },
        );
        let event = fb::Event::create(
            &mut b,
            &fb::EventArgs {
                body_type: fb::Body::Log,
                body: Some(table.as_union_value()),
                ..Default::default()
            },
        );
        fb::finish_event_buffer(&mut b, event);
        let root = fb::root_as_event(b.finished_data()).unwrap();
        assert_eq!(root.body_type(), fb::Body::Log);
        let t = root.body_as_log().unwrap();
        check_union_accessors(&root);
        assert!(format!("{root:?}").contains("Log"));
        assert_eq!(t.task(), Some("Log.task"));
        assert_eq!(t.stream(), fb::LogStream::Platform);
        assert_eq!(t.level(), Some("Log.level"));
        assert_eq!(t.bytes(), u64::MAX);
        assert_eq!(t.text(), Some("Log.text"));
        assert!(t.partial());
        assert!(t.continued());
        assert_eq!(t.truncated(), u64::MAX);
        let diagnostic = format!("{t:?}");
        assert!(diagnostic.starts_with("Log"));
        assert!(diagnostic.contains("task:"), "{diagnostic}");
        assert!(diagnostic.contains("stream:"), "{diagnostic}");
        assert!(diagnostic.contains("level:"), "{diagnostic}");
        assert!(diagnostic.contains("bytes:"), "{diagnostic}");
        assert!(diagnostic.contains("text:"), "{diagnostic}");
        assert!(diagnostic.contains("partial:"), "{diagnostic}");
        assert!(diagnostic.contains("continued:"), "{diagnostic}");
        assert!(diagnostic.contains("truncated:"), "{diagnostic}");
    }
}

#[test]
fn counters_setters_preserve_other_fields_and_little_endian_values() {
    let mut counters = fb::Counters::default();
    counters.set_morsels(u64::MAX);
    assert_eq!(counters.morsels(), u64::MAX);
    counters.set_rows_in(u64::MAX - 1);
    assert_eq!(counters.rows_in(), u64::MAX - 1);
    counters.set_rows_out(u64::MAX - 2);
    assert_eq!(counters.rows_out(), u64::MAX - 2);
    counters.set_bytes_in(u64::MAX - 3);
    assert_eq!(counters.bytes_in(), u64::MAX - 3);
    counters.set_bytes_out(u64::MAX - 4);
    assert_eq!(counters.bytes_out(), u64::MAX - 4);
    counters.set_skipped(u64::MAX - 5);
    assert_eq!(counters.skipped(), u64::MAX - 5);
    counters.set_errors(u64::MAX - 6);
    assert_eq!(counters.errors(), u64::MAX - 6);
    assert_eq!(counters.morsels(), u64::MAX);
    assert_eq!(counters.rows_in(), u64::MAX - 1);
    assert_eq!(counters.rows_out(), u64::MAX - 2);
    assert_eq!(counters.bytes_in(), u64::MAX - 3);
    assert_eq!(counters.bytes_out(), u64::MAX - 4);
    assert_eq!(counters.skipped(), u64::MAX - 5);
    assert_eq!(counters.errors(), u64::MAX - 6);
    let diagnostic = format!("{counters:?}");
    assert!(diagnostic.contains("morsels:"));
    assert!(diagnostic.contains("rows_in:"));
    assert!(diagnostic.contains("rows_out:"));
    assert!(diagnostic.contains("bytes_in:"));
    assert!(diagnostic.contains("bytes_out:"));
    assert!(diagnostic.contains("skipped:"));
    assert!(diagnostic.contains("errors:"));
}

#[test]
fn element_names_and_unknown_diagnostics() {
    assert_eq!(fb::Element::Source.variant_name(), Some("Source"));
    assert_eq!(format!("{:?}", fb::Element::Source), "Source");
    assert_eq!(fb::Element::Kernel.variant_name(), Some("Kernel"));
    assert_eq!(format!("{:?}", fb::Element::Kernel), "Kernel");
    assert_eq!(fb::Element::Sink.variant_name(), Some("Sink"));
    assert_eq!(format!("{:?}", fb::Element::Sink), "Sink");
    for value in fb::Element::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::Element(255)])
    {
        assert_eq!(
            <fb::Element as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::Element(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::Element(255)), "<UNKNOWN 255>");
}

#[test]
fn wait_reason_names_and_unknown_diagnostics() {
    assert_eq!(
        fb::WaitReason::EnginePreparing.variant_name(),
        Some("EnginePreparing")
    );
    assert_eq!(
        format!("{:?}", fb::WaitReason::EnginePreparing),
        "EnginePreparing"
    );
    assert_eq!(
        fb::WaitReason::AwaitingInput.variant_name(),
        Some("AwaitingInput")
    );
    assert_eq!(
        format!("{:?}", fb::WaitReason::AwaitingInput),
        "AwaitingInput"
    );
    assert_eq!(fb::WaitReason::PortCall.variant_name(), Some("PortCall"));
    assert_eq!(format!("{:?}", fb::WaitReason::PortCall), "PortCall");
    for value in fb::WaitReason::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::WaitReason(255)])
    {
        assert_eq!(
            <fb::WaitReason as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::WaitReason(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::WaitReason(255)), "<UNKNOWN 255>");
}

#[test]
fn phase_names_and_unknown_diagnostics() {
    assert_eq!(fb::Phase::Calibrating.variant_name(), Some("Calibrating"));
    assert_eq!(format!("{:?}", fb::Phase::Calibrating), "Calibrating");
    assert_eq!(fb::Phase::Reading.variant_name(), Some("Reading"));
    assert_eq!(format!("{:?}", fb::Phase::Reading), "Reading");
    assert_eq!(fb::Phase::Applying.variant_name(), Some("Applying"));
    assert_eq!(format!("{:?}", fb::Phase::Applying), "Applying");
    assert_eq!(fb::Phase::Writing.variant_name(), Some("Writing"));
    assert_eq!(format!("{:?}", fb::Phase::Writing), "Writing");
    assert_eq!(fb::Phase::Draining.variant_name(), Some("Draining"));
    assert_eq!(format!("{:?}", fb::Phase::Draining), "Draining");
    assert_eq!(fb::Phase::Committing.variant_name(), Some("Committing"));
    assert_eq!(format!("{:?}", fb::Phase::Committing), "Committing");
    for value in fb::Phase::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::Phase(255)])
    {
        assert_eq!(
            <fb::Phase as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::Phase(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::Phase(255)), "<UNKNOWN 255>");
}

#[test]
fn task_outcome_names_and_unknown_diagnostics() {
    assert_eq!(fb::TaskOutcome::Succeeded.variant_name(), Some("Succeeded"));
    assert_eq!(format!("{:?}", fb::TaskOutcome::Succeeded), "Succeeded");
    assert_eq!(fb::TaskOutcome::Stopped.variant_name(), Some("Stopped"));
    assert_eq!(format!("{:?}", fb::TaskOutcome::Stopped), "Stopped");
    assert_eq!(fb::TaskOutcome::Cancelled.variant_name(), Some("Cancelled"));
    assert_eq!(format!("{:?}", fb::TaskOutcome::Cancelled), "Cancelled");
    for value in fb::TaskOutcome::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::TaskOutcome(255)])
    {
        assert_eq!(
            <fb::TaskOutcome as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::TaskOutcome(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::TaskOutcome(255)), "<UNKNOWN 255>");
}

#[test]
fn step_outcome_names_and_unknown_diagnostics() {
    assert_eq!(fb::StepOutcome::Failed.variant_name(), Some("Failed"));
    assert_eq!(format!("{:?}", fb::StepOutcome::Failed), "Failed");
    assert_eq!(fb::StepOutcome::Cancelled.variant_name(), Some("Cancelled"));
    assert_eq!(format!("{:?}", fb::StepOutcome::Cancelled), "Cancelled");
    assert_eq!(fb::StepOutcome::Stopped.variant_name(), Some("Stopped"));
    assert_eq!(format!("{:?}", fb::StepOutcome::Stopped), "Stopped");
    assert_eq!(fb::StepOutcome::Skipped.variant_name(), Some("Skipped"));
    assert_eq!(format!("{:?}", fb::StepOutcome::Skipped), "Skipped");
    assert_eq!(fb::StepOutcome::Succeeded.variant_name(), Some("Succeeded"));
    assert_eq!(format!("{:?}", fb::StepOutcome::Succeeded), "Succeeded");
    for value in fb::StepOutcome::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::StepOutcome(255)])
    {
        assert_eq!(
            <fb::StepOutcome as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::StepOutcome(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::StepOutcome(255)), "<UNKNOWN 255>");
}

#[test]
fn failure_code_names_and_unknown_diagnostics() {
    assert_eq!(
        fb::FailureCode::SpecRefused.variant_name(),
        Some("SpecRefused")
    );
    assert_eq!(format!("{:?}", fb::FailureCode::SpecRefused), "SpecRefused");
    assert_eq!(
        fb::FailureCode::KernelError.variant_name(),
        Some("KernelError")
    );
    assert_eq!(format!("{:?}", fb::FailureCode::KernelError), "KernelError");
    assert_eq!(
        fb::FailureCode::BudgetExceeded.variant_name(),
        Some("BudgetExceeded")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::BudgetExceeded),
        "BudgetExceeded"
    );
    assert_eq!(
        fb::FailureCode::MemoryRefused.variant_name(),
        Some("MemoryRefused")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::MemoryRefused),
        "MemoryRefused"
    );
    assert_eq!(
        fb::FailureCode::SourceError.variant_name(),
        Some("SourceError")
    );
    assert_eq!(format!("{:?}", fb::FailureCode::SourceError), "SourceError");
    assert_eq!(fb::FailureCode::SinkError.variant_name(), Some("SinkError"));
    assert_eq!(format!("{:?}", fb::FailureCode::SinkError), "SinkError");
    assert_eq!(
        fb::FailureCode::AllocRefused.variant_name(),
        Some("AllocRefused")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::AllocRefused),
        "AllocRefused"
    );
    assert_eq!(fb::FailureCode::IoError.variant_name(), Some("IoError"));
    assert_eq!(format!("{:?}", fb::FailureCode::IoError), "IoError");
    assert_eq!(
        fb::FailureCode::StagingError.variant_name(),
        Some("StagingError")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::StagingError),
        "StagingError"
    );
    assert_eq!(
        fb::FailureCode::ConvertError.variant_name(),
        Some("ConvertError")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::ConvertError),
        "ConvertError"
    );
    assert_eq!(
        fb::FailureCode::ConfigRefused.variant_name(),
        Some("ConfigRefused")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::ConfigRefused),
        "ConfigRefused"
    );
    assert_eq!(
        fb::FailureCode::PlanRefused.variant_name(),
        Some("PlanRefused")
    );
    assert_eq!(format!("{:?}", fb::FailureCode::PlanRefused), "PlanRefused");
    assert_eq!(
        fb::FailureCode::ResumeRefused.variant_name(),
        Some("ResumeRefused")
    );
    assert_eq!(
        format!("{:?}", fb::FailureCode::ResumeRefused),
        "ResumeRefused"
    );
    assert_eq!(
        fb::FailureCode::Unsupported.variant_name(),
        Some("Unsupported")
    );
    assert_eq!(format!("{:?}", fb::FailureCode::Unsupported), "Unsupported");
    assert_eq!(fb::FailureCode::Cancelled.variant_name(), Some("Cancelled"));
    assert_eq!(format!("{:?}", fb::FailureCode::Cancelled), "Cancelled");
    assert_eq!(fb::FailureCode::Internal.variant_name(), Some("Internal"));
    assert_eq!(format!("{:?}", fb::FailureCode::Internal), "Internal");
    for value in fb::FailureCode::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::FailureCode(255)])
    {
        assert_eq!(
            <fb::FailureCode as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::FailureCode(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::FailureCode(255)), "<UNKNOWN 255>");
}

#[test]
fn failure_scope_names_and_unknown_diagnostics() {
    assert_eq!(fb::FailureScope::Task.variant_name(), Some("Task"));
    assert_eq!(format!("{:?}", fb::FailureScope::Task), "Task");
    assert_eq!(fb::FailureScope::Attempt.variant_name(), Some("Attempt"));
    assert_eq!(format!("{:?}", fb::FailureScope::Attempt), "Attempt");
    for value in fb::FailureScope::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::FailureScope(255)])
    {
        assert_eq!(
            <fb::FailureScope as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::FailureScope(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::FailureScope(255)), "<UNKNOWN 255>");
}

#[test]
fn skip_reason_names_and_unknown_diagnostics() {
    assert_eq!(
        fb::SkipReason::FailedTask.variant_name(),
        Some("FailedTask")
    );
    assert_eq!(format!("{:?}", fb::SkipReason::FailedTask), "FailedTask");
    assert_eq!(
        fb::SkipReason::AttemptFailed.variant_name(),
        Some("AttemptFailed")
    );
    assert_eq!(
        format!("{:?}", fb::SkipReason::AttemptFailed),
        "AttemptFailed"
    );
    assert_eq!(fb::SkipReason::Cancelled.variant_name(), Some("Cancelled"));
    assert_eq!(format!("{:?}", fb::SkipReason::Cancelled), "Cancelled");
    for value in fb::SkipReason::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::SkipReason(255)])
    {
        assert_eq!(
            <fb::SkipReason as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::SkipReason(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::SkipReason(255)), "<UNKNOWN 255>");
}

#[test]
fn cancel_disposition_names_and_unknown_diagnostics() {
    assert_eq!(
        fb::CancelDisposition::Cancelled.variant_name(),
        Some("Cancelled")
    );
    assert_eq!(
        format!("{:?}", fb::CancelDisposition::Cancelled),
        "Cancelled"
    );
    assert_eq!(
        fb::CancelDisposition::Completed.variant_name(),
        Some("Completed")
    );
    assert_eq!(
        format!("{:?}", fb::CancelDisposition::Completed),
        "Completed"
    );
    assert_eq!(fb::CancelDisposition::Failed.variant_name(), Some("Failed"));
    assert_eq!(format!("{:?}", fb::CancelDisposition::Failed), "Failed");
    for value in fb::CancelDisposition::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::CancelDisposition(255)])
    {
        assert_eq!(
            <fb::CancelDisposition as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::CancelDisposition(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::CancelDisposition(255)), "<UNKNOWN 255>");
}

#[test]
fn log_stream_names_and_unknown_diagnostics() {
    assert_eq!(fb::LogStream::Stdout.variant_name(), Some("Stdout"));
    assert_eq!(format!("{:?}", fb::LogStream::Stdout), "Stdout");
    assert_eq!(fb::LogStream::Stderr.variant_name(), Some("Stderr"));
    assert_eq!(format!("{:?}", fb::LogStream::Stderr), "Stderr");
    assert_eq!(fb::LogStream::Structured.variant_name(), Some("Structured"));
    assert_eq!(format!("{:?}", fb::LogStream::Structured), "Structured");
    assert_eq!(fb::LogStream::Platform.variant_name(), Some("Platform"));
    assert_eq!(format!("{:?}", fb::LogStream::Platform), "Platform");
    for value in fb::LogStream::ENUM_VALUES
        .iter()
        .copied()
        .chain([fb::LogStream(255)])
    {
        assert_eq!(
            <fb::LogStream as flatbuffers::EndianScalar>::from_little_endian(
                flatbuffers::EndianScalar::to_little_endian(value)
            ),
            value
        );
    }
    assert_eq!(fb::LogStream(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::LogStream(255)), "<UNKNOWN 255>");
}
/// The raw schema deliberately permits absent fields and future enum values. The event
/// reader applies the semantic contract after verification, with a stable field diagnosis.
fn assert_field_refusal(bytes: &[u8], field: &'static str, reason: &str) {
    fb::root_as_event(bytes).expect("structurally valid, independently built event");
    let record = moruna_record::encode(
        1,
        0,
        moruna_event::Kind::CancelAcknowledged.subject(),
        "3-process-1",
        &[("attempt", "3")],
        bytes,
    )
    .unwrap();
    let error = moruna_event::decode(&record).unwrap_err();
    assert_eq!(
        error,
        moruna_event::DecodeError::Field {
            field,
            reason: reason.into()
        }
    );
    assert_eq!(error.to_string(), format!("{field}: {reason}"));
}

macro_rules! unknown_enum {
    ($test:ident, $table:ident, $args:ident, $field:ident, $enum:ident, $diagnostic:literal, {$($extra:ident: $value:ident),* $(,)?}) => {
        #[test]
        fn $test() {
            let mut b = FlatBufferBuilder::new();
            let name = b.create_string("task");
            let table = fb::$table::create(&mut b, &fb::$args {
                $field: fb::$enum(255), $($extra: Some(name),)*
                ..Default::default()
            });
            let root = fb::Event::create(&mut b, &fb::EventArgs {
                start: Some(name), body_type: fb::Body::$table,
                body: Some(table.as_union_value()), ..Default::default()
            });
            fb::finish_event_buffer(&mut b, root);
            assert_field_refusal(b.finished_data(), $diagnostic, "255 is not a value this build knows");
        }
    };
}
unknown_enum!(unknown_element_is_refused, TaskRegistered, TaskRegisteredArgs, element, Element, "element", {task: name});
unknown_enum!(unknown_wait_is_refused, TaskWaiting, TaskWaitingArgs, reason, WaitReason, "reason", {task: name});
unknown_enum!(unknown_started_phase_is_refused, TaskStarted, TaskStartedArgs, phase, Phase, "phase", {task: name});
unknown_enum!(unknown_changed_phase_is_refused, TaskPhase, TaskPhaseArgs, phase, Phase, "phase", {task: name});
unknown_enum!(unknown_task_outcome_is_refused, TaskCompleted, TaskCompletedArgs, outcome, TaskOutcome, "outcome", {task: name});
unknown_enum!(unknown_step_outcome_is_refused, StepEnded, StepEndedArgs, outcome, StepOutcome, "outcome", {step: name, plan: name});
unknown_enum!(unknown_failure_code_is_refused, TaskFailed, TaskFailedArgs, code, FailureCode, "code", {message: name});
unknown_enum!(unknown_failure_scope_is_refused, TaskFailed, TaskFailedArgs, scope, FailureScope, "scope", {message: name});
unknown_enum!(unknown_skip_is_refused, TaskSkipped, TaskSkippedArgs, reason, SkipReason, "reason", {task: name});
#[test]
fn unknown_cancel_is_refused() {
    let mut b = FlatBufferBuilder::new();
    let table = fb::CancelEnded::create(
        &mut b,
        &fb::CancelEndedArgs {
            disposition: fb::CancelDisposition(255),
        },
    );
    let root = fb::Event::create(
        &mut b,
        &fb::EventArgs {
            body_type: fb::Body::CancelEnded,
            body: Some(table.as_union_value()),
            ..Default::default()
        },
    );
    fb::finish_event_buffer(&mut b, root);
    assert_field_refusal(
        b.finished_data(),
        "disposition",
        "255 is not a value this build knows",
    );
}
unknown_enum!(unknown_log_stream_is_refused, Log, LogArgs, stream, LogStream, "stream", {text: name});

#[test]
fn buffer_framing_and_default_root_are_explicit() {
    let mut b = FlatBufferBuilder::new();
    let root = fb::Event::create(&mut b, &fb::EventArgs::default());
    fb::finish_event_buffer(&mut b, root);
    let bytes = b.finished_data();
    assert!(fb::event_buffer_has_identifier(bytes));
    let root = fb::root_as_event(bytes).unwrap();
    assert_eq!(root.start(), None);
    assert_eq!(root.t_us(), 0);
    assert_eq!(root.body_type(), fb::Body::NONE);
    assert!(root.body().is_none());
    let diagnostic = format!("{root:?}");
    assert!(diagnostic.contains("body_type: NONE"), "{diagnostic}");
    check_union_accessors(&root);
    assert_field_refusal(bytes, "body", "0 is not a value this build knows");
    assert!(fb::root_as_event_with_opts(&flatbuffers::VerifierOptions::default(), bytes).is_ok());
    let mut broken = bytes.to_vec();
    broken[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(fb::root_as_event(&broken).is_err());
    assert!(fb::root_as_event(&bytes[..4]).is_err());
    assert!(fb::size_prefixed_root_as_event(bytes).is_err());
    assert!(!fb::event_size_prefixed_buffer_has_identifier(bytes));

    let mut b = FlatBufferBuilder::new();
    let start = b.create_string("process");
    let root = fb::Event::create(
        &mut b,
        &fb::EventArgs {
            start: Some(start),
            t_us: u64::MAX,
            ..Default::default()
        },
    );
    fb::finish_size_prefixed_event_buffer(&mut b, root);
    let bytes = b.finished_data();
    assert!(fb::event_size_prefixed_buffer_has_identifier(bytes));
    assert!(!fb::event_buffer_has_identifier(bytes));
    let root = fb::size_prefixed_root_as_event(bytes).unwrap();
    assert_eq!(root.start(), Some("process"));
    assert_eq!(root.t_us(), u64::MAX);
    assert_eq!(
        fb::size_prefixed_root_as_event_with_opts(&flatbuffers::VerifierOptions::default(), bytes)
            .unwrap()
            .t_us(),
        u64::MAX
    );
    assert!(fb::root_as_event(bytes).is_err());
}

#[test]
fn unknown_union_is_verified_but_not_misread_as_a_known_event() {
    assert_eq!(fb::Body(255).variant_name(), None);
    assert_eq!(format!("{:?}", fb::Body(255)), "<UNKNOWN 255>");
    for body in fb::Body::ENUM_VALUES {
        assert!(body.variant_name().is_some());
        assert_eq!(format!("{body:?}"), body.variant_name().unwrap());
    }
    let mut b = FlatBufferBuilder::new();
    let table = fb::CancelAcknowledged::create(&mut b, &fb::CancelAcknowledgedArgs::default());
    let root = fb::Event::create(
        &mut b,
        &fb::EventArgs {
            body_type: fb::Body(255),
            body: Some(table.as_union_value()),
            ..Default::default()
        },
    );
    fb::finish_event_buffer(&mut b, root);
    let root = fb::root_as_event(b.finished_data()).unwrap();
    check_union_accessors(&root);
    assert_field_refusal(
        b.finished_data(),
        "body",
        "255 is not a value this build knows",
    );
}
