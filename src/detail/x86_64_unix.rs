// This file is modified from libfringe, a low-level green threading library.
// Copyright (c) edef <edef@edef.eu>,
//               whitequark <whitequark@whitequark.org>
//               Amanieu d'Antras <amanieu@gmail.com>
//               Xudong Huang <huangxu008@hotmail.com>
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

// To understand the code in this file, keep in mind these two facts:
// * x86_64 SysV C ABI has a "red zone": 128 bytes under the top of the stack
//   that is defined to be unmolested by signal handlers, interrupts, etc.
//   Leaf functions can use the red zone without adjusting rsp or rbp.
// * x86_64 SysV C ABI requires the stack to be aligned at function entry,
//   so that (%rsp+8) is a multiple of 16. Aligned operands are a requirement
//   of SIMD instructions, and making this the responsibility of the caller
//   avoids having to maintain a frame pointer, which is necessary when
//   a function has to realign the stack from an unknown state.
// * x86_64 SysV C ABI passes the first argument in %rdi. We also use %rdi
//   to pass a value while swapping context; this is an arbitrary choice
//   (we clobber all registers and could use any of them) but this allows us
//   to reuse the swap function to perform the initial call. We do the same
//   thing with %rsi to pass the stack pointer to the new context.
//
// To understand the DWARF CFI code in this file, keep in mind these facts:
// * CFI is "call frame information"; a set of instructions to a debugger or
//   an unwinder that allow it to simulate returning from functions. This implies
//   restoring every register to its pre-call state, as well as the stack pointer.
// * CFA is "call frame address"; the value of stack pointer right before the call
//   instruction in the caller. Everything strictly below CFA (and inclusive until
//   the next CFA) is the call frame of the callee. This implies that the return
//   address is the part of callee's call frame.
// * Logically, DWARF CFI is a table where rows are instruction pointer values and
//   columns describe where registers are spilled (mostly using expressions that
//   compute a memory location as CFA+n). A .cfi_offset pseudoinstruction changes
//   the state of a column for all IP numerically larger than the one it's placed
//   after. A .cfi_def_* pseudoinstruction changes the CFA value similarly.
// * Simulating return is as easy as restoring register values from the CFI table
//   and then setting stack pointer to CFA.
//
// A high-level overview of the function of the trampolines when unwinding is:
// * The 2nd init trampoline puts a controlled value (written in swap to `new_cfa`)
//   into %rbp. This is then used as the CFA for the 1st trampoline.
// * This controlled value points to the bottom of the stack of the parent context,
//   which holds the saved %rbp and return address from the call to swap().
// * The 1st init trampoline tells the unwinder to restore %rbp and its return
//   address from the stack frame at %rbp (in the parent stack), thus continuing
//   unwinding at the swap call site instead of falling off the end of context stack.
use std::mem;
use reg_context::InitFn;
use stack::{Stack, StackPointer};

/// prefetch data
#[inline(always)]
unsafe fn prefetch(data: *const usize) {
    asm!(
    "prefetcht1 $0"
    : // no output
    : "m"(*data)
    :
    : "volatile")
}

unsafe fn initialize_call_frame(regs: &mut Registers, fptr: InitFn, stack: &Stack) {
    #[naked]
    unsafe extern "C" fn trampoline_1() {
        asm!(
        r#"
        # gdb has a hardcoded check that rejects backtraces where frame addresses
        # do not monotonically decrease. It is turned off if the function is called
        # "__morestack" and that is hardcoded. So, to make gdb backtraces match
        # the actual unwinder behavior, we call ourselves "__morestack" and mark
        # the symbol as local; it shouldn't interfere with anything.
        __morestack:
        .local __morestack

        # Set up the first part of our DWARF CFI linking stacks together. When
        # we reach this function from unwinding, %rbp will be pointing at the bottom
        # of the parent linked stack. This link is set each time swap() is called.
        # When unwinding the frame corresponding to this function, a DWARF unwinder
        # will use %rbp+16 as the next call frame address, restore return address
        # from CFA-8 and restore %rbp from CFA-16. This mirrors what the second half
        # of `swap_trampoline` does.
        .cfi_def_cfa %rbp, 16
        .cfi_offset %rbp, -16

        # This nop is here so that the initial swap doesn't return to the start
        # of the trampoline, which confuses the unwinder since it will look for
        # frame information in the previous symbol rather than this one. It is
        # never actually executed.
        nop

        # Stack unwinding in some versions of libunwind doesn't seem to like
        # 1-byte symbols, so we add a second nop here. This instruction isn't
        # executed either, it is only here to pad the symbol size.
        nop

        .Lend:
        .size __morestack, .Lend-__morestack
        "#
        : : : : "volatile")
    }

    #[cfg(target_vendor = "apple")]
    #[naked]
    unsafe extern "C" fn trampoline_1() {
        asm!(
        r#"
        # Identical to the above, except avoids .local/.size that aren't available on Mach-O.
        __morestack:
        .private_extern __morestack
        .cfi_def_cfa %rbp, 16
        .cfi_offset %rbp, -16
        nop
        nop
        "#
        : : : : "volatile")
    }

    #[naked]
    unsafe extern "C" fn trampoline_2() {
        asm!(
        r#"
        # Set up the second part of our DWARF CFI.
        # When unwinding the frame corresponding to this function, a DWARF unwinder
        # will restore %rbp (and thus CFA of the first trampoline) from the stack slot.
        # This stack slot is updated every time swap() is called to point to the bottom
        # of the stack of the context switch just switched from.
        .cfi_def_cfa %rbp, 16
        .cfi_offset %rbp, -16

        # This nop is here so that the return address of the swap trampoline
        # doesn't point to the start of the symbol. This confuses gdb's backtraces,
        # causing them to think the parent function is trampoline_1 instead of
        # trampoline_2.
        nop

        # Call with the provided function
        call    *16(%rsp)

        # Restore the stack pointer of the parent context. No CFI adjustments
        # are needed since we have the same stack frame as trampoline_1.
        movq    %rsi, %rsp

        # Restore frame pointer of the parent context.
        popq    %rbp
        .cfi_adjust_cfa_offset -8
        .cfi_restore %rbp

        # Clear the stack pointer. We can't call into this context any more once
        # the function has returned.
        xorq    %rsi, %rsi

        # Return into the parent context. Use `pop` and `jmp` instead of a `ret`
        # to avoid return address mispredictions (~8ns per `ret` on Ivy Bridge).
        popq    %rax
        .cfi_adjust_cfa_offset -8
        .cfi_register %rip, %rax
        jmpq    *%rax
        "#
        : : : : "volatile")
    }

    // We set up the stack in a somewhat special way so that to the unwinder it
    // looks like trampoline_1 has called trampoline_2, which has in turn called
    // swap::trampoline.
    //
    // There are 2 call frames in this setup, each containing the return address
    // followed by the %rbp value for that frame. This setup supports unwinding
    // using DWARF CFI as well as the frame pointer-based unwinding used by tools
    // such as perf or dtrace.
    let mut sp = StackPointer::new(stack.end());

    sp.push(0usize); // Padding to ensure the stack is properly aligned
    sp.push(fptr as usize); // Function that trampoline_2 should call

    // Call frame for trampoline_2. The CFA slot is updated by swap::trampoline
    // each time a context switch is performed.
    sp.push(trampoline_1 as usize + 2); // Return after the 2 nops
    sp.push(0xdeaddeaddead0cfa); // CFA slot

    // Call frame for swap::trampoline. We set up the %rbp value to point to the
    // parent call frame.
    let frame = sp.offset(0);
    sp.push(trampoline_2 as usize + 1); // Entry point, skip initial nop
    sp.push(frame as usize); // Pointer to parent call frame

    // save the sp in register
    regs.sp = sp.offset(0) as usize;
}

// set the return address
#[inline(always)]
pub unsafe fn set_ret(ret: usize, sp: usize) {
    asm!(
    ""
    :
    : "{rdi}" (ret)
      "{rsi}" (sp)
    : // no clobers
    : "volatile")
}

#[inline(always)]
pub unsafe fn swap_link(
    arg: usize,
    new_sp: StackPointer,
    new_stack_base: *mut usize,
) -> (usize, StackPointer) {
    let ret: usize;
    let ret_sp: usize;
    asm!(
    r#"
    # Push the return address
    leaq    0f(%rip), %rax
    pushq   %rax

    # Save frame pointer explicitly; the unwinder uses it to find CFA of
    # the caller, and so it has to have the correct value immediately after
    # the call instruction that invoked the trampoline.
    pushq   %rbp

    # Link the call stacks together by writing the current stack bottom
    # address to the CFA slot in the new stack.
    movq    %rsp, -32(%rcx)

    # Pass the stack pointer of the old context to the new one.
    movq    %rsp, %rsi

    # Load stack pointer of the new context.
    movq    %rdx, %rsp

    # Restore frame pointer of the new context.
    popq    %rbp

    # Return into the new context. Use `pop` and `jmp` instead of a `ret`
    # to avoid return address mispredictions (~8ns per `ret` on Ivy Bridge).
    popq    %rax
    jmpq    *%rax
    0:
    "#
    : "={rdi}" (ret)
      "={rsi}" (ret_sp)
    : "{rdi}" (arg)
      "{rdx}" (new_sp.offset(0))
      "{rcx}" (new_stack_base)
    : "rax",   "rbx",   "rcx",   "rdx", /*"rsi",   "rdi",   "rbp",   "rsp",*/
      "r8",    "r9",    "r10",   "r11",   "r12",   "r13",   "r14",   "r15",
      "mm0",   "mm1",   "mm2",   "mm3",   "mm4",   "mm5",   "mm6",   "mm7",
      "xmm0",  "xmm1",  "xmm2",  "xmm3",  "xmm4",  "xmm5",  "xmm6",  "xmm7",
      "xmm8",  "xmm9",  "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
      "xmm16", "xmm17", "xmm18", "xmm19", "xmm20", "xmm21", "xmm22", "xmm23",
      "xmm24", "xmm25", "xmm26", "xmm27", "xmm28", "xmm29", "xmm30", "xmm31",
      "cc", "dirflag", "fpsr", "flags", "memory"
      // Ideally, we would set the LLVM "noredzone" attribute on this function
      // (and it would be propagated to the call site). Unfortunately, rustc
      // provides no such functionality. Fortunately, by a lucky coincidence,
      // the "alignstack" LLVM inline assembly option does exactly the same
      // thing on x86_64.
    : "volatile", "alignstack");
    (ret, mem::transmute(ret_sp))
}

#[inline(always)]
pub unsafe fn swap(arg: usize, new_sp: StackPointer) -> (usize, StackPointer) {
    // This is identical to swap_link, but without the write to the CFA slot.
    let ret: usize;
    let ret_sp: usize;
    asm!(
    r#"
    leaq    0f(%rip), %rax
    pushq   %rax
    pushq   %rbp
    movq    %rsp, %rsi
    movq    %rdx, %rsp
    popq    %rbp
    popq    %rax
    jmpq    *%rax
    0:
    "#
    : "={rdi}" (ret)
      "={rsi}" (ret_sp)
    : "{rdi}" (arg)
      "{rdx}" (new_sp.offset(0))
    : "rax",   "rbx",   "rcx",   "rdx", /*"rsi",   "rdi",   "rbp",   "rsp",*/
      "r8",    "r9",    "r10",   "r11",   "r12",   "r13",   "r14",   "r15",
      "mm0",   "mm1",   "mm2",   "mm3",   "mm4",   "mm5",   "mm6",   "mm7",
      "xmm0",  "xmm1",  "xmm2",  "xmm3",  "xmm4",  "xmm5",  "xmm6",  "xmm7",
      "xmm8",  "xmm9",  "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
      "xmm16", "xmm17", "xmm18", "xmm19", "xmm20", "xmm21", "xmm22", "xmm23",
      "xmm24", "xmm25", "xmm26", "xmm27", "xmm28", "xmm29", "xmm30", "xmm31",
      "cc", "dirflag", "fpsr", "flags", "memory"
    : "volatile", "alignstack");
    (ret, mem::transmute(ret_sp))
}

#[repr(C)]
#[derive(Debug)]
pub struct Registers {
    sp: usize,
}

impl Registers {
    pub fn new() -> Registers {
        Registers { sp: 0 }
    }

    // use for root thread register init
    pub fn root() -> Registers {
        Self::new()
    }

    #[inline]
    pub fn get_sp(&self) -> StackPointer {
        unsafe { StackPointer::new(self.sp as *mut usize) }
    }

    #[inline]
    pub fn set_sp(&mut self, sp: StackPointer) {
        self.sp = unsafe { mem::transmute(sp) };
    }

    #[inline(always)]
    pub fn prefetch(&self) {
        if self.sp == 0 {
            #[cold]
            return;
        }
        unsafe { prefetch(self.sp as *const usize) };
    }

    #[inline]
    pub unsafe fn restore_context(&mut self) {}

    pub unsafe fn init_with(&mut self, fptr: InitFn, stack: &Stack) {
        initialize_call_frame(self, fptr, stack);
    }
}
