//! Support for compiling [ethers::solc::Project]
use crate::{term, TestFunctionExt};
use comfy_table::{presets::ASCII_MARKDOWN, *};
use ethers_etherscan::contract::Metadata;
use ethers_solc::{
    artifacts::{BytecodeObject, ContractBytecodeSome},
    remappings::Remapping,
    report::NoReporter,
    Artifact, ArtifactId, FileFilter, Graph, Project, ProjectCompileOutput, ProjectPathsConfig,
    Solc, SolcConfig,
};
use eyre::Result;
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fmt::Display,
    path::{Path, PathBuf},
    result,
    str::FromStr,
};

/// Helper type to configure how to compile a project
///
/// This is merely a wrapper for [Project::compile()] which also prints to stdout dependent on its
/// settings
#[derive(Debug, Clone, Default)]
pub struct ProjectCompiler {
    /// whether to also print the contract names
    print_names: bool,
    /// whether to also print the contract sizes
    print_sizes: bool,
    /// files to exclude
    filters: Vec<SkipBuildFilter>,
}

impl ProjectCompiler {
    /// Create a new instance with the settings
    pub fn new(print_names: bool, print_sizes: bool) -> Self {
        Self::with_filter(print_names, print_sizes, Vec::new())
    }

    /// Create a new instance with all settings
    pub fn with_filter(
        print_names: bool,
        print_sizes: bool,
        filters: Vec<SkipBuildFilter>,
    ) -> Self {
        Self { print_names, print_sizes, filters }
    }

    /// Compiles the project with [`Project::compile()`]
    pub fn compile(self, project: &Project) -> Result<ProjectCompileOutput> {
        let filters = self.filters.clone();
        self.compile_with(project, |prj| {
            let output = if filters.is_empty() {
                prj.compile()
            } else {
                prj.compile_sparse(SkipBuildFilters(filters))
            }?;
            Ok(output)
        })
    }

    /// Compiles the project with [`Project::compile_parse()`] and the given filter.
    ///
    /// This will emit artifacts only for files that match the given filter.
    /// Files that do _not_ match the filter are given a pruned output selection and do not generate
    /// artifacts.
    pub fn compile_sparse<F: FileFilter + 'static>(
        self,
        project: &Project,
        filter: F,
    ) -> Result<ProjectCompileOutput> {
        self.compile_with(project, |prj| Ok(prj.compile_sparse(filter)?))
    }

    /// Compiles the project with the given closure
    ///
    /// # Example
    ///
    /// ```no_run
    /// use foundry_common::compile::ProjectCompiler;
    /// let config = foundry_config::Config::load();
    /// ProjectCompiler::default()
    ///     .compile_with(&config.project().unwrap(), |prj| Ok(prj.compile()?)).unwrap();
    /// ```
    #[tracing::instrument(target = "forge::compile", skip_all)]
    pub fn compile_with<F>(self, project: &Project, f: F) -> Result<ProjectCompileOutput>
    where
        F: FnOnce(&Project) -> Result<ProjectCompileOutput>,
    {
        if !project.paths.has_input_files() {
            println!("Nothing to compile");
            // nothing to do here
            std::process::exit(0);
        }

        let now = std::time::Instant::now();
        tracing::trace!("start compiling project");

        let output = term::with_spinner_reporter(|| f(project))?;

        let elapsed = now.elapsed();
        tracing::trace!(?elapsed, "finished compiling");

        if output.has_compiler_errors() {
            tracing::warn!("compiled with errors");
            eyre::bail!(output.to_string())
        } else if output.is_unchanged() {
            println!("No files changed, compilation skipped");
            self.handle_output(&output);
        } else {
            // print the compiler output / warnings
            println!("{output}");

            self.handle_output(&output);
        }

        Ok(output)
    }

    /// If configured, this will print sizes or names
    fn handle_output(&self, output: &ProjectCompileOutput) {
        // print any sizes or names
        if self.print_names {
            let mut artifacts: BTreeMap<_, Vec<_>> = BTreeMap::new();
            for (name, (_, version)) in output.versioned_artifacts() {
                artifacts.entry(version).or_default().push(name);
            }
            for (version, names) in artifacts {
                println!(
                    "  compiler version: {}.{}.{}",
                    version.major, version.minor, version.patch
                );
                for name in names {
                    println!("    - {name}");
                }
            }
        }
        if self.print_sizes {
            // add extra newline if names were already printed
            if self.print_names {
                println!();
            }
            let mut size_report = SizeReport { contracts: BTreeMap::new() };
            let artifacts: BTreeMap<_, _> = output.artifacts().collect();
            for (name, artifact) in artifacts {
                let size = deployed_contract_size(artifact).unwrap_or_default();

                let dev_functions =
                    artifact.abi.as_ref().unwrap().abi.functions().into_iter().filter(|func| {
                        func.name.is_test() || func.name.eq("IS_TEST") || func.name.eq("IS_SCRIPT")
                    });

                let is_dev_contract = dev_functions.into_iter().count() > 0;
                size_report.contracts.insert(name, ContractInfo { size, is_dev_contract });
            }

            println!("{size_report}");

            // exit with error if any contract exceeds the size limit, excluding test contracts.
            if size_report.exceeds_size_limit() {
                std::process::exit(1);
            }
        }
    }
}

// https://eips.ethereum.org/EIPS/eip-170
const CONTRACT_SIZE_LIMIT: usize = 24576;

/// Contracts with info about their size
pub struct SizeReport {
    /// `<contract name>:info>`
    pub contracts: BTreeMap<String, ContractInfo>,
}

impl SizeReport {
    /// Returns the size of the largest contract, excluding test contracts.
    pub fn max_size(&self) -> usize {
        let mut max_size = 0;
        for contract in self.contracts.values() {
            if !contract.is_dev_contract && contract.size > max_size {
                max_size = contract.size;
            }
        }
        max_size
    }

    /// Returns true if any contract exceeds the size limit, excluding test contracts.
    pub fn exceeds_size_limit(&self) -> bool {
        self.max_size() > CONTRACT_SIZE_LIMIT
    }
}

impl Display for SizeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        let mut table = Table::new();
        table.load_preset(ASCII_MARKDOWN);
        table.set_header(vec![
            Cell::new("Contract").add_attribute(Attribute::Bold).fg(Color::Blue),
            Cell::new("Size (kB)").add_attribute(Attribute::Bold).fg(Color::Blue),
            Cell::new("Margin (kB)").add_attribute(Attribute::Bold).fg(Color::Blue),
        ]);

        let contracts = self.contracts.iter().filter(|(_, c)| !c.is_dev_contract && c.size > 0);
        for (name, contract) in contracts {
            let margin = CONTRACT_SIZE_LIMIT as isize - contract.size as isize;
            let color = match contract.size {
                0..=17999 => Color::Reset,
                18000..=CONTRACT_SIZE_LIMIT => Color::Yellow,
                _ => Color::Red,
            };

            table.add_row(vec![
                Cell::new(name).fg(color),
                Cell::new(contract.size as f64 / 1000.0).fg(color),
                Cell::new(margin as f64 / 1000.0).fg(color),
            ]);
        }

        writeln!(f, "{table}")?;
        Ok(())
    }
}

/// Returns the size of the deployed contract
pub fn deployed_contract_size<T: Artifact>(artifact: &T) -> Option<usize> {
    let bytecode = artifact.get_deployed_bytecode_object()?;
    let size = match bytecode.as_ref() {
        BytecodeObject::Bytecode(bytes) => bytes.len(),
        BytecodeObject::Unlinked(unlinked) => {
            // we don't need to account for placeholders here, because library placeholders take up
            // 40 characters: `__$<library hash>$__` which is the same as a 20byte address in hex.
            let mut size = unlinked.as_bytes().len();
            if unlinked.starts_with("0x") {
                size -= 2;
            }
            // hex -> bytes
            size / 2
        }
    };
    Some(size)
}

/// How big the contract is and whether it is a dev contract where size limits can be neglected
#[derive(Debug, Clone, Copy)]
pub struct ContractInfo {
    /// size of the contract in bytes
    pub size: usize,
    /// A development contract is either a Script or a Test contract.
    pub is_dev_contract: bool,
}

/// Compiles the provided [`Project`], throws if there's any compiler error and logs whether
/// compilation was successful or if there was a cache hit.
pub fn compile(
    project: &Project,
    print_names: bool,
    print_sizes: bool,
) -> Result<ProjectCompileOutput> {
    ProjectCompiler::new(print_names, print_sizes).compile(project)
}

/// Compiles the provided [`Project`], throws if there's any compiler error and logs whether
/// compilation was successful or if there was a cache hit.
///
/// Takes a list of [`SkipBuildFilter`] for files to exclude from the build.
pub fn compile_with_filter(
    project: &Project,
    print_names: bool,
    print_sizes: bool,
    skip: Vec<SkipBuildFilter>,
) -> Result<ProjectCompileOutput> {
    ProjectCompiler::with_filter(print_names, print_sizes, skip).compile(project)
}

/// Compiles the provided [`Project`], throws if there's any compiler error and logs whether
/// compilation was successful or if there was a cache hit.
/// Doesn't print anything to stdout, thus is "suppressed".
pub fn suppress_compile(project: &Project) -> Result<ProjectCompileOutput> {
    let output = ethers_solc::report::with_scoped(
        &ethers_solc::report::Report::new(NoReporter::default()),
        || project.compile(),
    )?;

    if output.has_compiler_errors() {
        eyre::bail!(output.to_string())
    }

    Ok(output)
}

/// Depending on whether the `skip` is empty this will [`suppress_compile_sparse`] or
/// [`suppress_compile`]
pub fn suppress_compile_with_filter(
    project: &Project,
    skip: Vec<SkipBuildFilter>,
) -> Result<ProjectCompileOutput> {
    if skip.is_empty() {
        suppress_compile(project)
    } else {
        suppress_compile_sparse(project, SkipBuildFilters(skip))
    }
}

/// Compiles the provided [`Project`], throws if there's any compiler error and logs whether
/// compilation was successful or if there was a cache hit.
/// Doesn't print anything to stdout, thus is "suppressed".
///
/// See [`Project::compile_sparse`]
pub fn suppress_compile_sparse<F: FileFilter + 'static>(
    project: &Project,
    filter: F,
) -> Result<ProjectCompileOutput> {
    let output = ethers_solc::report::with_scoped(
        &ethers_solc::report::Report::new(NoReporter::default()),
        || project.compile_sparse(filter),
    )?;

    if output.has_compiler_errors() {
        eyre::bail!(output.to_string())
    }

    Ok(output)
}

/// Compile a set of files not necessarily included in the `project`'s source dir
///
/// If `silent` no solc related output will be emitted to stdout
pub fn compile_files(
    project: &Project,
    files: Vec<PathBuf>,
    silent: bool,
) -> Result<ProjectCompileOutput> {
    let output = if silent {
        ethers_solc::report::with_scoped(
            &ethers_solc::report::Report::new(NoReporter::default()),
            || project.compile_files(files),
        )
    } else {
        term::with_spinner_reporter(|| project.compile_files(files))
    }?;

    if output.has_compiler_errors() {
        eyre::bail!(output.to_string())
    }
    if !silent {
        println!("{output}");
    }

    Ok(output)
}

/// Compiles target file path.
///
/// If `silent` no solc related output will be emitted to stdout.
///
/// If `verify` and it's a standalone script, throw error. Only allowed for projects.
///
/// **Note:** this expects the `target_path` to be absolute
pub fn compile_target(
    target_path: &Path,
    project: &Project,
    silent: bool,
    verify: bool,
) -> Result<ProjectCompileOutput> {
    compile_target_with_filter(target_path, project, silent, verify, Vec::new())
}

/// Compiles target file path.
pub fn compile_target_with_filter(
    target_path: &Path,
    project: &Project,
    silent: bool,
    verify: bool,
    skip: Vec<SkipBuildFilter>,
) -> Result<ProjectCompileOutput> {
    let graph = Graph::resolve(&project.paths)?;

    // Checking if it's a standalone script, or part of a project.
    if graph.files().get(target_path).is_none() {
        if verify {
            eyre::bail!("You can only verify deployments from inside a project! Make sure it exists with `forge tree`.");
        }
        return compile_files(project, vec![target_path.to_path_buf()], silent)
    }

    if silent {
        suppress_compile_with_filter(project, skip)
    } else {
        compile_with_filter(project, false, false, skip)
    }
}

/// Creates and compiles a project from an Etherscan source.
pub async fn compile_from_source(
    metadata: &Metadata,
) -> Result<(ArtifactId, ContractBytecodeSome)> {
    let root = tempfile::tempdir()?;
    let root_path = root.path();
    let project = etherscan_project(metadata, root_path)?;

    let project_output = project.compile()?;

    if project_output.has_compiler_errors() {
        eyre::bail!(project_output.to_string())
    }

    let (artifact_id, contract) = project_output
        .into_contract_bytecodes()
        .find(|(artifact_id, _)| artifact_id.name == metadata.contract_name)
        .expect("there should be a contract with bytecode");
    let bytecode = ContractBytecodeSome {
        abi: contract.abi.unwrap(),
        bytecode: contract.bytecode.unwrap().into(),
        deployed_bytecode: contract.deployed_bytecode.unwrap().into(),
    };

    root.close()?;

    Ok((artifact_id, bytecode))
}

/// Creates a [Project] from an Etherscan source.
pub fn etherscan_project(metadata: &Metadata, target_path: impl AsRef<Path>) -> Result<Project> {
    let target_path = dunce::canonicalize(target_path.as_ref())?;
    let sources_path = target_path.join(&metadata.contract_name);
    metadata.source_tree().write_to(&target_path)?;

    let mut settings = metadata.source_code.settings()?.unwrap_or_default();

    // make remappings absolute with our root
    for remapping in settings.remappings.iter_mut() {
        let new_path = sources_path.join(remapping.path.trim_start_matches('/'));
        remapping.path = new_path.display().to_string();
    }

    // add missing remappings
    if !settings.remappings.iter().any(|remapping| remapping.name.starts_with("@openzeppelin/")) {
        let oz = Remapping {
            name: "@openzeppelin/".into(),
            path: sources_path.join("@openzeppelin").display().to_string(),
        };
        settings.remappings.push(oz);
    }

    // root/
    //   ContractName/
    //     [source code]
    let paths = ProjectPathsConfig::builder()
        .sources(sources_path)
        .remappings(settings.remappings.clone())
        .build_with_root(target_path);

    let v = metadata.compiler_version()?;
    let v = format!("{}.{}.{}", v.major, v.minor, v.patch);
    let solc = Solc::find_or_install_svm_version(v)?;

    Ok(Project::builder()
        .solc_config(SolcConfig::builder().settings(settings).build())
        .paths(paths)
        .solc(solc)
        .ephemeral()
        .no_artifacts()
        .build()?)
}

/// Bundles multiple `SkipBuildFilter` into a single `FileFilter`
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SkipBuildFilters(pub Vec<SkipBuildFilter>);

impl FileFilter for SkipBuildFilters {
    /// Only returns a match if _no_  exclusion filter matches
    fn is_match(&self, file: &Path) -> bool {
        self.0.iter().all(|filter| filter.is_match(file))
    }
}

/// A filter that excludes matching contracts from the build
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SkipBuildFilter {
    /// Exclude all `.t.sol` contracts
    Tests,
    /// Exclude all `.s.sol` contracts
    Scripts,
    /// Exclude if the file matches
    Custom(String),
}

impl SkipBuildFilter {
    /// Returns the pattern to match against a file
    fn file_pattern(&self) -> &str {
        match self {
            SkipBuildFilter::Tests => ".t.sol",
            SkipBuildFilter::Scripts => ".s.sol",
            SkipBuildFilter::Custom(s) => s.as_str(),
        }
    }
}

impl<T: AsRef<str>> From<T> for SkipBuildFilter {
    fn from(s: T) -> Self {
        match s.as_ref() {
            "test" | "tests" => SkipBuildFilter::Tests,
            "script" | "scripts" => SkipBuildFilter::Scripts,
            s => SkipBuildFilter::Custom(s.to_string()),
        }
    }
}

impl FromStr for SkipBuildFilter {
    type Err = Infallible;

    fn from_str(s: &str) -> result::Result<Self, Self::Err> {
        Ok(s.into())
    }
}

impl FileFilter for SkipBuildFilter {
    /// Matches file only if the filter does not apply
    ///
    /// This is returns the inverse of `file.name.contains(pattern)`
    fn is_match(&self, file: &Path) -> bool {
        fn exclude(file: &Path, pattern: &str) -> Option<bool> {
            let file_name = file.file_name()?.to_str()?;
            Some(file_name.contains(pattern))
        }

        !exclude(file, self.file_pattern()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_filter() {
        let file = Path::new("A.t.sol");
        assert!(!SkipBuildFilter::Tests.is_match(file));
        assert!(SkipBuildFilter::Scripts.is_match(file));
        assert!(!SkipBuildFilter::Custom("A.t".to_string()).is_match(file));

        let file = Path::new("A.s.sol");
        assert!(SkipBuildFilter::Tests.is_match(file));
        assert!(!SkipBuildFilter::Scripts.is_match(file));
        assert!(!SkipBuildFilter::Custom("A.s".to_string()).is_match(file));
    }
}
