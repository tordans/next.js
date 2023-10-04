use anyhow::{bail, Result};
use indexmap::IndexMap;
use turbo_tasks::{
    graph::{GraphTraversal, NonDeterministic},
    Value, ValueToString, Vc,
};
use turbopack_binding::{
    swc::core::ecma::{
        ast::{CallExpr, Callee, Expr, Ident, Lit},
        visit::{Visit, VisitWith},
    },
    turbopack::{
        core::{
            issue::{IssueSeverity, OptionIssueSource},
            module::Module,
            output::OutputAssets,
            reference::primary_referenced_modules,
            reference_type::EcmaScriptModulesReferenceSubType,
            resolve::{origin::PlainResolveOrigin, parse::Request, pattern::Pattern},
        },
        ecmascript::{
            chunk::EcmascriptChunkPlaceable, parse::ParseResult, resolve::esm_resolve,
            EcmascriptModuleAsset,
        },
    },
};

pub(crate) async fn collect_next_dynamic_imports(
    entry: Vc<Box<dyn EcmascriptChunkPlaceable>>,
) -> Result<IndexMap<Vc<Box<dyn Module>>, Vec<(String, Vc<Box<dyn Module>>)>>> {
    // Traverse referenced modules graph, collect all of the dynamic imports:
    // - Read the Program AST of the Module, this is the origin (A)
    //  - If there's `dynamic(import(B))`, then B is the module that is being
    //    imported
    // Returned import mappings are in the form of
    // (Module<A>, Vec<(B, Module<B>)>) (where B is the raw import source string,
    // and Module<B> is the actual resolved Module)
    let imported_modules_mapping = NonDeterministic::new()
        .skip_duplicates()
        .visit([Vc::upcast(entry)], get_referenced_modules)
        .await
        .completed()?
        .into_inner()
        .into_iter()
        .map(|module| build_dynamic_imports_map_for_module(module));

    // Consolidate import mappings into a single indexmap
    let mut import_mappings: IndexMap<Vc<Box<dyn Module>>, Vec<(String, Vc<Box<dyn Module>>)>> =
        IndexMap::new();

    for module_mapping in imported_modules_mapping {
        if let Some(module_mapping) = &*module_mapping.await? {
            let (origin_module, dynamic_imports) = &*module_mapping.await?;
            import_mappings
                .entry(origin_module.clone())
                .or_insert_with(Vec::new)
                .extend(dynamic_imports.clone().drain(..))
        }
    }

    Ok(import_mappings)
}

async fn get_referenced_modules(
    parent: Vc<Box<dyn Module>>,
) -> Result<impl Iterator<Item = Vc<Box<dyn Module>>> + Send> {
    primary_referenced_modules(parent)
        .await
        .map(|modules| modules.clone_value().into_iter())
}

#[turbo_tasks::function]
async fn build_dynamic_imports_map_for_module(
    module: Vc<Box<dyn Module>>,
) -> Result<Vc<OptionDynamicImportsMap>> {
    let Some(ecmascript_asset) =
        Vc::try_resolve_downcast_type::<EcmascriptModuleAsset>(module).await?
    else {
        return Ok(OptionDynamicImportsMap::none());
    };

    let ParseResult::Ok { program, .. } = &*ecmascript_asset.parse().await? else {
        bail!(
            "failed to parse module '{}'",
            &*module.ident().to_string().await?
        );
    };

    // Reading the Program AST, collect raw imported module str if it's wrapped in
    // dynamic()
    let mut visitor = LodableImportVisitor::new();
    program.visit_with(&mut visitor);

    if visitor.import_sources.is_empty() {
        return Ok(OptionDynamicImportsMap::none());
    }

    let mut import_sources = vec![];
    for import in visitor.import_sources.drain(..) {
        // Using the given `Module` which is the origin of the dynamic import, trying to
        // resolve the module that is being imported.
        let dynamic_imported_resolved_module = *esm_resolve(
            Vc::upcast(PlainResolveOrigin::new(
                ecmascript_asset.await?.asset_context,
                module.ident().path(),
            )),
            Request::parse(Value::new(Pattern::Constant(import.to_string()))),
            Value::new(EcmaScriptModulesReferenceSubType::Undefined),
            OptionIssueSource::none(),
            IssueSeverity::Error.cell(),
        )
        .first_module()
        .await?;

        if let Some(dynamic_imported_resolved_module) = dynamic_imported_resolved_module {
            import_sources.push((import, dynamic_imported_resolved_module));
        }
    }

    Ok(Vc::cell(Some(Vc::cell((module.clone(), import_sources)))))
}

/// A visitor to check if there's import to `next/dynamic`, then collecting the import wrapped with dynamic()
/// via CollectImportSourceVisitor.
struct LodableImportVisitor {
    dynamic_ident: Option<Ident>,
    pub import_sources: Vec<String>,
}

impl LodableImportVisitor {
    fn new() -> Self {
        Self {
            import_sources: vec![],
            dynamic_ident: None,
        }
    }
}

impl Visit for LodableImportVisitor {
    fn visit_import_decl(&mut self, decl: &turbopack_binding::swc::core::ecma::ast::ImportDecl) {
        // find import decl from next/dynamic, i.e import dynamic from 'next/dynamic'
        if decl.src.value == *"next/dynamic" {
            if let Some(specifier) = decl.specifiers.first().map(|s| s.as_default()).flatten() {
                self.dynamic_ident = Some(specifier.local.clone());
            }
        }
    }

    fn visit_call_expr(&mut self, call_expr: &CallExpr) {
        // Collect imports if the import call is wrapped in the call dynamic()
        if let Callee::Expr(ident) = &call_expr.callee {
            if let Expr::Ident(ident) = &**ident {
                if let Some(dynamic_ident) = &self.dynamic_ident {
                    if ident.sym == *dynamic_ident.sym {
                        let mut collect_import_source_visitor = CollectImportSourceVisitor::new();
                        call_expr.visit_children_with(&mut collect_import_source_visitor);

                        if let Some(import_source) = collect_import_source_visitor.import_source {
                            self.import_sources.push(import_source);
                        }
                    }
                }
            }
        }

        call_expr.visit_children_with(self);
    }
}

/// A visitor to collect import source string from import('path/to/module')
struct CollectImportSourceVisitor {
    import_source: Option<String>,
}

impl CollectImportSourceVisitor {
    fn new() -> Self {
        Self {
            import_source: None,
        }
    }
}

impl Visit for CollectImportSourceVisitor {
    fn visit_call_expr(&mut self, call_expr: &CallExpr) {
        // find import source from import('path/to/module')
        // [NOTE]: Turbopack does not support webpack-specific comment directives, i.e
        // import(/* webpackChunkName: 'hello1' */ '../../components/hello3')
        // Renamed chunk in the comment will be ignored.
        if let Callee::Import(_import) = call_expr.callee {
            if let Some(arg) = call_expr.args.first() {
                if let Expr::Lit(lit) = &*arg.expr {
                    if let Lit::Str(str_) = &lit {
                        self.import_source = Some(str_.value.to_string());
                    }
                }
            }
        }

        // Don't need to visit children, we expect import() won't have any
        // nested calls as dynamic() should be statically analyzable import.
    }
}

/// A struct contains mapping for the dynamic imports to construct chunk per
/// each individual module (Origin Module, Vec<(ImportSourceString, Module)>)
#[turbo_tasks::value(transparent)]
pub struct DynamicImportsMap(pub (Vc<Box<dyn Module>>, Vec<(String, Vc<Box<dyn Module>>)>));

/// An Option wrapper around [DynamicImportsMap].
#[turbo_tasks::value(transparent)]
pub struct OptionDynamicImportsMap(Option<Vc<DynamicImportsMap>>);

#[turbo_tasks::value_impl]
impl OptionDynamicImportsMap {
    #[turbo_tasks::function]
    pub fn none() -> Vc<Self> {
        Vc::cell(None)
    }
}

#[turbo_tasks::value(transparent)]
pub struct DynamicImportedChunks(
    pub IndexMap<Vc<Box<dyn Module>>, Vec<(String, Vc<OutputAssets>)>>,
);
