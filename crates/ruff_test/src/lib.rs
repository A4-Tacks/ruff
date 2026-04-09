use crate::config::Log;
use crate::db::Db;
use crate::parser::{BacktickOffsets, EmbeddedFileSourceMap};
use anyhow::anyhow;
use camino::Utf8Path;
use colored::Colorize;
use config::SystemKind;
use itertools::Itertools;
use parser as test_parser;
use ruff_db::Db as _;
use ruff_db::diagnostic::{
    Diagnostic, DiagnosticFormat, DiagnosticId, DisplayDiagnosticConfig, DisplayDiagnostics, Span,
};
use ruff_db::files::{File, FileRootKind, system_path_to_file};
use ruff_db::panic::{PanicError, catch_unwind};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_db::system::{DbWithWritableSystem as _, SystemPath, SystemPathBuf};
use ruff_db::testing::{setup_logging, setup_logging_with_filter};
use ruff_diagnostics::Applicability;
use ruff_linter::codes::Rule;
use ruff_linter::fix::{FixResult, fix_file};
use ruff_linter::linter::check_path;
use ruff_linter::message::EmitterContext;
use ruff_linter::package::PackageRoot;
use ruff_linter::packaging::detect_package_root;
use ruff_linter::settings::types::UnsafeFixes;
use ruff_linter::settings::{LinterSettings, flags};
use ruff_linter::source_kind::SourceKind;
use ruff_linter::suppression::Suppressions;
use ruff_linter::{FixAvailability, Locator, directives};
use ruff_python_ast::PySourceType;
use ruff_python_codegen::Stylist;
use ruff_python_index::Indexer;
use ruff_python_parser::{ParseError, ParseOptions};
use ruff_source_file::{LineIndex, OneIndexed, SourceFileBuilder};
use rustc_hash::FxHashMap;
use std::backtrace::BacktraceStatus;
use std::borrow::Cow;
use std::fmt::{Display, Write};
use std::path::Path;
use ty_module_resolver::{
    Module, SearchPath, SearchPathSettings, list_modules, resolve_module_confident,
};
use ty_python_semantic::pull_types::pull_types;
use ty_python_semantic::types::UNDEFINED_REVEAL;
use ty_python_semantic::{
    FallibleStrategy, Program, ProgramSettings, PythonEnvironment, PythonPlatform,
    PythonVersionSource, PythonVersionWithSource, SysPrefixPathOrigin,
};

mod assertion;
mod config;
mod db;
mod diagnostic;
mod external_dependencies;
mod matcher;
mod parser;

use ty_static::EnvVars;

/// Run `path` as a markdown test suite with given `title`.
///
/// Panic on test failure, and print failure details.
pub fn run(
    absolute_fixture_path: &Utf8Path,
    relative_fixture_path: &Utf8Path,
    source: &str,
    snapshot_path: &Utf8Path,
    short_title: &str,
    test_name: &str,
    output_format: OutputFormat,
) -> anyhow::Result<()> {
    let suite = test_parser::parse(short_title, source)
        .map_err(|err| anyhow!("Failed to parse fixture: {err}"))?;

    let mut db = db::Db::setup();

    let filter = std::env::var(EnvVars::MDTEST_TEST_FILTER).ok();
    let mut any_failures = false;
    let mut assertion = String::new();
    for test in suite.tests() {
        if filter
            .as_ref()
            .is_some_and(|f| !(test.uncontracted_name().contains(f) || test.name() == *f))
        {
            continue;
        }

        let _tracing = test.configuration().log.as_ref().and_then(|log| match log {
            Log::Bool(enabled) => enabled.then(setup_logging),
            Log::Filter(filter) => setup_logging_with_filter(filter),
        });

        let result = run_test(
            &mut db,
            absolute_fixture_path,
            relative_fixture_path,
            snapshot_path,
            &test,
        );
        let inconsistencies = if result.as_ref().is_ok_and(|t| t.has_been_skipped()) {
            Ok(())
        } else {
            run_module_resolution_consistency_test(&db)
        };

        let this_test_failed = result.is_err() || inconsistencies.is_err();
        any_failures = any_failures || this_test_failed;

        if this_test_failed && output_format.is_cli() {
            let _ = writeln!(assertion, "\n\n{}\n", test.name().bold().underline());
        }

        if let Err(failures) = result {
            let md_index = LineIndex::from_source_text(source);

            for test_failures in failures {
                let source_map =
                    EmbeddedFileSourceMap::new(&md_index, test_failures.backtick_offsets);

                for (relative_line_number, failures) in test_failures.by_line.iter() {
                    let file = relative_fixture_path.as_str();

                    let absolute_line_number =
                        match source_map.to_absolute_line_number(relative_line_number) {
                            Ok(line_number) => line_number,
                            Err(last_line_number) => {
                                output_format.write_error(
                                    &mut assertion,
                                    file,
                                    last_line_number,
                                    "Found a trailing assertion comment \
                                        (e.g., `# revealed:` or `# error:`) \
                                        not followed by any statement.",
                                );

                                continue;
                            }
                        };

                    for failure in failures {
                        output_format.write_error(
                            &mut assertion,
                            file,
                            absolute_line_number,
                            failure,
                        );
                    }
                }
            }
        }
        if let Err(inconsistencies) = inconsistencies {
            any_failures = true;
            for inconsistency in inconsistencies {
                output_format.write_inconsistency(
                    &mut assertion,
                    relative_fixture_path,
                    &inconsistency,
                );
            }
        }

        if this_test_failed && output_format.is_cli() {
            let escaped_test_name = test.name().replace('\'', "\\'");
            let _ = writeln!(
                assertion,
                "\nTo rerun this specific test, \
                set the environment variable: {}='{escaped_test_name}'",
                EnvVars::MDTEST_TEST_FILTER,
            );
            let _ = writeln!(
                assertion,
                "{}='{escaped_test_name}' cargo test -p ty_python_semantic \
                --test mdtest -- {test_name}",
                EnvVars::MDTEST_TEST_FILTER,
            );

            let _ = writeln!(assertion, "\n{}", "-".repeat(50));
        }
    }

    assert!(!any_failures, "{}", &assertion);

    Ok(())
}

/// Defines the format in which mdtest should print an error to the terminal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// The format `cargo test` should use by default.
    Cli,
    /// A format that will provide annotations from GitHub Actions
    /// if mdtest fails on a PR.
    /// See <https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/workflow-commands-for-github-actions#setting-an-error-message>
    GitHub,
}

impl OutputFormat {
    const fn is_cli(self) -> bool {
        matches!(self, OutputFormat::Cli)
    }

    /// Write a test error in the appropriate format.
    ///
    /// For CLI format, errors are appended to `assertion_buf` so they appear
    /// in the assertion-failure message.
    ///
    /// For GitHub format, errors are printed directly to stdout so that GitHub
    /// Actions can detect them as workflow commands. Workflow commands must
    /// appear at the beginning of a line in stdout to be parsed by GitHub.
    #[expect(clippy::print_stdout)]
    fn write_error(
        self,
        assertion_buf: &mut String,
        file: &str,
        line: OneIndexed,
        failure: impl Display,
    ) {
        match self {
            OutputFormat::Cli => {
                let _ = writeln!(
                    assertion_buf,
                    "  {file_line} {failure}",
                    file_line = format!("{file}:{line}").cyan()
                );
            }
            OutputFormat::GitHub => {
                println!("::error file={file},line={line}::{failure}");
            }
        }
    }

    /// Write a module-resolution inconsistency in the appropriate format.
    ///
    /// See [`write_error`](Self::write_error) for details on why GitHub-format
    /// messages must be printed directly to stdout.
    #[expect(clippy::print_stdout)]
    fn write_inconsistency(
        self,
        assertion_buf: &mut String,
        fixture_path: &Utf8Path,
        inconsistency: &impl Display,
    ) {
        match self {
            OutputFormat::Cli => {
                let info = fixture_path.to_string().cyan();
                let _ = writeln!(assertion_buf, "  {info} {inconsistency}");
            }
            OutputFormat::GitHub => {
                println!("::error file={fixture_path}::{inconsistency}");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestOutcome {
    Success,
    Skipped,
}

impl TestOutcome {
    const fn has_been_skipped(self) -> bool {
        matches!(self, TestOutcome::Skipped)
    }
}

fn run_test(
    db: &mut db::Db,
    absolute_fixture_path: &Utf8Path,
    relative_fixture_path: &Utf8Path,
    snapshot_path: &Utf8Path,
    test: &parser::MarkdownTest,
) -> Result<TestOutcome, Failures> {
    // Initialize the system and remove all files and directories to reset the system to a clean state.
    match test.configuration().system.unwrap_or_default() {
        SystemKind::InMemory => {
            db.use_in_memory_system();
        }
        SystemKind::Os => {
            let dir = tempfile::TempDir::new().expect("Creating a temporary directory to succeed");
            let root_path = dir
                .path()
                .canonicalize()
                .expect("Canonicalizing to succeed");
            let root_path = SystemPathBuf::from_path_buf(root_path)
                .expect("Temp directory to be a valid UTF8 path")
                .simplified()
                .to_path_buf();

            db.use_os_system_with_temp_dir(root_path, dir);
        }
    }

    let project_root = SystemPathBuf::from("/src");
    db.create_directory_all(&project_root)
        .expect("Creating the project root to succeed");
    db.files()
        .try_add_root(db, &project_root, FileRootKind::Project);

    let src_path = project_root.clone();
    let custom_typeshed_path = test.configuration().typeshed();
    let python_version = test.configuration().python_version().unwrap_or_default();

    // Setup virtual environment with dependencies if specified
    let venv_for_external_dependencies = SystemPathBuf::from("/.venv");
    if let Some(dependencies) = test.configuration().dependencies() {
        if !std::env::var("MDTEST_EXTERNAL").is_ok_and(|v| v == "1") {
            return Ok(TestOutcome::Skipped);
        }

        let python_platform = test.configuration().python_platform().expect(
            "Tests with external dependencies must specify `python-platform` in the configuration",
        );

        let lockfile_path = absolute_fixture_path.with_extension("lock");

        external_dependencies::setup_venv(
            db,
            dependencies,
            python_version,
            &python_platform,
            &venv_for_external_dependencies,
            &lockfile_path,
        )
        .expect("Failed to setup in-memory virtual environment with dependencies");
    }

    let mut typeshed_files = vec![];
    let mut has_custom_versions_file = false;

    let test_files: Vec<_> = test
        .files()
        .filter_map(|embedded| {
            if embedded.lang == "ignore" {
                return None;
            }

            assert!(
                matches!(
                    embedded.lang,
                    "py" | "pyi" | "python" | "text" | "cfg" | "pth"
                ),
                "Supported file types are: py (or python), pyi, text, cfg and ignore"
            );

            let mut full_path = embedded.full_path(&project_root);

            if let Some(relative_path_to_custom_typeshed) = custom_typeshed_path
                .and_then(|typeshed| full_path.strip_prefix(typeshed.join("stdlib")).ok())
            {
                if relative_path_to_custom_typeshed.as_str() == "VERSIONS" {
                    has_custom_versions_file = true;
                } else if relative_path_to_custom_typeshed
                    .extension()
                    .is_some_and(|ext| ext == "pyi")
                {
                    typeshed_files.push(relative_path_to_custom_typeshed.to_path_buf());
                }
            } else if let Some(component_index) = full_path
                .components()
                .position(|c| c.as_str() == "<path-to-site-packages>")
            {
                // If the path contains `<path-to-site-packages>`, we need to replace it with the
                // actual site-packages directory based on the Python platform and version.
                let mut components = full_path.components();
                let mut new_path: SystemPathBuf =
                    components.by_ref().take(component_index).collect();
                if cfg!(target_os = "windows") {
                    new_path.extend(["Lib", "site-packages"]);
                } else {
                    new_path.push("lib");
                    new_path.push(format!("python{python_version}"));
                    new_path.push("site-packages");
                }
                new_path.extend(components.skip(1));
                full_path = new_path;
            }

            let temp_string;
            let to_write = if embedded.lang == "pth" && !embedded.code.starts_with('/') {
                // Make any relative .pths be relative to src_path
                temp_string = format!("{src_path}/{}", embedded.code);
                &*temp_string
            } else {
                &*embedded.code
            };

            db.write_file(&full_path, to_write).unwrap();

            if !(full_path.starts_with(&src_path)
                && matches!(embedded.lang, "py" | "python" | "pyi"))
            {
                // These files need to be written to the file system (above), but we don't run any checks on them.
                return None;
            }

            let file = system_path_to_file(db, full_path).unwrap();

            Some(TestFile {
                file,
                backtick_offsets: embedded.backtick_offsets.clone(),
            })
        })
        .collect();

    // Create a custom typeshed `VERSIONS` file if none was provided.
    if let Some(typeshed_path) = custom_typeshed_path {
        db.files()
            .try_add_root(db, typeshed_path, FileRootKind::LibrarySearchPath);
        if !has_custom_versions_file {
            let versions_file = typeshed_path.join("stdlib/VERSIONS");
            let contents = typeshed_files
                .iter()
                .fold(String::new(), |mut content, path| {
                    // This is intentionally kept simple:
                    let module_name = path
                        .as_str()
                        .trim_end_matches(".pyi")
                        .trim_end_matches("/__init__")
                        .replace('/', ".");
                    let _ = writeln!(content, "{module_name}: 3.8-");
                    content
                });
            db.write_file(&versions_file, contents).unwrap();
        }
    }

    let configuration = test.configuration();

    let site_packages_paths = if configuration.dependencies().is_some() {
        // If dependencies were specified, use the venv we just set up
        let environment = PythonEnvironment::new(
            &venv_for_external_dependencies,
            SysPrefixPathOrigin::PythonCliFlag,
            db.system(),
        )
        .expect("Python environment to point to a valid path");
        environment
            .site_packages_paths(db.system())
            .expect("Python environment to be valid")
            .into_vec()
    } else if let Some(python) = configuration.python() {
        let environment =
            PythonEnvironment::new(python, SysPrefixPathOrigin::PythonCliFlag, db.system())
                .expect("Python environment to point to a valid path");
        environment
            .site_packages_paths(db.system())
            .expect("Python environment to be valid")
            .into_vec()
    } else {
        vec![]
    };

    // Make any relative extra-paths be relative to src_path
    let extra_paths = configuration
        .extra_paths()
        .unwrap_or_default()
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                src_path.join(path)
            }
        })
        .collect();

    let settings = ProgramSettings {
        python_version: PythonVersionWithSource {
            version: python_version,
            source: PythonVersionSource::Cli,
        },
        python_platform: configuration
            .python_platform()
            .unwrap_or(PythonPlatform::Identifier("linux".to_string())),
        search_paths: SearchPathSettings {
            src_roots: vec![src_path],
            extra_paths,
            custom_typeshed: custom_typeshed_path.map(SystemPath::to_path_buf),
            site_packages_paths,
            real_stdlib_path: None,
        }
        .to_search_paths(db.system(), db.vendored(), &FallibleStrategy)
        .expect("Failed to resolve search path settings"),
    };

    Program::init_or_update(db, settings);
    db.update_analysis_options(configuration.analysis.as_ref());
    db.set_verbosity(test.configuration().verbose());

    // When snapshot testing is enabled, this is populated with
    // all diagnostics. Otherwise it remains empty.
    let mut snapshot_diagnostics = vec![];

    let mut any_pull_types_failures = false;

    let mut failures: Failures = test_files
        .iter()
        .filter_map(|test_file| {
            let parsed = parsed_module(db, test_file.file).load(db);

            let mut diagnostics: Vec<Diagnostic> = parsed
                .errors()
                .iter()
                .map(|error| Diagnostic::invalid_syntax(test_file.file, &error.error, error))
                .collect();

            diagnostics.extend(
                parsed
                    .unsupported_syntax_errors()
                    .iter()
                    .map(|error| Diagnostic::invalid_syntax(test_file.file, error, error)),
            );

            let source_kind = SourceKind::Python {
                code: source_text(db, test_file.file).as_str().to_string(),
                is_stub: test_file.file.is_stub(db),
            };
            let path = test_file
                .file
                .path(db)
                .as_system_path()
                .expect("mdtest files are on the system")
                .as_std_path();
            // TODO don't hard-code the rule
            let settings = LinterSettings::for_rule(Rule::NonPEP695GenericClass);
            let type_diagnostics = test_contents(&source_kind, path, &settings);

            diagnostics.extend(type_diagnostics);
            diagnostics.sort_by(|left, right| {
                left.rendering_sort_key(db)
                    .cmp(&right.rendering_sort_key(db))
            });

            let failure = match matcher::match_file(db, test_file.file, &diagnostics) {
                Ok(()) => None,
                Err(line_failures) => Some(FileFailures {
                    backtick_offsets: test_file.backtick_offsets.clone(),
                    by_line: line_failures,
                }),
            };

            // Filter out `revealed-type` and `undefined-reveal` diagnostics from snapshots,
            // since they make snapshots very noisy!
            if test.should_snapshot_diagnostics() {
                snapshot_diagnostics.extend(diagnostics.into_iter().filter(|diagnostic| {
                    diagnostic.id() != DiagnosticId::RevealedType
                        && !diagnostic.id().is_lint_named(&UNDEFINED_REVEAL.name())
                }));
            }

            let pull_types_result = attempt_test(db, pull_types, test_file);
            match pull_types_result {
                Ok(()) => {}
                Err(failures) => {
                    any_pull_types_failures = true;
                    if !test.should_skip_pulling_types() {
                        return Some(failures.into_file_failures(
                            db,
                            "\"pull types\"",
                            Some(
                                "Note: either fix the panic or add the `<!-- pull-types:skip -->` \
                    directive to this test",
                            ),
                        ));
                    }
                }
            }

            failure
        })
        .collect();

    if test.should_skip_pulling_types() && !any_pull_types_failures {
        let mut by_line = matcher::FailuresByLine::default();
        by_line.push(
            OneIndexed::from_zero_indexed(0),
            vec![
                "Remove the `<!-- pull-types:skip -->` directive from this test: pulling types \
                 succeeded for all files in the test."
                    .to_string(),
            ],
        );
        let failure = FileFailures {
            backtick_offsets: test_files[0].backtick_offsets.clone(),
            by_line,
        };
        failures.push(failure);
    }

    if snapshot_diagnostics.is_empty() && test.should_snapshot_diagnostics() {
        panic!(
            "Test `{}` requested snapshotting diagnostics but it didn't produce any.",
            test.name()
        );
    } else if !snapshot_diagnostics.is_empty() {
        let snapshot =
            create_diagnostic_snapshot(db, relative_fixture_path, test, snapshot_diagnostics);
        let name = test.name().replace(' ', "_").replace(':', "__");
        insta::with_settings!(
            {
                snapshot_path => snapshot_path,
                input_file => name.clone(),
                filters => vec![(r"\\", "/")],
                prepend_module_to_snapshot => false,
            },
            { insta::assert_snapshot!(name, snapshot) }
        );
    }

    if failures.is_empty() {
        Ok(TestOutcome::Success)
    } else {
        Err(failures)
    }
}

/// Reports an inconsistency between "list modules" and "resolve module."
///
/// Values of this type are only constructed when `from_list` and
/// `from_resolve` are not equivalent.
struct ModuleInconsistency<'db> {
    db: &'db db::Db,
    /// The module returned from `list_module`.
    from_list: Module<'db>,
    /// The module returned, if any, from `resolve_module`.
    from_resolve: Option<Module<'db>>,
}

/// Tests that "list modules" is consistent with "resolve module."
///
/// This only checks that everything returned by `list_module` is the
/// identical module we get back from `resolve_module`. It does not
/// check that all possible outputs of `resolve_module` are captured by
/// `list_module`.
fn run_module_resolution_consistency_test(db: &db::Db) -> Result<(), Vec<ModuleInconsistency<'_>>> {
    let mut errs = vec![];
    for from_list in list_modules(db) {
        // TODO: For now list_modules does not partake in desperate module resolution so
        // only compare against confident module resolution.
        errs.push(match resolve_module_confident(db, from_list.name(db)) {
            None => ModuleInconsistency {
                db,
                from_list,
                from_resolve: None,
            },
            Some(from_resolve) if from_list != from_resolve => ModuleInconsistency {
                db,
                from_list,
                from_resolve: Some(from_resolve),
            },
            _ => continue,
        });
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

impl std::fmt::Display for ModuleInconsistency<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        fn fmt_module(
            db: &db::Db,
            f: &mut std::fmt::Formatter,
            module: &Module<'_>,
        ) -> std::fmt::Result {
            let name = module.name(db);
            let path = module
                .file(db)
                .map(|file| file.path(db).to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let search_path = module
                .search_path(db)
                .map(SearchPath::to_string)
                .unwrap_or_else(|| "N/A".to_string());
            let known = module
                .known(db)
                .map(|known| known.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            write!(
                f,
                "Module(\
                   name={name}, \
                   file={path}, \
                   kind={kind:?}, \
                   search_path={search_path}, \
                   known={known}\
                 )",
                kind = module.kind(db),
            )
        }
        write!(f, "Found ")?;
        fmt_module(self.db, f, &self.from_list)?;
        match self.from_resolve {
            None => write!(
                f,
                " when listing modules, but `resolve_module` returned `None`",
            )?,
            Some(ref got) => {
                write!(f, " when listing modules, but `resolve_module` returned ",)?;
                fmt_module(self.db, f, got)?;
            }
        }
        Ok(())
    }
}

type Failures = Vec<FileFailures>;

/// The failures for a single file in a test by line number.
struct FileFailures {
    /// Positional information about the code block(s) to reconstruct absolute line numbers.
    backtick_offsets: Vec<BacktickOffsets>,

    /// The failures by lines in the file.
    by_line: matcher::FailuresByLine,
}

/// File in a test.
struct TestFile {
    file: File,

    /// Positional information about the code block(s) to reconstruct absolute line numbers.
    backtick_offsets: Vec<BacktickOffsets>,
}

fn create_diagnostic_snapshot(
    db: &mut db::Db,
    relative_fixture_path: &Utf8Path,
    test: &parser::MarkdownTest,
    diagnostics: impl IntoIterator<Item = Diagnostic>,
) -> String {
    let display_config = DisplayDiagnosticConfig::new("ty")
        .color(false)
        .show_fix_diff(true)
        .with_fix_applicability(Applicability::DisplayOnly);

    let mut snapshot = String::new();
    writeln!(snapshot).unwrap();
    writeln!(snapshot, "---").unwrap();
    writeln!(snapshot, "mdtest name: {}", test.uncontracted_name()).unwrap();
    writeln!(snapshot, "mdtest path: {relative_fixture_path}").unwrap();
    writeln!(snapshot, "---").unwrap();
    writeln!(snapshot).unwrap();

    writeln!(snapshot, "# Python source files").unwrap();
    writeln!(snapshot).unwrap();
    for file in test.files() {
        writeln!(snapshot, "## {}", file.relative_path()).unwrap();
        writeln!(snapshot).unwrap();
        // Note that we don't use ```py here because the line numbering
        // we add makes it invalid Python. This sacrifices syntax
        // highlighting when you look at the snapshot on GitHub,
        // but the line numbers are extremely useful for analyzing
        // snapshots. So we keep them.
        writeln!(snapshot, "```").unwrap();

        let line_number_width = file.code.lines().count().to_string().len();
        for (i, line) in file.code.lines().enumerate() {
            let line_number = i + 1;
            writeln!(snapshot, "{line_number:>line_number_width$} | {line}").unwrap();
        }
        writeln!(snapshot, "```").unwrap();
        writeln!(snapshot).unwrap();
    }

    writeln!(snapshot, "# Diagnostics").unwrap();
    writeln!(snapshot).unwrap();
    for (i, diag) in diagnostics.into_iter().enumerate() {
        if i > 0 {
            writeln!(snapshot).unwrap();
        }
        writeln!(snapshot, "```").unwrap();
        write!(snapshot, "{}", diag.display(db, &display_config)).unwrap();
        writeln!(snapshot, "```").unwrap();
    }
    snapshot
}

const MAX_ITERATIONS: usize = 10;

/// A convenient wrapper around [`check_path`], that additionally
/// asserts that fixes converge after a fixed number of iterations.
fn test_contents<'a>(
    source_kind: &'a SourceKind,
    path: &Path,
    settings: &LinterSettings,
) -> Vec<Diagnostic> {
    let source_type = PySourceType::from(path);
    let target_version = settings.resolve_target_version(path);
    let options =
        ParseOptions::from(source_type).with_target_version(target_version.parser_version());
    let parsed = ruff_python_parser::parse_unchecked(source_kind.source_code(), options.clone())
        .try_into_module()
        .expect("PySourceType always parses into a module");
    let locator = Locator::new(source_kind.source_code());
    let stylist = Stylist::from_tokens(parsed.tokens(), locator.contents());
    let indexer = Indexer::from_tokens(parsed.tokens(), locator.contents());
    let directives = directives::extract_directives(
        parsed.tokens(),
        directives::Flags::from_settings(settings),
        &locator,
        &indexer,
    );
    let suppressions = Suppressions::from_tokens(locator.contents(), parsed.tokens(), &indexer);
    let messages = check_path(
        path,
        path.parent()
            .and_then(|parent| detect_package_root(parent, &settings.namespace_packages))
            .map(|path| PackageRoot::Root { path }),
        &locator,
        &stylist,
        &indexer,
        &directives,
        settings,
        flags::Noqa::Enabled,
        source_kind,
        source_type,
        &parsed,
        target_version,
        &suppressions,
    );

    let source_has_errors = parsed.has_invalid_syntax();

    // Detect fixes that don't converge after multiple iterations.
    let mut iterations = 0;

    let mut transformed = Cow::Borrowed(source_kind);

    if messages.iter().any(|message| message.fix().is_some()) {
        let mut messages = messages.clone();

        while let Some(FixResult {
            code: fixed_contents,
            source_map,
            ..
        }) = fix_file(
            &messages,
            &Locator::new(transformed.source_code()),
            UnsafeFixes::Enabled,
        ) {
            if iterations < MAX_ITERATIONS {
                iterations += 1;
            } else {
                let output = print_diagnostics(messages);

                panic!(
                    "Failed to converge after {} iterations. This likely \
                     indicates a bug in the implementation of the fix. Last diagnostics:\n{}",
                    MAX_ITERATIONS, output
                );
            }

            transformed = Cow::Owned(transformed.updated(fixed_contents, &source_map));

            let parsed =
                ruff_python_parser::parse_unchecked(transformed.source_code(), options.clone())
                    .try_into_module()
                    .expect("PySourceType always parses into a module");
            let locator = Locator::new(transformed.source_code());
            let stylist = Stylist::from_tokens(parsed.tokens(), locator.contents());
            let indexer = Indexer::from_tokens(parsed.tokens(), locator.contents());
            let directives = directives::extract_directives(
                parsed.tokens(),
                directives::Flags::from_settings(settings),
                &locator,
                &indexer,
            );

            let suppressions =
                Suppressions::from_tokens(locator.contents(), parsed.tokens(), &indexer);
            let fixed_messages = check_path(
                path,
                None,
                &locator,
                &stylist,
                &indexer,
                &directives,
                settings,
                flags::Noqa::Enabled,
                &transformed,
                source_type,
                &parsed,
                target_version,
                &suppressions,
            );

            if parsed.has_invalid_syntax() && !source_has_errors {
                // Previous fix introduced a syntax error, abort
                let fixes = print_diagnostics(messages);
                let syntax_errors = print_syntax_errors(parsed.errors(), path, &transformed);

                panic!(
                    "Fixed source has a syntax error where the source document does not. This is a bug in one of the generated fixes:
{syntax_errors}
Last generated fixes:
{fixes}
Source with applied fixes:
{}",
                    transformed.source_code()
                );
            }

            messages = fixed_messages;
        }
    }

    let source_code = SourceFileBuilder::new(
        path.file_name().unwrap().to_string_lossy().as_ref(),
        source_kind.source_code(),
    )
    .finish();

    messages
        .into_iter()
        .filter_map(|msg| Some((msg.secondary_code()?.to_string(), msg)))
        .map(|(code, mut diagnostic)| {
            let rule = Rule::from_code(&code).unwrap();
            let fixable = diagnostic.fix().is_some_and(|fix| {
                matches!(
                    fix.applicability(),
                    Applicability::Safe | Applicability::Unsafe
                )
            });

            match (fixable, rule.fixable()) {
                (true, FixAvailability::Sometimes | FixAvailability::Always)
                | (false, FixAvailability::None | FixAvailability::Sometimes) => {
                    // Ok
                }
                (true, FixAvailability::None) => {
                    panic!(
                        "Rule {rule:?} is marked as non-fixable but it created a fix.
Change the `Violation::FIX_AVAILABILITY` to either \
`FixAvailability::Sometimes` or `FixAvailability::Always`"
                    );
                }
                (false, FixAvailability::Always) if source_has_errors => {
                    // Ok
                }
                (false, FixAvailability::Always) => {
                    panic!(
                        "\
Rule {rule:?} is marked to always-fixable but the diagnostic has no fix.
Either ensure you always emit a fix or change `Violation::FIX_AVAILABILITY` to either \
`FixAvailability::Sometimes` or `FixAvailability::None`"
                    )
                }
            }

            assert!(
                !(fixable && diagnostic.first_help_text().is_none()),
                "Diagnostic emitted by {rule:?} is fixable but \
                `Violation::fix_title` returns `None`"
            );

            // Not strictly necessary but adds some coverage for this code path by overriding the
            // noqa offset and the source file
            if let Some(range) = diagnostic.range() {
                diagnostic.set_noqa_offset(directives.noqa_line_for.resolve(range.start()));
            }
            // This part actually is necessary to avoid long relative paths in snapshots.
            for annotation in diagnostic.annotations_mut() {
                if let Some(range) = annotation.get_span().range() {
                    annotation.set_span(Span::from(source_code.clone()).with_range(range));
                }
            }
            for sub in diagnostic.sub_diagnostics_mut() {
                for annotation in sub.annotations_mut() {
                    if let Some(range) = annotation.get_span().range() {
                        annotation.set_span(Span::from(source_code.clone()).with_range(range));
                    }
                }
            }

            diagnostic
        })
        .chain(parsed.errors().iter().map(|parse_error| {
            Diagnostic::invalid_syntax(source_code.clone(), &parse_error.error, parse_error)
        }))
        .sorted_by(Diagnostic::ruff_start_ordering)
        .collect()
}

fn print_syntax_errors(errors: &[ParseError], path: &Path, source: &SourceKind) -> String {
    let filename = path.file_name().unwrap().to_string_lossy();
    let source_file = SourceFileBuilder::new(filename.as_ref(), source.source_code()).finish();

    let messages: Vec<_> = errors
        .iter()
        .map(|parse_error| {
            Diagnostic::invalid_syntax(source_file.clone(), &parse_error.error, parse_error)
        })
        .collect();

    print_messages(&messages)
}

/// Print the lint diagnostics in `diagnostics`.
fn print_diagnostics(mut diagnostics: Vec<Diagnostic>) -> String {
    diagnostics.retain(|msg| !msg.is_invalid_syntax());
    print_messages(&diagnostics)
}

pub(crate) fn print_messages(diagnostics: &[Diagnostic]) -> String {
    let config = DisplayDiagnosticConfig::new("ruff")
        .format(DiagnosticFormat::Full)
        .hide_severity(true)
        .with_show_fix_status(true)
        .show_fix_diff(true)
        .with_fix_applicability(Applicability::DisplayOnly);

    DisplayDiagnostics::new(
        &EmitterContext::new(&FxHashMap::default()),
        &config,
        diagnostics,
    )
    .to_string()
}

/// Run a function over an embedded test file, catching any panics that occur in the process.
///
/// If no panic occurs, the result of the function is returned as an `Ok()` variant.
///
/// If a panic occurs, a nicely formatted [`FileFailures`] is returned as an `Err()` variant.
/// This will be formatted into a diagnostic message by `ty_test`.
fn attempt_test<'db, 'a, T, F>(
    db: &'db Db,
    test_fn: F,
    test_file: &'a TestFile,
) -> Result<T, AttemptTestError<'a>>
where
    F: FnOnce(&'db dyn ty_python_semantic::Db, File) -> T + std::panic::UnwindSafe,
{
    catch_unwind(|| test_fn(db, test_file.file))
        .map_err(|info| AttemptTestError { info, test_file })
}

struct AttemptTestError<'a> {
    info: PanicError,
    test_file: &'a TestFile,
}

impl AttemptTestError<'_> {
    fn into_file_failures(
        self,
        db: &Db,
        action: &str,
        clarification: Option<&str>,
    ) -> FileFailures {
        let info = self.info;

        let mut by_line = matcher::FailuresByLine::default();
        let mut messages = vec![];
        match info.location {
            Some(location) => messages.push(format!(
                "Attempting to {action} caused a panic at {location}"
            )),
            None => messages.push(format!(
                "Attempting to {action} caused a panic at an unknown location",
            )),
        }
        if let Some(clarification) = clarification {
            messages.push(clarification.to_string());
        }
        messages.push(String::new());
        match info.payload.as_str() {
            Some(message) => messages.push(message.to_string()),
            // Mimic the default panic hook's rendering of the panic payload if it's
            // not a string.
            None => messages.push("Box<dyn Any>".to_string()),
        }
        messages.push(String::new());

        if let Some(backtrace) = info.backtrace {
            match backtrace.status() {
                BacktraceStatus::Disabled => {
                    let msg =
                        "run with `RUST_BACKTRACE=1` environment variable to display a backtrace";
                    messages.push(msg.to_string());
                }
                BacktraceStatus::Captured => {
                    messages.extend(backtrace.to_string().split('\n').map(String::from));
                }
                _ => {}
            }
        }

        if let Some(backtrace) = info.salsa_backtrace {
            salsa::attach(db, || {
                messages.extend(format!("{backtrace:#}").split('\n').map(String::from));
            });
        }

        by_line.push(OneIndexed::from_zero_indexed(0), messages);

        FileFailures {
            backtick_offsets: self.test_file.backtick_offsets.clone(),
            by_line,
        }
    }
}
