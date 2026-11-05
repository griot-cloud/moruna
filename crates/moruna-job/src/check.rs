//! Checking a job against a catalogue, without running anything (SDD 22 e.2, f.1).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::catalogue::Catalogue;
use crate::doc::is_task_id;
use crate::job::{Job, Task};
use crate::kind::{Declaration, Fact, Flow, Role};
use crate::rules::Known;
use crate::schema::{self, Step, Violation, Wrong};

/// One thing wrong with a document: where, what kind of wrong, and a sentence a person can act
/// on, in the voice of the runtime's own refusals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Where the problem is: `tasks[0].kind`, `tasks[2].settings.url`, `edges[3]`.
    pub field: String,
    /// What kind of problem it is, for a program that treats kinds of problem differently.
    pub code: RefusalCode,
    /// One sentence that says what to change.
    pub message: String,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// What kind of problem a [`Refusal`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefusalCode {
    /// A task's id is not a task id, or names two tasks.
    TaskId,
    /// No kind of that role is declared under the name the task gives.
    UnknownKind {
        /// The role the task is a kind of.
        role: Role,
        /// The kinds of that role the catalogue declares, sorted.
        known: Vec<String>,
    },
    /// The task's settings are not what the kind's settings schema allows.
    Settings {
        /// The value at fault, as a JSON pointer into the task's settings (`/options/columns/0`;
        /// empty is the settings as a whole).
        path: String,
    },
    /// Flows that do not fit: a kind whose flows do not fit its role, a task whose ports do not
    /// fit its role, or an edge whose flow is not what its producer gives or its consumer takes.
    Flow,
    /// An edge names a task the job does not have, or a port its task does not declare.
    UnknownPort,
    /// An input port no edge feeds, or an output port no edge reads.
    Unconnected,
    /// More than one edge into one input port: an input port takes one edge, and a task that
    /// takes several inputs declares a port for each.
    Crowded,
    /// The edges go round in a cycle.
    Cycle,
    /// A barrier with nothing on one side, or an edge that goes around one.
    Barrier,
    /// The columns an edge carries do not fit the schema rules of the served kind it feeds, or a
    /// served kind's output rules name a column its input does not have (SDD 22 e.6).
    Schema {
        /// The column at fault.
        column: String,
    },
    /// The kind needs a contract (`needs.contracts` is `yes`) and the job carries none.
    Contract,
    /// A task's inline definition is not one (SDD 18 e.1), differs from its kind's declaration,
    /// or does not describe the task: its params against its schema, its needs, the schema one
    /// task gives against the schema the next takes.
    Definition,
}

/// Check `job` against `catalogue`. Every refusal is collected, not only the first, in the order
/// the document is read: each task (its id, its kind, its kind's flows, its settings, its ports),
/// then each edge (its ends, its flow), then each task's ports (fed by exactly one edge, read by
/// at least one), then the graph (a cycle, a barrier), then the columns along the edges into
/// served kinds (SDD 22 e.6), then the contracts. Nothing is built or run.
pub fn check(job: &Job, catalogue: &Catalogue) -> Result<(), Vec<Refusal>> {
    walk(job, Some(catalogue))
}

/// The refusals of `check` that need no catalogue: task ids, ports, edges, a cycle and barriers,
/// in the same order. What a program that builds a job from its own registry checks first.
pub fn check_graph(job: &Job) -> Result<(), Vec<Refusal>> {
    walk(job, None)
}

fn walk(job: &Job, catalogue: Option<&Catalogue>) -> Result<(), Vec<Refusal>> {
    let mut refusals = Vec::new();
    let mut needing = Vec::new();
    let mut declared: Vec<Option<&Declaration>> = Vec::with_capacity(job.tasks.len());
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, task) in job.tasks.iter().enumerate() {
        let field = job.field(i);
        task_id(&field, task, i, &mut ids, &mut refusals);
        declared.push(catalogue.and_then(|catalogue| {
            task_entry(&field, task, catalogue, &mut refusals, &mut needing)
        }));
        ports_fit(&field, task, &mut refusals);
    }
    for (i, edge) in job.edges.iter().enumerate() {
        let field = format!("edges[{i}]");
        let from = end_of(job, &field, "from", &edge.from.task, &edge.from.port, false);
        let to = end_of(job, &field, "to", &edge.to.task, &edge.to.port, true);
        let (from, to) = match (from, to) {
            (Ok(from), Ok(to)) => (from, to),
            (from, to) => {
                refusals.extend(from.err());
                refusals.extend(to.err());
                continue;
            }
        };
        let gives = declared[from].map(|d| d.outputs);
        let takes = declared[to].map(|d| d.inputs);
        let unfit = [
            ("gives", gives, &edge.from.task),
            ("takes", takes, &edge.to.task),
        ]
        .into_iter()
        .find(|(_, flow, _)| flow.is_some_and(|f| f != edge.flow));
        if let Some((verb, Some(flow), task)) = unfit {
            refusals.push(Refusal {
                field: field.clone(),
                code: RefusalCode::Flow,
                message: format!(
                    "carries {}, and `{task}` {verb} {}: an edge carries what its producer gives \
                     and its consumer takes",
                    edge.flow.as_str(),
                    flow.as_str()
                ),
            });
        }
    }
    schemas_fit(job, &mut refusals);
    for (i, task) in job.tasks.iter().enumerate() {
        for port in &task.inputs {
            let into = job.edges_into(&task.id, port);
            let field = format!("{}.inputs.{port}", job.field(i));
            match into.as_slice() {
                [] => refusals.push(Refusal {
                    field,
                    code: RefusalCode::Unconnected,
                    message: format!(
                        "no edge feeds `{}`'s input `{port}`: add an edge into it, or drop the port",
                        task.id
                    ),
                }),
                [_] => {}
                many => refusals.push(Refusal {
                    field,
                    code: RefusalCode::Crowded,
                    message: format!(
                        "{} edges ({}) feed `{}`'s input `{port}`, which takes one: give the \
                         task a port for each input",
                        many.len(),
                        many.iter()
                            .map(|e| format!("edges[{e}]"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        task.id
                    ),
                }),
            }
        }
        for port in &task.outputs {
            if job.edges_from(&task.id, port).is_empty() {
                refusals.push(Refusal {
                    field: format!("{}.outputs.{port}", job.field(i)),
                    code: RefusalCode::Unconnected,
                    message: format!(
                        "no edge reads `{}`'s output `{port}`: add an edge out of it, or drop \
                         the port",
                        task.id
                    ),
                });
            }
        }
    }
    let acyclic = match job.topological_order() {
        Ok(_) => true,
        Err(on_cycle) => {
            let named: Vec<String> = on_cycle
                .iter()
                .map(|&i| format!("`{}`", job.tasks[i].id))
                .collect();
            refusals.push(Refusal {
                field: on_cycle
                    .first()
                    .map_or_else(|| "edges".to_string(), |&i| job.field(i)),
                code: RefusalCode::Cycle,
                message: format!(
                    "the edges go round in a cycle through {}: a job is a DAG, so no task may \
                     feed itself",
                    named.join(", ")
                ),
            });
            false
        }
    };
    if acyclic {
        barriers(job, &mut refusals);
        columns(job, &declared, &mut refusals);
    }
    if job.contracts.is_empty() {
        for (field, kind) in needing {
            refusals.push(Refusal {
                field: "contracts".to_string(),
                code: RefusalCode::Contract,
                message: format!(
                    "`{kind}` at `{field}` reads or writes under a contract, and the job carries \
                     none: put it in `contracts`"
                ),
            });
        }
    }
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(refusals)
    }
}

/// A task's id is a task id and names no other task.
fn task_id<'a>(
    field: &str,
    task: &'a Task,
    i: usize,
    ids: &mut BTreeMap<&'a str, usize>,
    refusals: &mut Vec<Refusal>,
) {
    if !is_task_id(&task.id) {
        refusals.push(Refusal {
            field: format!("{field}.id"),
            code: RefusalCode::TaskId,
            message: format!(
                "{:?} is not a task id: 1 to 128 of ASCII letters, digits, `_`, `-`, `.` and \
                 `:`, starting with a letter or a digit",
                task.id
            ),
        });
    } else if let Some(first) = ids.insert(&task.id, i) {
        refusals.push(Refusal {
            field: format!("{field}.id"),
            code: RefusalCode::TaskId,
            message: format!(
                "{:?} names tasks[{first}] already: a task id names one task",
                task.id
            ),
        });
    }
    if let Some(bad) = task.members.iter().find(|m| !is_task_id(m)) {
        refusals.push(Refusal {
            field: format!("{field}.members"),
            code: RefusalCode::TaskId,
            message: format!("{bad:?} is not a task id"),
        });
    }
}

/// One task's kind, its flows, its settings, and whether it needs a contract.
fn task_entry<'c>(
    field: &str,
    task: &Task,
    catalogue: &'c Catalogue,
    refusals: &mut Vec<Refusal>,
    needing: &mut Vec<(String, String)>,
) -> Option<&'c Declaration> {
    let role = task.role;
    let Some(declared) = catalogue.get(role, &task.kind) else {
        refusals.push(unknown(field, role, &task.kind, catalogue));
        return None;
    };
    if !flows_fit(declared, role) {
        refusals.push(Refusal {
            field: field.to_string(),
            code: RefusalCode::Flow,
            message: format!(
                "`{}` takes {} and gives {}, which a {} does not: a source gives batches, a \
                 function takes and gives batches, a sink takes batches",
                declared.name,
                declared.inputs.as_str(),
                declared.outputs.as_str(),
                role
            ),
        });
    }
    refusals.extend(settings_refusals(
        field,
        &task.kind,
        &declared.settings,
        &task.settings,
    ));
    if let Some(definition) = &task.definition {
        definition_refusals(field, task, declared, definition, refusals);
    }
    if declared.needs.contracts == Fact::Yes {
        needing.push((field.to_string(), task.kind.clone()));
    }
    Some(declared)
}

/// The kind whose tasks carry the definition their own code makes, not their kind's.
pub const IMAGE_KIND: &str = "image";

/// A task's inline definition checked: one a document can carry; its kind's own declaration,
/// for a kind whose tasks have no code of their own; for an image task, a definition its params
/// and its needs fit.
fn definition_refusals(
    field: &str,
    task: &Task,
    declared: &Declaration,
    definition: &Declaration,
    refusals: &mut Vec<Refusal>,
) {
    let refuse = |at: String, message: String| Refusal {
        field: at,
        code: RefusalCode::Definition,
        message,
    };
    let at = format!("{field}.definition");
    if let Err(why) = definition.check_definition(task.role) {
        refusals.push(refuse(at.clone(), why));
        return;
    }
    if task.kind != IMAGE_KIND {
        if definition != declared {
            refusals.push(refuse(
                at,
                format!(
                    "is not the declaration of the kind `{}` this program has: a task of a kind \
                     carries its kind's declaration as it is",
                    task.kind
                ),
            ));
        }
        return;
    }
    let params = task
        .settings
        .get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let Value::Object(params) = params else {
        refusals.push(refuse(
            format!("{field}.settings.params"),
            "an image task's params are an object".into(),
        ));
        return;
    };
    refusals.extend(
        settings_refusals(
            &format!("{field}.params"),
            &definition.name,
            &definition.settings,
            &params,
        )
        .into_iter()
        .map(|r| Refusal {
            code: RefusalCode::Definition,
            ..r
        }),
    );
    let needs: Vec<&str> = task
        .settings
        .get("needs")
        .and_then(Value::as_array)
        .map(|n| n.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let required: Vec<&str> = definition
        .requires
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    if needs != required {
        refusals.push(refuse(
            format!("{field}.settings.needs"),
            format!(
                "names {needs:?}, and the task's definition requires {required:?}: a task's needs \
                 are what its definition says it requires"
            ),
        ));
    }
}

/// Along every edge into a task of one input whose two ends' definitions declare one, the
/// schema the producer gives is the schema the consumer takes (SDD 18 e.1).
fn schemas_fit(job: &Job, refusals: &mut Vec<Refusal>) {
    for (i, task) in job.tasks.iter().enumerate() {
        let Some(takes) = task.definition.as_ref().and_then(|d| d.takes.as_ref()) else {
            continue;
        };
        if task.inputs.len() != 1 {
            continue;
        }
        for edge in job.edges.iter().filter(|e| e.to.task == task.id) {
            let Some(gives) = job
                .tasks
                .iter()
                .find(|t| t.id == edge.from.task)
                .and_then(|t| t.definition.as_ref())
                .and_then(|d| d.gives.as_ref())
            else {
                continue;
            };
            if gives != takes {
                refusals.push(Refusal {
                    field: format!("{}.inputs.{}", job.field(i), edge.to.port),
                    code: RefusalCode::Definition,
                    message: format!(
                        "`{}` takes {takes} and `{}` gives {gives}",
                        task.id, edge.from.task
                    ),
                });
            }
        }
    }
}

fn flows_fit(declared: &Declaration, role: Role) -> bool {
    match role {
        Role::Source => declared.inputs != Flow::Batches && declared.outputs == Flow::Batches,
        Role::Kernel => declared.inputs == Flow::Batches && declared.outputs == Flow::Batches,
        Role::Sink => declared.inputs == Flow::Batches && declared.outputs != Flow::Batches,
    }
}

/// A task's ports fit its role: a source has output ports and no input port, a sink input
/// ports and no output port, a kernel both; no port is named twice.
fn ports_fit(field: &str, task: &Task, refusals: &mut Vec<Refusal>) {
    let (inputs, outputs) = match task.role {
        Role::Source => ("none", "at least one"),
        Role::Kernel => ("at least one", "at least one"),
        Role::Sink => ("at least one", "none"),
    };
    let fits = |count: usize, rule: &str| match rule {
        "none" => count == 0,
        _ => count >= 1,
    };
    if !fits(task.inputs.len(), inputs) || !fits(task.outputs.len(), outputs) {
        refusals.push(Refusal {
            field: field.to_string(),
            code: RefusalCode::Flow,
            message: format!(
                "`{}` is a {} with {} input ports and {} output ports; a {} has {inputs} input \
                 ports and {outputs} output ports",
                task.id,
                task.role,
                task.inputs.len(),
                task.outputs.len(),
                task.role
            ),
        });
    }
    for (side, ports) in [("inputs", &task.inputs), ("outputs", &task.outputs)] {
        let mut seen = BTreeSet::new();
        for port in ports {
            if !seen.insert(port.as_str()) {
                refusals.push(Refusal {
                    field: format!("{field}.{side}.{port}"),
                    code: RefusalCode::UnknownPort,
                    message: format!("`{}` names the port `{port}` twice", task.id),
                });
            }
        }
    }
}

/// The task an edge's end names and the port it names on it, or the refusal of an end the job
/// does not have.
fn end_of(
    job: &Job,
    field: &str,
    end: &str,
    task: &str,
    port: &str,
    input: bool,
) -> Result<usize, Refusal> {
    let Some(at) = job.task_index(task) else {
        return Err(Refusal {
            field: format!("{field}.{end}"),
            code: RefusalCode::UnknownPort,
            message: format!("no task `{task}`: an edge joins two tasks of the job"),
        });
    };
    let (ports, side) = if input {
        (&job.tasks[at].inputs, "input")
    } else {
        (&job.tasks[at].outputs, "output")
    };
    if !ports.iter().any(|p| p == port) {
        let has = if ports.is_empty() {
            format!("`{task}` has no {side} ports")
        } else {
            format!(
                "`{task}`'s {side} ports are {}",
                ports
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(Refusal {
            field: format!("{field}.{end}"),
            code: RefusalCode::UnknownPort,
            message: format!("no {side} port `{port}` on `{task}`: {has}"),
        });
    }
    Ok(at)
}

/// A barrier task needs something on both sides, and no edge may go around a barrier task or a
/// barrier edge: an edge from something upstream of it to something downstream of it that does
/// not pass through it would carry what the barrier enforces past it (SDD 22 e.5).
fn barriers(job: &Job, refusals: &mut Vec<Refusal>) {
    let below = job.descendants();
    let above = |task: usize| -> BTreeSet<usize> {
        (0..job.tasks.len())
            .filter(|&a| below[a].contains(&task))
            .collect()
    };
    let index = |id: &str| job.task_index(id);
    for (i, task) in job.tasks.iter().enumerate() {
        if !task.barrier {
            continue;
        }
        let upstream = above(i);
        if upstream.is_empty() || below[i].is_empty() {
            refusals.push(Refusal {
                field: job.field(i),
                code: RefusalCode::Barrier,
                message: format!(
                    "`{}` is a barrier with nothing {} it: a barrier stands between tasks",
                    task.id,
                    if upstream.is_empty() {
                        "upstream of"
                    } else {
                        "downstream of"
                    }
                ),
            });
            continue;
        }
        for (e, edge) in job.edges.iter().enumerate() {
            if let (Some(a), Some(b)) = (index(&edge.from.task), index(&edge.to.task))
                && a != i
                && b != i
                && upstream.contains(&a)
                && below[i].contains(&b)
            {
                refusals.push(bypass(e, &edge.from.task, &edge.to.task, &task.id));
            }
        }
    }
    for (g, gate) in job.edges.iter().enumerate() {
        if !gate.barrier {
            continue;
        }
        let (Some(a), Some(b)) = (index(&gate.from.task), index(&gate.to.task)) else {
            continue;
        };
        let mut upstream = above(a);
        upstream.insert(a);
        let mut downstream = below[b].clone();
        downstream.insert(b);
        for (e, edge) in job.edges.iter().enumerate() {
            if e == g {
                continue;
            }
            if let (Some(x), Some(y)) = (index(&edge.from.task), index(&edge.to.task))
                && upstream.contains(&x)
                && downstream.contains(&y)
            {
                refusals.push(bypass(
                    e,
                    &edge.from.task,
                    &edge.to.task,
                    &format!("edges[{g}]"),
                ));
            }
        }
    }
}

fn bypass(e: usize, from: &str, to: &str, barrier: &str) -> Refusal {
    Refusal {
        field: format!("edges[{e}]"),
        code: RefusalCode::Barrier,
        message: format!(
            "goes from `{from}` to `{to}` around the barrier `{barrier}`: what crosses a barrier \
             passes through it"
        ),
    }
}

/// The refusal of a kind no declaration is found for, in the voice of the runtime's (SDD 18 KD-I1).
fn unknown(field: &str, role: Role, name: &str, catalogue: &Catalogue) -> Refusal {
    let known: Vec<String> = catalogue.names(role).map(str::to_string).collect();
    let has = if known.is_empty() {
        format!("this catalogue has no {role} kinds")
    } else {
        format!("this catalogue {} {}", role.verb(), known.join(", "))
    };
    Refusal {
        field: format!("{field}.kind"),
        message: format!("no {role} kind `{name}` is declared; {has}"),
        code: RefusalCode::UnknownKind { role, known },
    }
}

/// What a kind's settings schema refuses of a task's settings (SDD 22 e.2, step 3): one
/// [`RefusalCode::Settings`] refusal per violation, at `<field>.settings.<path>`, for the task at
/// `field` of the kind `kind`. What [`check`] says of a task's settings, and what the runtime says
/// of the settings of a kind it has no settings type for (a served kind, SDD 18 KD-I11).
pub fn settings_refusals(
    field: &str,
    kind: &str,
    schema: &Value,
    settings: &Map<String, Value>,
) -> Vec<Refusal> {
    let settings = Value::Object(settings.clone());
    schema::validate(schema, schema, &settings)
        .iter()
        .map(|violation| settings_refusal(field, kind, violation))
        .collect()
}

/// The columns along the edges into served kinds (SDD 22 e.6), as far as the job knows them
/// without running: what a served kind's output rules say it gives, from its input where they
/// are relative, compared with what the served kind it feeds says it takes. A column a task may
/// have that the job cannot know of is never refused.
fn columns(job: &Job, declared: &[Option<&Declaration>], refusals: &mut Vec<Refusal>) {
    let Ok(order) = job.topological_order() else {
        return;
    };
    let mut known: Vec<Option<Known>> = vec![None; job.tasks.len()];
    for &i in &order {
        let task = &job.tasks[i];
        let Some(served) = declared[i].and_then(|d| d.served.as_ref()) else {
            continue;
        };
        // A catalogue admits only rules that parse (JB-I4); a declaration put in another way
        // has its rules read as none.
        let input_rules = served.input_rules().ok().flatten();
        let mut first_input: Option<Known> = None;
        for (p, port) in task.inputs.iter().enumerate() {
            for e in job.edges_into(&task.id, port) {
                let edge = &job.edges[e];
                let Some(producer) = job.task_index(&edge.from.task) else {
                    continue;
                };
                let Some(given) = &known[producer] else {
                    continue;
                };
                if p == 0 {
                    first_input = Some(given.clone());
                }
                let Some(rules) = &input_rules else {
                    continue;
                };
                for misfit in rules.misfits(given) {
                    refusals.push(Refusal {
                        field: format!("edges[{e}]"),
                        code: RefusalCode::Schema {
                            column: misfit.column().to_string(),
                        },
                        message: format!(
                            "`{}` (`{}`) does not take what `{}` gives it: {misfit}",
                            task.id, task.kind, edge.from.task
                        ),
                    });
                }
            }
        }
        // What the task is given is what its own input rules admit, when nothing upstream says
        // more.
        let input = first_input.or_else(|| {
            input_rules
                .as_ref()
                .and_then(|rules| rules.known(None).ok().flatten())
        });
        let Some(output_rules) = served.output_rules().ok().flatten() else {
            continue;
        };
        match output_rules.known(input.as_ref()) {
            Ok(gives) => known[i] = gives,
            Err(misfits) => {
                for misfit in misfits {
                    refusals.push(Refusal {
                        field: job.field(i),
                        code: RefusalCode::Schema {
                            column: misfit.column().to_string(),
                        },
                        message: format!(
                            "`{}`'s output_schema drops or changes a column its input does not \
                             have: {misfit}",
                            task.kind
                        ),
                    });
                }
            }
        }
    }
}

/// A violation of a kind's settings schema, as a refusal naming the value at fault.
fn settings_refusal(field: &str, kind: &str, violation: &Violation) -> Refusal {
    let dotted = dotted(&violation.path);
    let named = if violation.path.is_empty() {
        "the settings".to_string()
    } else {
        format!("`{dotted}`")
    };
    let said = match &violation.kind {
        Wrong::Missing => format!("{named} is required"),
        Wrong::Undeclared => format!("{named} is not a setting of `{kind}`"),
        Wrong::Type { allowed, found } => {
            format!("{named} must be {}, not {found}", types(allowed))
        }
        Wrong::NotOneOf(listed) => format!("{named} must be one of {}", values(listed)),
        Wrong::NotConst(only) => format!("{named} must be {only}"),
        Wrong::Below(least) => format!("{named} must be at least {least}"),
        Wrong::Above(most) => format!("{named} must be at most {most}"),
        Wrong::TooShort(least) => format!("{named} must be at least {least} characters"),
        Wrong::TooFewItems(least) => format!("{named} must have at least {least} items"),
        Wrong::TooManyItems(most) => format!("{named} must have at most {most} items"),
        Wrong::NoForm { forms, found } => {
            format!("{named} must be {}, not {found}", forms.join(" or "))
        }
        Wrong::ManyForms => format!(
            "{named} matches more than one of the forms it may take, and must match exactly one"
        ),
        Wrong::Forbidden => format!("{named} is not allowed"),
    };
    Refusal {
        field: if violation.path.is_empty() {
            format!("{field}.settings")
        } else {
            format!("{field}.settings.{dotted}")
        },
        code: RefusalCode::Settings {
            path: pointer(&violation.path),
        },
        message: format!("`{kind}` settings: {said}"),
    }
}

/// `options.columns[0]`.
fn dotted(path: &[Step]) -> String {
    let mut out = String::new();
    for step in path {
        match step {
            Step::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Step::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// `/options/columns/0`, a JSON pointer.
fn pointer(path: &[Step]) -> String {
    let mut out = String::new();
    for step in path {
        out.push('/');
        match step {
            Step::Key(key) => out.push_str(&key.replace('~', "~0").replace('/', "~1")),
            Step::Index(i) => out.push_str(&i.to_string()),
        }
    }
    out
}

/// A list of type names as a phrase: `a string`, `an array or a string`.
fn types(allowed: &[String]) -> String {
    let phrases: Vec<String> = allowed
        .iter()
        .map(|name| match name.as_str() {
            "array" | "object" | "integer" => format!("an {name}"),
            "null" => "null".to_string(),
            other => format!("a {other}"),
        })
        .collect();
    phrases.join(" or ")
}

/// The most listed values a sentence names before it says how many more there are.
const LISTED: usize = 12;

/// A list of allowed values as a phrase.
fn values(listed: &[Value]) -> String {
    let shown: Vec<String> = listed.iter().take(LISTED).map(Value::to_string).collect();
    if listed.len() > LISTED {
        format!("{} and {} more", shown.join(", "), listed.len() - LISTED)
    } else {
        shown.join(", ")
    }
}

/// Every image task of `job` whose image is not built (its `image` names no digest), other than
/// the tasks `held` (a program running them from objects it holds), each as `<field>
/// (build key <key>)`: a document with any is a draft, complete for drawing and checking and
/// refused for running.
pub fn drafts(job: &Job, held: &[String]) -> Vec<String> {
    job.tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == IMAGE_KIND && !held.contains(&t.id))
        .filter_map(|(i, t)| {
            let image = t.settings.get("image")?;
            if image.get("digest").is_some_and(|d| !d.is_null()) {
                return None;
            }
            let key = image
                .get("build_key")
                .and_then(Value::as_str)
                .unwrap_or("none");
            Some(format!("{} `{}` (build key {key})", job.field(i), t.id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{JobSpec, KindDoc};
    use crate::kind::{Isolation, Needs};
    use serde_json::json;

    /// JB-T11 flows_are_read_from_the_declaration: a declaration whose flows do not fit its role
    /// is refused at the task, by its flows. The catalogue admits none (JB-T3), so one is put in
    /// without the check to prove the checker does not depend on that.
    #[test]
    fn jb_t11_flows_are_read_from_the_declaration() {
        let mut catalogue = Catalogue::empty();
        let odd = |role: Role, inputs: Flow, outputs: Flow| Declaration {
            name: "odd".into(),
            role,
            summary: "flows no role has".into(),
            settings: json!({"type": "object"}),
            inputs,
            outputs,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
            served: None,
            title: None,
            description: None,
            takes: None,
            gives: None,
            requires: Vec::new(),
            provenance: None,
        };
        catalogue.declare_unchecked(odd(Role::Source, Flow::Batches, Flow::Batches));
        catalogue.declare_unchecked(odd(Role::Kernel, Flow::Files, Flow::Batches));
        catalogue.declare_unchecked(odd(Role::Sink, Flow::Batches, Flow::Batches));
        let entry = |kind: &str| KindDoc::new(kind, serde_json::Map::new());
        let job = Job::from_v1(&JobSpec::new(
            entry("odd"),
            vec![entry("odd")],
            entry("odd"),
        ));
        let found = check(&job, &catalogue).expect_err("no flow fits");
        let said: Vec<String> = found
            .iter()
            .filter(|r| !r.field.starts_with("edges"))
            .map(Refusal::to_string)
            .collect();
        assert_eq!(
            said,
            [
                "source: `odd` takes batches and gives batches, which a source does not: a \
                 source gives batches, a function takes and gives batches, a sink takes batches",
                "kernels[0]: `odd` takes files and gives batches, which a function does not: a \
                 source gives batches, a function takes and gives batches, a sink takes batches",
                "sink: `odd` takes batches and gives batches, which a sink does not: a source \
                 gives batches, a function takes and gives batches, a sink takes batches",
            ]
        );
        assert!(found.iter().all(|r| r.code == RefusalCode::Flow));
        // The kernel takes files, so the edge of batches into it does not fit either.
        assert!(found.iter().any(|r| r.field == "edges[0]"
            && r.message
                == "carries batches, and `kernel-1` takes files: an edge carries what its \
                    producer gives and its consumer takes"));
    }
}
