use std::fmt::Debug;
use std::io::{BufWriter, Write};
use std::ops::Deref;
use std::path::Path;

use ahash::HashSet;
use bimap::BiHashMap;
use guppy::graph::PackageGraph;
use guppy::PackageId;
use indexmap::{IndexMap, IndexSet};
use miette::miette;
use proc_macro2::Ident;
use quote::format_ident;

use pavex_builder::{AppBlueprint, Lifecycle};

use crate::diagnostic;
use crate::diagnostic::{CompilerDiagnostic, LocationExt, SourceSpanExt};
use crate::language::ResolvedType;
use crate::rustdoc::{CrateCollection, TOOLCHAIN_CRATES};
use crate::web::analyses::call_graph::{
    application_state_call_graph, handler_call_graph, ApplicationStateCallGraph, CallGraph,
};
use crate::web::analyses::components::ComponentDb;
use crate::web::analyses::computations::ComputationDb;
use crate::web::analyses::constructibles::ConstructibleDb;
use crate::web::analyses::raw_identifiers::RawCallableIdentifiersDb;
use crate::web::analyses::resolved_paths::ResolvedPathDb;
use crate::web::analyses::user_components::UserComponentDb;
use crate::web::codegen;
use crate::web::generated_app::GeneratedApp;
use crate::web::resolvers::CallableResolutionError;
use crate::web::traits::{assert_trait_is_implemented, MissingTraitImplementationError};
use crate::web::utils::process_framework_path;

pub(crate) const GENERATED_APP_PACKAGE_ID: &str = "crate";

pub struct App {
    package_graph: PackageGraph,
    handler_call_graphs: IndexMap<String, CallGraph>,
    application_state_call_graph: ApplicationStateCallGraph,
    runtime_singleton_bindings: BiHashMap<Ident, ResolvedType>,
    request_scoped_framework_bindings: BiHashMap<Ident, ResolvedType>,
    codegen_types: HashSet<ResolvedType>,
    component_db: ComponentDb,
    computation_db: ComputationDb,
}

#[tracing::instrument]
fn compute_package_graph() -> Result<PackageGraph, miette::Error> {
    // `cargo metadata` seems to be the only reliable way of retrieving the path to
    // the root manifest of the current workspace for a Rust project.
    guppy::MetadataCommand::new()
        .exec()
        .map_err(|e| miette!(e))?
        .build_graph()
        .map_err(|e| miette!(e))
}

/// Exit early if there is at least one error.
macro_rules! exit_on_errors {
    ($var:ident) => {
        if !$var.is_empty() {
            return Err($var);
        }
    };
}

impl App {
    #[tracing::instrument(skip_all)]
    pub fn build(bp: AppBlueprint) -> Result<Self, Vec<miette::Error>> {
        let raw_identifiers_db = RawCallableIdentifiersDb::build(&bp);
        let user_component_db = UserComponentDb::build(&bp, &raw_identifiers_db);
        let package_graph = compute_package_graph().map_err(|e| vec![e])?;
        let mut diagnostics = vec![];
        let krate_collection = CrateCollection::new(package_graph.clone());
        let resolved_path_db = ResolvedPathDb::build(
            &user_component_db,
            &raw_identifiers_db,
            &package_graph,
            &mut diagnostics,
        );
        exit_on_errors!(diagnostics);
        let mut computation_db = ComputationDb::build(
            &user_component_db,
            &resolved_path_db,
            &package_graph,
            &krate_collection,
            &raw_identifiers_db,
            &mut diagnostics,
        );
        exit_on_errors!(diagnostics);
        let mut component_db = ComponentDb::build(
            &user_component_db,
            &mut computation_db,
            &package_graph,
            &raw_identifiers_db,
            &krate_collection,
            &mut diagnostics,
        );
        exit_on_errors!(diagnostics);
        let request_scoped_framework_bindings =
            framework_bindings(&package_graph, &krate_collection);
        let mut constructible_db = ConstructibleDb::build(
            &component_db,
            &computation_db,
            &package_graph,
            &krate_collection,
            &user_component_db,
            &raw_identifiers_db,
            &request_scoped_framework_bindings.right_values().collect(),
            &mut diagnostics,
        );
        exit_on_errors!(diagnostics);
        let handler_call_graphs = {
            let router = component_db.router();
            let mut handler_call_graphs = IndexMap::with_capacity(router.len());
            for (route, handler_id) in router {
                let call_graph = handler_call_graph(
                    *handler_id,
                    &computation_db,
                    &component_db,
                    &constructible_db,
                );
                handler_call_graphs.insert(route.to_owned(), call_graph);
            }
            handler_call_graphs
        };

        let runtime_singletons: IndexSet<ResolvedType> = get_required_singleton_types(
            handler_call_graphs.iter(),
            &request_scoped_framework_bindings,
            &constructible_db,
            &component_db,
        );

        verify_singletons(
            &runtime_singletons,
            &constructible_db,
            &component_db,
            &package_graph,
            &user_component_db,
            &raw_identifiers_db,
            &krate_collection,
            &mut diagnostics,
        );
        let runtime_singleton_bindings = runtime_singletons
            .iter()
            .enumerate()
            // Assign a unique name to each singleton
            .map(|(i, type_)| (format_ident!("s{}", i), type_.to_owned()))
            .collect();
        let application_state_call_graph = application_state_call_graph(
            &runtime_singleton_bindings,
            &mut computation_db,
            &mut component_db,
            &mut constructible_db,
        );
        let codegen_types = codegen_types(&package_graph, &krate_collection);
        exit_on_errors!(diagnostics);
        Ok(Self {
            package_graph,
            handler_call_graphs,
            component_db,
            computation_db,
            application_state_call_graph,
            runtime_singleton_bindings,
            request_scoped_framework_bindings,
            codegen_types,
        })
    }

    /// Generate the manifest and the Rust code for the analysed application.
    ///
    /// They are generated in-memory, they are not persisted to disk.
    pub fn codegen(&self) -> Result<GeneratedApp, anyhow::Error> {
        let (cargo_toml, mut package_ids2deps) = codegen::codegen_manifest(
            &self.package_graph,
            &self.handler_call_graphs,
            &self.application_state_call_graph.call_graph,
            &self.request_scoped_framework_bindings,
            &self.codegen_types,
            &self.component_db,
            &self.computation_db,
        );
        let generated_app_package_id = PackageId::new(GENERATED_APP_PACKAGE_ID);
        let toolchain_package_ids = TOOLCHAIN_CRATES
            .iter()
            .map(|p| PackageId::new(*p))
            .collect::<Vec<_>>();
        for package_id in &toolchain_package_ids {
            package_ids2deps.insert(package_id.clone(), package_id.repr().into());
        }
        package_ids2deps.insert(generated_app_package_id, "crate".into());

        let lib_rs = codegen::codegen_app(
            &self.handler_call_graphs,
            &self.application_state_call_graph,
            &self.request_scoped_framework_bindings,
            &package_ids2deps,
            &self.runtime_singleton_bindings,
            &self.component_db,
            &self.computation_db,
        )?;
        Ok(GeneratedApp { lib_rs, cargo_toml })
    }

    /// A representation of an `App` geared towards debugging and testing.
    pub fn diagnostic_representation(&self) -> AppDiagnostics {
        let mut handler_graphs = IndexMap::new();
        let (_, mut package_ids2deps) = codegen::codegen_manifest(
            &self.package_graph,
            &self.handler_call_graphs,
            &self.application_state_call_graph.call_graph,
            &self.request_scoped_framework_bindings,
            &self.codegen_types,
            &self.component_db,
            &self.computation_db,
        );
        // TODO: dry this up in one place.
        let generated_app_package_id = PackageId::new(GENERATED_APP_PACKAGE_ID);
        let toolchain_package_ids = TOOLCHAIN_CRATES
            .iter()
            .map(|p| PackageId::new(*p))
            .collect::<Vec<_>>();
        for package_id in &toolchain_package_ids {
            package_ids2deps.insert(package_id.clone(), package_id.repr().into());
        }
        package_ids2deps.insert(generated_app_package_id, "crate".into());

        for (route, handler_call_graph) in &self.handler_call_graphs {
            handler_graphs.insert(
                route.to_owned(),
                handler_call_graph
                    .dot(&package_ids2deps, &self.component_db, &self.computation_db)
                    .replace("digraph", &format!("digraph \"{route}\"")),
            );
        }
        let application_state_graph = self
            .application_state_call_graph
            .call_graph
            .dot(&package_ids2deps, &self.component_db, &self.computation_db)
            .replace("digraph", "digraph app_state");
        AppDiagnostics {
            handlers: handler_graphs,
            application_state: application_state_graph,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BuildError {
    #[error(transparent)]
    HandlerError(#[from] Box<CallableResolutionError>),
    #[error(transparent)]
    GenericError(#[from] anyhow::Error),
}

/// A representation of an `App` geared towards debugging and testing.
///
/// It contains the DOT representation of all the call graphs underpinning the originating `App`.
/// The DOT representation can be used for snapshot testing and/or troubleshooting.
pub struct AppDiagnostics {
    pub handlers: IndexMap<String, String>,
    pub application_state: String,
}

impl AppDiagnostics {
    /// Persist the diagnostic information to disk, using one file per handler within the specified
    /// directory.
    pub fn persist(&self, directory: &Path) -> Result<(), anyhow::Error> {
        let handler_directory = directory.join("handlers");
        fs_err::create_dir_all(&handler_directory)?;
        for (route, handler) in &self.handlers {
            let path = handler_directory.join(format!("{route}.dot").trim_start_matches('/'));
            let mut file = fs_err::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?;
            file.write_all(handler.as_bytes())?;
        }
        let mut file = fs_err::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(directory.join("app_state.dot"))?;
        file.write_all(self.application_state.as_bytes())?;
        Ok(())
    }

    /// Save all diagnostics in a single file instead of having one file per handler.
    pub fn persist_flat(&self, filepath: &Path) -> Result<(), anyhow::Error> {
        let file = fs_err::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(filepath)?;
        let mut file = BufWriter::new(file);

        for handler in self.handlers.values() {
            file.write_all(handler.as_bytes())?;
        }
        file.write_all(self.application_state.as_bytes())?;
        file.flush()?;
        Ok(())
    }
}

/// Determine the set of singleton types that are required to execute the constructors and handlers
/// registered by the application.
/// These singletons will be attached to the overall application state.
fn get_required_singleton_types<'a>(
    handler_call_graphs: impl Iterator<Item = (&'a String, &'a CallGraph)>,
    types_provided_by_the_framework: &BiHashMap<Ident, ResolvedType>,
    constructibles_db: &ConstructibleDb,
    component_db: &ComponentDb,
) -> IndexSet<ResolvedType> {
    let mut singletons_to_be_built = IndexSet::new();
    for (_, handler_call_graph) in handler_call_graphs {
        for required_input in handler_call_graph.required_input_types() {
            let required_input = if let ResolvedType::Reference(t) = &required_input {
                if !t.is_static {
                    // We can't store non-'static references in the application state, so we expect
                    // to see the referenced type in there.
                    t.inner.deref()
                } else {
                    &required_input
                }
            } else {
                &required_input
            };
            if !types_provided_by_the_framework.contains_right(required_input) {
                let component_id = constructibles_db[required_input];
                assert_eq!(
                    component_db.lifecycle(component_id),
                    Some(&Lifecycle::Singleton)
                );
                singletons_to_be_built.insert(required_input.to_owned());
            }
        }
    }
    singletons_to_be_built
}

/// Return the set of name bindings injected by `pavex` into the processing context for
/// an incoming request (e.g. the incoming request itself!).  
/// The types injected here can be used by constructors and handlers even though no constructor
/// has been explicitly registered for them by the developer.
fn framework_bindings(
    package_graph: &PackageGraph,
    krate_collection: &CrateCollection,
) -> BiHashMap<Ident, ResolvedType> {
    let http_request = "pavex_runtime::http::Request::<pavex_runtime::hyper::Body>";
    let http_request = process_framework_path(http_request, package_graph, krate_collection);
    BiHashMap::from_iter([(format_ident!("request"), http_request)].into_iter())
}

/// Return the set of types that will be used in the generated code to build a functional
/// server scaffolding.  
fn codegen_types(
    package_graph: &PackageGraph,
    krate_collection: &CrateCollection,
) -> HashSet<ResolvedType> {
    let error = process_framework_path("pavex_runtime::Error", package_graph, krate_collection);
    HashSet::from_iter([error])
}

/// Verify that all singletons needed at runtime implement `Send`, `Sync` and `Clone`.
/// This is required since `pavex` runs on a multi-threaded `tokio` runtime.
fn verify_singletons(
    runtime_singletons: &IndexSet<ResolvedType>,
    constructible_db: &ConstructibleDb,
    component_db: &ComponentDb,
    package_graph: &PackageGraph,
    user_component_db: &UserComponentDb,
    raw_identifiers_db: &RawCallableIdentifiersDb,
    krate_collection: &CrateCollection,
    diagnostics: &mut Vec<miette::Error>,
) {
    fn missing_trait_implementation(
        e: MissingTraitImplementationError,
        package_graph: &PackageGraph,
        constructible_db: &ConstructibleDb,
        component_db: &ComponentDb,
        user_component_db: &UserComponentDb,
        raw_identifiers_db: &RawCallableIdentifiersDb,
        diagnostics: &mut Vec<miette::Error>,
    ) {
        let t = if let ResolvedType::Reference(ref t) = e.type_ {
            if !t.is_static {
                t.inner.deref().clone()
            } else {
                e.type_.clone()
            }
        } else {
            e.type_.clone()
        };
        let component_id = constructible_db[&t];
        let user_component_id = component_db.user_component_id(component_id).unwrap();
        let user_component = &user_component_db[user_component_id];
        let raw_identifier_id = user_component.raw_callable_identifiers_id();
        let component_kind = user_component.callable_type();

        let location = raw_identifiers_db.get_location(raw_identifier_id);
        let source = match location.source_file(package_graph) {
            Ok(s) => s,
            Err(e) => {
                diagnostics.push(e.into());
                return;
            }
        };
        let label = diagnostic::get_f_macro_invocation_span(&source, location)
            .map(|s| s.labeled(format!("The {component_kind} was registered here")));
        let help = "All singletons must implement the `Send`, `Sync` and `Clone` traits.\n \
                `pavex` runs on a multi-threaded HTTP server and singletons must be shared \
                 across all worker threads."
            .into();
        let diagnostic = CompilerDiagnostic::builder(source, e)
            .optional_label(label)
            .help(help)
            .build();
        diagnostics.push(diagnostic.into());
    }

    let send = process_framework_path("core::marker::Send", package_graph, krate_collection);
    let sync = process_framework_path("core::marker::Sync", package_graph, krate_collection);
    let clone = process_framework_path("core::clone::Clone", package_graph, krate_collection);
    for singleton_type in runtime_singletons {
        for trait_ in [&send, &sync, &clone] {
            let ResolvedType::ResolvedPath(trait_) = trait_ else { unreachable!() };
            if let Err(e) = assert_trait_is_implemented(krate_collection, singleton_type, trait_) {
                missing_trait_implementation(
                    e,
                    package_graph,
                    constructible_db,
                    component_db,
                    user_component_db,
                    raw_identifiers_db,
                    diagnostics,
                );
            }
        }
    }
}
