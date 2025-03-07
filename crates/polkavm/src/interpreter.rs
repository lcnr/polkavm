use crate::api::{BackendAccess, ExecutionConfig, MemoryAccessError, Module, OnHostcall};
use crate::error::{bail, Error};
use crate::utils::RegImm;
use core::mem::MaybeUninit;
use polkavm_common::abi::{VM_ADDR_RETURN_TO_HOST, VM_CODE_ADDRESS_ALIGNMENT};
use polkavm_common::error::Trap;
use polkavm_common::init::GuestProgramInit;
use polkavm_common::operation::*;
use polkavm_common::program::{Instruction, InstructionVisitor, Reg};
use polkavm_common::utils::{byte_slice_init, Access, AsUninitSliceMut, Gas};

type ExecutionError<E = core::convert::Infallible> = polkavm_common::error::ExecutionError<E>;

pub(crate) struct InterpretedModule {
    pub(crate) instructions: Vec<Instruction>,
    ro_data: Vec<u8>,
    rw_data: Vec<u8>,
    gas_cost_for_basic_block: Vec<u32>,
}

impl InterpretedModule {
    pub fn new(init: GuestProgramInit, gas_cost_for_basic_block: Vec<u32>, instructions: Vec<Instruction>) -> Result<Self, Error> {
        let memory_config = init.memory_config().map_err(Error::from_static_str)?;
        let mut ro_data: Vec<_> = init.ro_data().into();
        ro_data.resize(memory_config.ro_data_size() as usize, 0);

        Ok(InterpretedModule {
            instructions,
            ro_data,
            rw_data: init.rw_data().into(),
            gas_cost_for_basic_block,
        })
    }
}

pub(crate) type OnSetReg<'a> = &'a mut dyn FnMut(Reg, u32) -> Result<(), Trap>;
pub(crate) type OnStore<'a> = &'a mut dyn for<'r> FnMut(u32, &'r [u8]) -> Result<(), Trap>;

#[derive(Default)]
pub(crate) struct InterpreterContext<'a> {
    on_hostcall: Option<OnHostcall<'a>>,
    on_set_reg: Option<OnSetReg<'a>>,
    on_store: Option<OnStore<'a>>,
}

impl<'a> InterpreterContext<'a> {
    pub fn set_on_hostcall(&mut self, on_hostcall: OnHostcall<'a>) {
        self.on_hostcall = Some(on_hostcall);
    }

    pub fn set_on_set_reg(&mut self, on_set_reg: OnSetReg<'a>) {
        self.on_set_reg = Some(on_set_reg);
    }

    pub fn set_on_store(&mut self, on_store: OnStore<'a>) {
        self.on_store = Some(on_store);
    }
}

pub(crate) struct InterpretedInstance {
    module: Module,
    heap: Vec<u8>,
    stack: Vec<u8>,
    regs: [u32; Reg::ALL.len()],
    nth_instruction: u32,
    nth_basic_block: u32,
    return_to_host: bool,
    cycle_counter: u64,
    gas_remaining: Option<i64>,
    in_new_execution: bool,
}

impl InterpretedInstance {
    pub fn new(module: Module) -> Result<Self, Error> {
        if module.interpreted_module().is_none() {
            bail!("an interpreter cannot be created from the given module")
        }

        let mut heap = Vec::new();
        let mut stack = Vec::new();

        heap.reserve_exact(module.memory_config().heap_size() as usize);
        stack.reserve_exact(module.memory_config().stack_size() as usize);

        let mut interpreter = Self {
            heap,
            stack,
            module,
            regs: [0; Reg::ALL.len()],
            nth_instruction: VM_ADDR_RETURN_TO_HOST,
            nth_basic_block: 0,
            return_to_host: true,
            cycle_counter: 0,
            gas_remaining: None,
            in_new_execution: false,
        };

        if interpreter.module.gas_metering().is_some() {
            interpreter.gas_remaining = Some(0);
        }

        interpreter.reset_memory();
        Ok(interpreter)
    }

    pub fn call(&mut self, export_index: usize, on_hostcall: OnHostcall, config: &ExecutionConfig) -> Result<(), ExecutionError<Error>> {
        let mut ctx = InterpreterContext::default();
        ctx.set_on_hostcall(on_hostcall);
        self.prepare_for_call(export_index, config);

        let result = self.run(ctx);
        if config.reset_memory_after_execution {
            self.reset_memory();
        }

        result
    }

    pub fn run(&mut self, ctx: InterpreterContext) -> Result<(), ExecutionError<Error>> {
        fn translate_error(error: Result<(), ExecutionError>) -> Result<(), ExecutionError<Error>> {
            error.map_err(|error| match error {
                ExecutionError::Trap(trap) => ExecutionError::Trap(trap),
                ExecutionError::OutOfGas => ExecutionError::OutOfGas,
                ExecutionError::Error(_) => unreachable!(),
            })
        }

        if self.in_new_execution {
            self.in_new_execution = false;
            translate_error(self.on_start_new_basic_block())?;
        }

        let mut visitor = Visitor { inner: self, ctx };
        loop {
            visitor.inner.cycle_counter += 1;
            let Some(instruction) = visitor
                .inner
                .module
                .instructions()
                .get(visitor.inner.nth_instruction as usize)
                .copied()
            else {
                return Err(ExecutionError::Trap(Default::default()));
            };

            translate_error(instruction.visit(&mut visitor))?;
            if visitor.inner.return_to_host {
                break;
            }
        }

        Ok(())
    }

    pub fn reset_memory(&mut self) {
        let interpreted_module = self.module.interpreted_module().unwrap();
        self.heap.clear();
        self.heap.extend_from_slice(&interpreted_module.rw_data);
        self.heap.resize(self.module.memory_config().heap_size() as usize, 0);
        self.stack.clear();
        self.stack.resize(self.module.memory_config().stack_size() as usize, 0);
    }

    pub fn prepare_for_call(&mut self, export_index: usize, config: &ExecutionConfig) {
        // TODO: If this function becomes public then this needs to return an error.
        let nth_basic_block = self
            .module
            .get_export(export_index)
            .expect("internal error: invalid export index")
            .address();

        let nth_instruction = self
            .module
            .instruction_by_basic_block(nth_basic_block)
            .expect("internal error: invalid export address");

        self.return_to_host = false;
        self.regs.copy_from_slice(&config.initial_regs);
        self.nth_instruction = nth_instruction;
        self.nth_basic_block = nth_basic_block;
        if self.module.gas_metering().is_some() {
            if let Some(gas) = config.gas {
                self.gas_remaining = Some(gas.get() as i64);
            }
        } else {
            self.gas_remaining = None;
        }

        self.in_new_execution = true;
    }

    pub fn step_once(&mut self, ctx: InterpreterContext) -> Result<(), ExecutionError> {
        if self.in_new_execution {
            self.in_new_execution = false;
            self.on_start_new_basic_block()?;
        }

        self.cycle_counter += 1;
        let Some(instruction) = self.module.instructions().get(self.nth_instruction as usize).copied() else {
            return Err(ExecutionError::Trap(Default::default()));
        };

        let mut visitor = Visitor { inner: self, ctx };

        instruction.visit(&mut visitor)
    }

    pub fn access(&mut self) -> InterpretedAccess {
        InterpretedAccess { instance: self }
    }

    fn get_memory_slice(&self, address: u32, length: u32) -> Option<&[u8]> {
        let memory_config = self.module.memory_config();
        let (range, memory) = if memory_config.ro_data_range().contains(&address) {
            let module = self.module.interpreted_module().unwrap();
            (memory_config.ro_data_range(), &module.ro_data)
        } else if memory_config.heap_range().contains(&address) {
            (memory_config.heap_range(), &self.heap)
        } else if memory_config.stack_range().contains(&address) {
            (memory_config.stack_range(), &self.stack)
        } else {
            return None;
        };

        let offset = address - range.start;
        memory.get(offset as usize..offset as usize + length as usize)
    }

    fn get_memory_slice_mut(&mut self, address: u32, length: u32) -> Option<&mut [u8]> {
        let memory_config = self.module.memory_config();
        let (range, memory_slice) = if memory_config.heap_range().contains(&address) {
            (memory_config.heap_range(), &mut self.heap)
        } else if memory_config.stack_range().contains(&address) {
            (memory_config.stack_range(), &mut self.stack)
        } else {
            return None;
        };

        let offset = (address - range.start) as usize;
        memory_slice.get_mut(offset..offset + length as usize)
    }

    fn on_start_new_basic_block(&mut self) -> Result<(), ExecutionError> {
        if let Some(ref mut gas_remaining) = self.gas_remaining {
            let module = self.module.interpreted_module().unwrap();
            let gas_cost = i64::from(module.gas_cost_for_basic_block[self.nth_basic_block as usize]);
            *gas_remaining -= gas_cost;
            if *gas_remaining < 0 {
                return Err(ExecutionError::OutOfGas);
            }
        }

        Ok(())
    }

    fn check_gas(&mut self) -> Result<(), ExecutionError> {
        if let Some(ref mut gas_remaining) = self.gas_remaining {
            if *gas_remaining < 0 {
                return Err(ExecutionError::OutOfGas);
            }
        }

        Ok(())
    }
}

pub struct InterpretedAccess<'a> {
    instance: &'a mut InterpretedInstance,
}

impl<'a> Access<'a> for InterpretedAccess<'a> {
    type Error = MemoryAccessError<&'static str>;

    fn get_reg(&self, reg: Reg) -> u32 {
        self.instance.regs[reg as usize]
    }

    fn set_reg(&mut self, reg: Reg, value: u32) {
        self.instance.regs[reg as usize] = value;
    }

    fn read_memory_into_slice<'slice, T>(&self, address: u32, buffer: &'slice mut T) -> Result<&'slice mut [u8], Self::Error>
    where
        T: ?Sized + AsUninitSliceMut,
    {
        let buffer: &mut [MaybeUninit<u8>] = buffer.as_uninit_slice_mut();
        let Some(slice) = self.instance.get_memory_slice(address, buffer.len() as u32) else {
            return Err(MemoryAccessError {
                address,
                length: buffer.len() as u64,
                error: "out of range read",
            });
        };

        Ok(byte_slice_init(buffer, slice))
    }

    fn write_memory(&mut self, address: u32, data: &[u8]) -> Result<(), Self::Error> {
        let Some(slice) = self.instance.get_memory_slice_mut(address, data.len() as u32) else {
            return Err(MemoryAccessError {
                address,
                length: data.len() as u64,
                error: "out of range write",
            });
        };

        slice.copy_from_slice(data);
        Ok(())
    }

    fn program_counter(&self) -> Option<u32> {
        Some(self.instance.nth_instruction)
    }

    fn native_program_counter(&self) -> Option<u64> {
        None
    }

    fn gas_remaining(&self) -> Option<Gas> {
        let gas = self.instance.gas_remaining?;
        Some(Gas::new(gas as u64).unwrap_or(Gas::MIN))
    }

    fn consume_gas(&mut self, gas: u64) {
        if let Some(ref mut gas_remaining) = self.instance.gas_remaining {
            *gas_remaining = gas_remaining.checked_sub_unsigned(gas).unwrap_or(-1);
        }
    }
}

struct Visitor<'a, 'b> {
    inner: &'a mut InterpretedInstance,
    ctx: InterpreterContext<'b>,
}

impl<'a, 'b> Visitor<'a, 'b> {
    #[inline(always)]
    fn get(&self, regimm: impl Into<RegImm>) -> u32 {
        match regimm.into() {
            RegImm::Reg(reg) => self.inner.regs[reg as usize],
            RegImm::Imm(value) => value,
        }
    }

    #[inline(always)]
    fn set(&mut self, dst: Reg, value: u32) -> Result<(), ExecutionError> {
        self.inner.regs[dst as usize] = value;
        log::trace!("{dst} = 0x{value:x}");

        if let Some(on_set_reg) = self.ctx.on_set_reg.as_mut() {
            let result = (on_set_reg)(dst, value);
            Ok(result.map_err(ExecutionError::Trap)?)
        } else {
            Ok(())
        }
    }

    #[inline(always)]
    fn set3(
        &mut self,
        dst: Reg,
        s1: impl Into<RegImm>,
        s2: impl Into<RegImm>,
        callback: impl Fn(u32, u32) -> u32,
    ) -> Result<(), ExecutionError> {
        let s1 = self.get(s1);
        let s2 = self.get(s2);
        self.set(dst, callback(s1, s2))?;
        self.inner.nth_instruction += 1;
        Ok(())
    }

    fn branch(
        &mut self,
        s1: impl Into<RegImm>,
        s2: impl Into<RegImm>,
        target: u32,
        callback: impl Fn(u32, u32) -> bool,
    ) -> Result<(), ExecutionError> {
        let s1 = self.get(s1);
        let s2 = self.get(s2);
        if callback(s1, s2) {
            self.inner.nth_instruction = self
                .inner
                .module
                .instruction_by_basic_block(target)
                .expect("internal error: couldn't fetch the instruction index for a branch");
            self.inner.nth_basic_block = target;
        } else {
            self.inner.nth_instruction += 1;
            self.inner.nth_basic_block += 1;
        }

        self.inner.on_start_new_basic_block()
    }

    fn load<T: LoadTy>(&mut self, dst: Reg, base: Option<Reg>, offset: u32) -> Result<(), ExecutionError> {
        assert!(core::mem::size_of::<T>() >= 1);

        let address = base.map_or(0, |base| self.inner.regs[base as usize]).wrapping_add(offset);
        let length = core::mem::size_of::<T>() as u32;
        let Some(slice) = self.inner.get_memory_slice(address, length) else {
            log::debug!(
                "Load of {length} bytes from 0x{address:x} failed! (pc = #{pc}, cycle = {cycle})",
                pc = self.inner.nth_instruction,
                cycle = self.inner.cycle_counter
            );
            self.inner
                .module
                .debug_print_location(log::Level::Debug, self.inner.nth_instruction);
            return Err(ExecutionError::Trap(Default::default()));
        };

        log::trace!("{dst} = {kind} [0x{address:x}]", kind = core::any::type_name::<T>());

        let value = T::from_slice(slice);
        self.set(dst, value)?;
        self.inner.nth_instruction += 1;
        Ok(())
    }

    fn store<T: StoreTy>(&mut self, src: impl Into<RegImm>, base: Option<Reg>, offset: u32) -> Result<(), ExecutionError> {
        assert!(core::mem::size_of::<T>() >= 1);

        let address = base.map_or(0, |base| self.inner.regs[base as usize]).wrapping_add(offset);
        let value = match src.into() {
            RegImm::Reg(src) => {
                let value = self.inner.regs[src as usize];
                log::trace!("{kind} [0x{address:x}] = {src} = 0x{value:x}", kind = core::any::type_name::<T>());
                value
            }
            RegImm::Imm(value) => {
                log::trace!("{kind} [0x{address:x}] = 0x{value:x}", kind = core::any::type_name::<T>());
                value
            }
        };

        let length = core::mem::size_of::<T>() as u32;
        let Some(slice) = self.inner.get_memory_slice_mut(address, length) else {
            log::debug!(
                "Store of {length} bytes to 0x{address:x} failed! (pc = #{pc}, cycle = {cycle})",
                pc = self.inner.nth_instruction,
                cycle = self.inner.cycle_counter
            );
            self.inner
                .module
                .debug_print_location(log::Level::Debug, self.inner.nth_instruction);
            return Err(ExecutionError::Trap(Default::default()));
        };

        let value = T::into_bytes(value);
        slice.copy_from_slice(value.as_ref());

        if let Some(on_store) = self.ctx.on_store.as_mut() {
            (on_store)(address, value.as_ref()).map_err(ExecutionError::Trap)?;
        }

        self.inner.nth_instruction += 1;
        Ok(())
    }

    fn get_return_address(&self) -> u32 {
        self.inner
            .module
            .jump_table_index_by_basic_block(self.inner.nth_basic_block + 1)
            .expect("internal error: couldn't fetch the jump table index for the return basic block")
            * VM_CODE_ADDRESS_ALIGNMENT
    }

    fn set_return_address(&mut self, ra: Reg, return_address: u32) -> Result<(), ExecutionError> {
        log::trace!(
            "Setting a call's return address: {ra} = @dyn {:x} (@{:x})",
            return_address / VM_CODE_ADDRESS_ALIGNMENT,
            self.inner.nth_basic_block + 1
        );

        self.set(ra, return_address)
    }

    fn dynamic_jump(&mut self, call: Option<(Reg, u32)>, base: Reg, offset: u32) -> Result<(), ExecutionError> {
        let target = self.inner.regs[base as usize].wrapping_add(offset);
        if let Some((ra, return_address)) = call {
            self.set(ra, return_address)?;
        }

        if target == VM_ADDR_RETURN_TO_HOST {
            self.inner.return_to_host = true;
            return Ok(());
        }

        if target == 0 {
            return Err(ExecutionError::Trap(Default::default()));
        }

        if target % VM_CODE_ADDRESS_ALIGNMENT != 0 {
            log::error!("Found a dynamic jump with a misaligned target: target = {target}");
            return Err(ExecutionError::Trap(Default::default()));
        }

        let Some(nth_basic_block) = self
            .inner
            .module
            .basic_block_by_jump_table_index(target / VM_CODE_ADDRESS_ALIGNMENT)
        else {
            return Err(ExecutionError::Trap(Default::default()));
        };

        let nth_instruction = self
            .inner
            .module
            .instruction_by_basic_block(nth_basic_block)
            .expect("internal error: couldn't fetch the instruction index for a dynamic jump");

        log::trace!("Dynamic jump to: #{nth_instruction}: @{nth_basic_block:x}");
        self.inner.nth_basic_block = nth_basic_block;
        self.inner.nth_instruction = nth_instruction;
        self.inner.on_start_new_basic_block()
    }
}

trait LoadTy {
    fn from_slice(xs: &[u8]) -> u32;
}

impl LoadTy for u8 {
    fn from_slice(xs: &[u8]) -> u32 {
        u32::from(xs[0])
    }
}

impl LoadTy for i8 {
    fn from_slice(xs: &[u8]) -> u32 {
        i32::from(xs[0] as i8) as u32
    }
}

impl LoadTy for u16 {
    fn from_slice(xs: &[u8]) -> u32 {
        u32::from(u16::from_le_bytes([xs[0], xs[1]]))
    }
}

impl LoadTy for i16 {
    fn from_slice(xs: &[u8]) -> u32 {
        i32::from(i16::from_le_bytes([xs[0], xs[1]])) as u32
    }
}

impl LoadTy for u32 {
    fn from_slice(xs: &[u8]) -> u32 {
        u32::from_le_bytes([xs[0], xs[1], xs[2], xs[3]])
    }
}

trait StoreTy: Sized {
    type Array: AsRef<[u8]>;
    fn into_bytes(value: u32) -> Self::Array;
}

impl StoreTy for u8 {
    type Array = [u8; 1];
    fn into_bytes(value: u32) -> Self::Array {
        (value as u8).to_le_bytes()
    }
}

impl StoreTy for u16 {
    type Array = [u8; 2];
    fn into_bytes(value: u32) -> Self::Array {
        (value as u16).to_le_bytes()
    }
}

impl StoreTy for u32 {
    type Array = [u8; 4];
    fn into_bytes(value: u32) -> Self::Array {
        value.to_le_bytes()
    }
}

impl<'a, 'b> InstructionVisitor for Visitor<'a, 'b> {
    type ReturnTy = Result<(), ExecutionError>;

    fn trap(&mut self) -> Self::ReturnTy {
        log::debug!(
            "Trap at instruction {} in block @{:x}",
            self.inner.nth_instruction,
            self.inner.nth_basic_block
        );
        Err(ExecutionError::Trap(Default::default()))
    }

    fn fallthrough(&mut self) -> Self::ReturnTy {
        self.inner.nth_instruction += 1;
        self.inner.nth_basic_block += 1;
        self.inner.on_start_new_basic_block()
    }

    fn ecalli(&mut self, imm: u32) -> Self::ReturnTy {
        if let Some(on_hostcall) = self.ctx.on_hostcall.as_mut() {
            let access = BackendAccess::Interpreted(self.inner.access());
            (on_hostcall)(imm, access).map_err(ExecutionError::Trap)?;
            self.inner.nth_instruction += 1;
            self.inner.check_gas()?;
            Ok(())
        } else {
            log::debug!("Hostcall called without any hostcall handler set!");
            Err(ExecutionError::Trap(Default::default()))
        }
    }

    fn set_less_than_unsigned(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| u32::from(s1 < s2))
    }

    fn set_less_than_signed(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| u32::from((s1 as i32) < (s2 as i32)))
    }

    fn shift_logical_right(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_shr)
    }

    fn shift_arithmetic_right(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| ((s1 as i32).wrapping_shr(s2)) as u32)
    }

    fn shift_logical_left(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_shl)
    }

    fn xor(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s1 ^ s2)
    }

    fn and(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s1 & s2)
    }

    fn or(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s1 | s2)
    }

    fn add(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_add)
    }

    fn sub(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_sub)
    }

    fn negate_and_add_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s2.wrapping_sub(s1))
    }

    fn mul(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_mul)
    }

    fn mul_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_mul)
    }

    fn mul_upper_signed_signed(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| mulh(s1 as i32, s2 as i32) as u32)
    }

    fn mul_upper_signed_signed_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| mulh(s1 as i32, s2 as i32) as u32)
    }

    fn mul_upper_unsigned_unsigned(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, mulhu)
    }

    fn mul_upper_unsigned_unsigned_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, mulhu)
    }

    fn mul_upper_signed_unsigned(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| mulhsu(s1 as i32, s2) as u32)
    }

    fn div_unsigned(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, divu)
    }

    fn div_signed(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| div(s1 as i32, s2 as i32) as u32)
    }

    fn rem_unsigned(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, remu)
    }

    fn rem_signed(&mut self, d: Reg, s1: Reg, s2: Reg) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| rem(s1 as i32, s2 as i32) as u32)
    }

    fn set_less_than_unsigned_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| u32::from(s1 < s2))
    }

    fn set_greater_than_unsigned_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| u32::from(s1 > s2))
    }

    fn set_less_than_signed_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| u32::from((s1 as i32) < (s2 as i32)))
    }

    fn set_greater_than_signed_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| u32::from((s1 as i32) > (s2 as i32)))
    }

    fn shift_logical_right_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_shr)
    }

    fn shift_logical_right_imm_alt(&mut self, d: Reg, s2: Reg, s1: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_shr)
    }

    fn shift_arithmetic_right_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| ((s1 as i32) >> s2) as u32)
    }

    fn shift_arithmetic_right_imm_alt(&mut self, d: Reg, s2: Reg, s1: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| ((s1 as i32) >> s2) as u32)
    }

    fn shift_logical_left_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_shl)
    }

    fn shift_logical_left_imm_alt(&mut self, d: Reg, s2: Reg, s1: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_shl)
    }

    fn or_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s1 | s2)
    }

    fn and_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s1 & s2)
    }

    fn xor_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, |s1, s2| s1 ^ s2)
    }

    fn load_imm(&mut self, dst: Reg, imm: u32) -> Self::ReturnTy {
        self.set(dst, imm)?;
        self.inner.nth_instruction += 1;
        Ok(())
    }

    fn move_reg(&mut self, d: Reg, s: Reg) -> Self::ReturnTy {
        let imm = self.get(s);
        self.set(d, imm)?;
        self.inner.nth_instruction += 1;
        Ok(())
    }

    fn cmov_if_zero(&mut self, d: Reg, s: Reg, c: Reg) -> Self::ReturnTy {
        self.set3(d, s, c, |s, c| if c == 0 { s } else { 0 })
    }

    fn cmov_if_not_zero(&mut self, d: Reg, s: Reg, c: Reg) -> Self::ReturnTy {
        self.set3(d, s, c, |s, c| if c != 0 { s } else { 0 })
    }

    fn add_imm(&mut self, d: Reg, s1: Reg, s2: u32) -> Self::ReturnTy {
        self.set3(d, s1, s2, u32::wrapping_add)
    }

    fn store_imm_u8(&mut self, value: u32, offset: u32) -> Self::ReturnTy {
        self.store::<u8>(value, None, offset)
    }

    fn store_imm_u16(&mut self, value: u32, offset: u32) -> Self::ReturnTy {
        self.store::<u16>(value, None, offset)
    }

    fn store_imm_u32(&mut self, value: u32, offset: u32) -> Self::ReturnTy {
        self.store::<u32>(value, None, offset)
    }

    fn store_imm_indirect_u8(&mut self, base: Reg, offset: u32, value: u32) -> Self::ReturnTy {
        self.store::<u8>(value, Some(base), offset)
    }

    fn store_imm_indirect_u16(&mut self, base: Reg, offset: u32, value: u32) -> Self::ReturnTy {
        self.store::<u16>(value, Some(base), offset)
    }

    fn store_imm_indirect_u32(&mut self, base: Reg, offset: u32, value: u32) -> Self::ReturnTy {
        self.store::<u32>(value, Some(base), offset)
    }

    fn store_indirect_u8(&mut self, src: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.store::<u8>(src, Some(base), offset)
    }

    fn store_indirect_u16(&mut self, src: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.store::<u16>(src, Some(base), offset)
    }

    fn store_indirect_u32(&mut self, src: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.store::<u32>(src, Some(base), offset)
    }

    fn store_u8(&mut self, src: Reg, offset: u32) -> Self::ReturnTy {
        self.store::<u8>(src, None, offset)
    }

    fn store_u16(&mut self, src: Reg, offset: u32) -> Self::ReturnTy {
        self.store::<u16>(src, None, offset)
    }

    fn store_u32(&mut self, src: Reg, offset: u32) -> Self::ReturnTy {
        self.store::<u32>(src, None, offset)
    }

    fn load_u8(&mut self, dst: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<u8>(dst, None, offset)
    }

    fn load_i8(&mut self, dst: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<i8>(dst, None, offset)
    }

    fn load_u16(&mut self, dst: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<u16>(dst, None, offset)
    }

    fn load_i16(&mut self, dst: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<i16>(dst, None, offset)
    }

    fn load_u32(&mut self, dst: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<u32>(dst, None, offset)
    }

    fn load_indirect_u8(&mut self, dst: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<u8>(dst, Some(base), offset)
    }

    fn load_indirect_i8(&mut self, dst: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<i8>(dst, Some(base), offset)
    }

    fn load_indirect_u16(&mut self, dst: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<u16>(dst, Some(base), offset)
    }

    fn load_indirect_i16(&mut self, dst: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<i16>(dst, Some(base), offset)
    }

    fn load_indirect_u32(&mut self, dst: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        self.load::<u32>(dst, Some(base), offset)
    }

    fn branch_less_unsigned(&mut self, s1: Reg, s2: Reg, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 < s2)
    }

    fn branch_less_unsigned_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 < s2)
    }

    fn branch_less_signed(&mut self, s1: Reg, s2: Reg, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| (s1 as i32) < (s2 as i32))
    }

    fn branch_less_signed_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| (s1 as i32) < (s2 as i32))
    }

    fn branch_eq(&mut self, s1: Reg, s2: Reg, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 == s2)
    }

    fn branch_eq_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 == s2)
    }

    fn branch_not_eq(&mut self, s1: Reg, s2: Reg, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 != s2)
    }

    fn branch_not_eq_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 != s2)
    }

    fn branch_greater_or_equal_unsigned(&mut self, s1: Reg, s2: Reg, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 >= s2)
    }

    fn branch_greater_or_equal_unsigned_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 >= s2)
    }

    fn branch_greater_or_equal_signed(&mut self, s1: Reg, s2: Reg, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| (s1 as i32) >= (s2 as i32))
    }

    fn branch_greater_or_equal_signed_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| (s1 as i32) >= (s2 as i32))
    }

    fn branch_less_or_equal_unsigned_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 <= s2)
    }

    fn branch_less_or_equal_signed_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| (s1 as i32) <= (s2 as i32))
    }

    fn branch_greater_unsigned_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| s1 > s2)
    }

    fn branch_greater_signed_imm(&mut self, s1: Reg, s2: u32, i: u32) -> Self::ReturnTy {
        self.branch(s1, s2, i, |s1, s2| (s1 as i32) > (s2 as i32))
    }

    fn jump(&mut self, target: u32) -> Self::ReturnTy {
        let nth_instruction = self
            .inner
            .module
            .instruction_by_basic_block(target)
            .expect("internal error: couldn't fetch the instruction index for a jump");

        log::trace!("Static jump to: #{nth_instruction}: @{target:x}");
        self.inner.nth_basic_block = target;
        self.inner.nth_instruction = nth_instruction;
        self.inner.on_start_new_basic_block()
    }

    fn jump_indirect(&mut self, base: Reg, offset: u32) -> Self::ReturnTy {
        self.dynamic_jump(None, base, offset)
    }

    fn call(&mut self, ra: Reg, target: u32) -> Self::ReturnTy {
        let return_address = self.get_return_address();
        self.set_return_address(ra, return_address)?;
        self.jump(target)
    }

    fn call_indirect(&mut self, ra: Reg, base: Reg, offset: u32) -> Self::ReturnTy {
        let return_address = self.get_return_address();
        self.dynamic_jump(Some((ra, return_address)), base, offset)
    }
}
