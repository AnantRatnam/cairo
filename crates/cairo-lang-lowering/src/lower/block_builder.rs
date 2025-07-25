use cairo_lang_debug::DebugWithDb;
use cairo_lang_defs::ids::{MemberId, NamedLanguageElementId};
use cairo_lang_diagnostics::Maybe;
use cairo_lang_semantic as semantic;
use cairo_lang_semantic::expr::fmt::ExprFormatter;
use cairo_lang_semantic::types::{peel_snapshots, wrap_in_snapshots};
use cairo_lang_semantic::usage::{MemberPath, Usage};
use cairo_lang_syntax::node::TypedStablePtr;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use cairo_lang_utils::{Intern, LookupIntern, require};
use itertools::{Itertools, chain, zip_eq};
use semantic::{ConcreteTypeId, ExprVarMemberPath, TypeLongId};

use super::context::{LoweredExpr, LoweringContext, LoweringFlowError, LoweringResult, VarRequest};
use super::generators;
use super::generators::StatementsBuilder;
use super::refs::{SemanticLoweringMapping, StructRecomposer, merge_semantics};
use crate::db::LoweringGroup;
use crate::diagnostic::{LoweringDiagnosticKind, LoweringDiagnosticsBuilder};
use crate::ids::LocationId;
use crate::lower::refs::ClosureInfo;
use crate::{Block, BlockEnd, BlockId, MatchInfo, Statement, VarRemapping, VarUsage, VariableId};

/// Block builder, describing its current state.
#[derive(Clone)]
pub struct BlockBuilder {
    /// A store for semantic variables, owning their OwnedVariable instances.
    pub semantics: SemanticLoweringMapping,
    /// The semantic variables that are captured as snapshots in this block.
    pub snapped_semantics: OrderedHashMap<MemberPath, VariableId>,
    /// The semantic variables that are added/changed in this block.
    changed_member_paths: OrderedHashSet<MemberPath>,
    /// Current sequence of lowered statements emitted.
    pub statements: StatementsBuilder,
    /// The block id to use for this block when it's finalized.
    pub block_id: BlockId,
}
impl BlockBuilder {
    /// Creates a new [BlockBuilder] for the root block of a function body.
    pub fn root(block_id: BlockId) -> Self {
        BlockBuilder {
            semantics: Default::default(),
            snapped_semantics: Default::default(),
            changed_member_paths: Default::default(),
            statements: Default::default(),
            block_id,
        }
    }

    /// Creates a [BlockBuilder] for a subscope.
    pub fn child_block_builder(&self, block_id: BlockId) -> BlockBuilder {
        BlockBuilder {
            semantics: self.semantics.clone(),
            snapped_semantics: self.snapped_semantics.clone(),
            changed_member_paths: Default::default(),
            statements: Default::default(),
            block_id,
        }
    }

    /// Creates a [BlockBuilder] for a sibling builder. This is used when an original block is split
    /// (e.g. after a match statement) to add the ability to 'goto' to after-the-block.
    pub fn sibling_block_builder(&self, block_id: BlockId) -> BlockBuilder {
        BlockBuilder {
            semantics: self.semantics.clone(),
            snapped_semantics: self.snapped_semantics.clone(),
            changed_member_paths: self.changed_member_paths.clone(),
            statements: Default::default(),
            block_id,
        }
    }

    /// Binds a semantic variable to a lowered variable.
    pub fn put_semantic(&mut self, semantic_var_id: semantic::VarId, var: VariableId) {
        self.semantics.introduce(MemberPath::Var(semantic_var_id), var);
        self.changed_member_paths.insert(MemberPath::Var(semantic_var_id));
    }

    pub fn update_ref(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        member_path: &ExprVarMemberPath,
        var: VariableId,
    ) {
        let location = ctx.get_location(member_path.stable_ptr().untyped());
        self.update_ref_raw(ctx, member_path.into(), var, location);
    }

    pub fn update_ref_raw(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        member_path: MemberPath,
        var: VariableId,
        location: LocationId,
    ) {
        // Invalidate snapshot to the given memberpath.
        self.snapped_semantics.swap_remove(&member_path);
        self.semantics.update(
            &mut BlockStructRecomposer { statements: &mut self.statements, ctx, location },
            &member_path,
            var,
        );
        self.changed_member_paths.insert(member_path);
    }

    pub fn get_ref(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        member_path: &ExprVarMemberPath,
    ) -> Option<VarUsage> {
        let location = ctx.get_location(member_path.stable_ptr().untyped());
        self.get_ref_raw(ctx, &member_path.into(), location)
    }

    pub fn get_ref_raw(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        member_path: &MemberPath,
        location: LocationId,
    ) -> Option<VarUsage> {
        self.semantics
            .get(
                BlockStructRecomposer { statements: &mut self.statements, ctx, location },
                member_path,
            )
            .map(|var_id| VarUsage { var_id, location })
    }

    /// Updates the reference of a semantic variable to a snapshot of its lowered variable.
    pub fn update_snap_ref(&mut self, member_path: &ExprVarMemberPath, var: VariableId) {
        self.snapped_semantics.insert(member_path.into(), var);
    }

    /// Gets the reference of a snapshot of semantic variable, possibly by deconstructing its
    /// parents.
    pub fn get_snap_ref(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        member_path: &ExprVarMemberPath,
    ) -> Option<VarUsage> {
        let location = ctx.get_location(member_path.stable_ptr().untyped());
        if let Some(var_id) = self.snapped_semantics.get::<MemberPath>(&member_path.into()) {
            return Some(VarUsage { var_id: *var_id, location });
        }
        let ExprVarMemberPath::Member { parent, member_id, concrete_struct_id, .. } = member_path
        else {
            return None;
        };
        // TODO(TomerStarkware): Consider adding the result to snap_semantics to avoid
        // recomputation.
        let parent_var = self.get_snap_ref(ctx, parent)?;
        let members = ctx.db.concrete_struct_members(*concrete_struct_id).ok()?;
        let (parent_number_of_snapshots, _) =
            peel_snapshots(ctx.db, ctx.variables[parent_var.var_id].ty);
        let member_idx = members.iter().position(|(_, member)| member.id == *member_id)?;
        Some(
            generators::StructMemberAccess {
                input: parent_var,
                member_tys: members
                    .iter()
                    .map(|(_, member)| {
                        wrap_in_snapshots(ctx.db, member.ty, parent_number_of_snapshots)
                    })
                    .collect(),
                member_idx,
                location,
            }
            .add(ctx, &mut self.statements),
        )
    }

    /// Adds a statement to the block.
    pub fn push_statement(&mut self, statement: Statement) {
        self.statements.push_statement(statement);
    }

    /// Ends a block with an unreachable match.
    pub fn unreachable_match(self, ctx: &mut LoweringContext<'_, '_>, match_info: MatchInfo) {
        self.finalize(ctx, BlockEnd::Match { info: match_info });
    }

    /// Ends a block with Panic.
    pub fn panic(self, ctx: &mut LoweringContext<'_, '_>, data: VarUsage) -> Maybe<()> {
        self.finalize(ctx, BlockEnd::Panic(data));
        Ok(())
    }

    /// Ends a block with Callsite.
    pub fn goto_callsite(self, expr: Option<VarUsage>) -> SealedBlockBuilder {
        SealedBlockBuilder::GotoCallsite(SealedGotoCallsite { builder: self, expr })
    }

    /// Ends a block with Return.
    pub fn ret(
        mut self,
        ctx: &mut LoweringContext<'_, '_>,
        expr: VarUsage,
        location: LocationId,
    ) -> Maybe<()> {
        let refs = ctx
            .signature
            .extra_rets
            .clone()
            .iter()
            .map(|member_path| self.get_ref(ctx, member_path))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                ctx.diagnostics.report_by_location(
                    location.lookup_intern(ctx.db),
                    LoweringDiagnosticKind::UnexpectedError,
                )
            })?;

        self.finalize(ctx, BlockEnd::Return(chain!(refs, [expr]).collect(), location));
        Ok(())
    }

    /// Ends a block with known ending information. Used by [SealedBlockBuilder].
    pub fn finalize(self, ctx: &mut LoweringContext<'_, '_>, end: BlockEnd) {
        let block = Block { statements: self.statements.statements, end };
        ctx.blocks.set_block(self.block_id, block);
    }

    /// Merges the sealed blocks and ends the block with a match-end.
    /// Replaces `self` with a sibling builder.
    pub fn merge_and_end_with_match(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        match_info: MatchInfo,
        sealed_blocks: Vec<SealedBlockBuilder>,
        location: LocationId,
    ) -> LoweringResult<LoweredExpr> {
        let Some((merged_expr, following_block)) = self.merge_sealed(ctx, sealed_blocks, location)
        else {
            return Err(LoweringFlowError::Match(match_info));
        };
        let new_scope = self.sibling_block_builder(following_block);
        let prev_scope = std::mem::replace(self, new_scope);
        prev_scope.finalize(ctx, BlockEnd::Match { info: match_info });
        Ok(merged_expr)
    }

    /// Captures the variable in a usage associated with a closure.
    ///
    /// Returns the variable usage of the closure.
    pub fn capture(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        usage: Usage,
        expr: &semantic::ExprClosure,
    ) -> (VarUsage, ClosureInfo) {
        let location = ctx.get_location(expr.stable_ptr.untyped());

        let inputs = chain!(
            usage.usage.values().map(|expr| { LoweredExpr::MemberPath(expr.clone(), location) }),
            usage.snap_usage.values().map(|expr| {
                LoweredExpr::Snapshot {
                    expr: Box::new(LoweredExpr::MemberPath(expr.clone(), location)),
                    location,
                }
            })
        )
        .map(|expr| expr.as_var_usage(ctx, self).unwrap())
        .collect_vec();

        let members: OrderedHashMap<MemberPath, semantic::TypeId> =
            chain!(usage.usage.values(), usage.changes.values())
                .map(|expr| (expr.into(), expr.ty()))
                .collect();

        let snapshots = usage
            .snap_usage
            .keys()
            .cloned()
            .zip(
                inputs
                    .iter()
                    .skip(members.len())
                    .map(|var_usage| ctx.variables.variables[var_usage.var_id].ty),
            )
            .collect();

        let TypeLongId::Closure(closure_id) = expr.ty.lookup_intern(ctx.db) else {
            unreachable!("Closure Expr should have a Closure type.");
        };

        // Assert that the closure type matches the input we pass to it.
        assert!(
            closure_id
                .captured_types
                .iter()
                .eq(inputs.iter().map(|var_usage| &ctx.variables[var_usage.var_id].ty))
        );

        let var_usage =
            generators::StructConstruct { inputs: inputs.clone(), ty: expr.ty, location }
                .add(ctx, &mut self.statements);

        (var_usage, ClosureInfo { members, snapshots })
    }

    /// Merges sibling sealed blocks.
    /// If there are reachable blocks, returns the converged expression of the blocks, usable at the
    /// calling builder, and the following block ID.
    /// Otherwise, returns None.
    fn merge_sealed(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        sealed_blocks: Vec<SealedBlockBuilder>,
        location: LocationId,
    ) -> Option<(LoweredExpr, BlockId)> {
        // TODO(spapini): When adding Gotos, include the callsite target in the required information
        // to merge.
        // TODO(spapini): Don't remap if we have a single reachable branch.

        let mut semantic_remapping = SemanticRemapping::default();
        let mut n_reachable_blocks = 0;

        // Remap Variables from all blocks.
        for sealed_block in &sealed_blocks {
            let SealedBlockBuilder::GotoCallsite(SealedGotoCallsite { builder: subscope, expr }) =
                sealed_block
            else {
                continue;
            };
            n_reachable_blocks += 1;
            if let Some(var_usage) = expr {
                semantic_remapping.expr.get_or_insert_with(|| {
                    let var = ctx.variables[var_usage.var_id].clone();
                    ctx.variables.variables.alloc(var)
                });
            }
            for member_path in subscope.changed_member_paths.iter() {
                let Some(containing_member_path) =
                    self.semantics.topmost_mapped_containing_member_path(member_path.clone())
                else {
                    // This variable is local to the subscope.
                    continue;
                };
                // Consider the topmost used(==mapped) member path that contains the changed member,
                // as it might be needed in the merge site.
                let mut queue = vec![containing_member_path];
                while let Some(v) = queue.pop() {
                    // If we reached the original member path, it needs to be restructured - so no
                    // need to continue recursively.
                    if v != *member_path {
                        // If it is scattered - add its members instead of itself, to avoid a
                        // possibly unnecessary restructuring.
                        if let Some(members) = self.semantics.get_scattered_members(&v) {
                            queue.extend(members);
                            continue;
                        }
                    }
                    // Actually adding to the remapping.
                    semantic_remapping.member_path_value.entry(v.clone()).or_insert_with(|| {
                        let ty = get_ty(ctx, &v);
                        ctx.new_var(VarRequest { ty, location })
                    });
                }
            }
        }

        // Prune parents from semantic_remapping.
        for member_path in semantic_remapping.member_path_value.keys().cloned().collect_vec() {
            let mut current = member_path.clone();
            while let MemberPath::Member { parent, .. } = current {
                if semantic_remapping.member_path_value.contains_key(&*parent) {
                    semantic_remapping.member_path_value.swap_remove(&member_path);
                }
                current = *parent;
            }
        }

        require(n_reachable_blocks != 0)?;

        // If there are reachable blocks, create a new empty block for the code after this match.
        let following_block = ctx.blocks.alloc_empty();

        for sealed_block in sealed_blocks {
            sealed_block.finalize(ctx, following_block, &semantic_remapping, location);
        }

        // Apply remapping on builder.
        for (semantic, var) in semantic_remapping.member_path_value {
            self.update_ref_raw(ctx, semantic, var, location);
        }

        let expr = match semantic_remapping.expr {
            Some(var_id) => LoweredExpr::AtVariable(VarUsage { var_id, location }),
            None => LoweredExpr::Tuple { exprs: vec![], location },
        };
        Some((expr, following_block))
    }

    /// Destructures a closure.
    /// Invalidates the closure variable and returns the captured variables.
    pub fn destructure_closure(
        &mut self,
        ctx: &mut LoweringContext<'_, '_>,
        location: LocationId,
        closure_var: VariableId,
        closure_info: &ClosureInfo,
    ) -> Vec<VariableId> {
        self.semantics.destructure_closure(
            &mut BlockStructRecomposer { statements: &mut self.statements, ctx, location },
            closure_var,
            closure_info,
        )
    }
}

impl<'a> DebugWithDb<ExprFormatter<'a>> for BlockBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>, db: &ExprFormatter<'a>) -> std::fmt::Result {
        writeln!(f, "block_id: {:?}", self.block_id)?;
        if !self.statements.statements.is_empty() {
            writeln!(f, "statements:")?;
            for statement in &self.statements.statements {
                writeln!(f, "  {statement:?}")?;
            }
        }
        writeln!(f, "semantics:")?;
        write!(f, "{}", indent::indent_all_with("  ", format!("{:?}", self.semantics.debug(db))))?;

        Ok(())
    }
}

/// Gets the type of a semantic variable.
fn get_ty(ctx: &LoweringContext<'_, '_>, member_path: &MemberPath) -> semantic::TypeId {
    match member_path {
        MemberPath::Var(var) => ctx.semantic_defs[var].ty(),
        MemberPath::Member { member_id, concrete_struct_id, .. } => {
            ctx.db.concrete_struct_members(*concrete_struct_id).unwrap()[&member_id.name(ctx.db)].ty
        }
    }
}

/// Remapping of lowered variables with more semantic information regarding what is the semantic
/// role of the lowered variables.
#[derive(Debug, Default)]
pub struct SemanticRemapping {
    expr: Option<VariableId>,
    member_path_value: OrderedHashMap<MemberPath, VariableId>,
}

/// Represents a sealed [BlockBuilder] that ends with returning (goto) to the callsite.
pub struct SealedGotoCallsite {
    /// The sealed builder.
    pub builder: BlockBuilder,
    /// The expression that is returned by the block to the callsite. Can be `None` if the block
    /// returns the unit type.
    pub expr: Option<VarUsage>,
}

/// A sealed [BlockBuilder], ready to be merged with sibling blocks to end the block.
#[allow(clippy::large_enum_variant)]
pub enum SealedBlockBuilder {
    /// Block should end by goto callsite.
    GotoCallsite(SealedGotoCallsite),
    /// Block end is already known.
    Ends(BlockId),
}
impl SealedBlockBuilder {
    /// Finalizes a non-finalized block, given the semantic remapping of variables and the target
    /// block to jump to.
    fn finalize(
        self,
        ctx: &mut LoweringContext<'_, '_>,
        target: BlockId,
        semantic_remapping: &SemanticRemapping,
        location: LocationId,
    ) {
        if let SealedBlockBuilder::GotoCallsite(SealedGotoCallsite { mut builder, expr }) = self {
            let mut remapping = VarRemapping::default();
            // Since SemanticRemapping should have unique variable ids, these asserts will pass.
            for (semantic, remapped_var) in semantic_remapping.member_path_value.iter() {
                assert!(
                    remapping
                        .insert(
                            *remapped_var,
                            builder.get_ref_raw(ctx, semantic, location).unwrap()
                        )
                        .is_none()
                );
            }
            if let Some(remapped_var) = semantic_remapping.expr {
                let var_usage = expr.unwrap_or_else(|| {
                    LoweredExpr::Tuple {
                        exprs: vec![],
                        location: ctx.variables[remapped_var].location,
                    }
                    .as_var_usage(ctx, &mut builder)
                    .unwrap()
                });
                assert!(remapping.insert(remapped_var, var_usage).is_none());
            }

            builder.finalize(ctx, BlockEnd::Goto(target, remapping));
        }
    }
}

struct BlockStructRecomposer<'a, 'b, 'c> {
    statements: &'a mut StatementsBuilder,
    ctx: &'a mut LoweringContext<'b, 'c>,
    location: LocationId,
}
impl StructRecomposer for BlockStructRecomposer<'_, '_, '_> {
    fn deconstruct(
        &mut self,
        concrete_struct_id: semantic::ConcreteStructId,
        value: VariableId,
    ) -> OrderedHashMap<MemberId, VariableId> {
        let members = self.ctx.db.concrete_struct_members(concrete_struct_id).unwrap();
        let members = members.values().collect_vec();
        let member_ids = members.iter().map(|m| m.id);

        let member_values =
            self.deconstruct_by_types(value, members.iter().map(|member| member.ty));
        OrderedHashMap::from_iter(zip_eq(member_ids, member_values))
    }

    fn deconstruct_by_types(
        &mut self,
        value: VariableId,
        types: impl Iterator<Item = semantic::TypeId>,
    ) -> Vec<VariableId> {
        // We use the location of the variable being deconstructed for the members
        // to get a better location for variable not dropped errors.
        let location = self.ctx.variables[value].location;
        let var_reqs = types.map(|ty| VarRequest { ty, location }).collect_vec();

        generators::StructDestructure {
            input: VarUsage { var_id: value, location: self.location },
            var_reqs,
        }
        .add(self.ctx, self.statements)
    }

    fn reconstruct(
        &mut self,
        concrete_struct_id: semantic::ConcreteStructId,
        members: Vec<VariableId>,
    ) -> VariableId {
        let ty =
            TypeLongId::Concrete(ConcreteTypeId::Struct(concrete_struct_id)).intern(self.ctx.db);
        // TODO(ilya): Is using the `self.location` correct here?
        generators::StructConstruct {
            inputs: members
                .into_iter()
                .map(|var_id| VarUsage { var_id, location: self.location })
                .collect_vec(),
            ty,
            location: self.location,
        }
        .add(self.ctx, self.statements)
        .var_id
    }

    fn var_ty(&self, var: VariableId) -> semantic::TypeId {
        self.ctx.variables[var].ty
    }

    fn db(&self) -> &dyn LoweringGroup {
        self.ctx.db
    }
}

/// Given a list of block builders, creates a new single block builder and finalizes all
/// the block builders with a [BlockEnd::Goto] to the new block.
///
/// The mapping from semantic variables to lowered variables in the new block follows these rules:
///
/// * Variables mapped to the same lowered variable across all input blocks are kept as-is.
/// * Local variables that appear in only a subset of the blocks are removed.
/// * Variables with different mappings across blocks are remapped to a new lowered variable.
///
/// If only one parent builder is given, returns it without creating a new block.
// TODO(lior): Remove `allow(dead_code)` once the function is used.
#[allow(dead_code)]
pub fn merge_block_builders(
    ctx: &mut LoweringContext<'_, '_>,
    parent_builders: Vec<BlockBuilder>,
    location: LocationId,
) -> BlockBuilder {
    // If there is only one parent builder, return it.
    if parent_builders.len() == 1 {
        return parent_builders.into_iter().next().unwrap();
    }

    // TODO(lior): Support snapped semantics.
    for builder in &parent_builders {
        assert!(builder.snapped_semantics.is_empty(), "Snapped semantics is not supported yet.");
    }

    // Compute the merged semantics.

    // A map from [MemberPath] that requires a new lowered variable (due to remapping) to the
    // corresponding lowered variable.
    let mut member_path_value = OrderedHashMap::<MemberPath, VariableId>::default();

    let semantics =
        merge_semantics(parent_builders.iter().map(|builder| &builder.semantics), &mut |path| {
            let ty = get_ty(ctx, path);
            let var = ctx.new_var(VarRequest { ty, location });
            member_path_value.insert(path.clone(), var);
            var
        });

    let semantic_remapping = SemanticRemapping { expr: None, member_path_value };

    // Assign a new block id for the current node.
    let block_id = ctx.blocks.alloc_empty();

    // Finalize the intermediate blocks.
    for parent_builder in parent_builders.into_iter() {
        // TODO(lior): Consider extracting the relevant code from `finalize` to a separate
        //   `finalize` function.
        let sealed_block = parent_builder.goto_callsite(None);
        sealed_block.finalize(ctx, block_id, &semantic_remapping, location);
    }

    // Create a new [BlockBuilder] with the merged `semantics`.
    BlockBuilder {
        semantics,
        snapped_semantics: Default::default(),
        changed_member_paths: Default::default(),
        statements: Default::default(),
        block_id,
    }
}
