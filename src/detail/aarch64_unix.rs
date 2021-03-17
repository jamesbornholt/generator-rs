use crate::detail::{align_down, mut_offset};
use crate::reg_context::InitFn;
use crate::stack::Stack;

#[link(name = "asm", kind = "static")]
extern "C" {
    pub fn bootstrap_green_task();
    pub fn prefetch(data: *const usize);
    pub fn swap_registers(out_regs: *mut Registers, in_regs: *const Registers);
}

#[repr(C, align(16))]
#[derive(Debug)]
pub struct Registers {
    // We only save the 13 callee-saved registers:
    //  x19--x28, fp (x29), lr (x30), sp
    gpr: [usize; 16],
}

impl Registers {
    pub fn new() -> Registers {
        Registers { gpr: [0; 16] }
    }

    #[inline]
    pub fn prefetch(&self) {
        unsafe {
            prefetch(self as *const _ as *const usize);
            prefetch(self.gpr[1] as *const usize);
        }
    }
}

pub fn initialize_call_frame(
    regs: &mut Registers,
    fptr: InitFn,
    arg: usize,
    arg2: *mut usize,
    stack: &Stack,
) {
    // Callee-saved registers start at x19
    const X19: usize = 19 - 19;
    const X20: usize = 20 - 19;
    const X21: usize = 21 - 19;
    const FP: usize  = 29 - 19;
    const LR: usize  = 30 - 19;
    const SP: usize  = 31 - 19;
    
    let sp = align_down(stack.end());

    // These registers are frobbed by bootstrap_green_task into the right
    // location so we can invoke the "real init function", `fptr`.
    regs.gpr[X19] = arg;
    regs.gpr[X20] = arg2 as usize;
    regs.gpr[X21] = fptr as usize;

    // Aarch64 current stack frame pointer
    regs.gpr[FP] = mut_offset(sp, -4) as usize;
    
    regs.gpr[LR] = bootstrap_green_task as usize;

    // setup the init stack
    // this is prepared for the swap context
    // leave enough space for stack unwind access
    regs.gpr[SP] = mut_offset(sp, -4) as usize;

    unsafe {
        // setup the correct stack frame for unwind
        *mut_offset(sp, -0) = 0;
        *mut_offset(sp, -1) = 0;
        *mut_offset(sp, -2) = 0;
        *mut_offset(sp, -3) = 0;
    }
}
