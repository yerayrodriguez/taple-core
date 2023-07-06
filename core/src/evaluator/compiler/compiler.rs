use crate::database::Error as DbError;
use crate::evaluator::errors::CompilerError;
use crate::governance::GovernanceInterface;
use crate::identifier::{Derivable, DigestIdentifier};
use crate::{database::DB, evaluator::errors::CompilerErrorResponses, DatabaseCollection};
use async_std::fs;
use std::collections::HashSet;
use std::fs::create_dir;
use std::path::Path;
use std::process::Command;
use wasm_gc::garbage_collect_file;
use wasmtime::{Engine, ExternType};

use super::manifest::get_toml;

pub struct Compiler<C: DatabaseCollection, G: GovernanceInterface> {
    database: DB<C>,
    gov_api: G,
    engine: Engine,
    contracts_path: String,
    available_imports_set: HashSet<String>,
}

impl<C: DatabaseCollection, G: GovernanceInterface> Compiler<C, G> {
    pub fn new(database: DB<C>, gov_api: G, engine: Engine, contracts_path: String) -> Self {
        let available_imports_set = get_sdk_functions_identifier();
        Self {
            database,
            gov_api,
            engine,
            contracts_path,
            available_imports_set,
        }
    }

    pub async fn init(&self) -> Result<(), CompilerError> {
        // Comprueba si existe el contrato de gobernanza en el sistema
        // Si no existe, lo compila y lo guarda
        let cargo_path = format!("{}/Cargo.toml", self.contracts_path);
        if !Path::new(&cargo_path).exists() {
            let toml: String = get_toml();
            // Escribimos cargo.toml
            fs::write(cargo_path, toml)
                .await
                .map_err(|_| CompilerErrorResponses::WriteFileError)?;
        }
        let src_path = format!("{}/src", self.contracts_path);
        if !Path::new(&src_path).exists() {
            create_dir(&src_path).map_err(|e| {
                CompilerErrorResponses::FolderNotCreated(src_path.to_string(), e.to_string())
            })?;
        }
        match self.database.get_governance_contract() {
            Ok(_) => return Ok(()),
            Err(DbError::EntryNotFound) => {
                self.compile(
                    super::gov_contract::get_gov_contract(),
                    "taple",
                    "governance",
                )
                .await
                .map_err(|e| CompilerError::InitError(e.to_string()))?;
                let compiled_contract = self
                    .add_contract()
                    .await
                    .map_err(|e| CompilerError::InitError(e.to_string()))?;
                self.database
                    .put_governance_contract(compiled_contract)
                    .map_err(|error| CompilerError::DatabaseError(error.to_string()))?;
            }
            Err(error) => return Err(CompilerError::DatabaseError(error.to_string())),
        }
        Ok(())
    }

    pub async fn update_contracts(
        &self,
        governance_id: DigestIdentifier,
        governance_version: u64,
    ) -> Result<(), CompilerErrorResponses> {
        // TODO: Pillar contrato de base de datos, comprobar si el hash cambia y compilar, si no cambia no compilar
        // Read the contract from database
        let contracts = self
            .gov_api
            .get_contracts(governance_id.clone(), governance_version)
            .await
            .map_err(CompilerErrorResponses::GovernanceError)?;
        log::error!("COMPILER AFTER GET CONTRACTS");
        for (contract_info, schema_id) in contracts {
            let contract_data = match self.database.get_contract(&governance_id, &schema_id) {
                Ok((contract, hash, contract_gov_version)) => {
                    Some((contract, hash, contract_gov_version))
                }
                Err(DbError::EntryNotFound) => {
                    // Añadir en la response
                    None
                }
                Err(error) => return Err(CompilerErrorResponses::DatabaseError(error.to_string())),
            };
            let new_contract_hash =
                DigestIdentifier::from_serializable_borsh(&contract_info.raw)
                    .map_err(|_| CompilerErrorResponses::BorshSerializeContractError)?;
            if let Some(contract_data) = contract_data {
                if governance_version == contract_data.2 {
                    continue;
                }
                if contract_data.1 == new_contract_hash {
                    // Se actualiza la versión de la gobernanza asociada
                    self.database
                        .put_contract(
                            &governance_id,
                            &schema_id,
                            contract_data.0,
                            new_contract_hash,
                            governance_version,
                        )
                        .map_err(|error| {
                            CompilerErrorResponses::DatabaseError(error.to_string())
                        })?;
                    continue;
                }
            }
            self.compile(contract_info.raw, &governance_id.to_str(), &schema_id)
                .await?;
            log::error!("COMPILER AFTER COMPILER");
            let compiled_contract = self
                .add_contract()
                .await?;
            self.database
                .put_contract(
                    &governance_id,
                    &schema_id,
                    compiled_contract,
                    new_contract_hash,
                    governance_version,
                )
                .map_err(|error| CompilerErrorResponses::DatabaseError(error.to_string()))?;
        }
        Ok(())
    }

    async fn compile(
        &self,
        contract: String,
        governance_id: &str,
        schema_id: &str,
    ) -> Result<(), CompilerErrorResponses> {
        fs::write(format!("{}/src/lib.rs", self.contracts_path), contract)
            .await
            .map_err(|_| CompilerErrorResponses::WriteFileError)?;
        let status = Command::new("cargo")
            .arg("build")
            .arg(format!(
                "--manifest-path={}/Cargo.toml",
                self.contracts_path
            ))
            .arg("--target")
            .arg("wasm32-unknown-unknown")
            .arg("--release")
            .output()
            // No muestra stdout. Genera proceso hijo y espera
            .map_err(|_| CompilerErrorResponses::CargoExecError)?;
        println!("status {:?}", status);
        if !status.status.success() {
            return Err(CompilerErrorResponses::CargoExecError);
        }
        // Utilidad para optimizar el Wasm resultante
        // Es una API, así que requiere de Wasm-gc en el sistema

        std::fs::create_dir_all(format!(
            "/tmp/taple_contracts/{}/{}",
            governance_id, schema_id
        ))
        .map_err(|_| CompilerErrorResponses::TempFolderCreationFailed)?;

        Ok(())
    }

    async fn add_contract(
        &self,
    ) -> Result<Vec<u8>, CompilerErrorResponses> {
        // AOT COMPILATION
        let file = fs::read(format!(
            "{}/target/wasm32-unknown-unknown/release/contract.wasm",
            self.contracts_path
        ))
        .await
        .map_err(|_| CompilerErrorResponses::AddContractFail)?;
        let module_bytes = self
            .engine
            .precompile_module(&file)
            .map_err(|_| CompilerErrorResponses::AddContractFail)?;
        let module = unsafe { wasmtime::Module::deserialize(&self.engine, &module_bytes).unwrap() };
        let imports = module.imports();
        let mut pending_sdk = self.available_imports_set.clone();
        for import in imports {
            match import.ty() {
                ExternType::Func(_) => {
                    if !self.available_imports_set.contains(import.name()) {
                        return Err(CompilerErrorResponses::InvalidImportFound);
                    }
                    pending_sdk.remove(import.name());
                }
                _ => return Err(CompilerErrorResponses::InvalidImportFound),
            }
        }
        if !pending_sdk.is_empty() {
            return Err(CompilerErrorResponses::NoSDKFound);
        }
        Ok(module_bytes)
    }
}

fn get_sdk_functions_identifier() -> HashSet<String> {
    HashSet::from_iter(
        vec![
            "alloc".to_owned(),
            "write_byte".to_owned(),
            "pointer_len".to_owned(),
            "read_byte".to_owned(),
            // "cout".to_owned(),
        ]
        .into_iter(),
    )
}
