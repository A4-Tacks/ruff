use crate::parser::{BacktickOffsets, EmbeddedFileSourceMap};
use anyhow::anyhow;
use camino::Utf8Path;
use colored::Colorize;
use itertools::Itertools;
use parser as test_parser;
use ruff_db::Db as _;
use ruff_db::diagnostic::{
    Diagnostic, DiagnosticFormat, DisplayDiagnosticConfig, DisplayDiagnostics, Span,
};
use ruff_db::files::{File, FileRootKind, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{DbWithWritableSystem as _, SystemPathBuf};
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
use ruff_python_ast::{PySourceType, PythonVersion};
use ruff_python_codegen::Stylist;
use ruff_python_index::Indexer;
use ruff_python_parser::{ParseError, ParseOptions};
use ruff_source_file::{LineIndex, OneIndexed, SourceFileBuilder};
use ruff_workspace::configuration::Configuration;
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::fmt::{Display, Write};
use std::path::Path;
use ty_module_resolver::SearchPathSettings;
use ty_python_semantic::{
    FallibleStrategy, Program, ProgramSettings, PythonPlatform, PythonVersionSource,
    PythonVersionWithSource,
};

mod assertion;
mod db;
mod diagnostic;
mod matcher;
mod parser;

use ty_static::EnvVars;

/// Run `path` as a markdown test suite with given `title`.
///
/// Panic on test failure, and print failure details.
pub fn run(
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

        let result = run_test(&mut db, relative_fixture_path, snapshot_path, &test);

        let this_test_failed = result.is_err();
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestOutcome {
    Success,
}

fn run_test(
    db: &mut db::Db,
    relative_fixture_path: &Utf8Path,
    snapshot_path: &Utf8Path,
    test: &parser::MarkdownTest,
) -> Result<TestOutcome, Failures> {
    // Initialize the system and remove all files and directories to reset the system to a clean state.
    db.use_in_memory_system();

    let project_root = SystemPathBuf::from("/src");
    db.create_directory_all(&project_root)
        .expect("Creating the project root to succeed");
    db.files()
        .try_add_root(db, &project_root, FileRootKind::Project);

    let src_path = project_root.clone();

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

            let full_path = embedded.full_path(&project_root);

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

    let settings = Configuration::from_options(
        test.configuration().clone(),
        None,
        project_root.as_std_path(),
    )
    .expect("Failed to construct configuration from options")
    .into_settings(project_root.as_std_path())
    .expect("Failed to construct settings");

    let program_settings = ProgramSettings {
        python_version: PythonVersionWithSource {
            version: PythonVersion::default(),
            source: PythonVersionSource::Cli,
        },
        search_paths: SearchPathSettings::new(vec![src_path])
            .to_search_paths(db.system(), db.vendored(), &FallibleStrategy)
            .expect("Failed to resolve search path settings"),
        python_platform: PythonPlatform::All,
    };

    Program::init_or_update(db, program_settings);

    // When snapshot testing is enabled, this is populated with
    // all diagnostics. Otherwise it remains empty.
    let mut snapshot_diagnostics = vec![];

    let failures: Failures = test_files
        .iter()
        .filter_map(|test_file| {
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
            let mut diagnostics = test_contents(&source_kind, path, &settings.linter);

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

            if test.should_snapshot_diagnostics() {
                snapshot_diagnostics.extend(diagnostics);
            }

            failure
        })
        .collect();

    if snapshot_diagnostics.is_empty() && test.should_snapshot_diagnostics() {
        panic!(
            "Test `{}` requested snapshotting diagnostics but it didn't produce any.",
            test.name()
        );
    } else if !snapshot_diagnostics.is_empty() {
        let snapshot =
            create_diagnostic_snapshot(relative_fixture_path, test, snapshot_diagnostics);
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
    relative_fixture_path: &Utf8Path,
    test: &parser::MarkdownTest,
    diagnostics: impl IntoIterator<Item = Diagnostic>,
) -> String {
    let display_config = DisplayDiagnosticConfig::new("ruff")
        .color(false)
        .show_fix_diff(true)
        .with_fix_applicability(Applicability::DisplayOnly);
    let notebook_indexes = FxHashMap::default();
    let resolver = EmitterContext::new(&notebook_indexes);

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
        write!(snapshot, "{}", diag.display(&resolver, &display_config)).unwrap();
        writeln!(snapshot, "```").unwrap();
    }
    snapshot
}

const MAX_ITERATIONS: usize = 10;

/// A convenient wrapper around [`check_path`], that additionally
/// asserts that fixes converge after a fixed number of iterations.
fn test_contents(
    source_kind: &SourceKind,
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
                    "Failed to converge after {MAX_ITERATIONS} iterations. This likely \
                     indicates a bug in the implementation of the fix. Last diagnostics:\n{output}"
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
