use super::{InstrResult, Instrumenter};
use crate::debug::dwarf::R;
use crate::debug::instrument::InstrError;
use crate::debug::{get_location, get_variables};
use crate::types::{CodeOffset, DebugFunction, WasmLocation};
use std::collections::{BTreeSet, HashMap, HashSet};
use wasm_encoder::reencode::Reencode;
use wasm_encoder::{Instruction, MemArg, reencode};
use wasmparser::ValType;

macro_rules! error {
    ($($arg:tt)*) => {
        Err($crate::debug::instrument::InstrError::UserError(
            $crate::debug::instrument::Error::msg(format!($($arg)*)),
        ))
    };
}

#[derive(Default)]
struct WasmLocations {
    operands: BTreeSet<usize>,
    locals: BTreeSet<usize>,
    globals: BTreeSet<usize>,
}

impl WasmLocations {
    fn operation(&mut self, op: &gimli::Operation<R>) {
        match *op {
            gimli::Operation::WasmLocal { index } => self.locals.insert(index as usize),
            gimli::Operation::WasmGlobal { index } => self.globals.insert(index as usize),
            gimli::Operation::WasmStack { index } => self.operands.insert(index as usize),
            _ => true,
        };
    }
}

pub struct FnInstrumenter<'a, 'b> {
    instr: &'a mut Instrumenter,
    func_idx: usize,
    func_body: wasmparser::FunctionBody<'b>,

    /// A [wasmparser] validator used to track the types of locals and operands
    /// throughout the instrumentation of this function
    validator: wasmparser::FuncValidator<wasmparser::ValidatorResources>,

    /// A vector of instructions forming the body of the instrumented function
    instructions: Vec<Instruction<'b>>,

    /// During instrumentation, the size of the function's debug frame is not
    /// yet known. This vector stores indexes into [FnInstrumenter::instructions]
    /// representing `i32.const` instructions which should eventually contain
    /// `i32.const F`, where `F` is the eventual size of this function's stack frame.
    /// Before emitting the instrumented function body, such instructions will be
    /// replaced with the correct frame size.
    stack_intructions: Vec<usize>,

    /// A vector of the types of scratch locals which will be used to store
    /// operand stack values when peeling off the operand stack to recover
    /// a value.
    ///
    /// In the instrumented function, these will follow the function's
    /// own locals. If `N` is the number of original locals
    /// (parameters + additional locals), then each of these will have local
    /// indices `N, N+1, ...`.
    scratch_locals: Vec<wasmparser::ValType>,
    // When present, scratch local 0 retains the caller's debug-stack pointer.
    unwind_block: Option<wasm_encoder::BlockType>,
}

impl<'a, 'b> FnInstrumenter<'a, 'b> {
    pub fn new(
        instr: &'a mut Instrumenter,
        func_idx: usize,
        func_body: wasmparser::FunctionBody<'b>,
    ) -> InstrResult<Self> {
        let mut validator = instr
            .validator
            .code_section_entry(&func_body)
            .map_err(reencode::Error::from)?
            .into_validator(Default::default());

        validator.read_locals(&mut func_body.get_binary_reader())?;

        let has_tags = instr
            .validator
            .types(0)
            .is_some_and(|types| types.tag_count() != 0);
        let unwind_block = if has_tags {
            let Some(wasmparser::Frame {
                block_type: wasmparser::BlockType::FuncType(index),
                ..
            }) = validator.get_control_frame(0)
            else {
                return error!("Missing function type for exception cleanup");
            };
            Some(*instr.unwind_blocks.get(index).ok_or_else(|| {
                InstrError::UserError(anyhow::anyhow!("Missing exception cleanup block type"))
            })?)
        } else {
            None
        };

        Ok(Self {
            instr,
            func_idx,
            func_body,
            validator,

            instructions: Vec::default(),
            stack_intructions: Vec::default(),
            scratch_locals: if unwind_block.is_some() {
                vec![ValType::I32]
            } else {
                Vec::new()
            },
            unwind_block,
        })
    }

    fn func(&self) -> &DebugFunction {
        &self.instr.functions[self.func_idx]
    }

    fn func_mut(&mut self) -> &mut DebugFunction {
        &mut self.instr.functions[self.func_idx]
    }

    fn emit_op(&mut self, op: wasmparser::Operator<'b>) -> InstrResult {
        self.instructions.push(self.instr.instruction(op)?);
        Ok(())
    }

    fn emit_header(&mut self) {
        let frame_size = self.func_mut().size;
        self.instructions
            .push(Instruction::GlobalGet(self.instr.sp_gl_index));
        if self.unwind_block.is_some() {
            self.instructions
                .push(Instruction::LocalTee(self.validator.len_locals()));
        }
        self.stack_intructions.push(self.instructions.len());
        self.instructions.extend([
            Instruction::I32Const(frame_size as i32),
            Instruction::I32Sub,
            Instruction::GlobalSet(self.instr.sp_gl_index),
        ]);
        if let Some(block) = self.unwind_block {
            self.instructions.push(Instruction::Try(block));
        }
    }

    fn emit_footer(&mut self) {
        if self.unwind_block.is_some() {
            self.instructions.extend([
                Instruction::LocalGet(self.validator.len_locals()),
                Instruction::GlobalSet(self.instr.sp_gl_index),
            ]);
            return;
        }
        let instr_count = self.instructions.len();
        let frame_size = self.func_mut().size;
        self.instructions.extend([
            Instruction::GlobalGet(self.instr.sp_gl_index),
            Instruction::I32Const(frame_size as i32),
            Instruction::I32Add,
            Instruction::GlobalSet(self.instr.sp_gl_index),
        ]);
        self.stack_intructions.push(instr_count + 1);
    }

    fn emit_call(&mut self, pc: CodeOffset, op: wasmparser::Operator<'b>) -> InstrResult {
        self.instructions.extend([
            Instruction::GlobalGet(self.instr.sp_gl_index),
            Instruction::I32Const(u32::from(pc) as i32),
        ]);
        self.emit_store(ValType::I32, 0);
        self.emit_op(op)?;
        Ok(())
    }

    fn locations_at(&self, pc: CodeOffset) -> InstrResult<WasmLocations> {
        let fun = self
            .func()
            .die_ref
            .deref(&self.instr.info.dwarf)
            .map_err(|e| InstrError::UserError(e))?;

        let vars = get_variables(&fun, pc);
        let mut locs = WasmLocations::default();
        let mut fbreg = fun.expression(gimli::DW_AT_frame_base, pc);

        for var in vars {
            let Some(expr) = get_location(&var, pc) else {
                continue;
            };

            for op in expr.operations(fun.ctx().unit.encoding()) {
                let op = op.map_err(|e| InstrError::UserError(e.into()))?;
                locs.operation(&op);

                /* Add frame base locations for FrameOffset */
                if matches!(op, gimli::Operation::FrameOffset { .. })
                    && let Some(expr) = fbreg
                {
                    for op in expr.operations(fun.ctx().unit.encoding()) {
                        let op = op.map_err(|e| InstrError::UserError(e.into()))?;
                        locs.operation(&op);
                    }

                    /* Don't need to handle frame base locations more than once */
                    fbreg = None;
                }
            }
        }

        Ok(locs)
    }

    fn emit_bkpt(&mut self, loc_idx: usize, pc: CodeOffset) -> InstrResult {
        // High-level goal:
        // Loop through all variables of the function.
        // For every variable with an active location at this point in the
        // program, insert instrumentation code to store the WASM internals
        // needed to derive the variable's value at engine onto the debug
        // stack frame of this function.
        //
        // All instrumentation code must have no observable side effects.
        // In particular, all values of locals must be preserved and the
        // state of the operand stack must be preserved.
        let locs = self.locations_at(pc)?;

        self.emit_operands(&locs)?;
        self.emit_locals(&locs)?;
        self.emit_globals(&locs)?;

        self.instructions
            .push(Instruction::I32Const(loc_idx as i32));
        self.instructions
            .push(Instruction::Call(self.instr.bkpt_fn_index));
        Ok(())
    }

    /// Emits instrumentation code to recover the operands in [WasmLocations::operands].
    ///
    /// The basic strategy is:
    ///     - Pop all values off of the operand stack, storing each in a scratch local variable
    ///     - If the popped value is one of the values we care about, then store it into the debug stack.
    ///     - Push all values stoerd in scratch locals back onto the operand stack.
    ///
    /// Note that this approach will be underperformant for large stack sizes.
    /// In the future, an optimization we can perform would be to recover an operand
    /// preemptively as soon as it is pushed by recognizing in advance that its value will
    /// be needed at a later point in the program. This would avoid the need to unroll the
    /// stack in this manner.
    fn emit_operands(&mut self, locs: &WasmLocations) -> InstrResult {
        let Some(&first) = locs.operands.first() else {
            return Ok(());
        };
        let Some(&last) = locs.operands.last() else {
            return Ok(());
        };

        let height = self.validator.operand_stack_height() as usize;

        // If the indices of the operands that we need exceed the number of operands we have
        // available, then it will be impossible to recover an operand value
        if first >= height || last >= height {
            // TODO: How to handle this case? Sometimes, for example in external/linked libraries,
            // we try to instrument operand values and can't becuase the validator says they don't exist.
            // Could either just skip, as we do here, or avoid instrumenting external code altogether.
            return Ok(());
        }

        let nlocals = self.validator.len_locals() as usize;

        // `scratch_indices` contains the indices in `self.scratch_locals`
        // which have been consumed to store operands while unrolling the operand stack.
        //
        // `scratch_map` maps operand stack indices to their corresponding index in `self.scratch_locals`
        // to be used when returning values to the operand stack.
        let mut scratch_indices = HashSet::new();
        let mut scratch_map = HashMap::new();

        // Loop through operands, starting from the top of the operand stack
        // and ending with the bottom-most operand that we care about
        for operand_idx in (first..height).rev() {
            // Get the type of this operand using
            let Some(Some(ty)) = self.validator.get_operand_type(height - operand_idx - 1) else {
                return error!(
                    "Couldn't instrument operand {:?}, unknown operand type",
                    operand_idx
                );
            };

            // Get or allocate a scratch local to store this stack operand
            let scratch_idx = self
                .scratch_locals
                .iter()
                .enumerate()
                .position(|(scratch_idx, &scratch_ty)| {
                    !(self.unwind_block.is_some() && scratch_idx == 0)
                        && !scratch_indices.contains(&scratch_idx)
                        && scratch_ty == ty
                })
                .unwrap_or_else(|| {
                    let scratch_idx = self.scratch_locals.len();
                    self.scratch_locals.push(ty);
                    scratch_idx
                });

            scratch_indices.insert(scratch_idx);
            scratch_map.insert(operand_idx, scratch_idx);

            let scratch_idx = (scratch_idx + nlocals) as u32;

            // Consume the operand at index `operand_idx` by pushing it to the scratch local
            self.instructions.push(Instruction::LocalSet(scratch_idx));

            // Store the operand value to the debug stack
            if locs.operands.contains(&operand_idx) {
                let offset = self.func_mut().place(WasmLocation::Operand(operand_idx));
                self.instructions
                    .push(Instruction::GlobalGet(self.instr.sp_gl_index));
                self.instructions.push(Instruction::LocalGet(scratch_idx));
                self.emit_store(ty, offset);
            }
        }

        // Loop through operands, starting with the bottom-most operand that we care about
        // and ending with the original top value in the stack
        for operand_idx in first..height {
            let Some(&scratch_idx) = scratch_map.get(&operand_idx) else {
                return error!(
                    "Could not recover operand {:?}: no corresponding scratch local",
                    operand_idx
                );
            };

            let scratch_idx = (scratch_idx + nlocals) as u32;
            self.instructions.push(Instruction::LocalGet(scratch_idx));
        }

        Ok(())
    }

    fn emit_locals(&mut self, locs: &WasmLocations) -> InstrResult {
        for &local_idx in &locs.locals {
            let Some(ty) = self.validator.get_local_type(local_idx as u32) else {
                return error!("Couldn't get type of local {:?}", local_idx);
            };

            let offset = self.func_mut().place(WasmLocation::Local(local_idx));
            self.instructions
                .push(Instruction::GlobalGet(self.instr.sp_gl_index));
            self.instructions
                .push(Instruction::LocalGet(local_idx as u32));

            self.emit_store(ty, offset);
        }

        Ok(())
    }

    fn emit_globals(&mut self, locs: &WasmLocations) -> InstrResult {
        let Some(types) = self.instr.validator.types(0) else {
            return error!("Could not get module types");
        };

        let globals: Vec<(usize, wasmparser::ValType)> = locs
            .globals
            .iter()
            .map(|&idx| (idx, types.global_at(idx as u32).content_type))
            .collect();

        for (global_idx, ty) in globals {
            let offset = self.func_mut().place(WasmLocation::Global(global_idx));

            self.instructions
                .push(Instruction::GlobalGet(self.instr.sp_gl_index));
            self.instructions.push(Instruction::GlobalGet(
                self.instr.global_index(global_idx as u32)?,
            ));
            self.emit_store(ty, offset);
        }

        Ok(())
    }

    fn emit_store(&mut self, ty: ValType, offset: usize) {
        let mem = MemArg {
            offset: offset as u64,
            align: 2,
            memory_index: self.instr.stack_mem_index,
        };
        self.instructions.push(match ty {
            ValType::I32 => Instruction::I32Store(mem),
            ValType::F32 => Instruction::F32Store(mem),
            ValType::I64 => Instruction::I64Store(mem),
            ValType::F64 => Instruction::F64Store(mem),
            ValType::V128 => Instruction::V128Store(mem),
            ValType::Ref(_) => unreachable!(),
        });
    }

    pub fn instrument(mut self) -> InstrResult<wasm_encoder::Function> {
        // Clear the stack frame for this function
        // This is a safety check to ensure we always start instrumentation at a known state.
        self.func_mut().reset();
        self.emit_header();

        let mut reader = self
            .func_body
            .get_operators_reader()
            .map_err(reencode::Error::from)?;

        while !reader.eof() {
            let (op, binary_ofs) = reader.read_with_offset().map_err(reencode::Error::from)?;
            let pc = self.instr.code_ofs(binary_ofs);

            if let Some(&location) = self.instr.breakpoints.get(&pc) {
                let loc_index = self.instr.next_location(location);
                self.emit_bkpt(loc_index, pc)?;
            }

            // Pass this operator to the wasmparser validator. It will internally
            // update its state to keep track of operand stack types, depth, etc.
            // according to the instruction given.
            //
            // It is important that this is run *after* doing instrumentation code for
            // this instruction's breakpoint, if any, because breakpoints should stop the
            // code immediately before the instruction has run. In other words, the
            // instrumentation code should not have observed the effects of that instruction yet.
            self.validator.op(binary_ofs, &op)?;

            match op {
                wasmparser::Operator::Call { .. } => {
                    self.emit_call(pc, op)?;
                }
                wasmparser::Operator::Return => {
                    self.emit_footer();
                    self.emit_op(wasmparser::Operator::Return)?;
                }
                wasmparser::Operator::ReturnCall { function_index } => {
                    self.emit_call(pc, wasmparser::Operator::Call { function_index })?;
                    self.emit_footer();
                    self.emit_op(wasmparser::Operator::Return)?;
                }
                wasmparser::Operator::ReturnCallIndirect {
                    type_index,
                    table_index,
                } => {
                    self.emit_call(
                        pc,
                        wasmparser::Operator::CallIndirect {
                            type_index,
                            table_index,
                        },
                    )?;
                    self.emit_footer();
                    self.emit_op(wasmparser::Operator::Return)?;
                }
                wasmparser::Operator::ReturnCallRef { type_index } => {
                    self.emit_call(pc, wasmparser::Operator::CallRef { type_index })?;
                    self.emit_footer();
                    self.emit_op(wasmparser::Operator::Return)?;
                }
                wasmparser::Operator::End => {
                    if reader.eof() {
                        if self.unwind_block.is_some() {
                            // Unwinding skips ordinary returns; discard this debug frame too.
                            self.instructions.push(Instruction::CatchAll);
                            self.emit_footer();
                            self.instructions.push(Instruction::Rethrow(0));
                            self.instructions.push(Instruction::End);
                        }
                        self.emit_footer();
                    }
                    self.emit_op(op)?;
                }
                _ => {
                    self.emit_op(op)?;
                }
            }
        }

        reader.finish()?;

        /* Adjust stack instructions to include stack size */
        let frame_size = self.func_mut().size;
        for instr_index in self.stack_intructions {
            let inst = &mut self.instructions[instr_index];
            assert!(matches!(*inst, Instruction::I32Const(_)));
            *inst = Instruction::I32Const(frame_size as i32);
        }

        /* Compute locals of emitted function (old locals + scratch locals) */
        let mut locals: Vec<(u32, wasm_encoder::ValType)> = Vec::new();
        for pair in self
            .func_body
            .get_locals_reader()
            .map_err(reencode::Error::from)?
        {
            let (cnt, ty) = pair.map_err(reencode::Error::from)?;
            locals.push((cnt, self.instr.val_type(ty)?));
        }
        for &ty in &self.scratch_locals {
            locals.push((1, self.instr.val_type(ty)?));
        }

        /* Emit the new function with new instructions and return */
        let mut func = wasm_encoder::Function::new(locals);
        for inst in &self.instructions {
            func.instruction(inst);
        }

        Ok(func)
    }
}
