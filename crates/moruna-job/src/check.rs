//! Checking a job document against a catalogue, without running anything (SDD 22 f.1).

use serde_json::Value;

use crate::catalogue::Catalogue;
use crate::doc::{JobSpec, KindDoc};
use crate::kind::{Declaration, Fact, Flow, Role};
use crate::schema::{self, Step, Violation, Wrong};

/// One thing wrong with a document: where, what kind of wrong, and a sentence a person can act
/// on, in the voice of the runtime's own refusals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Where the problem is: `source.kind`, `kernels[2]`, `sink.settings.url`.
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
    /// No kind of that role is declared under the name the entry gives.
    UnknownKind {
        /// The role the entry is a kind of.
        role: Role,
        /// The kinds of that role the catalogue declares, sorted.
        known: Vec<String>,
    },
    /// The entry's settings are not what the kind's settings schema allows.
    Settings {
        /// The value at fault, as a JSON pointer into the entry's settings (`/options/columns/0`;
        /// empty is the settings as a whole).
        path: String,
    },
    /// The kind's flows do not fit its place in the run: a source gives batches, a kernel takes
    /// and gives batches, a sink takes batches.
    Flow,
    /// The kind needs a contract (`needs.contracts` is `yes`) and the job carries none.
    Contract,
}

/// Check `job` against `catalogue`: every part of the run names a kind the catalogue declares for
/// its role, with settings its schema allows, in a place its flows fit, and the job carries a
/// contract for each kind that needs one. Every refusal is collected, not only the first, in the
/// order the document is read: the source, the kernels, the sink, then the contracts. Nothing is
/// built or run.
pub fn check(job: &JobSpec, catalogue: &Catalogue) -> Result<(), Vec<Refusal>> {
    let mut refusals = Vec::new();
    let mut needing = Vec::new();
    entry(
        &job.source,
        Role::Source,
        "source",
        catalogue,
        &mut refusals,
        &mut needing,
    );
    for (i, kernel) in job.kernels.iter().enumerate() {
        entry(
            kernel,
            Role::Kernel,
            &format!("kernels[{i}]"),
            catalogue,
            &mut refusals,
            &mut needing,
        );
    }
    entry(
        &job.sink,
        Role::Sink,
        "sink",
        catalogue,
        &mut refusals,
        &mut needing,
    );
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

/// One entry: its kind, its flows, its settings, and whether it needs a contract.
fn entry(
    doc: &KindDoc,
    role: Role,
    field: &str,
    catalogue: &Catalogue,
    refusals: &mut Vec<Refusal>,
    needing: &mut Vec<(String, String)>,
) {
    let Some(declared) = catalogue.get(role, &doc.kind) else {
        refusals.push(unknown(field, role, &doc.kind, catalogue));
        return;
    };
    if !flows_fit(declared, role) {
        refusals.push(Refusal {
            field: field.to_string(),
            code: RefusalCode::Flow,
            message: format!(
                "`{}` takes {} and gives {}, which a {} does not: a source gives batches, a \
                 kernel takes and gives batches, a sink takes batches",
                declared.name,
                declared.inputs.as_str(),
                declared.outputs.as_str(),
                role
            ),
        });
    }
    let settings = Value::Object(doc.settings.clone());
    for violation in schema::validate(&declared.settings, &declared.settings, &settings) {
        refusals.push(settings_refusal(field, &doc.kind, &violation));
    }
    if declared.needs.contracts == Fact::Yes {
        needing.push((field.to_string(), doc.kind.clone()));
    }
}

fn flows_fit(declared: &Declaration, role: Role) -> bool {
    match role {
        Role::Source => declared.inputs != Flow::Batches && declared.outputs == Flow::Batches,
        Role::Kernel => declared.inputs == Flow::Batches && declared.outputs == Flow::Batches,
        Role::Sink => declared.inputs == Flow::Batches && declared.outputs != Flow::Batches,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::{Isolation, Needs};
    use serde_json::json;

    /// JB-T11 flows_are_the_declarations: a declaration whose flows do not fit its place is
    /// refused at the entry, by its flows. The catalogue admits none (JB-T3), so one is put in
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
        };
        catalogue.declare_unchecked(odd(Role::Source, Flow::Batches, Flow::Batches));
        catalogue.declare_unchecked(odd(Role::Kernel, Flow::Files, Flow::Batches));
        catalogue.declare_unchecked(odd(Role::Sink, Flow::Batches, Flow::Batches));
        let entry = |kind: &str| KindDoc::new(kind, serde_json::Map::new());
        let job = JobSpec::new(entry("odd"), vec![entry("odd")], entry("odd"));
        let found = check(&job, &catalogue).expect_err("no flow fits");
        let said: Vec<String> = found.iter().map(Refusal::to_string).collect();
        assert_eq!(
            said,
            [
                "source: `odd` takes batches and gives batches, which a source does not: a \
                 source gives batches, a kernel takes and gives batches, a sink takes batches",
                "kernels[0]: `odd` takes files and gives batches, which a kernel does not: a \
                 source gives batches, a kernel takes and gives batches, a sink takes batches",
                "sink: `odd` takes batches and gives batches, which a sink does not: a source \
                 gives batches, a kernel takes and gives batches, a sink takes batches",
            ]
        );
        assert!(found.iter().all(|r| r.code == RefusalCode::Flow));
    }
}
