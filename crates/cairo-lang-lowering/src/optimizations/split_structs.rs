#[cfg(test)]
#[path = "split_structs_test.rs"]
mod test;

use std::vec;

use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use id_arena::Arena;
use itertools::{zip_eq, Itertools};

use crate::ids::LocationId;
use crate::utils::{Rebuilder, RebuilderEx};
use crate::{
    BlockId, FlatBlockEnd, FlatLowered, Statement, StatementStructConstruct,
    StatementStructDestructure, VarRemapping, VarUsage, Variable, VariableId,
};

/// Splits all the variables that were created by struct_construct and
/// reintroduces the struct_construct statement when needed.
/// Note that if a member is used after the struct then is means that that struct is duplicatable.

pub fn split_structs(lowered: &mut FlatLowered) {
    if lowered.blocks.is_empty() {
        return;
    }

    let split = get_var_split(lowered);
    rebuild_blocks(lowered, split);
}

/// Information about a split variable.
struct SplitInfo {
    /// The block_id where the variable was defined.
    block_id: BlockId,
    /// The variables resulting from the split.
    vars: Vec<VariableId>,
}

type SplitMapping = UnorderedHashMap<VariableId, SplitInfo>;

/// Keeps track of the variables that were reconstructed.
/// If the value is None the variable was reconstructed at the first usage.
/// If the value is Some(Block_id) then the variable needs to be reconstructed at the end of
/// `block_id`
type ReconstructionMapping = OrderedHashMap<VariableId, Option<BlockId>>;

/// Returns a mapping from variables that should be split to the variables resulting from the split.
fn get_var_split(lowered: &mut FlatLowered) -> SplitMapping {
    let mut split = UnorderedHashMap::<VariableId, SplitInfo>::default();

    let mut stack = vec![BlockId::root()];
    let mut visited = vec![false; lowered.blocks.len()];
    while let Some(block_id) = stack.pop() {
        if visited[block_id.0] {
            continue;
        }
        visited[block_id.0] = true;

        let block = &lowered.blocks[block_id];

        for stmt in block.statements.iter() {
            if let Statement::StructConstruct(stmt) = stmt {
                assert!(
                    split
                        .insert(
                            stmt.output,
                            SplitInfo {
                                block_id,
                                vars: stmt.inputs.iter().map(|input| input.var_id).collect_vec(),
                            },
                        )
                        .is_none()
                );
            }
        }

        match &block.end {
            FlatBlockEnd::Goto(block_id, remappings) => {
                stack.push(*block_id);

                for (dst, src) in remappings.iter() {
                    split_remapping(
                        *block_id,
                        &mut split,
                        &mut lowered.variables,
                        *dst,
                        src.var_id,
                    );
                }
            }
            FlatBlockEnd::Match { info } => {
                stack.extend(info.arms().iter().map(|arm| arm.block_id));
            }
            FlatBlockEnd::Return(_) => {}
            FlatBlockEnd::Panic(_) | FlatBlockEnd::NotSet => unreachable!(),
        }
    }

    split
}

/// Splits 'dst' according to the split of 'src'.
///
/// For example if if we have
///     split('dst') is None
///     split('src') = (v0, v1) and split(`v1`) = (v3, v4, v5).
/// The function will create new variables and set:
///     split('dst') = (v100, v101) and split(`v101`) = (v102, v103, v104).
fn split_remapping(
    target_block_id: BlockId,
    split: &mut SplitMapping,
    variables: &mut Arena<Variable>,
    dst: VariableId,
    src: VariableId,
) {
    let mut stack = vec![(dst, src)];

    while let Some((dst, src)) = stack.pop() {
        if split.contains_key(&dst) {
            continue;
        }
        if let Some(SplitInfo { block_id: _, vars: src_vars }) = split.get(&src) {
            let mut dst_vars = vec![];
            for split_src in src_vars {
                let new_var = variables.alloc(variables[*split_src].clone());
                // queue inner remmapping for for possible splitting.
                stack.push((new_var, *split_src));
                dst_vars.push(new_var);
            }

            split.insert(dst, SplitInfo { block_id: target_block_id, vars: dst_vars });
        }
    }
}

// Context for rebuilding the blocks.
struct SplitStructsContext<'a> {
    /// The variables that were reconstructed as they were needed.
    reconstructed: ReconstructionMapping,
    // A renamer that keeps track of renamed vars.
    var_remapper: VarRename,
    // The variables arena.
    variables: &'a mut Arena<Variable>,
}

/// Rebuilds the blocks, with the splitting.
fn rebuild_blocks(lowered: &mut FlatLowered, split: SplitMapping) {
    let mut ctx = SplitStructsContext {
        reconstructed: Default::default(),
        var_remapper: VarRename::default(),
        variables: &mut lowered.variables,
    };

    let mut stack = vec![BlockId::root()];
    let mut visited = vec![false; lowered.blocks.len()];
    while let Some(block_id) = stack.pop() {
        if visited[block_id.0] {
            continue;
        }
        visited[block_id.0] = true;

        let block = &mut lowered.blocks[block_id];
        let old_statements = std::mem::take(&mut block.statements);
        let statements = &mut block.statements;

        for stmt in old_statements.into_iter() {
            match stmt {
                Statement::StructDestructure(stmt) => {
                    if let Some(output_split) =
                        split.get(&ctx.var_remapper.map_var_id(stmt.input.var_id))
                    {
                        for (output, new_var) in
                            zip_eq(stmt.outputs.iter(), output_split.vars.to_vec())
                        {
                            assert!(
                                ctx.var_remapper.renamed_vars.insert(*output, new_var).is_none()
                            )
                        }
                    } else {
                        statements.push(Statement::StructDestructure(stmt));
                    }
                }
                Statement::StructConstruct(stmt)
                    if split.contains_key(&ctx.var_remapper.map_var_id(stmt.output)) =>
                {
                    // Remove StructConstruct statement.
                }
                _ => {
                    for input in stmt.inputs() {
                        ctx.check_if_var_needs_reconstruction(
                            &split,
                            input.var_id,
                            block_id,
                            statements,
                            input.location,
                        );
                    }

                    statements.push(stmt);
                }
            }
        }

        match &mut block.end {
            FlatBlockEnd::Goto(target_block_id, remappings) => {
                stack.push(*target_block_id);

                let mut old_remappings = std::mem::take(remappings);

                ctx.rebuild_remapping(
                    &split,
                    block_id,
                    &mut block.statements,
                    std::mem::take(&mut old_remappings.remapping).into_iter(),
                    remappings,
                );
            }
            FlatBlockEnd::Match { info } => {
                stack.extend(info.arms().iter().map(|arm| arm.block_id));

                for input in info.inputs() {
                    ctx.check_if_var_needs_reconstruction(
                        &split,
                        input.var_id,
                        block_id,
                        statements,
                        input.location,
                    );
                }
            }
            FlatBlockEnd::Return(vars) => {
                for var in vars {
                    ctx.check_if_var_needs_reconstruction(
                        &split,
                        var.var_id,
                        block_id,
                        statements,
                        var.location,
                    );
                }
            }
            FlatBlockEnd::Panic(_) | FlatBlockEnd::NotSet => unreachable!(),
        }

        // Remap block variables.
        *block = ctx.var_remapper.rebuild_block(block);
    }

    // Add all the end of block reconstructions.
    for (var_id, opt_block_id) in ctx.reconstructed.iter() {
        if let Some(block_id) = opt_block_id {
            let split_vars =
                split.get(var_id).expect("Should be check in `check_if_var_needs_reconstruction`.");
            lowered.blocks[*block_id].statements.push(Statement::StructConstruct(
                StatementStructConstruct {
                    inputs: split_vars
                        .vars
                        .iter()
                        .map(|var_id| VarUsage {
                            var_id: ctx.var_remapper.map_var_id(*var_id),
                            location: ctx.variables[*var_id].location,
                        })
                        .collect_vec(),
                    output: *var_id,
                },
            ));
        }
    }
}

impl SplitStructsContext<'_> {
    /// Given 'var_id' check if `var_remapper.map_var_id(*var_id)` was split and not reconstructed
    /// yet, if this is the case, marks the variable for reconstruction.
    fn check_if_var_needs_reconstruction(
        &mut self,
        split: &SplitMapping,
        var_id: VariableId,
        block_id: BlockId,
        statements: &mut Vec<Statement>,
        location: LocationId,
    ) {
        let var_id = self.var_remapper.map_var_id(var_id);
        if self.reconstructed.contains_key(&var_id) {
            return;
        }

        let Some(split_info) = split.get(&var_id) else {
            return;
        };

        for var in &split_info.vars {
            self.check_if_var_needs_reconstruction(split, *var, block_id, statements, location);
        }

        // If the variable was defined in the same block then we can reconstruct it before the first
        // usage. If not we need to reconstruct it at the end of the of the original block as it
        // might be used by more then one of the children.
        let opt_block_id = if block_id == split_info.block_id {
            statements.push(Statement::StructConstruct(StatementStructConstruct {
                inputs: split_info
                    .vars
                    .iter()
                    .map(|var_id| VarUsage {
                        var_id: self.var_remapper.map_var_id(*var_id),
                        location,
                    })
                    .collect_vec(),
                output: var_id,
            }));

            None
        } else {
            Some(split_info.block_id)
        };

        self.reconstructed.insert(var_id, opt_block_id);
    }

    /// Given an iterator over the original remapping, rebuilds the remapping with the given
    /// splitting of variables.
    fn rebuild_remapping(
        &mut self,
        split: &SplitMapping,
        block_id: BlockId,
        statements: &mut Vec<Statement>,
        remappings: impl DoubleEndedIterator<Item = (VariableId, VarUsage)>,
        new_remappings: &mut VarRemapping,
    ) {
        let mut stack = remappings.rev().collect_vec();
        while let Some((orig_dst, orig_src)) = stack.pop() {
            let dst = self.var_remapper.map_var_id(orig_dst);
            let src = self.var_remapper.map_var_id(orig_src.var_id);
            match (split.get(&dst), split.get(&src)) {
                (None, None) => {
                    new_remappings
                        .insert(dst, VarUsage { var_id: src, location: orig_src.location });
                }
                (Some(dst_split), Some(src_split)) => {
                    stack.extend(zip_eq(
                        dst_split.vars.iter().cloned().rev(),
                        src_split
                            .vars
                            .iter()
                            .map(|var_id| VarUsage { var_id: *var_id, location: orig_src.location })
                            .rev(),
                    ));
                }
                (Some(dst_split), None) => {
                    let mut src_vars = vec![];

                    for dst in &dst_split.vars {
                        src_vars.push(self.variables.alloc(self.variables[*dst].clone()));
                    }

                    statements.push(Statement::StructDestructure(StatementStructDestructure {
                        input: VarUsage { var_id: src, location: orig_src.location },
                        outputs: src_vars.clone(),
                    }));

                    stack.extend(zip_eq(
                        dst_split.vars.iter().cloned().rev(),
                        src_vars
                            .into_iter()
                            .map(|var_id| VarUsage { var_id, location: orig_src.location })
                            .rev(),
                    ));
                }
                (None, Some(_src_vars)) => {
                    self.check_if_var_needs_reconstruction(
                        split,
                        src,
                        block_id,
                        statements,
                        orig_src.location,
                    );
                    new_remappings
                        .insert(dst, VarUsage { var_id: src, location: orig_src.location });
                }
            }
        }
    }
}

#[derive(Default)]
pub struct VarRename {
    renamed_vars: UnorderedHashMap<VariableId, VariableId>,
}

impl Rebuilder for VarRename {
    fn map_var_id(&mut self, var: VariableId) -> VariableId {
        let Some(mut new_var_id) = self.renamed_vars.get(&var).cloned() else {
            return var;
        };
        while let Some(new_id) = self.renamed_vars.get(&new_var_id) {
            assert_ne!(new_var_id, *new_id);
            new_var_id = *new_id;
        }

        self.renamed_vars.insert(var, new_var_id);
        new_var_id
    }

    fn map_block_id(&mut self, block: BlockId) -> BlockId {
        block
    }
}
