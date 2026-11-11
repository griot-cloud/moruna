//! A task's declared `gives`, enforced at the runtime boundary (0.6.6, "Output, identity and
//! recovery"): every batch a task produces, its first, every later one and its final
//! (completion) output, is compared with the [`ArrowSchema`] its kind declares before it is
//! published downstream. A refusal names the task, which batch, and the mismatched field. A kind
//! that declares no `gives` keeps its documented, undeclared behaviour: nothing is checked.

use moruna_job::{ArrowField, ArrowSchema};
use moruna_kernel::arrow::datatypes::Schema;
use moruna_kernel::{MorunaError, Result};

/// Which of a task's batches is being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Produced {
    /// A batch the task produced while running; the guard numbers them.
    Batch,
    /// The task's completion output (a function's final flush, a source's last batch).
    Final,
}

/// The check of one task's produced batches against what its kind declares it gives.
#[derive(Clone, Debug)]
pub struct GivesGuard {
    task: String,
    declared: Option<ArrowSchema>,
    batches: u64,
}

impl GivesGuard {
    /// The guard for task `task`, whose kind declares `declared` (`None`: undeclared).
    pub fn new(task: impl Into<String>, declared: Option<ArrowSchema>) -> GivesGuard {
        GivesGuard {
            task: task.into(),
            declared,
            batches: 0,
        }
    }

    /// Whether the kind declares what it gives, so batches are checked.
    pub fn declared(&self) -> bool {
        self.declared.is_some()
    }

    /// How many batches have been checked and passed.
    pub fn checked(&self) -> u64 {
        self.batches
    }

    /// Check one produced batch's schema; on a mismatch nothing is counted and the batch must
    /// not be published.
    pub fn check(&mut self, produced: Produced, schema: &Schema) -> Result<()> {
        let Some(declared) = &self.declared else {
            return Ok(());
        };
        let misfits = misfits(declared, schema);
        if !misfits.is_empty() {
            let which = match produced {
                Produced::Final => "its final output".to_string(),
                Produced::Batch if self.batches == 0 => "its first batch".to_string(),
                Produced::Batch => format!("its batch {}", self.batches + 1),
            };
            return Err(MorunaError::Plan(format!(
                "`{}` declares it gives {declared}, and {which} does not fit: {}",
                self.task,
                misfits.join("; ")
            )));
        }
        self.batches += 1;
        Ok(())
    }
}

/// The type of `field` as an [`ArrowField`] writes it.
fn written(field: &moruna_kernel::arrow::datatypes::Field) -> ArrowField {
    ArrowField {
        name: field.name().clone(),
        data_type: field.data_type().to_string(),
        nullable: field.is_nullable(),
    }
}

/// Every way `schema` differs from `declared`, each naming its field; empty when it fits.
pub fn misfits(declared: &ArrowSchema, schema: &Schema) -> Vec<String> {
    let mut out = Vec::new();
    let actual: Vec<ArrowField> = schema.fields().iter().map(|f| written(f)).collect();
    for (at, want) in declared.fields.iter().enumerate() {
        match actual.iter().position(|f| f.name == want.name) {
            None => out.push(format!("field `{}` is missing", want.name)),
            Some(found) => {
                let got = &actual[found];
                if got.data_type != want.data_type {
                    out.push(format!(
                        "field `{}` is {}, declared {}",
                        want.name, got.data_type, want.data_type
                    ));
                }
                if got.nullable && !want.nullable {
                    out.push(format!(
                        "field `{}` may hold nulls, declared not nullable",
                        want.name
                    ));
                }
                if found != at && got.data_type == want.data_type {
                    out.push(format!(
                        "field `{}` is at position {found}, declared {at}",
                        want.name
                    ));
                }
            }
        }
    }
    for got in &actual {
        if !declared.fields.iter().any(|w| w.name == got.name) {
            out.push(format!("field `{}` is not declared", got.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::datatypes::{DataType, Field};

    fn declared() -> ArrowSchema {
        ArrowSchema {
            fields: vec![
                ArrowField {
                    name: "id".into(),
                    data_type: "Int64".into(),
                    nullable: false,
                },
                ArrowField {
                    name: "text".into(),
                    data_type: "Utf8".into(),
                    nullable: true,
                },
            ],
        }
    }

    fn schema(fields: Vec<Field>) -> Schema {
        Schema::new(fields)
    }

    fn good() -> Schema {
        schema(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("text", DataType::Utf8, true),
        ])
    }

    #[test]
    fn undeclared_checks_nothing() {
        let mut g = GivesGuard::new("t", None);
        assert!(!g.declared());
        g.check(Produced::Batch, &schema(vec![])).unwrap();
        assert_eq!(g.checked(), 0);
    }

    #[test]
    fn matching_batches_pass_and_are_counted() {
        let mut g = GivesGuard::new("t", Some(declared()));
        assert!(g.declared());
        g.check(Produced::Batch, &good()).unwrap();
        g.check(Produced::Batch, &good()).unwrap();
        g.check(Produced::Final, &good()).unwrap();
        assert_eq!(g.checked(), 3);
        // A non-nullable column satisfies a nullable declaration.
        let narrower = schema(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("text", DataType::Utf8, false),
        ]);
        g.check(Produced::Batch, &narrower).unwrap();
    }

    #[test]
    fn wrong_first_batch_is_refused_naming_task_and_field() {
        let mut g = GivesGuard::new("score", Some(declared()));
        let bad = schema(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("text", DataType::Utf8, true),
        ]);
        let e = g.check(Produced::Batch, &bad).unwrap_err().to_string();
        assert!(e.contains("`score`"), "{e}");
        assert!(e.contains("its first batch"), "{e}");
        assert!(e.contains("field `id` is Int32, declared Int64"), "{e}");
        assert_eq!(g.checked(), 0);
    }

    #[test]
    fn wrong_later_batch_is_refused_with_its_ordinal() {
        let mut g = GivesGuard::new("score", Some(declared()));
        g.check(Produced::Batch, &good()).unwrap();
        let bad = schema(vec![Field::new("id", DataType::Int64, false)]);
        let e = g.check(Produced::Batch, &bad).unwrap_err().to_string();
        assert!(e.contains("its batch 2"), "{e}");
        assert!(e.contains("field `text` is missing"), "{e}");
        assert_eq!(g.checked(), 1);
    }

    #[test]
    fn wrong_final_output_is_refused() {
        let mut g = GivesGuard::new("score", Some(declared()));
        let bad = schema(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("text", DataType::Utf8, true),
            Field::new("extra", DataType::Boolean, true),
        ]);
        let e = g.check(Produced::Final, &bad).unwrap_err().to_string();
        assert!(e.contains("its final output"), "{e}");
        assert!(e.contains("field `id` may hold nulls"), "{e}");
        assert!(e.contains("field `extra` is not declared"), "{e}");
    }

    #[test]
    fn reordered_fields_are_refused() {
        let swapped = schema(vec![
            Field::new("text", DataType::Utf8, true),
            Field::new("id", DataType::Int64, false),
        ]);
        let m = misfits(&declared(), &swapped);
        assert_eq!(
            m,
            vec![
                "field `id` is at position 1, declared 0".to_string(),
                "field `text` is at position 0, declared 1".to_string(),
            ]
        );
        assert!(misfits(&declared(), &good()).is_empty());
    }
}
