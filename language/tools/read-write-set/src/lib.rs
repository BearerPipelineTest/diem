// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

pub mod dynamic_analysis;

use crate::dynamic_analysis::ConcretizedSecondaryIndexes;
use anyhow::{bail, Result};
use move_binary_format::file_format::CompiledModule;
use move_bytecode_utils::Modules;
use move_core_types::{
    account_address::AccountAddress,
    identifier::IdentStr,
    language_storage::{ModuleId, ResourceKey, TypeTag},
};
use move_model::model::{FunctionEnv, GlobalEnv};
use move_vm_runtime::data_cache::MoveStorage;
use prover_bytecode::{
    access_path::Offset,
    function_target_pipeline::{FunctionTargetPipeline, FunctionTargetsHolder, FunctionVariant},
    read_write_set_analysis::{ReadWriteSetProcessor, ReadWriteSetState},
};

pub struct ReadWriteSetAnalysis {
    targets: FunctionTargetsHolder,
    env: GlobalEnv,
}

/// Infer read/write set results for `modules`.
/// The `modules` list must be topologically sorted by the dependency relation
/// (i.e., a child node in the dependency graph should appear earlier in the
/// vector than its parents), and all dependencies of each module must be
/// included.
pub fn analyze<'a>(
    modules: impl IntoIterator<Item = &'a CompiledModule>,
) -> Result<ReadWriteSetAnalysis> {
    let module_map = Modules::new(modules);
    let dep_graph = module_map.compute_dependency_graph();
    let topo_order = dep_graph.compute_topological_order()?;
    analyze_sorted(topo_order)
}

/// Like analyze_unsorted, but assumes that `modules` is already topologically sorted
pub fn analyze_sorted<'a>(
    modules: impl IntoIterator<Item = &'a CompiledModule>,
) -> Result<ReadWriteSetAnalysis> {
    let env = move_model::run_bytecode_model_builder(modules)?;
    let mut pipeline = FunctionTargetPipeline::default();
    pipeline.add_processor(ReadWriteSetProcessor::new());
    let mut targets = FunctionTargetsHolder::default();
    for module_env in env.get_modules() {
        for func_env in module_env.get_functions() {
            targets.add_target(&func_env)
        }
    }
    pipeline.run(&env, &mut targets);

    Ok(ReadWriteSetAnalysis { targets, env })
}

impl ReadWriteSetAnalysis {
    /// Return an overapproximation access paths read/written by `module`::`fun`.
    /// Returns `None` if the function or module does not exist.
    pub fn get_summary(&self, module: &ModuleId, fun: &IdentStr) -> Option<&ReadWriteSetState> {
        self.get_function_env(module, fun).and_then(|fenv| {
            self.targets
                .get_data(&fenv.get_qualified_id(), &FunctionVariant::Baseline)
                .and_then(|data| data.annotations.get::<ReadWriteSetState>())
        })
    }

    fn get_summary_(&self, module: &ModuleId, fun: &IdentStr) -> Result<&ReadWriteSetState> {
        if let Some(state) = self.get_summary(module, fun) {
            Ok(state)
        } else {
            bail!("Couldn't resolve function {:?}::{:?}", module, fun)
        }
    }

    /// Returns an overapproximation of the access paths in global storage that will be read/written
    /// by `module::fun` if called with arguments `signers`, `actuals`, `type_actuals` in state
    /// `blockchain_view`.
    pub fn get_concretized_summary(
        &self,
        module: &ModuleId,
        fun: &IdentStr,
        signers: &[AccountAddress],
        actuals: &[Vec<u8>],
        type_actuals: &[TypeTag],
        blockchain_view: &dyn MoveStorage,
    ) -> Result<ConcretizedSecondaryIndexes> {
        let state = self.get_summary_(module, fun)?;
        dynamic_analysis::concretize(
            state.accesses().clone(),
            signers,
            actuals,
            type_actuals,
            &self.get_function_env(module, fun).unwrap(),
            blockchain_view,
        )
    }

    /// Return `true` if `module`::`fun` may read an address from the blockchain state and
    /// subsequently read/write a resource stored at that address. Return `false` if the function
    /// will not do this in any possible concrete execution. Return an error if `module`::`fun` does
    /// not exist.
    pub fn may_have_secondary_indexes(&self, module: &ModuleId, fun: &IdentStr) -> Result<bool> {
        let state = self.get_summary_(module, fun)?;
        let mut has_secondary_index = false;
        state.accesses().iter_offsets(|offset| {
            if matches!(offset, Offset::Global(_)) {
                has_secondary_index = true
            }
        });
        Ok(has_secondary_index)
    }

    /// Returns an overapproximation of the `ResourceKey`'s in global storage that will be written
    /// by `module::fun` if called with arguments `signers`, `actuals`, `type_actuals` in state
    /// `blockchain_view`.
    pub fn get_keys_written(
        &self,
        module: &ModuleId,
        fun: &IdentStr,
        signers: &[AccountAddress],
        actuals: &[Vec<u8>],
        type_actuals: &[TypeTag],
        blockchain_view: &dyn MoveStorage,
    ) -> Result<Vec<ResourceKey>> {
        self.get_concretized_keys(
            module,
            fun,
            signers,
            actuals,
            type_actuals,
            blockchain_view,
            true,
        )
    }

    /// Returns an overapproximation of the `ResourceKey`'s in global storage that will be read by
    /// `module::fun` if called with arguments `signers`, `actuals`, `type_actuals` in state
    /// `blockchain_view`.
    pub fn get_keys_read(
        &self,
        module: &ModuleId,
        fun: &IdentStr,
        signers: &[AccountAddress],
        actuals: &[Vec<u8>],
        type_actuals: &[TypeTag],
        blockchain_view: &dyn MoveStorage,
    ) -> Result<Vec<ResourceKey>> {
        self.get_concretized_keys(
            module,
            fun,
            signers,
            actuals,
            type_actuals,
            blockchain_view,
            false,
        )
    }

    /// Returns an overapproximation of the `ResourceKey`'s in global storage that will be accesses
    /// by module::fun` if called with arguments `signers`, `actuals`, `type_actuals` in state
    /// `blockchain_view`.
    /// If `is_write` is true, only ResourceKey's written will be returned; otherwise, only
    /// ResourceKey's read will be returned.
    pub fn get_concretized_keys(
        &self,
        module: &ModuleId,
        fun: &IdentStr,
        signers: &[AccountAddress],
        actuals: &[Vec<u8>],
        type_actuals: &[TypeTag],
        blockchain_view: &dyn MoveStorage,
        is_write: bool,
    ) -> Result<Vec<ResourceKey>> {
        if let Some(state) = self.get_summary(module, fun) {
            let results = dynamic_analysis::concretize(
                state.accesses().clone(),
                signers,
                actuals,
                type_actuals,
                &self.get_function_env(module, fun).unwrap(),
                blockchain_view,
            )?;
            Ok(if is_write {
                results.get_keys_written(&self.env)
            } else {
                results.get_keys_read(&self.env)
            })
        } else {
            bail!("Couldn't resolve function {:?}::{:?}", module, fun)
        }
    }

    /// Returns the FunctionEnv for `module`::`fun`
    /// Returns `None` if this function does not exist
    pub fn get_function_env(&self, module: &ModuleId, fun: &IdentStr) -> Option<FunctionEnv> {
        self.env
            .find_function_by_language_storage_id_name(module, &fun.to_owned())
    }
}
