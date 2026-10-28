//! The catalogue: every declaration an embedder can name (SDD 22 e.2).
//!
//! Moruna's own are the two files `moruna kinds` printed, embedded: `catalogue/standard.json`,
//! what a build with default features lists, and `catalogue/peql.json`, the two kinds a build with
//! the `peql` feature adds. An embedder adds its own declarations to them, or starts from a
//! listing it read from a `moruna kinds` of its own.

use std::collections::BTreeMap;

use crate::kind::{Declaration, LISTING_VERSION, Listing, Role};
use crate::schema;

/// The standard kinds, as `moruna kinds` printed them for a build with default features.
const STANDARD: &str = include_str!("../catalogue/standard.json");

/// The kinds the `peql` feature adds, as `moruna kinds` printed them.
const PEQL: &str = include_str!("../catalogue/peql.json");

/// Why a declaration, or a listing of them, was not taken into a catalogue.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogueError {
    /// The declaration fails [`Declaration::check`] for the role it was added as.
    #[error("not a {role} kind: {why}")]
    Refused {
        /// The role it was added as.
        role: Role,
        /// What `Declaration::check` said.
        why: String,
    },
    /// A kind of that role and name is declared already.
    #[error("a {role} kind `{name}` is declared already")]
    Taken {
        /// The role.
        role: Role,
        /// The name.
        name: String,
    },
    /// The settings schema uses what the validator does not read (SDD 22 e.3).
    #[error("the {role} kind `{name}`'s settings schema cannot be checked against: {fault}")]
    Schema {
        /// The role.
        role: Role,
        /// The name.
        name: String,
        /// What is wrong with the schema, and where.
        fault: schema::SchemaFault,
    },
    /// A listing of a format this crate does not read.
    #[error("a listing of format {found}, and this crate reads format {LISTING_VERSION}")]
    Format {
        /// The listing's `moruna_kinds`.
        found: u32,
    },
}

/// Every declaration an embedder can name, by role and name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalogue {
    sources: BTreeMap<String, Declaration>,
    kernels: BTreeMap<String, Declaration>,
    sinks: BTreeMap<String, Declaration>,
}

impl Catalogue {
    /// A catalogue that declares nothing.
    pub fn empty() -> Catalogue {
        Catalogue::default()
    }

    /// Moruna's standard kinds: what a build of `moruna-runtime` with default features lists
    /// (`moruna kinds`), without the governed ends the `peql` feature adds.
    pub fn standard() -> Catalogue {
        Catalogue::embedded(&[("standard.json", STANDARD)])
    }

    /// The standard kinds and the two the `peql` feature adds, the `datafusion` source and the
    /// `peql` sink: what a build with that feature lists.
    pub fn standard_with_peql() -> Catalogue {
        Catalogue::embedded(&[("standard.json", STANDARD), ("peql.json", PEQL)])
    }

    /// The catalogue of the kinds a listing declares: what an embedder that ran `moruna kinds`
    /// on its own binary reads, or a registry's [`Listing`].
    pub fn from_listing(listing: Listing) -> Result<Catalogue, CatalogueError> {
        let mut catalogue = Catalogue::empty();
        catalogue.extend(listing)?;
        Ok(catalogue)
    }

    /// Declare `declaration`, a kind of the role it says it is. Refused when it fails
    /// [`Declaration::check`] for that role, when a kind of that role and name is declared
    /// already, and when its settings schema uses a keyword outside the subset the validator
    /// reads (SDD 22 e.3), so a schema that cannot be checked against never enters.
    pub fn add(&mut self, declaration: Declaration) -> Result<(), CatalogueError> {
        self.add_as(declaration.role, declaration)
    }

    /// [`Catalogue::add`] for a declaration that must be a kind of `role`: what a listing's
    /// `sources`, `kernels` and `sinks` each hold.
    fn add_as(&mut self, role: Role, declaration: Declaration) -> Result<(), CatalogueError> {
        declaration
            .check(role)
            .map_err(|why| CatalogueError::Refused { role, why })?;
        if self.of(role).contains_key(&declaration.name) {
            return Err(CatalogueError::Taken {
                role,
                name: declaration.name,
            });
        }
        schema::admit(&declaration.settings).map_err(|fault| CatalogueError::Schema {
            role,
            name: declaration.name.clone(),
            fault,
        })?;
        self.of_mut(role)
            .insert(declaration.name.clone(), declaration);
        Ok(())
    }

    /// The declaration of the `role` kind called `name`.
    pub fn get(&self, role: Role, name: &str) -> Option<&Declaration> {
        self.of(role).get(name)
    }

    /// The names of the `role` kinds, sorted.
    pub fn names(&self, role: Role) -> impl Iterator<Item = &str> {
        self.of(role).keys().map(String::as_str)
    }

    /// The declarations of the `role` kinds, by name.
    pub fn declarations(&self, role: Role) -> impl Iterator<Item = &Declaration> {
        self.of(role).values()
    }

    fn extend(&mut self, listing: Listing) -> Result<(), CatalogueError> {
        if listing.moruna_kinds != LISTING_VERSION {
            return Err(CatalogueError::Format {
                found: listing.moruna_kinds,
            });
        }
        let by_role = [
            (Role::Source, listing.sources),
            (Role::Kernel, listing.kernels),
            (Role::Sink, listing.sinks),
        ];
        for (role, declarations) in by_role {
            for declaration in declarations {
                self.add_as(role, declaration)?;
            }
        }
        Ok(())
    }

    /// A declaration taken in as it is, whatever it says: the only way to hold one that
    /// [`Declaration::check`] refuses, which the checker's flow test needs.
    #[cfg(test)]
    pub(crate) fn declare_unchecked(&mut self, declaration: Declaration) {
        self.of_mut(declaration.role)
            .insert(declaration.name.clone(), declaration);
    }

    fn of(&self, role: Role) -> &BTreeMap<String, Declaration> {
        match role {
            Role::Source => &self.sources,
            Role::Kernel => &self.kernels,
            Role::Sink => &self.sinks,
        }
    }

    fn of_mut(&mut self, role: Role) -> &mut BTreeMap<String, Declaration> {
        match role {
            Role::Source => &mut self.sources,
            Role::Kernel => &mut self.kernels,
            Role::Sink => &mut self.sinks,
        }
    }

    /// The catalogue of the embedded files. They are checked by JB-T2 and by the runtime's
    /// JB-T25, so one that is not a catalogue is a defect of this crate that no release carries;
    /// a program that meets one stops, saying which.
    fn embedded(files: &[(&str, &str)]) -> Catalogue {
        let mut catalogue = Catalogue::empty();
        for (file, text) in files {
            let read = serde_json::from_str::<Listing>(text)
                .map_err(|e| e.to_string())
                .and_then(|listing| catalogue.extend(listing).map_err(|e| e.to_string()));
            if let Err(why) = read {
                panic!("moruna-job's embedded catalogue/{file} is not a catalogue: {why}");
            }
        }
        catalogue
    }
}
