mod compilation_database;
mod information_leak;
mod suppressions;

use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    vec,
};

use anyhow::{anyhow, Context, Result};
use clang::{Clang, Entity, EntityKind, Index};
use compilation_database::CompileCommands;
use glob::glob;
use information_leak::{BinaryLocation, ConfirmedLeak};
use structopt::StructOpt;
use suppressions::Suppressions;

use crate::{
    compilation_database::{CompilationDatabase, CompileCommandsDatabase, FileListDatabase},
    information_leak::{print_confirmed_leaks, PotentialLeak},
    suppressions::parse_suppressions_file,
};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");

#[derive(Debug, StructOpt)]
#[structopt(name = PKG_NAME, about = "An information leak detector for C and C++ code bases")]
struct CpplumberOptions {
    /// Path to the output binary to scan for leaked information.
    #[structopt(parse(from_os_str), short, long = "bin")]
    binary_file_path: PathBuf,

    /// Additional include directories.
    /// Only used when project files aren't used.
    #[structopt(short = "I")]
    include_directories: Vec<String>,

    /// Additional preprocessor definitions.
    /// Only used when project files aren't used.
    #[structopt(short = "D")]
    compile_definitions: Vec<String>,

    /// Compilation database.
    #[structopt(parse(from_os_str), short, long = "project")]
    project_file_path: Option<PathBuf>,

    /// Path to a file containing rules to prevent certain errors from being
    /// generated.
    #[structopt(parse(from_os_str), short, long)]
    suppressions_list: Option<PathBuf>,

    /// Only report leaks once for artifacts declared in multiple locations.
    #[structopt(long)]
    ignore_multiple_declarations: bool,

    /// Report leaks for data declared in system headers
    #[structopt(long)]
    report_system_headers: bool,

    /// Generate output as JSON.
    #[structopt(short, long = "json")]
    json_output: bool,

    /// List of source files to scan for (can be glob expressions).
    source_path_globs: Vec<String>,
}

fn main() -> Result<()> {
    env_logger::init();
    let options = CpplumberOptions::from_args();

    // Initial checks before starting work
    if !options.binary_file_path.is_file() {
        return Err(anyhow!(
            "'{}' is not a valid file path.",
            options.binary_file_path.display()
        ));
    }

    // Parse the suppression list if used
    let suppressions = if let Some(ref suppressions_list) = options.suppressions_list {
        log::info!("Parsing suppressions file...");
        Some(
            parse_suppressions_file(suppressions_list)
                .with_context(|| "Failed to parse suppressions list")?,
        )
    } else {
        None
    };

    log::info!("Gathering source files...");
    // Parse project file or process glob expressions
    let compilation_db = generate_compilation_database(&options)?;

    log::info!("Filtering suppressed files...");
    // Filter suppressed files
    let compile_commands =
        filter_suppressed_files(compilation_db.get_all_compile_commands(), &suppressions);

    log::info!("Extracting artifacts from source files...");
    // Parse source files and extract information that could leak
    let potential_leaks =
        extract_artifacts_from_source_files(compile_commands, options.report_system_headers)?;

    log::info!("Filtering suppressed artifacts...");
    // Filter suppressed artifacts if needed
    let potential_leaks = filter_suppressed_artifacts(potential_leaks, &suppressions);

    log::info!(
        "Looking for leaks in '{}'...",
        options.binary_file_path.display()
    );
    let leaks = if options.ignore_multiple_declarations {
        // Remove duplicated artifacts if needed
        let potential_leaks: HashSet<PotentialLeak> = HashSet::from_iter(potential_leaks);
        log::debug!("{:#?}", potential_leaks);
        find_leaks_in_binary_file(&options.binary_file_path, &potential_leaks)?
    } else {
        log::debug!("{:#?}", potential_leaks);
        find_leaks_in_binary_file(&options.binary_file_path, &potential_leaks)?
    };
    log::debug!("Done!");

    // Print the result to stdout
    print_confirmed_leaks(leaks, options.json_output)?;

    Ok(())
}

fn gather_entities_by_kind<'tu>(
    root_entity: Entity<'tu>,
    entity_kind_filter: &[EntityKind],
    ignore_system_headers: bool,
) -> Vec<Entity<'tu>> {
    gather_entities_by_kind_rec(root_entity, entity_kind_filter, ignore_system_headers, 0)
}

fn gather_entities_by_kind_rec<'tu>(
    root_entity: Entity<'tu>,
    entity_kind_filter: &[EntityKind],
    ignore_system_headers: bool,
    current_depth: usize,
) -> Vec<Entity<'tu>> {
    let mut entities = vec![];

    let root_entity_kind = root_entity.get_kind();
    // Check the if entity's kind is one we're looking for
    if entity_kind_filter
        .iter()
        .any(|elem| elem == &root_entity_kind)
    {
        entities.push(root_entity);
    }

    for child in root_entity.get_children() {
        // Ignore entity if requested
        if ignore_system_headers && child.is_in_system_header() {
            continue;
        }

        let entities_sub = gather_entities_by_kind_rec(
            child,
            entity_kind_filter,
            ignore_system_headers,
            current_depth + 1,
        );
        entities.extend(entities_sub);
    }

    entities
}

fn generate_compilation_database(
    options: &CpplumberOptions,
) -> Result<Box<dyn CompilationDatabase>> {
    if let Some(ref project_file_path) = options.project_file_path {
        // Parse compile commands from the JSON database
        Ok(Box::new(CompileCommandsDatabase::new(project_file_path)?))
    } else {
        // Otherwise, process glob expressions
        let mut file_paths = vec![];
        for glob_expressions in options.source_path_globs.iter() {
            if let Ok(paths) = glob(glob_expressions) {
                for path in paths {
                    file_paths.push(path?);
                }
            } else {
                log::warn!(
                    "'{}' is not a valid path or glob expression, ignoring it",
                    glob_expressions
                );
            }
        }

        // Generate `arguments` from the CLI arguments
        let mut arguments = vec![];

        // Add include directories to the list of arguments
        for include_dir in options.include_directories.iter() {
            arguments.push(format!("-I{}", include_dir));
        }
        // Add preprocessor defitions to the list of arguments
        for compile_def in options.compile_definitions.iter() {
            arguments.push(format!("-D{}", compile_def));
        }

        log::debug!("Using arguments: {:?}", arguments);
        Ok(Box::new(FileListDatabase::new(&file_paths, arguments)))
    }
}

fn filter_suppressed_files(
    compile_cmds: CompileCommands,
    suppressions: &Option<Suppressions>,
) -> CompileCommands {
    if let Some(suppressions) = suppressions {
        compile_cmds
            .into_iter()
            .filter(|compile_cmd| {
                let file_path = compile_cmd.directory.join(&compile_cmd.filename);
                if let Some(file_path) = file_path.to_str() {
                    !suppressions
                        .files
                        .iter()
                        .any(|pattern| pattern.matches(file_path))
                } else {
                    true
                }
            })
            .collect()
    } else {
        compile_cmds
    }
}

fn extract_artifacts_from_source_files(
    compile_commands: CompileCommands,
    ignore_system_headers: bool,
) -> Result<Vec<PotentialLeak>> {
    // Prepare the clang index
    let clang = Clang::new().map_err(|e| anyhow!(e))?;
    let index = Index::new(&clang, false, false);

    // Populate index by parsing source files
    let mut potential_leaks: Vec<PotentialLeak> = vec![];
    for compile_cmd in compile_commands {
        let file_path = compile_cmd.directory.join(compile_cmd.filename);
        let translation_unit = index
            .parser(&file_path)
            .arguments(&compile_cmd.arguments)
            .parse()
            .with_context(|| format!("Failed to parse source file '{}'", file_path.display()))?;

        let string_literals = gather_entities_by_kind(
            translation_unit.get_entity(),
            &[EntityKind::StringLiteral],
            ignore_system_headers,
        );

        potential_leaks.extend(
            string_literals
                .into_iter()
                .filter_map(|literal| literal.try_into().ok()),
        );
    }

    Ok(potential_leaks)
}

fn filter_suppressed_artifacts(
    potential_leaks: Vec<PotentialLeak>,
    suppressions: &Option<Suppressions>,
) -> Vec<PotentialLeak> {
    if let Some(suppressions) = suppressions {
        potential_leaks
            .into_iter()
            .filter(|leak| !suppressions.artifacts.contains(&leak.leaked_information))
            .collect()
    } else {
        potential_leaks
    }
}

fn find_leaks_in_binary_file<'l, PotentialLeakCollection>(
    binary_file_path: &Path,
    leak_desc: PotentialLeakCollection,
) -> Result<Vec<ConfirmedLeak>>
where
    PotentialLeakCollection: IntoIterator<Item = &'l PotentialLeak>,
{
    let mut bin_file = File::open(binary_file_path)?;

    let mut bin_data = vec![];
    bin_file.read_to_end(&mut bin_data)?;

    Ok(leak_desc
        .into_iter()
        .filter_map(|leak| {
            bin_data
                .windows(leak.bytes.len())
                .position(|window| window == leak.bytes)
                .map(|offset| ConfirmedLeak {
                    leaked_information: leak.leaked_information.clone(),
                    location: information_leak::LeakLocation {
                        source: leak.declaration_metadata.clone(),
                        binary: BinaryLocation {
                            file: binary_file_path.to_path_buf(),
                            offset: offset as u64,
                        },
                    },
                })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_LIST_PROJ_PATH: &str = "tests/data/main/file_list_proj";

    #[test]
    fn extract_artifacts_from_source_files_file_list() {
        let root_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FILE_LIST_PROJ_PATH);
        let file_list_db = FileListDatabase::new(
            &[
                root_dir_path.join("main.cc"),
                root_dir_path.join("header.h"),
            ],
            vec!["-DDEF_TEST".to_string()],
        );
        let potential_leaks =
            extract_artifacts_from_source_files(file_list_db.get_all_compile_commands(), true)
                .expect("extract_artifacts_from_source_files failed");

        let expected_string_literals = vec![
            // header.h
            "\"included_string_literal\"",
            // main.cc
            "\"included_string_literal\"",
            "\"c_string\"",
            "u8\"utf8_string\"",
            "L\"wide_string\"",
            "u\"utf16_string\"",
            "U\"utf32_string\"",
            "\"raw_string\"",
            "u8\"raw_utf8_string\"",
            "L\"wide_raw_string\"",
            "u\"raw_utf16_string\"",
            "U\"raw_utf32_string\"",
            "\"def_test\"",
            "\"concatenated_string\"",
            r#""multiline\nstring""#,
            r#""'\"\n\t\a\b|\220|\220|\351\246\231|\351\246\231|\360\237\230\202""#,
            r#""%s\n""#,
            "\"preprocessor_string_literal\"",
            r#"L"%s\n""#,
            "L\"preprocessor_string_literal\"",
            r#""%s\n""#,
        ];
        assert_eq!(expected_string_literals.len(), potential_leaks.len());
        // Check extracted string literals
        assert!(potential_leaks.iter().enumerate().all(|(i, leak)| {
            println!("{:?}", leak.leaked_information);
            leak.leaked_information == expected_string_literals[i]
        }));
    }
}
