use crate::{execute::LinkedState, ScriptArgs, ScriptConfig};

use alloy_primitives::{Address, Bytes};
use eyre::{Context, OptionExt, Result};
use foundry_cheatcodes::ScriptWallets;
use foundry_cli::utils::get_cached_entry_by_name;
use foundry_common::{
    compile::{self, ContractSources, ProjectCompiler},
    ContractsByArtifact,
};
use foundry_compilers::{
    artifacts::{BytecodeObject, ContractBytecode, ContractBytecodeSome, Libraries},
    cache::SolFilesCache,
    contracts::ArtifactContracts,
    info::ContractInfo,
    ArtifactId,
};
use foundry_linking::{LinkOutput, Linker};
use std::str::FromStr;

/// Container for the compiled contracts.
pub struct BuildData {
    /// Linker which can be used to link contracts, owns [ArtifactContracts] map.
    pub linker: Linker,
    /// Id of target contract artifact.
    pub target: ArtifactId,
    /// Source files of the contracts. Used by debugger.
    pub sources: ContractSources,
}

impl BuildData {
    /// Links the build data with given libraries, using sender and nonce to compute addresses of
    /// missing libraries.
    pub fn link(
        self,
        known_libraries: Libraries,
        sender: Address,
        nonce: u64,
    ) -> Result<LinkedBuildData> {
        let link_output =
            self.linker.link_with_nonce_or_address(known_libraries, sender, nonce, &self.target)?;

        LinkedBuildData::new(link_output, self)
    }

    /// Links the build data with the given libraries. Expects supplied libraries set being enough
    /// to fully link target contract.
    pub fn link_with_libraries(self, libraries: Libraries) -> Result<LinkedBuildData> {
        let link_output =
            self.linker.link_with_nonce_or_address(libraries, Address::ZERO, 0, &self.target)?;

        if !link_output.libs_to_deploy.is_empty() {
            eyre::bail!("incomplete libraries set");
        }

        LinkedBuildData::new(link_output, self)
    }
}

/// Container for the linked contracts and their dependencies
pub struct LinkedBuildData {
    /// Original build data, might be used to relink this object with different libraries.
    pub build_data: BuildData,
    /// Known fully linked contracts.
    pub highlevel_known_contracts: ArtifactContracts<ContractBytecodeSome>,
    /// Libraries used to link the contracts.
    pub libraries: Libraries,
    /// Libraries that need to be deployed by sender before script execution.
    pub predeploy_libraries: Vec<Bytes>,
}

impl LinkedBuildData {
    pub fn new(link_output: LinkOutput, build_data: BuildData) -> Result<Self> {
        let highlevel_known_contracts = build_data
            .linker
            .get_linked_artifacts(&link_output.libraries)?
            .iter()
            .filter_map(|(id, contract)| {
                ContractBytecodeSome::try_from(ContractBytecode::from(contract.clone()))
                    .ok()
                    .map(|tc| (id.clone(), tc))
            })
            .filter(|(_, tc)| tc.bytecode.object.is_non_empty_bytecode())
            .collect();

        Ok(Self {
            build_data,
            highlevel_known_contracts,
            libraries: link_output.libraries,
            predeploy_libraries: link_output.libs_to_deploy,
        })
    }

    /// Flattens the contracts into  (`id` -> (`JsonAbi`, `Vec<u8>`)) pairs
    pub fn get_flattened_contracts(&self, deployed_code: bool) -> ContractsByArtifact {
        ContractsByArtifact(
            self.highlevel_known_contracts
                .iter()
                .filter_map(|(id, c)| {
                    let bytecode = if deployed_code {
                        c.deployed_bytecode.bytes()
                    } else {
                        c.bytecode.bytes()
                    };
                    bytecode.cloned().map(|code| (id.clone(), (c.abi.clone(), code.into())))
                })
                .collect(),
        )
    }

    /// Fetches target bytecode from linked contracts.
    pub fn get_target_contract(&self) -> Result<ContractBytecodeSome> {
        self.highlevel_known_contracts
            .get(&self.build_data.target)
            .cloned()
            .ok_or_eyre("target not found in linked artifacts")
    }
}

/// First state basically containing only inputs of the user.
pub struct PreprocessedState {
    pub args: ScriptArgs,
    pub script_config: ScriptConfig,
    pub script_wallets: ScriptWallets,
}

impl PreprocessedState {
    /// Parses user input and compiles the contracts depending on script target.
    /// After compilation, finds exact [ArtifactId] of the target contract.
    pub fn compile(self) -> Result<CompiledState> {
        let Self { args, script_config, script_wallets } = self;
        let project = script_config.config.project()?;
        let filters = args.skip.clone().unwrap_or_default();

        let mut target_name = args.target_contract.clone();

        // If we've received correct path, use it as target_path
        // Otherwise, parse input as <path>:<name> and use the path from the contract info, if
        // present.
        let target_path = if let Ok(path) = dunce::canonicalize(&args.path) {
            Some(path)
        } else {
            let contract = ContractInfo::from_str(&args.path)?;
            target_name = Some(contract.name.clone());
            if let Some(path) = contract.path {
                Some(dunce::canonicalize(path)?)
            } else {
                None
            }
        };

        // If we've found target path above, only compile it.
        // Otherwise, compile everything to match contract by name later.
        let output = if let Some(target_path) = target_path.clone() {
            compile::compile_target_with_filter(
                &target_path,
                &project,
                args.opts.silent,
                args.verify,
                filters,
            )
        } else if !project.paths.has_input_files() {
            Err(eyre::eyre!("The project doesn't have any input files. Make sure the `script` directory is configured properly in foundry.toml. Otherwise, provide the path to the file."))
        } else {
            ProjectCompiler::new().compile(&project)
        }?;

        // If we still don't have target path, find it by name in the compilation cache.
        let target_path = if let Some(target_path) = target_path {
            target_path
        } else {
            let target_name = target_name.clone().expect("was set above");
            let cache = SolFilesCache::read_joined(&project.paths)
                .wrap_err("Could not open compiler cache")?;
            let (path, _) = get_cached_entry_by_name(&cache, &target_name)
                .wrap_err("Could not find target contract in cache")?;
            path
        };

        let target_path = project.root().join(target_path);

        let mut target_id: Option<ArtifactId> = None;

        // Find target artfifact id by name and path in compilation artifacts.
        for (id, contract) in output.artifact_ids().filter(|(id, _)| id.source == target_path) {
            if let Some(name) = &target_name {
                if id.name != *name {
                    continue;
                }
            } else if contract.abi.as_ref().map_or(true, |abi| abi.is_empty()) ||
                contract.bytecode.as_ref().map_or(true, |b| match &b.object {
                    BytecodeObject::Bytecode(b) => b.is_empty(),
                    BytecodeObject::Unlinked(_) => false,
                })
            {
                // Ignore contracts with empty abi or linked bytecode of length 0 which are
                // interfaces/abstract contracts/libraries.
                continue;
            }

            if let Some(target) = target_id {
                // We might have multiple artifacts for the same contract but with different
                // solc versions. Their names will have form of {name}.0.X.Y, so we are
                // stripping versions off before comparing them.
                let target_name = target.name.split('.').next().unwrap();
                let id_name = id.name.split('.').next().unwrap();
                if target_name != id_name {
                    eyre::bail!("Multiple contracts in the target path. Please specify the contract name with `--tc ContractName`")
                }
            }
            target_id = Some(id);
        }

        let sources = ContractSources::from_project_output(&output, project.root())?;
        let contracts = output.into_artifacts().collect();
        let target = target_id.ok_or_eyre("Could not find target contract")?;
        let linker = Linker::new(project.root(), contracts);

        Ok(CompiledState {
            args,
            script_config,
            script_wallets,
            build_data: BuildData { linker, target, sources },
        })
    }
}

/// State after we have determined and compiled target contract to be executed.
pub struct CompiledState {
    pub args: ScriptArgs,
    pub script_config: ScriptConfig,
    pub script_wallets: ScriptWallets,
    pub build_data: BuildData,
}

impl CompiledState {
    /// Uses provided sender address to compute library addresses and link contracts with them.
    pub fn link(self) -> Result<LinkedState> {
        let Self { args, script_config, script_wallets, build_data } = self;

        let sender = script_config.evm_opts.sender;
        let nonce = script_config.sender_nonce;
        let known_libraries = script_config.config.libraries_with_remappings()?;
        let build_data = build_data.link(known_libraries, sender, nonce)?;

        Ok(LinkedState { args, script_config, script_wallets, build_data })
    }
}
