//! Models the semantic relationships that pytest creates between fixtures and parameters.
//!
//! Pytest injects fixture values by matching parameter names to fixture providers. Ordinary Python
//! name resolution does not represent that relationship: the parameter is a local definition, and
//! the fixture function may be defined in another provider scope. This module therefore overlays
//! the pytest relationship on top of the parameter's normal Python definition (which is preserved).
//!
//! The model distinguishes four concepts:
//!
//! - A [`FixtureDeclaration`] is a function decorated with pytest's canonical `fixture` or
//!   `yield_fixture` decorator.
//! - A [`FixtureExposure`] is the name under which a provider makes that declaration available.
//!   The decorator's `name` argument can make this differ from the Python binding name.
//! - A [`FixtureRequest`] is an eligible parameter in a collected test or another fixture function.
//! - A [`FixtureBinding`] links a request to the declaration selected by static fixture provider lookup.
//!
//! For example:
//!
//! ```py
//! import pytest
//!
//! @pytest.fixture(name="database")  # Exposure: the public fixture name is `database`.
//! def make_database():              # Declaration: this decorated function is the fixture identity.
//!     return object()
//!
//! # Request: the parameter asks for the fixture exposed as `database`.
//! # Binding: provider lookup connects the request to the `make_database` declaration.
//! def test_query(database):
//!     assert database is not None
//! ```
//!
//! [`fixture_bindings_for_parameter`] provides the public interface to the model. Given a parameter
//! definition, it classifies the parameter as a possible request, searches providers in pytest
//! precedence order, and returns every equally viable declaration in the first matching provider
//! layer. Language server and type-inference features can consume this data without changing
//! general definition, reference or rename behavior for the parameter.

use std::cmp::Ordering;

use ruff_db::files::system_path_to_file;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::{self as ast, name::Name};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;
use ty_module_resolver::{ImportingFile, KnownModule, file_to_module, resolve_module};
use ty_python_core::definition::{Definition, DefinitionKind, ParameterDefinitionNodeKind};
use ty_python_core::scope::{FileScopeId, ScopeId, ScopeKind};
use ty_python_core::{ProgramFile, global_scope, place_table, semantic_index, use_def_map};

use crate::Db;
use crate::types::function::FunctionDecorators;
use crate::types::ide_support::{
    ResolvedDefinition, map_stub_definition, resolve_definition_targets,
};
use crate::types::infer::{
    function_known_decorator_flags, function_known_decorators, original_class_type,
};
use crate::types::{ClassBase, KnownClass, ProgramEnvironment, Type, definition_expression_type};

/// Resolves pytest fixtures requested by `parameter`.
///
/// The fixture resolution implemented here is best-effort. Specifically,
/// fixtures are looked up lexically. This matches how the fixture is actually
/// resolved at runtime for a parameter on a collected test, but it is
/// incorrect for a parameter on a fixture declaration itself when the test
/// that requests the fixture overrides one of the fixture's dependencies. For
/// example:
///
/// ```py
/// import pytest
///
/// @pytest.fixture
/// def dependency(): ...
///
/// @pytest.fixture
/// def consumer(dependency): ... # fixture_bindings_for_parameter called here for `dependency`
///
/// class TestOverride:
///     @pytest.fixture
///     def dependency(self): ...
///
///     def test_consumer(self, consumer): ...
/// ```
///
/// At runtime, pytest resolves `consumer`'s `dependency` parameter to
/// `TestOverride.dependency`; this query instead resolves it to the
/// module-level `dependency` fixture.
///
/// This query searches the parameter's class hierarchy, module, and enclosing conftest hierarchy.
/// Built-in and plugin fixtures are added by later provider layers.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn fixture_bindings_for_parameter<'db>(
    db: &'db dyn Db,
    parameter: Definition<'db>,
) -> Box<[FixtureBinding<'db>]> {
    let Some(request) = FixtureRequest::from_parameter(db, parameter) else {
        return Box::default();
    };

    if let Some(class_scope) = request.class_scope {
        let file = parameter.program_file(db);
        let index = semantic_index(db, file);
        for class_scope in std::iter::successors(Some(class_scope), |scope| {
            let parent = non_type_parameter_parent(index, *scope)?;
            (index.scope(parent).kind() == ScopeKind::Class).then_some(parent)
        }) {
            let bindings = bindings_in_provider(db, &request, FixtureProvider::Class(class_scope));
            if !bindings.is_empty() {
                return bindings;
            }
        }
    }

    let request_file = parameter.program_file(db);
    let bindings = bindings_in_provider(
        db,
        &request,
        FixtureProvider::Scope(global_scope(db, request_file)),
    );
    if !bindings.is_empty() {
        return bindings;
    }

    for conftest in conftest_files(db, request_file) {
        let bindings = bindings_in_provider(
            db,
            &request,
            FixtureProvider::Scope(global_scope(db, *conftest)),
        );
        if !bindings.is_empty() {
            return bindings;
        }
    }

    Box::default()
}

/// A pytest fixture request and the declaration selected by static provider lookup.
#[derive(Debug, Clone, Copy, Eq, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct FixtureBinding<'db> {
    request: Definition<'db>,
    fixture: Definition<'db>,
}

impl<'db> FixtureBinding<'db> {
    /// Returns the parameter definition that requests the fixture.
    pub fn request(self) -> Definition<'db> {
        self.request
    }

    /// Returns the decorated function that declares the fixture.
    pub fn fixture(self) -> Definition<'db> {
        self.fixture
    }
}

/// An eligible fixture parameter and the context needed to resolve its request.
#[derive(Debug)]
struct FixtureRequest<'db> {
    definition: Definition<'db>,
    function_definition: Definition<'db>,
    name: Name,
    class_scope: Option<FileScopeId>,
}

impl<'db> FixtureRequest<'db> {
    fn from_parameter(db: &'db dyn Db, definition: Definition<'db>) -> Option<Self> {
        let DefinitionKind::Parameter(ParameterDefinitionNodeKind::Parameter(parameter)) =
            definition.kind(db)
        else {
            return None;
        };

        let file = definition.program_file(db);
        let module = parsed_module(db, file.python_file(db)).load(db);
        let parameter = parameter.node(&module);
        let index = semantic_index(db, file);
        let function_scope = definition.scope(db).file_scope_id(db);
        let function_ref = index.scope(function_scope).node().as_function()?;
        let function = function_ref.node(&module);
        let function_definition = index.expect_single_definition(function_ref);

        // Match pytest's logic for only injecting fixtures for required
        // parameters and by keyword:
        // https://docs.pytest.org/en/9.0.x/how-to/fixtures.html#requesting-fixtures
        // https://github.com/pytest-dev/pytest/blob/9.0.1/src/_pytest/compat.py#L145-L153
        if parameter.default.is_some() || is_positional_only_parameter(function, parameter) {
            return None;
        }

        let parent_scope = non_type_parameter_parent(index, function_scope)?;
        let parent_kind = index.scope(parent_scope).kind();
        if !matches!(parent_kind, ScopeKind::Module | ScopeKind::Class) {
            return None;
        }

        let class_scope = (parent_kind == ScopeKind::Class).then_some(parent_scope);
        if class_scope.is_some() && is_method_receiver(db, function_definition, function, parameter)
        {
            return None;
        }

        if is_mock_patch_parameter(db, function_definition, function, parameter, class_scope) {
            return None;
        }

        // Check whether the fixture request itself appears in a fixture declaration e.g.
        //
        // ```py
        // @pytest.fixture
        // def database(): ...
        //
        // @pytest.fixture
        // def service(database): ...  # `database` is a fixture request.
        // ```
        let is_fixture_dependency = fixture_declaration(db, function_definition).is_some();
        if !is_fixture_dependency
            && (!is_collected_test(db, file, function, class_scope, &module)
                || class_scope
                    .is_some_and(|class_scope| is_unittest_test_case(db, file, class_scope)))
        {
            return None;
        }

        let name = parameter.name().id.clone();
        if !is_fixture_dependency
            && directly_parametrized(
                db,
                function_definition,
                function,
                class_scope,
                &module,
                index,
                name.as_str(),
            )
        {
            return None;
        }

        Some(Self {
            definition,
            function_definition,
            name,
            class_scope,
        })
    }
}

/// Returns whether `parameter` is positional-only in `function`.
fn is_positional_only_parameter(
    function: &ast::StmtFunctionDef,
    parameter: &ast::ParameterWithDefault,
) -> bool {
    function
        .parameters
        .posonlyargs
        .iter()
        .any(|candidate| candidate.range() == parameter.range())
}

/// Returns whether `parameter` is the implicit receiver of a method.
fn is_method_receiver<'db>(
    db: &'db dyn Db,
    function_definition: Definition<'db>,
    function: &ast::StmtFunctionDef,
    parameter: &ast::ParameterWithDefault,
) -> bool {
    function
        .parameters
        .posonlyargs
        .first()
        .or_else(|| function.parameters.args.first())
        .is_some_and(|first| first.range() == parameter.range())
        && !function_known_decorator_flags(db, function_definition)
            .contains(FunctionDecorators::STATICMETHOD)
}

/// Returns whether `parameter` is supplied by `unittest.mock.patch`.
fn is_mock_patch_parameter(
    db: &dyn Db,
    function_definition: Definition<'_>,
    function: &ast::StmtFunctionDef,
    parameter: &ast::ParameterWithDefault,
    class_scope: Option<FileScopeId>,
) -> bool {
    let patch_count = function
        .decorator_list
        .iter()
        .filter(|decorator| {
            let Some(call) = decorator.expression.as_call_expr() else {
                return false;
            };
            call.arguments.find_argument_value("new", 1).is_none()
                && is_known_class_instance(
                    db,
                    function_definition,
                    definition_expression_type(db, function_definition, &call.func),
                    "_patcher",
                    &[KnownModule::UnittestMock],
                )
        })
        .count();

    let skips_receiver = class_scope.is_some()
        && function.parameters.posonlyargs.is_empty()
        && !function_known_decorator_flags(db, function_definition)
            .contains(FunctionDecorators::STATICMETHOD);
    function
        .parameters
        .args
        .iter()
        .chain(&function.parameters.kwonlyargs)
        .filter(|parameter| parameter.default.is_none())
        .skip(usize::from(skips_receiver))
        .take(patch_count)
        .any(|candidate| candidate.range() == parameter.range())
}

/// A decorated fixture function.
#[derive(Debug, Clone)]
struct FixtureDeclaration<'db> {
    // The definition for the fixture function.
    definition: Definition<'db>,
    // The way in which the fixture exposes a name.
    name: FixtureName,
}

/// A fixture declaration made available under a name in a provider scope.
#[derive(Debug, Clone)]
struct FixtureExposure<'db> {
    name: Name,
    declaration: FixtureDeclaration<'db>,
}

/// How a fixture decorator determines the fixture's public name.
#[derive(Debug, Clone)]
enum FixtureName {
    /// Uses the Python binding name at the exposure site.
    Default,
    /// Uses a statically known explicit name.
    Explicit(Name),
    /// Uses a dynamically computed name (we do not support this type of fixture).
    Dynamic,
}

/// A fixture provider layer in which to resolve a request.
#[derive(Clone, Copy)]
enum FixtureProvider<'db> {
    /// Uses a class and its statically known ancestors.
    Class(FileScopeId),
    /// Uses a single provider scope.
    Scope(ScopeId<'db>),
}

/// Resolves a request against the fixture exposures in `provider`.
fn bindings_in_provider<'db>(
    db: &'db dyn Db,
    request: &FixtureRequest<'db>,
    provider: FixtureProvider<'db>,
) -> Box<[FixtureBinding<'db>]> {
    let class_scopes;
    let provider_scopes = match &provider {
        FixtureProvider::Class(class_scope) => {
            class_scopes = class_mro_scopes(db, request.definition.program_file(db), *class_scope);
            class_scopes.as_slice()
        }
        FixtureProvider::Scope(scope) => std::slice::from_ref(scope),
    };

    let mut seen_attributes = FxHashSet::default();
    let mut winning_attribute: Option<Name> = None;
    let mut bindings = Vec::new();

    for provider_scope in provider_scopes {
        let table = place_table(db, *provider_scope);
        let use_def = use_def_map(db, *provider_scope);
        for (symbol_id, definitions) in use_def.all_end_of_scope_symbol_bindings() {
            let symbol_name = table.symbol(symbol_id).name();
            // An attribute supplied by an earlier scope shadows the same-named attribute here.
            if !seen_attributes.insert(symbol_name.clone()) {
                continue;
            }
            for definition in definitions.filter_map(|binding| binding.binding.definition()) {
                for definition in resolve_definition_targets(db, definition, symbol_name) {
                    for declaration in fixture_declarations_for_definition(db, definition) {
                        let definition = declaration.definition;
                        let Some(exposure) = fixture_exposure(symbol_name, declaration) else {
                            continue;
                        };

                        // Request must match public name of the fixture
                        if request.name != exposure.name
                            // A fixture definition cannot fulfill a request for itself
                            || request.function_definition == exposure.declaration.definition
                        {
                            continue;
                        }

                        // Semantic-index traversal is unordered. Pytest registers fixture
                        // attributes in sorted `dir()` order and selects the last registration, so
                        // retain bindings for the lexicographically last matching attribute. Thus,
                        // if `first_provider` and `second_provider` both expose `resource`,
                        // `second_provider` wins.
                        //
                        // `dir()` ordering: https://docs.python.org/3/library/functions.html#dir
                        // Fixture discovery: https://github.com/pytest-dev/pytest/blob/9.0.1/src/_pytest/fixtures.py#L1852-L1880
                        // Registration order: https://github.com/pytest-dev/pytest/blob/9.0.1/src/_pytest/fixtures.py#L1788-L1797
                        // Fixture selection: https://github.com/pytest-dev/pytest/blob/9.0.1/src/_pytest/fixtures.py#L583-L599
                        match winning_attribute
                            .as_ref()
                            .map(|winner| winner.cmp(symbol_name))
                        {
                            Some(Ordering::Greater) => continue,
                            Some(Ordering::Less) | None => {
                                winning_attribute = Some(symbol_name.clone());
                                bindings.clear();
                            }
                            Some(Ordering::Equal) => {}
                        }
                        if bindings
                            .iter()
                            .any(|binding: &FixtureBinding<'db>| binding.fixture == definition)
                        {
                            continue;
                        }
                        bindings.push(FixtureBinding {
                            request: request.definition,
                            fixture: definition,
                        });
                    }
                }
            }
        }
    }

    bindings.into_boxed_slice()
}

/// Returns the scopes that supply effective attributes for a class.
fn class_mro_scopes<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class_scope: FileScopeId,
) -> Vec<ScopeId<'db>> {
    let requesting_class_scope = class_scope.to_scope_id(db, file);
    let index = semantic_index(db, file);
    let class_ref = index.scope(class_scope).node().expect_class();
    let class_definition = index.expect_single_definition(class_ref);
    let Some(class) = original_class_type(db, class_definition) else {
        return vec![requesting_class_scope];
    };

    let mut scopes = vec![requesting_class_scope];
    let mut seen: FxHashSet<_> = scopes.iter().copied().collect();
    for ancestor in class.iter_mro(db).skip(1) {
        let ClassBase::Class(ancestor) = ancestor else {
            continue;
        };
        let Some((ancestor, _)) = ancestor.static_class_literal(db) else {
            continue;
        };
        if ancestor.is_known(db, KnownClass::Object) {
            continue;
        }
        let ancestor_scope = ancestor.body_scope(db);
        if seen.insert(ancestor_scope) {
            scopes.push(ancestor_scope);
        }
    }

    scopes
}

/// Returns applicable `conftest.py` files from nearest to outermost.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn conftest_files<'db>(db: &'db dyn Db, request_file: ProgramFile<'db>) -> Box<[ProgramFile<'db>]> {
    let file = request_file.file(db);
    let Some(path) = file.path(db).as_system_path() else {
        return Box::default();
    };
    let Some(file_root) = db.files().root(db, path) else {
        return Box::default();
    };
    let root = file_root.path(db);
    let Some(request_directory) = path.parent() else {
        return Box::default();
    };

    // The current conftest's module scope was already searched above. Starting at its parent avoids
    // processing the same provider twice and lets same-name overrides continue outward.
    let start_directory = if path.file_name() == Some("conftest.py") {
        request_directory.parent()
    } else {
        Some(request_directory)
    };
    let Some(start_directory) = start_directory else {
        return Box::default();
    };

    start_directory
        .ancestors()
        .take_while(|directory| directory.starts_with(root))
        .filter_map(|directory| system_path_to_file(db, directory.join("conftest.py")).ok())
        .map(|file| ProgramFile::new(db, file, request_file.program(db)))
        .collect()
}

/// Exposes a declaration under its explicit fixture name or local Python binding name.
fn fixture_exposure<'db>(
    symbol_name: &Name,
    declaration: FixtureDeclaration<'db>,
) -> Option<FixtureExposure<'db>> {
    let name = match &declaration.name {
        FixtureName::Default => symbol_name.clone(),
        FixtureName::Explicit(name) => name.clone(),
        FixtureName::Dynamic => return None,
    };
    Some(FixtureExposure { name, declaration })
}

/// Returns a fixture declaration for a function with a canonical pytest fixture decorator.
fn fixture_declaration<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<FixtureDeclaration<'db>> {
    let DefinitionKind::Function(function_ref) = definition.kind(db) else {
        return None;
    };
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = function_ref.node(&module);
    let inference = function_known_decorators(db, definition);
    let expression = &function.decorator_list.first()?.expression;
    let (callee, name) = match expression {
        ast::Expr::Call(call) => (
            call.func.as_ref(),
            fixture_name_from_arguments(&call.arguments),
        ),
        expression => (expression, FixtureName::Default),
    };
    let Type::FunctionLiteral(decorator) = inference.expression_type(callee)? else {
        return None;
    };
    if file_to_module(db, decorator.program_file(db).resolver_file(db))
        .is_some_and(|module| module.known(db) == Some(KnownModule::PytestFixtures))
        && matches!(decorator.name(db).as_str(), "fixture" | "yield_fixture")
    {
        Some(FixtureDeclaration { definition, name })
    } else {
        None
    }
}

/// Returns fixture declarations for a definition, mapping a stub to its source when possible.
fn fixture_declarations_for_definition<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Vec<FixtureDeclaration<'db>> {
    if definition.file(db).is_stub(db) {
        let resolved = ResolvedDefinition::Definition(definition);
        if let Some(mapped) = map_stub_definition(db, &resolved, None) {
            return mapped
                .into_iter()
                .filter_map(|resolved| resolved.definition())
                .filter_map(|definition| fixture_declaration(db, definition))
                .collect();
        }
    }
    fixture_declaration(db, definition).into_iter().collect()
}

/// Classifies the `name` argument to a fixture decorator.
fn fixture_name_from_arguments(arguments: &ast::Arguments) -> FixtureName {
    let Some(name_keyword_value) = arguments
        .keywords
        .iter()
        .find(|keyword| keyword.arg.as_ref().is_some_and(|arg| arg == "name"))
        .map(|keyword| &keyword.value)
    else {
        return FixtureName::Default;
    };

    if name_keyword_value.is_none_literal_expr() {
        FixtureName::Default
    } else if let Some(string) = name_keyword_value.as_string_literal_expr() {
        let name_keyword_value = string.value.to_str();
        if name_keyword_value.is_empty() {
            FixtureName::Default
        } else {
            FixtureName::Explicit(Name::new(name_keyword_value))
        }
    } else {
        FixtureName::Dynamic
    }
}

/// Returns a scope's lexical parent, skipping an intervening type-parameter scope.
fn non_type_parameter_parent(
    index: &ty_python_core::SemanticIndex<'_>,
    scope: FileScopeId,
) -> Option<FileScopeId> {
    let parent = index.parent_scope_id(scope)?;
    if index.scope(parent).kind() == ScopeKind::TypeParams {
        index.parent_scope_id(parent)
    } else {
        Some(parent)
    }
}

/// Returns whether a function matches pytest's default naming conventions for
/// [test discovery](https://docs.pytest.org/en/9.0.x/explanation/goodpractices.html#test-discovery).
fn is_collected_test(
    db: &dyn Db,
    file: ProgramFile<'_>,
    function: &ast::StmtFunctionDef,
    class_scope: Option<FileScopeId>,
    module: &ParsedModuleRef,
) -> bool {
    let Some(file_name) = file
        .file(db)
        .path(db)
        .as_system_path()
        .and_then(|path| path.file_name())
    else {
        return false;
    };

    let Some(stem) = file_name.strip_suffix(".py") else {
        return false;
    };

    if !(stem.starts_with("test_") || stem.ends_with("_test"))
        || !function.name.as_str().starts_with("test")
    {
        return false;
    }

    let index = semantic_index(db, file);
    let mut class_scope = class_scope;
    while let Some(scope) = class_scope {
        let Some(class_ref) = index.scope(scope).node().as_class() else {
            return false;
        };
        if !class_ref.node(module).name.as_str().starts_with("Test") {
            return false;
        }
        let Some(parent_scope) = non_type_parameter_parent(index, scope) else {
            return false;
        };
        match index.scope(parent_scope).kind() {
            ScopeKind::Class => class_scope = Some(parent_scope),
            ScopeKind::Module => class_scope = None,
            _ => return false,
        }
    }
    true
}

/// Returns whether the class inherits from the canonical `unittest.TestCase`.
///
/// Pytest does not inject fixture parameters into
/// [`unittest.TestCase` methods](https://docs.pytest.org/en/9.0.x/how-to/unittest.html#pytest-features-in-unittest-testcase-subclasses).
fn is_unittest_test_case(db: &dyn Db, file: ProgramFile<'_>, class_scope: FileScopeId) -> bool {
    let index = semantic_index(db, file);
    let class_ref = index.scope(class_scope).node().expect_class();
    let definition = index.expect_single_definition(class_ref);
    let Some(class) = original_class_type(db, definition) else {
        return false;
    };

    class.iter_mro(db).any(|ancestor| {
        let ClassBase::Class(ancestor) = ancestor else {
            return false;
        };
        let Some((ancestor, _)) = ancestor.static_class_literal(db) else {
            return false;
        };
        ancestor.name(db) == "TestCase"
            && file_to_module(db, ancestor.program_file(db).resolver_file(db))
                .is_some_and(|module| module.name(db).as_str() == "unittest.case")
    })
}

/// Returns whether static parametrization prevents this fixture request.
fn directly_parametrized(
    db: &dyn Db,
    function_definition: Definition<'_>,
    function: &ast::StmtFunctionDef,
    class_scope: Option<FileScopeId>,
    module: &ParsedModuleRef,
    index: &ty_python_core::SemanticIndex<'_>,
    parameter_name: &str,
) -> bool {
    if function.decorator_list.iter().any(|decorator| {
        mark_excludes_fixture(
            db,
            function_definition,
            &decorator.expression,
            parameter_name,
        )
    }) {
        return true;
    }

    class_scope.is_some_and(|class_scope| {
        let class_ref = index.scope(class_scope).node().expect_class();
        let definition = index.expect_single_definition(class_ref);
        class_ref
            .node(module)
            .decorator_list
            .iter()
            .any(|decorator| {
                mark_excludes_fixture(db, definition, &decorator.expression, parameter_name)
            })
    })
}

/// Returns whether a static mark supplies this parameter directly or cannot be interpreted.
fn mark_excludes_fixture(
    db: &dyn Db,
    definition: Definition<'_>,
    expression: &ast::Expr,
    parameter_name: &str,
) -> bool {
    let Some(call) = expression.as_call_expr() else {
        return false;
    };
    let Some(attribute) = call.func.as_attribute_expr() else {
        return false;
    };
    if attribute.attr.as_str() != "parametrize"
        || !is_known_class_instance(
            db,
            definition,
            definition_expression_type(db, definition, &attribute.value),
            "MarkGenerator",
            &[KnownModule::Pytest, KnownModule::PytestMarkStructures],
        )
    {
        return false;
    }

    let Some(names) = parametrized_names(&call.arguments) else {
        return true;
    };
    if !names.contains(&parameter_name) {
        return false;
    }

    is_indirect(&call.arguments, parameter_name) != Some(true)
}

/// Returns whether a type is an instance of `class_name` from one of `modules`.
fn is_known_class_instance(
    db: &dyn Db,
    definition: Definition<'_>,
    ty: Type<'_>,
    class_name: &str,
    modules: &[KnownModule],
) -> bool {
    let Some(instance) = ty.as_nominal_instance() else {
        return false;
    };
    let environment = ProgramEnvironment::from_file(definition.program_file(db));
    if instance.class_name(db, &environment) != class_name {
        return false;
    }
    let Some(module_name) = instance.class_module_name(db, &environment) else {
        return false;
    };
    let importing_file = ImportingFile::ResolverFile(definition.program_file(db).resolver_file(db));

    resolve_module(db, importing_file, module_name)
        .and_then(|module| module.known(db))
        .is_some_and(|module| modules.contains(&module))
}

/// Returns the statically known parameter names passed to `pytest.mark.parametrize`.
fn parametrized_names(arguments: &ast::Arguments) -> Option<Vec<&str>> {
    let expression = arguments.args.first().or_else(|| {
        arguments
            .keywords
            .iter()
            .find(|keyword| keyword.arg.as_ref().is_some_and(|arg| arg == "argnames"))
            .map(|keyword| &keyword.value)
    })?;
    static_string_list(expression)
}

/// Returns how `parameter_name` is configured by the `indirect` argument.
///
/// `Some(true)` means the parameter is definitely indirect, `Some(false)` means it is definitely
/// direct, and `None` preserves uncertainty when the argument cannot be interpreted statically.
fn is_indirect(arguments: &ast::Arguments, parameter_name: &str) -> Option<bool> {
    let Some(expression) = arguments.find_argument_value("indirect", 2) else {
        return Some(false);
    };
    if let Some(boolean) = expression.as_boolean_literal_expr() {
        return Some(boolean.value);
    }
    static_string_list(expression).map(|names| names.contains(&parameter_name))
}

/// Extracts strings from a static pytest name string, list, or tuple.
fn static_string_list(expression: &ast::Expr) -> Option<Vec<&str>> {
    if let Some(string) = expression.as_string_literal_expr() {
        return Some(
            string
                .value
                .to_str()
                .split(|character: char| character == ',' || character.is_whitespace())
                .filter(|name| !name.is_empty())
                .collect(),
        );
    }

    let (ast::Expr::List(ast::ExprList { elts: elements, .. })
    | ast::Expr::Tuple(ast::ExprTuple { elts: elements, .. })) = expression
    else {
        return None;
    };

    elements
        .iter()
        .map(|element| {
            element
                .as_string_literal_expr()
                .map(|string| string.value.to_str())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_db::diagnostic::{
        Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, DisplayDiagnostics,
        Severity, SubDiagnostic, SubDiagnosticSeverity,
    };
    use ruff_db::files::system_path_to_file;
    use ruff_db::parsed::parsed_module;
    use ruff_db::system::DbWithWritableSystem;
    use ruff_python_ast as ast;
    use ty_python_core::definition::Definition;
    use ty_python_core::semantic_index;

    use super::fixture_bindings_for_parameter;
    use crate::Db as _;
    use crate::db::tests::{TestDb, TestDbBuilder};

    #[test]
    fn resolves_same_file_fixture_declarations_and_dependencies() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest
from pytest import fixture as make_fixture, yield_fixture

@pytest.fixture
def database(): ...

@make_fixture()
@pytest.mark.parametrize("database", [1])
def service(database): ...

@yield_fixture()
def legacy_cache(): ...

def test_use(database, service, legacy_cache): ...

def wrapper(function): return lambda: function()

@wrapper
@pytest.fixture
def wrapped(): ...

def test_wrapped(wrapped): ...
"#,
        );

        let service = test.function("service");
        let test_use = test.function("test_use");
        let test_wrapped = test.function("test_wrapped");

        assert_snapshot!(service.fixture_resolution("database"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:10:13
           |
        10 | def service(database): ...
           |             ^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:6:5
          |
        6 | def database(): ...
          |     --------
        ");

        assert_snapshot!(test_use.fixture_resolution("database"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:15:14
           |
        15 | def test_use(database, service, legacy_cache): ...
           |              ^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:6:5
          |
        6 | def database(): ...
          |     --------
        ");

        assert_snapshot!(test_use.fixture_resolution("service"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:15:24
           |
        15 | def test_use(database, service, legacy_cache): ...
           |                        ^^^^^^^ fixture requested here
        info: Found 1 fixture
          --> src/test_example.py:10:5
           |
        10 | def service(database): ...
           |     -------
        ");

        assert_snapshot!(test_use.fixture_resolution("legacy_cache"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:15:33
           |
        15 | def test_use(database, service, legacy_cache): ...
           |                                 ^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
          --> src/test_example.py:13:5
           |
        13 | def legacy_cache(): ...
           |     ------------
        ");

        assert_snapshot!(test_wrapped.fixture_resolution("wrapped"), @"No fixture resolved for parameter `wrapped`");
    }

    #[test]
    fn honors_explicit_names_and_ignores_dynamic_names() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

fixture_name = "dynamic"

@pytest.fixture(name="public_name")
def implementation(): ...

@pytest.fixture(name="public_name")
def later_implementation(): ...

@pytest.fixture(name=fixture_name)
def dynamic_implementation(): ...

def test_use(public_name, implementation, dynamic): ...
"#,
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("public_name"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:15:14
           |
        15 | def test_use(public_name, implementation, dynamic): ...
           |              ^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
          --> src/test_example.py:10:5
           |
        10 | def later_implementation(): ...
           |     --------------------
        ");

        assert_snapshot!(test_use.fixture_resolution("implementation"), @"No fixture resolved for parameter `implementation`");
        assert_snapshot!(test_use.fixture_resolution("dynamic"), @"No fixture resolved for parameter `dynamic`");
    }

    #[test]
    fn prefers_class_fixtures_and_skips_method_receivers() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

@pytest.fixture
def value(): ...

class TestExample:
    @pytest.fixture
    def value(self): ...

    @pytest.fixture
    def dependent(self, value): ...

    def test_use(self, value, dependent): ...
"#,
        );

        let test_use = test.function("TestExample.test_use");
        let dependent = test.function("TestExample.dependent");

        assert_snapshot!(test_use.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:14:24
           |
        14 |     def test_use(self, value, dependent): ...
           |                        ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:9:9
          |
        9 |     def value(self): ...
          |         -----
        ");

        assert_snapshot!(dependent.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:12:25
           |
        12 |     def dependent(self, value): ...
           |                         ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:9:9
          |
        9 |     def value(self): ...
          |         -----
        ");

        assert_snapshot!(test_use.fixture_resolution("self"), @"No fixture resolved for parameter `self`");
    }

    #[test]
    fn uses_module_fixture_for_same_name_class_override() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

@pytest.fixture
def value(): ...

class TestExample:
    @pytest.fixture
    def value(self, value): ...
"#,
        );

        let class_fixture = test.function("TestExample.value");

        assert_snapshot!(class_fixture.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:9:21
          |
        9 |     def value(self, value): ...
          |                     ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:5:5
          |
        5 | def value(): ...
          |     -----
        ");
    }

    #[test]
    fn uses_lexical_context_for_fixture_dependencies() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

@pytest.fixture
def dependency(): ...

@pytest.fixture
def consumer(dependency): ...

class TestExample:
    @pytest.fixture
    def dependency(self): ...

    def test_use(self, consumer): ...
"#,
        );

        let consumer = test.function("consumer");

        assert_snapshot!(consumer.fixture_resolution("dependency"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:8:14
          |
        8 | def consumer(dependency): ...
          |              ^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:5:5
          |
        5 | def dependency(): ...
          |     ----------
        ");
    }

    #[test]
    fn resolves_fixtures_in_test_class_bases() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

class Base:
    @pytest.fixture
    def inherited(self): ...

class TestExample(Base):
    def test_use(self, inherited): ...

class TestShadowed(Base):
    inherited = None
    def test_use(self, inherited): ...
"#,
        );

        let test_use = test.function("TestExample.test_use");
        let shadowed = test.function("TestShadowed.test_use");

        assert_snapshot!(test_use.fixture_resolution("inherited"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:9:24
          |
        9 |     def test_use(self, inherited): ...
          |                        ^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:6:9
          |
        6 |     def inherited(self): ...
          |         ---------
        ");

        assert_snapshot!(shadowed.fixture_resolution("inherited"), @"No fixture resolved for parameter `inherited`");
    }

    #[test]
    fn follows_test_class_mro() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

class First:
    @pytest.fixture(name="resource")
    def first_provider(self): ...

class Second:
    @pytest.fixture(name="resource")
    def second_provider(self): ...

class TestExample(First, Second):
    def test_use(self, resource): ...
"#,
        );

        let test_use = test.function("TestExample.test_use");

        assert_snapshot!(test_use.fixture_resolution("resource"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:13:24
           |
        13 |     def test_use(self, resource): ...
           |                        ^^^^^^^^ fixture requested here
        info: Found 1 fixture
          --> src/test_example.py:10:9
           |
        10 |     def second_provider(self): ...
           |         ---------------
        ");
    }

    #[test]
    fn classifies_only_supported_fixture_requests() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

@pytest.fixture
def value(): ...

def helper(value): ...

def test_defaults(positional_only, /, value=None, *args, **kwargs): ...

class Example:
    def test_method(value): ...
"#,
        );

        let helper = test.function("helper");
        let test_defaults = test.function("test_defaults");
        let example_method = test.function("Example.test_method");

        assert_snapshot!(helper.fixture_resolution("value"), @"No fixture resolved for parameter `value`");
        assert_snapshot!(test_defaults.fixture_resolution("positional_only"), @"No fixture resolved for parameter `positional_only`");
        assert_snapshot!(test_defaults.fixture_resolution("value"), @"No fixture resolved for parameter `value`");
        assert_snapshot!(test_defaults.fixture_resolution("args"), @"No fixture resolved for parameter `args`");
        assert_snapshot!(test_defaults.fixture_resolution("kwargs"), @"No fixture resolved for parameter `kwargs`");
        assert_snapshot!(example_method.fixture_resolution("value"), @"No fixture resolved for parameter `value`");
    }

    #[test]
    fn excludes_mock_patch_and_unittest_parameters() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import unittest
from unittest import mock

import pytest

@pytest.fixture
def patched(): ...

@pytest.fixture
def value(): ...

@mock.patch("module.target")
def test_patched(patched, value): ...

class TestUnit(unittest.TestCase):
    def test_method(self, value): ...

@mock.patch.multiple("module", value=mock.DEFAULT)
def test_patch_multiple(value): ...
"#,
        );

        let patched = test.function("test_patched");
        let unittest_method = test.function("TestUnit.test_method");
        let patch_multiple = test.function("test_patch_multiple");

        assert_snapshot!(patched.fixture_resolution("patched"), @"No fixture resolved for parameter `patched`");

        assert_snapshot!(patched.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:14:27
           |
        14 | def test_patched(patched, value): ...
           |                           ^^^^^ fixture requested here
        info: Found 1 fixture
          --> src/test_example.py:11:5
           |
        11 | def value(): ...
           |     -----
        ");

        assert_snapshot!(unittest_method.fixture_resolution("value"), @"No fixture resolved for parameter `value`");

        assert_snapshot!(patch_multiple.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:20:25
           |
        20 | def test_patch_multiple(value): ...
           |                         ^^^^^ fixture requested here
        info: Found 1 fixture
          --> src/test_example.py:11:5
           |
        11 | def value(): ...
           |     -----
        ");
    }

    #[test]
    fn resolves_fixtures_for_nested_test_classes() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

@pytest.fixture
def value(): ...

class TestOuter:
    @pytest.fixture
    def outer(self): ...

    class TestInner:
        def test_method(self, value, outer): ...
"#,
        );

        let nested_method = test.function("TestOuter.TestInner.test_method");

        assert_snapshot!(nested_method.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:12:31
           |
        12 |         def test_method(self, value, outer): ...
           |                               ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:5:5
          |
        5 | def value(): ...
          |     -----
        ");

        assert_snapshot!(nested_method.fixture_resolution("outer"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:12:38
           |
        12 |         def test_method(self, value, outer): ...
           |                                      ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:9:9
          |
        9 |     def outer(self): ...
          |         -----
        ");
    }

    #[test]
    fn resolves_fixture_after_positional_only_method_receiver() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest

@pytest.fixture
def value(): ...

class TestExample:
    def test_method(self, /, value): ...
"#,
        );

        let test_method = test.function("TestExample.test_method");

        assert_snapshot!(test_method.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:8:30
          |
        8 |     def test_method(self, /, value): ...
          |                              ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:5:5
          |
        5 | def value(): ...
          |     -----
        ");
    }

    #[test]
    fn excludes_direct_parameters_and_keeps_indirect_parameters() {
        let test = PytestTestCase::new(
            "/src/test_example.py",
            r#"
import pytest
from pytest import mark as aliased_mark

@pytest.fixture
def value(): ...

@pytest.fixture
def other(): ...

@pytest.mark.parametrize("value", [1])
def test_direct(value): ...

@pytest.mark.parametrize("value", [1], True)
def test_indirect(value): ...

@pytest.mark.parametrize("value, other", [(1, 2)], indirect=["value"])
def test_mixed(value, other): ...

@aliased_mark.parametrize("value", [1])
def test_aliased_direct(value): ...

@aliased_mark.parametrize("value", [1], indirect=True)
def test_aliased_indirect(value): ...

@pytest.mark.parametrize("value", [1])
class TestParametrized:
    def test_value(self, value): ...
"#,
        );

        let test_direct = test.function("test_direct");
        let test_indirect = test.function("test_indirect");
        let test_mixed = test.function("test_mixed");
        let test_aliased_direct = test.function("test_aliased_direct");
        let test_aliased_indirect = test.function("test_aliased_indirect");
        let test_class_parametrized = test.function("TestParametrized.test_value");

        assert_snapshot!(test_direct.fixture_resolution("value"), @"No fixture resolved for parameter `value`");

        assert_snapshot!(test_indirect.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:15:19
           |
        15 | def test_indirect(value): ...
           |                   ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:6:5
          |
        6 | def value(): ...
          |     -----
        ");

        assert_snapshot!(test_mixed.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:18:16
           |
        18 | def test_mixed(value, other): ...
           |                ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:6:5
          |
        6 | def value(): ...
          |     -----
        ");

        assert_snapshot!(test_mixed.fixture_resolution("other"), @"No fixture resolved for parameter `other`");
        assert_snapshot!(test_aliased_direct.fixture_resolution("value"), @"No fixture resolved for parameter `value`");

        assert_snapshot!(test_aliased_indirect.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:24:27
           |
        24 | def test_aliased_indirect(value): ...
           |                           ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/test_example.py:6:5
          |
        6 | def value(): ...
          |     -----
        ");

        assert_snapshot!(test_class_parametrized.fixture_resolution("value"), @"No fixture resolved for parameter `value`");
    }

    #[test]
    fn requires_a_default_pytest_test_module_name() {
        let test = PytestTestCase::new(
            "/src/example.py",
            r#"
import pytest

@pytest.fixture
def value(): ...

def test_use(value): ...
"#,
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("value"), @"No fixture resolved for parameter `value`");
    }

    #[test]
    fn resolves_imported_fixture_exposures() {
        let test = PytestTestCase::with_files(
            "/src/test_example.py",
            &[
                (
                    "/src/fixtures.py",
                    r#"
import pytest

@pytest.fixture
def resource(): ...

@pytest.fixture(name="public_name")
def implementation(): ...
"#,
                ),
                (
                    "/src/reexports.py",
                    r#"
from fixtures import resource as middle
"#,
                ),
                (
                    "/src/star_fixtures.py",
                    r#"
import pytest

@pytest.fixture
def star_fixture(): ...
"#,
                ),
                (
                    "/src/test_example.py",
                    r#"
from fixtures import resource as direct_alias, implementation
from reexports import middle as chained
from star_fixtures import *

def test_use(
    direct_alias,
    chained,
    public_name,
    implementation,
    resource,
    star_fixture,
): ...
"#,
                ),
            ],
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("direct_alias"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:7:5
          |
        7 |     direct_alias,
          |     ^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/fixtures.py:5:5
          |
        5 | def resource(): ...
          |     --------
        ");

        assert_snapshot!(test_use.fixture_resolution("chained"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:8:5
          |
        8 |     chained,
          |     ^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/fixtures.py:5:5
          |
        5 | def resource(): ...
          |     --------
        ");

        assert_snapshot!(test_use.fixture_resolution("public_name"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:9:5
          |
        9 |     public_name,
          |     ^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/fixtures.py:8:5
          |
        8 | def implementation(): ...
          |     --------------
        ");

        assert_snapshot!(test_use.fixture_resolution("star_fixture"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/test_example.py:12:5
           |
        12 |     star_fixture,
           |     ^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/star_fixtures.py:5:5
          |
        5 | def star_fixture(): ...
          |     ------------
        ");

        assert_snapshot!(test_use.fixture_resolution("implementation"), @"No fixture resolved for parameter `implementation`");
        assert_snapshot!(test_use.fixture_resolution("resource"), @"No fixture resolved for parameter `resource`");
    }

    #[test]
    fn deduplicates_aliases_to_same_fixture_declaration() {
        let test = PytestTestCase::with_files(
            "/src/test_example.py",
            &[
                (
                    "/src/fixtures.py",
                    r#"
import pytest

@pytest.fixture(name="resource")
def implementation(): ...
"#,
                ),
                (
                    "/src/test_example.py",
                    r#"
from fixtures import implementation
from fixtures import implementation as second_exposure

def test_use(resource): ...
"#,
                ),
            ],
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("resource"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:5:14
          |
        5 | def test_use(resource): ...
          |              ^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/fixtures.py:5:5
          |
        5 | def implementation(): ...
          |     --------------
        ");
    }

    #[test]
    fn resolves_imported_fixture_declarations_from_source() {
        let test = PytestTestCase::with_files(
            "/src/test_example.py",
            &[
                (
                    "/src/fixtures.py",
                    r#"
import pytest

@pytest.fixture(name="public_name")
def implementation(): ...
"#,
                ),
                ("/src/fixtures.pyi", "def implementation() -> object: ...\n"),
                (
                    "/src/test_example.py",
                    r#"
from fixtures import implementation

def test_use(public_name): ...
"#,
                ),
            ],
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("public_name"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:4:14
          |
        4 | def test_use(public_name): ...
          |              ^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/fixtures.py:5:5
          |
        5 | def implementation(): ...
          |     --------------
        ");
    }

    #[test]
    fn ignores_overwritten_imported_fixture_exposures() {
        let test = PytestTestCase::with_files(
            "/src/test_example.py",
            &[
                (
                    "/src/fixtures.py",
                    r#"
import pytest

@pytest.fixture
def resource(): ...
"#,
                ),
                (
                    "/src/test_example.py",
                    r#"
from fixtures import resource

resource = object()

def test_use(resource): ...
"#,
                ),
            ],
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("resource"), @"No fixture resolved for parameter `resource`");
    }

    #[test]
    fn preserves_conditional_imported_fixture_definitions() {
        let test = PytestTestCase::with_files(
            "/src/test_example.py",
            &[
                (
                    "/src/first.py",
                    r#"
import pytest

@pytest.fixture
def first(): ...
"#,
                ),
                (
                    "/src/second.py",
                    r#"
import pytest

@pytest.fixture
def second(): ...
"#,
                ),
                (
                    "/src/test_example.py",
                    r#"
flag: bool

if flag:
    from first import first as resource
else:
    from second import second as resource

def test_use(resource): ...
"#,
                ),
            ],
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("resource"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/test_example.py:9:14
          |
        9 | def test_use(resource): ...
          |              ^^^^^^^^ fixture requested here
        info: Found 2 fixtures
         --> src/first.py:5:5
          |
        5 | def first(): ...
          |     -----
          |
         ::: src/second.py:5:5
          |
        5 | def second(): ...
          |     ------
        ");
    }

    #[test]
    fn resolves_nearest_to_outermost_conftest_providers() {
        let test_file = "/src/project/tests/test_example.py";
        let test = PytestTestCase::with_files(
            test_file,
            &[
                (
                    "/conftest.py",
                    r#"
import pytest

@pytest.fixture
def outside_root(): ...
"#,
                ),
                (
                    "/src/conftest.py",
                    r#"
import pytest

@pytest.fixture
def root_fixture(): ...

@pytest.fixture
def shadowed(): ...

@pytest.fixture
def module_shadowed(): ...
"#,
                ),
                (
                    "/src/shared_fixtures.py",
                    r#"
import pytest

@pytest.fixture
def imported_fixture(): ...
"#,
                ),
                (
                    "/src/project/conftest.py",
                    r#"
import pytest
from shared_fixtures import imported_fixture as conftest_alias

@pytest.fixture
def middle_fixture(): ...

@pytest.fixture
def shadowed(): ...
"#,
                ),
                (
                    "/src/project/tests/conftest.py",
                    r#"
import pytest

@pytest.fixture
def nearest_fixture(): ...

@pytest.fixture
def shadowed(): ...
"#,
                ),
                (
                    "/src/project/sibling/conftest.py",
                    r#"
import pytest

@pytest.fixture
def sibling_fixture(): ...
"#,
                ),
                (
                    "/src/project/tests/test_example.py",
                    r#"
import pytest

@pytest.fixture
def module_shadowed(): ...

def test_use(
    nearest_fixture,
    middle_fixture,
    root_fixture,
    shadowed,
    module_shadowed,
    conftest_alias,
    sibling_fixture,
    outside_root,
): ...
"#,
                ),
            ],
        );

        let test_use = test.function("test_use");

        assert_snapshot!(test_use.fixture_resolution("nearest_fixture"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/project/tests/test_example.py:8:5
          |
        8 |     nearest_fixture,
          |     ^^^^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/project/tests/conftest.py:5:5
          |
        5 | def nearest_fixture(): ...
          |     ---------------
        ");

        assert_snapshot!(test_use.fixture_resolution("middle_fixture"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/project/tests/test_example.py:9:5
          |
        9 |     middle_fixture,
          |     ^^^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/project/conftest.py:6:5
          |
        6 | def middle_fixture(): ...
          |     --------------
        ");

        assert_snapshot!(test_use.fixture_resolution("root_fixture"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/project/tests/test_example.py:10:5
           |
        10 |     root_fixture,
           |     ^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/conftest.py:5:5
          |
        5 | def root_fixture(): ...
          |     ------------
        ");

        assert_snapshot!(test_use.fixture_resolution("shadowed"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/project/tests/test_example.py:11:5
           |
        11 |     shadowed,
           |     ^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/project/tests/conftest.py:8:5
          |
        8 | def shadowed(): ...
          |     --------
        ");

        assert_snapshot!(test_use.fixture_resolution("module_shadowed"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/project/tests/test_example.py:12:5
           |
        12 |     module_shadowed,
           |     ^^^^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/project/tests/test_example.py:5:5
          |
        5 | def module_shadowed(): ...
          |     ---------------
        ");

        assert_snapshot!(test_use.fixture_resolution("conftest_alias"), @"
        info[pytest-fixture]: Resolve fixture for parameter
          --> src/project/tests/test_example.py:13:5
           |
        13 |     conftest_alias,
           |     ^^^^^^^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/shared_fixtures.py:5:5
          |
        5 | def imported_fixture(): ...
          |     ----------------
        ");

        assert_snapshot!(test_use.fixture_resolution("sibling_fixture"), @"No fixture resolved for parameter `sibling_fixture`");
        assert_snapshot!(test_use.fixture_resolution("outside_root"), @"No fixture resolved for parameter `outside_root`");
    }

    #[test]
    fn same_name_conftest_override_requests_outer_fixture() {
        let test = PytestTestCase::with_files(
            "/src/project/conftest.py",
            &[
                (
                    "/src/conftest.py",
                    r#"
import pytest

@pytest.fixture
def value(): ...
"#,
                ),
                (
                    "/src/project/conftest.py",
                    r#"
import pytest

@pytest.fixture
def value(value): ...
"#,
                ),
            ],
        );

        let fixture = test.function("value");

        assert_snapshot!(fixture.fixture_resolution("value"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/project/conftest.py:5:11
          |
        5 | def value(value): ...
          |           ^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/conftest.py:5:5
          |
        5 | def value(): ...
          |     -----
        ");
    }

    #[test]
    fn conftest_discovery_tracks_file_updates() {
        let mut test = PytestTestCase::new(
            "/src/project/test_example.py",
            r#"
def test_use(resource): ...
"#,
        );

        assert_snapshot!(test.function("test_use").fixture_resolution("resource"), @"No fixture resolved for parameter `resource`");

        test.write_file(
            "/src/project/conftest.py",
            r#"
import pytest

@pytest.fixture
def resource(): ...
"#,
        );
        assert_snapshot!(test.function("test_use").fixture_resolution("resource"), @"
        info[pytest-fixture]: Resolve fixture for parameter
         --> src/project/test_example.py:2:14
          |
        2 | def test_use(resource): ...
          |              ^^^^^^^^ fixture requested here
        info: Found 1 fixture
         --> src/project/conftest.py:5:5
          |
        5 | def resource(): ...
          |     --------
        ");

        test.write_file(
            "/src/project/conftest.py",
            r#"
import pytest

@pytest.fixture
def replacement(): ...
"#,
        );
        assert_snapshot!(test.function("test_use").fixture_resolution("resource"), @"No fixture resolved for parameter `resource`");
    }

    struct PytestTestCase {
        db: TestDb,
        path: &'static str,
    }

    impl PytestTestCase {
        fn new(path: &'static str, source: &'static str) -> Self {
            Self {
                db: pytest_db(path, source),
                path,
            }
        }

        fn with_files(path: &'static str, files: &[(&'static str, &'static str)]) -> Self {
            Self {
                db: pytest_db_with_files(files),
                path,
            }
        }

        fn write_file(&mut self, path: &'static str, source: &'static str) {
            self.db
                .write_file(path, source)
                .expect("valid pytest test file update");
        }

        fn function<'test>(&'test self, name: &str) -> PytestTestFunction<'test> {
            PytestTestFunction {
                test: self,
                name: name.to_owned(),
            }
        }
    }

    struct PytestTestFunction<'test> {
        test: &'test PytestTestCase,
        name: String,
    }

    impl PytestTestFunction<'_> {
        fn fixture_resolution(&self, parameter_name: &str) -> String {
            let db = &self.test.db;
            let parameter = self.parameter_definition(parameter_name);
            let fixtures = fixture_bindings_for_parameter(db, parameter);
            if fixtures.is_empty() {
                return format!("No fixture resolved for parameter `{parameter_name}`");
            }

            let parameter_module = parsed_module(db, parameter.python_file(db)).load(db);
            let mut diagnostic = Diagnostic::new(
                DiagnosticId::lint("pytest-fixture"),
                Severity::Info,
                "Resolve fixture for parameter",
            );
            diagnostic.annotate(
                Annotation::primary(parameter.focus_range(db, &parameter_module).into())
                    .message("fixture requested here"),
            );

            let mut resolved = SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                format_args!(
                    "Found {} fixture{}",
                    fixtures.len(),
                    if fixtures.len() == 1 { "" } else { "s" }
                ),
            );
            for binding in fixtures {
                let fixture = binding.fixture();
                let module = parsed_module(db, fixture.python_file(db)).load(db);
                resolved.annotate(Annotation::secondary(
                    fixture.focus_range(db, &module).into(),
                ));
            }
            diagnostic.sub(resolved);

            DisplayDiagnostics::new(
                db,
                &DisplayDiagnosticConfig::new("ty").context(0),
                &[diagnostic],
            )
            .to_string()
            .replace('\\', "/")
        }

        fn parameter_definition<'db>(&'db self, parameter_name: &str) -> Definition<'db> {
            let db = &self.test.db;
            let file = system_path_to_file(db, self.test.path).expect("test file exists");
            let file = db.program_file(file);
            let module = parsed_module(db, file.python_file(db)).load(db);
            let function = find_function(module.suite(), &self.name).expect("test function exists");
            let index = semantic_index(db, file);
            let parameter = function
                .parameters
                .iter()
                .find(|candidate| candidate.name().as_str() == parameter_name)
                .expect("test parameter exists");
            match parameter {
                ast::AnyParameterRef::Variadic(parameter) => {
                    index.expect_single_definition(parameter)
                }
                ast::AnyParameterRef::NonVariadic(parameter) => {
                    index.expect_single_definition(parameter)
                }
            }
        }
    }

    fn find_function<'ast>(
        statements: &'ast [ast::Stmt],
        selector: &str,
    ) -> Option<&'ast ast::StmtFunctionDef> {
        if let Some((class_name, nested)) = selector.split_once('.') {
            return statements.iter().find_map(|statement| {
                let class = statement.as_class_def_stmt()?;
                (class.name.as_str() == class_name)
                    .then(|| find_function(&class.body, nested))
                    .flatten()
            });
        }

        statements.iter().find_map(|statement| {
            statement
                .as_function_def_stmt()
                .filter(|function| function.name.as_str() == selector)
        })
    }

    fn pytest_db(path: &'static str, source: &'static str) -> TestDb {
        pytest_db_with_files(&[(path, source)])
    }

    fn pytest_db_with_files(files: &[(&'static str, &'static str)]) -> TestDb {
        let mut builder = TestDbBuilder::new()
            .with_third_party_packages()
            .with_file(
                "/.venv/lib/python3.13/site-packages/_pytest/__init__.pyi",
                "",
            )
            .with_file(
                "/.venv/lib/python3.13/site-packages/_pytest/mark/__init__.pyi",
                "",
            )
            .with_file(
                "/.venv/lib/python3.13/site-packages/_pytest/mark/structures.pyi",
                r#"
class MarkDecorator:
    def __call__(self, *args: object, **kwargs: object) -> object: ...

class MarkGenerator:
    parametrize: MarkDecorator
"#,
            )
            .with_file(
                "/.venv/lib/python3.13/site-packages/_pytest/fixtures.pyi",
                r#"
from typing import Any, Callable

def fixture(
    function: Callable[..., Any] | None = ...,
    *,
    name: str | None = ...,
) -> Any: ...

def yield_fixture(
    function: Callable[..., Any] | None = ...,
    *,
    name: str | None = ...,
) -> Any: ...
"#,
            )
            .with_file(
                "/.venv/lib/python3.13/site-packages/pytest/__init__.pyi",
                r#"
from _pytest.fixtures import fixture as fixture, yield_fixture as yield_fixture
from _pytest.mark.structures import MarkGenerator

mark: MarkGenerator
"#,
            );
        for (path, source) in files {
            builder = builder.with_file(*path, source);
        }
        builder.build().expect("valid pytest test database")
    }
}
