//! The schema rules of what a kernel takes and gives (MH 4.9), as a package's declaration spells
//! them (SDD 16 e.2), read without Arrow: what a served kind's declaration carries
//! (`Served::input_schema`, `Served::output_schema`, SDD 18 e.4) and what [`crate::check`] compares
//! along the edges of a job.
//!
//! The spelling:
//!
//! - an array of `[name, type]` or `[name, type, nullable]` is exact: these columns, in this
//!   order, and no others (`nullable` absent is `true`);
//! - an object `{column: type}` is a subset: at least these columns, of these types, anywhere;
//! - for an output, an object whose keys are a non-empty subset of `adds`, `drops` and `changes`
//!   is relative to the input: `adds` and `changes` are objects `{column: type}`, `drops` an array
//!   of names.
//!
//! A type is a string of the type grammar of MH 4.9, read by `moruna_types::parse`, the grammar's
//! one reader; the runtime converts what it reads into Arrow types (`moruna_kernel::declare`).

use moruna_types::ColumnType;
use serde_json::Value;

/// One declared column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Its name.
    pub name: String,
    /// Its type, as the grammar read it.
    pub ty: ColumnType,
    /// Whether it may hold nulls.
    pub nullable: bool,
}

/// The rules of one side of a kernel (MH 4.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaRules {
    /// Exactly these columns, in this order, and no others.
    Exact(Vec<Column>),
    /// At least these columns, of these types, anywhere.
    Subset(Vec<Column>),
    /// The input's columns less `drops`, with `changes` replacing their types, then `adds`: an
    /// output only.
    Relative {
        /// Columns appended.
        adds: Vec<Column>,
        /// Input columns removed.
        drops: Vec<String>,
        /// Input columns whose type changes.
        changes: Vec<Column>,
    },
}

/// Why a spelling is not schema rules: where in it (`input_schema[1]`, `output_schema.adds.x`)
/// and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RulesFault {
    /// Where, from the field the caller named.
    pub field: String,
    /// Why.
    pub reason: String,
}

impl core::fmt::Display for RulesFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

fn fault(field: impl Into<String>, reason: impl Into<String>) -> RulesFault {
    RulesFault {
        field: field.into(),
        reason: reason.into(),
    }
}

impl SchemaRules {
    /// The rules `value` spells, at `field`; `output` admits a relative spelling.
    pub fn parse(value: &Value, field: &str, output: bool) -> Result<SchemaRules, RulesFault> {
        match value {
            Value::Array(items) => {
                let mut columns = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let at = format!("{field}[{i}]");
                    let shape = || {
                        fault(
                            &at,
                            "an exact column is [name, type] or [name, type, nullable]",
                        )
                    };
                    let parts = item.as_array().ok_or_else(shape)?;
                    let (name, ty, nullable) = match parts.as_slice() {
                        [n, t] => (n, t, true),
                        [n, t, Value::Bool(b)] => (n, t, *b),
                        _ => return Err(shape()),
                    };
                    let name = name
                        .as_str()
                        .ok_or_else(|| fault(&at, "the column name must be a string"))?;
                    columns.push(column(name, ty, &at, nullable)?);
                }
                Ok(SchemaRules::Exact(columns))
            }
            Value::Object(map) => {
                let relative = !map.is_empty()
                    && map
                        .keys()
                        .all(|k| matches!(k.as_str(), "adds" | "drops" | "changes"));
                if !relative {
                    return Ok(SchemaRules::Subset(columns_of(Some(value), field)?));
                }
                if !output {
                    return Err(fault(
                        field,
                        "an input schema cannot be relative: adds, drops and changes describe an \
                         output",
                    ));
                }
                let adds = columns_of(map.get("adds"), &format!("{field}.adds"))?;
                let changes = columns_of(map.get("changes"), &format!("{field}.changes"))?;
                let drops = match map.get("drops") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(names)) => names
                        .iter()
                        .map(|n| {
                            n.as_str().map(str::to_string).ok_or_else(|| {
                                fault(format!("{field}.drops"), "names must be strings")
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(_) => {
                        return Err(fault(
                            format!("{field}.drops"),
                            "must be a list of column names",
                        ));
                    }
                };
                Ok(SchemaRules::Relative {
                    adds,
                    drops,
                    changes,
                })
            }
            _ => Err(fault(
                field,
                "a schema is an object of column to type, or a list of [name, type] columns",
            )),
        }
    }
}

fn columns_of(value: Option<&Value>, field: &str) -> Result<Vec<Column>, RulesFault> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        // The map's own order: the order the declaration wrote, which is the order `moruna
        // check` generates a subset's columns in (MH 4.9).
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, ty)| column(name, ty, &format!("{field}.{name}"), true))
            .collect(),
        Some(_) => Err(fault(field, "must be an object of column to type")),
    }
}

fn column(name: &str, ty: &Value, field: &str, nullable: bool) -> Result<Column, RulesFault> {
    let spelling = ty
        .as_str()
        .ok_or_else(|| fault(field, "a type is a string of the type grammar (MH 4.9)"))?;
    let ty = moruna_types::parse(spelling).map_err(|e| fault(field, e.to_string()))?;
    Ok(Column {
        name: name.to_string(),
        ty,
        nullable,
    })
}

/// The columns a side of a kernel is known to have, where a job can know them without running it
/// (SDD 22 e.6): `closed` when no other column is there, `ordered` when they are in this order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    /// The columns known to be there.
    pub columns: Vec<Column>,
    /// No other column is there.
    pub closed: bool,
    /// They are in this order, at these positions.
    pub ordered: bool,
}

/// Why the columns along an edge do not fit a kind's declared input (SDD 22 e.6), in the words of
/// MH 4.9's comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Misfit {
    /// Declared, and known to be given with another type.
    Type {
        /// The column.
        column: String,
        /// The declared type's spelling.
        declared: String,
        /// The given type's spelling.
        given: String,
    },
    /// Declared, and known not to be given.
    Missing {
        /// The column.
        column: String,
        /// The declared type's spelling.
        declared: String,
    },
    /// Given, and not declared by rules that admit no other column.
    Undeclared {
        /// The column.
        column: String,
        /// The given type's spelling.
        given: String,
    },
    /// Declared at one position and given at another, by exact rules.
    Position {
        /// The column.
        column: String,
        /// Its declared position.
        declared: usize,
        /// Its given position.
        given: usize,
    },
    /// Relative rules drop or change a column the input is known not to have.
    Unknown {
        /// The column.
        column: String,
    },
}

impl Misfit {
    /// The column the misfit is about.
    pub fn column(&self) -> &str {
        match self {
            Misfit::Type { column, .. }
            | Misfit::Missing { column, .. }
            | Misfit::Undeclared { column, .. }
            | Misfit::Position { column, .. }
            | Misfit::Unknown { column } => column,
        }
    }
}

impl core::fmt::Display for Misfit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Misfit::Type {
                column,
                declared,
                given,
            } => write!(f, "column `{column}`: declared {declared}, given {given}"),
            Misfit::Missing { column, declared } => {
                write!(f, "column `{column}`: declared {declared}, not given")
            }
            Misfit::Undeclared { column, given } => {
                write!(f, "column `{column}`: given {given}, not declared")
            }
            Misfit::Position {
                column,
                declared,
                given,
            } => write!(
                f,
                "column `{column}`: declared at position {declared}, given at {given}"
            ),
            Misfit::Unknown { column } => {
                write!(
                    f,
                    "column `{column}`: named by the output, not in the input"
                )
            }
        }
    }
}

impl SchemaRules {
    /// What the side these rules describe is known to have, given what its input is known to
    /// have (`None`: nothing is known of it). Exact rules are known whatever the input, subset
    /// rules are known open, and relative rules are known as far as the input is; a relative
    /// rule naming a column a closed input does not have is a [`Misfit::Unknown`].
    pub fn known(&self, input: Option<&Known>) -> Result<Option<Known>, Vec<Misfit>> {
        match self {
            SchemaRules::Exact(columns) => Ok(Some(Known {
                columns: columns.clone(),
                closed: true,
                ordered: true,
            })),
            SchemaRules::Subset(columns) => Ok(Some(Known {
                columns: columns.clone(),
                closed: false,
                ordered: false,
            })),
            SchemaRules::Relative {
                adds,
                drops,
                changes,
            } => {
                let Some(input) = input else {
                    return Ok(None);
                };
                if input.closed {
                    let unknown: Vec<Misfit> = drops
                        .iter()
                        .chain(changes.iter().map(|c| &c.name))
                        .filter(|name| !input.columns.iter().any(|c| &c.name == *name))
                        .map(|name| Misfit::Unknown {
                            column: name.clone(),
                        })
                        .collect();
                    if !unknown.is_empty() {
                        return Err(unknown);
                    }
                }
                let mut columns = Vec::with_capacity(input.columns.len() + adds.len());
                for column in &input.columns {
                    if drops.contains(&column.name) {
                        continue;
                    }
                    match changes.iter().find(|c| c.name == column.name) {
                        Some(changed) => columns.push(changed.clone()),
                        None => columns.push(column.clone()),
                    }
                }
                columns.extend(adds.iter().cloned());
                Ok(Some(Known {
                    columns,
                    closed: input.closed,
                    ordered: false,
                }))
            }
        }
    }

    /// Every way columns known to be `given` do not fit these rules as an input, in the declared
    /// columns' order and then the given ones'. Only what is known is judged: a column an open
    /// input may have is not missing, and a position an unordered input may have is not wrong.
    pub fn misfits(&self, given: &Known) -> Vec<Misfit> {
        let (declared, ordered, closed) = match self {
            SchemaRules::Exact(columns) => (columns, true, true),
            SchemaRules::Subset(columns) => (columns, false, false),
            // An input's rules are never relative (`parse` refuses them).
            SchemaRules::Relative { .. } => return Vec::new(),
        };
        let mut out = Vec::new();
        for (at, column) in declared.iter().enumerate() {
            match given.columns.iter().position(|c| c.name == column.name) {
                None if given.closed => out.push(Misfit::Missing {
                    column: column.name.clone(),
                    declared: column.ty.spelling(),
                }),
                None => {}
                Some(index) => {
                    let found = &given.columns[index];
                    if !column.ty.accepts(&found.ty) {
                        out.push(Misfit::Type {
                            column: column.name.clone(),
                            declared: column.ty.spelling(),
                            given: found.ty.spelling(),
                        });
                    } else if ordered && given.ordered && index != at {
                        out.push(Misfit::Position {
                            column: column.name.clone(),
                            declared: at,
                            given: index,
                        });
                    }
                }
            }
        }
        if closed {
            for found in &given.columns {
                if !declared.iter().any(|c| c.name == found.name) {
                    out.push(Misfit::Undeclared {
                        column: found.name.clone(),
                        given: found.ty.spelling(),
                    });
                }
            }
        }
        out
    }
}
