use std::{
    cell::RefMut,
    hash::{Hash, Hasher},
};

use allocator_api2::vec;
use gc_arena::{
    allocator_api::MetricsAlloc,
    lock::{Lock, RefLock},
    Collect, Gc, Mutation,
};
use thiserror::Error;

use crate::{
    closure::{UpValue, UpValueState},
    meta_ops,
    types::{RegisterIndex, VarCount},
    AnyCallback, AnySequence, CallbackReturn, Closure, Context, Error, FromMultiValue, Fuel,
    Function, IntoMultiValue, SequencePoll, Stack, TypeError, VMError, Value, Variadic,
};

use super::vm::run_vm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadMode {
    /// No frames are on the thread and there are no available results, the thread can be started.
    Stopped,
    /// The thread has an error or has returned (or yielded) values that must be taken to move the
    /// thread back to the `Stopped` (or `Suspended`) state.
    Result,
    /// Thread has an active Lua, Callback, or Sequence frame.
    Normal,
    /// Thread has yielded and is waiting on being resumed.
    Suspended,
    /// The thread is waiting on another thread to finish.
    Waiting,
    /// Thread is currently inside its own `Thread::step` function.
    Running,
}

#[derive(Debug, Copy, Clone, Error)]
#[error("bad thread mode: {found:?}{}", if let Some(expected) = *.expected {
        format!(", expected {:?}", expected)
    } else {
        format!("")
    })]
pub struct BadThreadMode {
    pub found: ThreadMode,
    pub expected: Option<ThreadMode>,
}

#[derive(Debug, Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct Thread<'gc>(Gc<'gc, RefLock<ThreadState<'gc>>>);

impl<'gc> PartialEq for Thread<'gc> {
    fn eq(&self, other: &Thread<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Thread<'gc> {}

impl<'gc> Hash for Thread<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_ptr().hash(state)
    }
}

impl<'gc> Thread<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> Thread<'gc> {
        Thread(Gc::new(
            mc,
            RefLock::new(ThreadState {
                frames: vec::Vec::new_in(MetricsAlloc::new(mc)),
                stack: vec::Vec::new_in(MetricsAlloc::new(mc)),
                open_upvalues: vec::Vec::new_in(MetricsAlloc::new(mc)),
            }),
        ))
    }

    pub fn as_ptr(self) -> *const () {
        Gc::as_ptr(self.0) as *const ()
    }

    pub fn mode(self) -> ThreadMode {
        match self.0.try_borrow() {
            Ok(state) => state.mode(),
            Err(_) => ThreadMode::Running,
        }
    }

    /// If this thread is `Stopped`, start a new function with the given arguments.
    pub fn start(
        self,
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(&ctx, ThreadMode::Stopped)?;
        assert!(state.stack.is_empty());
        state.stack.extend(args.into_multi_value(ctx));
        state.push_call(0, function);
        Ok(())
    }

    /// If this thread is `Stopped`, start a new suspended function.
    pub fn start_suspended(
        self,
        mc: &Mutation<'gc>,
        function: Function<'gc>,
    ) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(mc, ThreadMode::Stopped)?;
        state.frames.push(Frame::Start(function));
        Ok(())
    }

    /// If the thread is in the `Result` mode, take the returned (or yielded) values. Moves the
    /// thread back to the `Stopped` (or `Suspended`) mode.
    pub fn take_result<T: FromMultiValue<'gc>>(
        self,
        ctx: Context<'gc>,
    ) -> Result<Result<T, Error<'gc>>, BadThreadMode> {
        let mut state = self.check_mode(&ctx, ThreadMode::Result)?;
        Ok(state
            .take_result()
            .and_then(|vals| Ok(T::from_multi_value(ctx, vals)?)))
    }

    /// If the thread is in `Suspended` mode, resume it.
    pub fn resume(
        self,
        ctx: Context<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(&ctx, ThreadMode::Suspended)?;

        let bottom = state.stack.len();
        state.stack.extend(args.into_multi_value(ctx));

        match state.frames.pop().expect("no frame to resume") {
            Frame::Start(function) => {
                assert!(bottom == 0 && state.open_upvalues.is_empty() && state.frames.is_empty());
                state.push_call(0, function);
            }
            Frame::Yielded => {
                state.return_to(bottom);
            }
            _ => panic!("top frame not a suspended thread"),
        }
        Ok(())
    }

    /// If the thread is in `Suspended` mode, cause an error wherever the thread was suspended.
    pub fn resume_err(self, mc: &Mutation<'gc>, error: Error<'gc>) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(mc, ThreadMode::Suspended)?;
        assert!(matches!(
            state.frames.pop(),
            Some(Frame::Start(_) | Frame::Yielded)
        ));
        state.frames.push(Frame::Error(error));
        Ok(())
    }

    /// If this thread is in any other mode than `Running`, reset the thread completely and restore
    /// it to the `Stopped` state.
    pub fn reset(self, mc: &Mutation<'gc>) -> Result<(), BadThreadMode> {
        match self.0.try_borrow_mut(mc) {
            Ok(mut state) => {
                state.close_upvalues(mc, 0);
                assert!(state.open_upvalues.is_empty());
                state.stack.clear();
                state.frames.clear();
                Ok(())
            }
            Err(_) => Err(BadThreadMode {
                found: ThreadMode::Running,
                expected: None,
            }),
        }
    }

    fn check_mode(
        &self,
        mc: &Mutation<'gc>,
        expected: ThreadMode,
    ) -> Result<RefMut<ThreadState<'gc>>, BadThreadMode> {
        assert!(expected != ThreadMode::Running);
        if let Ok(state) = self.0.try_borrow_mut(mc) {
            let found = state.mode();
            if found == expected {
                Ok(state)
            } else {
                Err(BadThreadMode {
                    found,
                    expected: Some(expected),
                })
            }
        } else {
            Err(BadThreadMode {
                found: ThreadMode::Running,
                expected: Some(expected),
            })
        }
    }
}

/// The entry-point for the Lua VM.
///
/// `Executor` runs networks of `Threads` that may depend on each other and may pass control
/// back and forth. All Lua code that is run is done so directly or indirectly by calling
/// `Executor::step`.
///
/// # Panics
///
/// `Executor` is dangerous to use from within any kind of Lua callback. It has no protection
/// against re-entrency, and calling `Executor` methods from within a callback that it is running
/// (other than `Executor::mode`) will panic. Additionally, even if an independent `Executor` is
/// used, cross-thread upvalues may cause a panic if one `Executor` is used within the other.
///
/// `Executor`s are not meant to be used from or available to callbacks at all, and `Executor`s
/// should not be nested. Instead, use the normal mechanisms for callbacks to call Lua code so that
/// it is run on the same executor calling the callback.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Executor<'gc>(Gc<'gc, RefLock<vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>>>);

impl<'gc> PartialEq for Executor<'gc> {
    fn eq(&self, other: &Executor<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Executor<'gc> {}

impl<'gc> Executor<'gc> {
    const FUEL_PER_CALLBACK: i32 = 8;
    const FUEL_PER_SEQ_STEP: i32 = 4;
    const FUEL_PER_STEP: i32 = 4;

    /// Creates a new `Executor` with a stopped main thread.
    pub fn new(mc: &Mutation<'gc>) -> Self {
        Self::run(mc, Thread::new(mc))
    }

    /// Creates a new `Executor` that begins running the given thread.
    pub fn run(mc: &Mutation<'gc>, thread: Thread<'gc>) -> Self {
        let mut thread_stack = vec::Vec::new_in(MetricsAlloc::new(mc));
        thread_stack.push(thread);
        Executor(Gc::new(mc, RefLock::new(thread_stack)))
    }

    /// Creates a new `Executor` with a new `Thread` running the given function.
    pub fn start(
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Self {
        let thread = Thread::new(&ctx);
        thread.start(ctx, function, args).unwrap();
        Self::run(&ctx, thread)
    }

    pub fn mode(self) -> ThreadMode {
        if let Ok(thread_stack) = self.0.try_borrow() {
            if thread_stack.len() > 1 {
                ThreadMode::Normal
            } else {
                thread_stack[0].mode()
            }
        } else {
            ThreadMode::Running
        }
    }

    /// Runs the VM for a period of time controlled by the `fuel` parameter.
    ///
    /// The VM and callbacks will consume fuel as they run, and `Executor::step` will return as soon
    /// as `Fuel::can_continue()` returns false *and some minimal positive progress has been made*.
    ///
    /// Returns `false` if the method has exhausted its fuel, but there is more work to do. If
    /// `true` is returned, there is only one thread remaining on the execution stack and it is no
    /// longer in the 'Normal' state.
    pub fn step(self, ctx: Context<'gc>, fuel: &mut Fuel) -> bool {
        let mut thread_stack = self.0.borrow_mut(&ctx);

        loop {
            let mut top_thread = thread_stack.last().copied().unwrap();
            let mut res_thread = None;
            match top_thread.mode() {
                ThreadMode::Normal => {}
                ThreadMode::Running => {
                    panic!("`Executor` thread already running")
                }
                _ => {
                    if thread_stack.len() == 1 {
                        break true;
                    } else {
                        thread_stack.pop();
                        res_thread = Some(top_thread);
                        top_thread = thread_stack.last().copied().unwrap();
                    }
                }
            }

            let mut top_state = top_thread.0.borrow_mut(&ctx);
            if let Some(res_thread) = res_thread {
                let mode = top_state.mode();
                if mode == ThreadMode::Waiting {
                    assert!(matches!(top_state.frames.pop(), Some(Frame::WaitThread)));
                    match res_thread.mode() {
                        ThreadMode::Result => {
                            // Take the results from the res_thread and return them to our top
                            // thread.
                            let mut res_state = res_thread.0.borrow_mut(&ctx);
                            match res_state.take_result() {
                                Ok(vals) => {
                                    let bottom = top_state.stack.len();
                                    top_state.stack.extend(vals);
                                    top_state.return_to(bottom);
                                }
                                Err(err) => {
                                    top_state.frames.push(Frame::Error(err.into()));
                                }
                            }
                            drop(res_state);
                        }
                        ThreadMode::Normal => unreachable!(),
                        res_mode => top_state.frames.push(Frame::Error(
                            BadThreadMode {
                                found: res_mode,
                                expected: None,
                            }
                            .into(),
                        )),
                    }
                } else {
                    // Shenanigans have happened and the upper thread has had its state externally
                    // changed.
                    top_state.frames.push(Frame::Error(
                        BadThreadMode {
                            found: mode,
                            expected: None,
                        }
                        .into(),
                    ));
                }
            }

            if top_state.mode() == ThreadMode::Normal {
                fn callback_ret<'gc>(
                    ctx: Context<'gc>,
                    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
                    mut top_state: RefMut<ThreadState<'gc>>,
                    stack_bottom: usize,
                    ret: CallbackReturn<'gc>,
                ) {
                    match ret {
                        CallbackReturn::Return => {
                            top_state.return_to(stack_bottom);
                        }
                        CallbackReturn::Sequence(sequence) => {
                            top_state.frames.push(Frame::Sequence {
                                bottom: stack_bottom,
                                sequence,
                                pending_error: None,
                            });
                        }
                        CallbackReturn::Yield { to_thread, then } => {
                            if let Some(sequence) = then {
                                top_state.frames.push(Frame::Sequence {
                                    bottom: stack_bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            top_state.frames.push(Frame::Yielded);

                            if let Some(to_thread) = to_thread {
                                if let Err(err) = to_thread
                                    .resume(ctx, Variadic(top_state.stack.drain(stack_bottom..)))
                                {
                                    top_state.frames.push(Frame::Error(err.into()));
                                } else {
                                    thread_stack.pop();
                                    thread_stack.push(to_thread);
                                }
                            } else {
                                top_state.frames.push(Frame::Result {
                                    bottom: stack_bottom,
                                });
                            }
                        }
                        CallbackReturn::Call { function, then } => {
                            if let Some(sequence) = then {
                                top_state.frames.push(Frame::Sequence {
                                    bottom: stack_bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            top_state.push_call(stack_bottom, function);
                        }
                        CallbackReturn::Resume { thread, then } => {
                            if let Some(sequence) = then {
                                top_state.frames.push(Frame::Sequence {
                                    bottom: stack_bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            top_state.frames.push(Frame::WaitThread);

                            if let Err(err) =
                                thread.resume(ctx, Variadic(top_state.stack.drain(stack_bottom..)))
                            {
                                top_state.frames.push(Frame::Error(err.into()));
                            } else {
                                if top_state.frames.len() == 1 {
                                    // Tail call the thread resume if we can.
                                    assert!(matches!(top_state.frames[0], Frame::WaitThread));
                                    thread_stack.pop();
                                }
                                thread_stack.push(thread);
                            }
                        }
                    }
                }

                match top_state.frames.pop() {
                    Some(Frame::Callback { bottom, callback }) => {
                        fuel.consume_fuel(Self::FUEL_PER_CALLBACK);
                        match callback.call(ctx, fuel, Stack::new(&mut top_state.stack, bottom)) {
                            Ok(ret) => {
                                callback_ret(ctx, &mut *thread_stack, top_state, bottom, ret)
                            }
                            Err(err) => top_state.frames.push(Frame::Error(err)),
                        }
                    }
                    Some(Frame::Sequence {
                        bottom,
                        mut sequence,
                        pending_error,
                    }) => {
                        fuel.consume_fuel(Self::FUEL_PER_SEQ_STEP);
                        let fin = if let Some(err) = pending_error {
                            sequence.error(ctx, fuel, err, Stack::new(&mut top_state.stack, bottom))
                        } else {
                            sequence.poll(ctx, fuel, Stack::new(&mut top_state.stack, bottom))
                        };

                        match fin {
                            Ok(ret) => callback_ret(
                                ctx,
                                &mut *thread_stack,
                                top_state,
                                bottom,
                                match ret {
                                    SequencePoll::Pending => CallbackReturn::Sequence(sequence),
                                    SequencePoll::Return => CallbackReturn::Return,
                                    SequencePoll::Yield { to_thread, is_tail } => {
                                        CallbackReturn::Yield {
                                            to_thread,
                                            then: if is_tail { None } else { Some(sequence) },
                                        }
                                    }
                                    SequencePoll::Call { function, is_tail } => {
                                        CallbackReturn::Call {
                                            function,
                                            then: if is_tail { None } else { Some(sequence) },
                                        }
                                    }
                                    SequencePoll::Resume { thread, is_tail } => {
                                        CallbackReturn::Resume {
                                            thread,
                                            then: if is_tail { None } else { Some(sequence) },
                                        }
                                    }
                                },
                            ),
                            Err(error) => {
                                top_state.frames.push(Frame::Error(error));
                            }
                        }
                    }
                    Some(frame @ Frame::Lua { .. }) => {
                        top_state.frames.push(frame);

                        const VM_GRANULARITY: u32 = 64;

                        let lua_frame = LuaFrame {
                            state: &mut top_state,
                            thread: top_thread,
                            fuel,
                        };
                        match run_vm(ctx, lua_frame, VM_GRANULARITY) {
                            Err(err) => {
                                top_state.frames.push(Frame::Error(err.into()));
                            }
                            Ok(instructions_run) => {
                                fuel.consume_fuel(instructions_run.try_into().unwrap());
                            }
                        }
                    }
                    Some(Frame::Error(err)) => {
                        match top_state
                            .frames
                            .pop()
                            .expect("normal thread must have frame above error")
                        {
                            Frame::Lua { bottom, .. } => {
                                top_state.close_upvalues(&ctx, bottom);
                                top_state.stack.truncate(bottom);
                                top_state.frames.push(Frame::Error(err));
                            }
                            Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: error,
                            } => {
                                assert!(error.is_none());
                                top_state.frames.push(Frame::Sequence {
                                    bottom,
                                    sequence,
                                    pending_error: Some(err),
                                });
                            }
                            _ => top_state.frames.push(Frame::Error(err)),
                        }
                    }
                    _ => panic!("tried to step invalid frame type"),
                }
            }

            fuel.consume_fuel(Self::FUEL_PER_STEP);

            if !fuel.should_continue() {
                break false;
            }
        }
    }

    pub fn take_result<T: FromMultiValue<'gc>>(
        self,
        ctx: Context<'gc>,
    ) -> Result<Result<T, Error<'gc>>, BadThreadMode> {
        let thread_stack = self.0.borrow();
        if thread_stack.len() > 1 {
            Err(BadThreadMode {
                found: ThreadMode::Normal,
                expected: Some(ThreadMode::Result),
            })
        } else {
            thread_stack[0].take_result(ctx)
        }
    }

    pub fn resume<T: FromMultiValue<'gc>>(
        self,
        ctx: Context<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadThreadMode> {
        let thread_stack = self.0.borrow();
        if thread_stack.len() > 1 {
            Err(BadThreadMode {
                found: ThreadMode::Normal,
                expected: Some(ThreadMode::Suspended),
            })
        } else {
            thread_stack[0].resume(ctx, args)
        }
    }

    pub fn resume_err(self, mc: &Mutation<'gc>, error: Error<'gc>) -> Result<(), BadThreadMode> {
        let thread_stack = self.0.borrow();
        if thread_stack.len() > 1 {
            Err(BadThreadMode {
                found: ThreadMode::Normal,
                expected: Some(ThreadMode::Suspended),
            })
        } else {
            thread_stack[0].resume_err(mc, error)
        }
    }

    /// Reset this `Executor` entirely and begins running the given thread.
    pub fn reset(self, mc: &Mutation<'gc>, thread: Thread<'gc>) {
        let mut thread_stack = self.0.borrow_mut(mc);
        thread_stack.clear();
        thread_stack.push(thread);
    }

    /// Reset this `Executor` entirely and begins running the given function.
    pub fn restart(
        self,
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) {
        let mut thread_stack = self.0.borrow_mut(&ctx);
        thread_stack.truncate(1);
        thread_stack[0].reset(&ctx).unwrap();
        thread_stack[0].start(ctx, function, args).unwrap();
    }
}

pub(super) struct LuaFrame<'gc, 'a> {
    thread: Thread<'gc>,
    state: &'a mut ThreadState<'gc>,
    fuel: &'a mut Fuel,
}

impl<'gc, 'a> LuaFrame<'gc, 'a> {
    const FUEL_PER_CALL: i32 = 4;
    const FUEL_PER_ITEM: i32 = 1;

    // Returns the active closure for this Lua frame
    pub(super) fn closure(&self) -> Closure<'gc> {
        match self.state.frames.last() {
            Some(Frame::Lua { bottom, .. }) => match self.state.stack[*bottom] {
                Value::Function(Function::Closure(c)) => c,
                _ => panic!("thread bottom is not a closure"),
            },
            _ => panic!("top frame is not lua frame"),
        }
    }

    // returns a view of the Lua frame's registers
    pub(super) fn registers<'b>(&'b mut self) -> LuaRegisters<'gc, 'b> {
        match self.state.frames.last_mut() {
            Some(Frame::Lua {
                bottom, base, pc, ..
            }) => {
                let (upper_stack, stack_frame) = self.state.stack[..].split_at_mut(*base);
                LuaRegisters {
                    pc,
                    stack_frame,
                    upper_stack,
                    bottom: *bottom,
                    base: *base,
                    open_upvalues: &mut self.state.open_upvalues,
                    thread: self.thread,
                }
            }
            _ => panic!("top frame is not lua frame"),
        }
    }

    // Place the current frame's varargs at the given register, expecting the given count
    pub(super) fn varargs(&mut self, dest: RegisterIndex, count: VarCount) -> Result<(), VMError> {
        let Some(Frame::Lua {
            bottom,
            base,
            is_variable,
            ..
        }) = self.state.frames.last_mut()
        else {
            panic!("top frame is not lua frame");
        };

        if *is_variable {
            return Err(VMError::ExpectedVariableStack(false));
        }

        let varargs_start = *bottom + 1;
        let varargs_len = *base - varargs_start;

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);
        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, varargs_len));

        let dest = *base + dest.0 as usize;
        if let Some(count) = count.to_constant() {
            for i in 0..count as usize {
                self.state.stack[dest + i] = if i < varargs_len {
                    self.state.stack[varargs_start + i]
                } else {
                    Value::Nil
                };
            }
        } else {
            *is_variable = true;
            self.state.stack.resize(dest + varargs_len, Value::Nil);
            for i in 0..varargs_len {
                self.state.stack[dest + i] = self.state.stack[varargs_start + i];
            }
        }

        Ok(())
    }

    pub(super) fn set_table_list(
        &mut self,
        mc: &Mutation<'gc>,
        table_base: RegisterIndex,
        count: VarCount,
    ) -> Result<(), VMError> {
        let Some(&mut Frame::Lua {
            base,
            ref mut is_variable,
            stack_size,
            ..
        }) = self.state.frames.last_mut()
        else {
            panic!("top frame is not lua frame");
        };

        if count.is_variable() != *is_variable {
            return Err(VMError::ExpectedVariableStack(count.is_variable()));
        }

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);

        let table_ind = base + table_base.0 as usize;
        let start_ind = table_ind + 1;
        let table = self.state.stack[table_ind];
        let Value::Table(table) = table else {
            return Err(TypeError {
                expected: "table",
                found: table.type_name(),
            }
            .into());
        };

        let set_count = count
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(self.state.stack.len() - table_ind - 2);

        let Value::Integer(mut start) = self.state.stack[start_ind] else {
            return Err(TypeError {
                expected: "integer",
                found: self.state.stack[start_ind].type_name(),
            }
            .into());
        };

        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, set_count));
        for i in 0..set_count {
            if let Some(inc) = start.checked_add(1) {
                start = inc;
                table
                    .set_value(mc, inc.into(), self.state.stack[table_ind + 2 + i])
                    .unwrap();
            } else {
                break;
            }
        }

        self.state.stack[start_ind] = Value::Integer(start);

        if count.is_variable() {
            self.state.stack.resize(base + stack_size, Value::Nil);
            *is_variable = false;
        }

        Ok(())
    }

    // Call the function at the given register with the given arguments. On return, results will be
    // placed starting at the function register.
    pub(super) fn call_function(
        self,
        ctx: Context<'gc>,
        func: RegisterIndex,
        args: VarCount,
        returns: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            expected_return,
            is_variable,
            base,
            ..
        }) = self.state.frames.last_mut()
        else {
            panic!("top frame is not lua frame");
        };

        if *is_variable != args.is_variable() {
            return Err(VMError::ExpectedVariableStack(args.is_variable()));
        }

        *expected_return = Some(LuaReturn::Normal(returns));
        let function_index = *base + func.0 as usize;
        let arg_count = args
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(self.state.stack.len() - function_index - 1);

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);
        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, arg_count));

        match meta_ops::call(ctx, self.state.stack[function_index])? {
            Function::Closure(closure) => {
                self.state.stack[function_index] = closure.into();
                let fixed_params = closure.0.proto.fixed_params as usize;
                let stack_size = closure.0.proto.stack_size as usize;

                let base = if arg_count > fixed_params {
                    self.state.stack.truncate(function_index + 1 + arg_count);
                    self.state.stack[function_index + 1..].rotate_left(fixed_params);
                    function_index + 1 + (arg_count - fixed_params)
                } else {
                    function_index + 1
                };

                self.state.stack.resize(base + stack_size, Value::Nil);

                self.state.frames.push(Frame::Lua {
                    bottom: function_index,
                    base,
                    is_variable: false,
                    pc: 0,
                    stack_size,
                    expected_return: None,
                });
            }
            Function::Callback(callback) => {
                self.state.stack.remove(function_index);
                self.state.stack.truncate(function_index + arg_count);
                self.state.frames.push(Frame::Callback {
                    bottom: function_index,
                    callback,
                });
            }
        }
        Ok(())
    }

    // Calls the function at the given index with a constant number of arguments without
    // invalidating the function or its arguments. Returns are placed *after* the function and its
    // aruments, and all registers past this are invalidated as normal.
    pub(super) fn call_function_keep(
        self,
        ctx: Context<'gc>,
        func: RegisterIndex,
        arg_count: u8,
        returns: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            expected_return,
            is_variable,
            base,
            ..
        }) = self.state.frames.last_mut()
        else {
            panic!("top frame is not lua frame");
        };

        if *is_variable {
            return Err(VMError::ExpectedVariableStack(false));
        }

        let arg_count = arg_count as usize;

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);
        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, arg_count));

        *expected_return = Some(LuaReturn::Normal(returns));
        let function_index = *base + func.0 as usize;
        let top = function_index + 1 + arg_count;

        match meta_ops::call(ctx, self.state.stack[function_index])? {
            Function::Closure(closure) => {
                self.state.stack.resize(top + 1 + arg_count, Value::Nil);
                for i in 1..arg_count + 1 {
                    self.state.stack[top + i] = self.state.stack[function_index + i];
                }

                self.state.stack[top] = closure.into();
                let fixed_params = closure.0.proto.fixed_params as usize;
                let stack_size = closure.0.proto.stack_size as usize;

                let base = if arg_count > fixed_params {
                    self.state.stack[top + 1..].rotate_left(fixed_params);
                    top + 1 + (arg_count - fixed_params)
                } else {
                    top + 1
                };

                self.state.stack.resize(base + stack_size, Value::Nil);

                self.state.frames.push(Frame::Lua {
                    bottom: top,
                    base,
                    is_variable: false,
                    pc: 0,
                    stack_size,
                    expected_return: None,
                });
            }
            Function::Callback(callback) => {
                self.state.stack.truncate(top);
                self.state
                    .stack
                    .extend_from_within(function_index + 1..function_index + 1 + arg_count);
                self.state.frames.push(Frame::Callback {
                    bottom: top,
                    callback,
                });
            }
        }
        Ok(())
    }

    // Calls an externally defined function in a completely non-destructive way in a new frame, and
    // places an optional single result of this function call at the given register.
    //
    // Nothing at all in the frame is invalidated, other than optionally placing the return value.
    pub(super) fn call_meta_function(
        self,
        ctx: Context<'gc>,
        func: Function<'gc>,
        args: &[Value<'gc>],
        ret_index: Option<RegisterIndex>,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            expected_return,
            is_variable,
            base,
            stack_size,
            ..
        }) = self.state.frames.last_mut()
        else {
            panic!("top frame is not lua frame");
        };

        if *is_variable {
            return Err(VMError::ExpectedVariableStack(false));
        }

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);
        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, args.len()));

        *expected_return = Some(LuaReturn::Meta(ret_index));
        let top = *base + *stack_size;

        match meta_ops::call(ctx, func.into())? {
            Function::Closure(closure) => {
                self.state.stack.resize(top + 1 + args.len(), Value::Nil);
                self.state.stack[top] = closure.into();
                self.state.stack[top + 1..top + 1 + args.len()].copy_from_slice(args);

                let fixed_params = closure.0.proto.fixed_params as usize;
                let stack_size = closure.0.proto.stack_size as usize;

                let base = if args.len() > fixed_params {
                    self.state.stack[top + 1..].rotate_left(fixed_params);
                    top + 1 + (args.len() - fixed_params)
                } else {
                    top + 1
                };

                self.state.stack.resize(base + stack_size, Value::Nil);

                self.state.frames.push(Frame::Lua {
                    bottom: top,
                    base,
                    is_variable: false,
                    pc: 0,
                    stack_size,
                    expected_return: None,
                });
            }
            Function::Callback(callback) => {
                self.state.stack.extend(args);
                self.state.frames.push(Frame::Callback {
                    bottom: top,
                    callback,
                });
            }
        }
        Ok(())
    }

    // Tail-call the function at the given register with the given arguments. Pops the current Lua
    // frame, pushing a new frame for the given function.
    pub(super) fn tail_call_function(
        self,
        ctx: Context<'gc>,
        func: RegisterIndex,
        args: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            bottom,
            base,
            is_variable,
            ..
        }) = self.state.frames.pop()
        else {
            panic!("top frame is not lua frame");
        };

        if is_variable != args.is_variable() {
            return Err(VMError::ExpectedVariableStack(args.is_variable()));
        }

        self.state.close_upvalues(&ctx, bottom);

        let function_index = base + func.0 as usize;
        let arg_count = args
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(self.state.stack.len() - function_index - 1);

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);
        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, arg_count));

        match meta_ops::call(ctx, self.state.stack[function_index])? {
            Function::Closure(closure) => {
                self.state.stack[bottom] = closure.into();
                for i in 0..arg_count {
                    self.state.stack[bottom + 1 + i] = self.state.stack[function_index + 1 + i];
                }

                let fixed_params = closure.0.proto.fixed_params as usize;
                let stack_size = closure.0.proto.stack_size as usize;

                let base = if arg_count > fixed_params {
                    self.state.stack.truncate(bottom + 1 + arg_count);
                    self.state.stack[bottom + 1..].rotate_left(fixed_params);
                    bottom + 1 + (arg_count - fixed_params)
                } else {
                    bottom + 1
                };

                self.state.stack.resize(base + stack_size, Value::Nil);

                self.state.frames.push(Frame::Lua {
                    bottom,
                    base,
                    is_variable: false,
                    pc: 0,
                    stack_size,
                    expected_return: None,
                });
            }
            Function::Callback(callback) => {
                self.state
                    .stack
                    .copy_within(function_index + 1..function_index + 1 + arg_count, bottom);
                self.state.stack.truncate(bottom + arg_count);
                self.state.frames.push(Frame::Callback { bottom, callback });
            }
        }
        Ok(())
    }

    // Return to the upper frame with results starting at the given register index.
    pub(super) fn return_upper(
        self,
        mc: &Mutation<'gc>,
        start: RegisterIndex,
        count: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            bottom,
            base,
            is_variable,
            ..
        }) = self.state.frames.pop()
        else {
            panic!("top frame is not lua frame");
        };

        if is_variable != count.is_variable() {
            return Err(VMError::ExpectedVariableStack(count.is_variable()));
        }
        self.state.close_upvalues(mc, bottom);

        let start = base + start.0 as usize;
        let count = count
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(self.state.stack.len() - start);

        self.fuel.consume_fuel(Self::FUEL_PER_CALL);
        self.fuel
            .consume_fuel(count_fuel(Self::FUEL_PER_ITEM, count));

        match self.state.frames.last_mut() {
            Some(Frame::Sequence {
                bottom: seq_bottom, ..
            }) => {
                assert_eq!(bottom, *seq_bottom);
                self.state.stack.copy_within(start..start + count, bottom);
                self.state.stack.truncate(bottom + count);
            }
            Some(Frame::Lua {
                expected_return,
                is_variable,
                base,
                stack_size,
                ..
            }) => match expected_return {
                Some(LuaReturn::Normal(expected_return)) => {
                    let returning = expected_return
                        .to_constant()
                        .map(|c| c as usize)
                        .unwrap_or(count);

                    for i in 0..returning.min(count) {
                        self.state.stack[bottom + i] = self.state.stack[start + i]
                    }

                    self.state.stack.resize(bottom + returning, Value::Nil);
                    for i in count..returning {
                        self.state.stack[bottom + i] = Value::Nil;
                    }

                    if expected_return.is_variable() {
                        *is_variable = true;
                    } else {
                        self.state.stack.resize(*base + *stack_size, Value::Nil);
                        *is_variable = false;
                    }
                }
                Some(LuaReturn::Meta(meta_ind)) => {
                    let meta_ret = if count > 0 {
                        self.state.stack[start]
                    } else {
                        Value::Nil
                    };
                    self.state.stack.resize(*base + *stack_size, Value::Nil);
                    *is_variable = false;
                    if let Some(meta_ind) = meta_ind {
                        self.state.stack[*base + meta_ind.0 as usize] = meta_ret;
                    }
                }
                None => {
                    panic!("no expected returns set for returned to lua frame")
                }
            },
            None => {
                assert_eq!(bottom, 0);
                self.state.stack.copy_within(start..start + count, bottom);
                self.state.stack.truncate(bottom + count);
                self.state.frames.push(Frame::Result { bottom });
            }
            _ => panic!("lua frame must be above a sequence or lua frame"),
        }
        Ok(())
    }
}

pub(super) struct LuaRegisters<'gc, 'a> {
    pub pc: &'a mut usize,
    pub stack_frame: &'a mut [Value<'gc>],
    upper_stack: &'a mut [Value<'gc>],
    bottom: usize,
    base: usize,
    open_upvalues: &'a mut vec::Vec<UpValue<'gc>, MetricsAlloc<'gc>>,
    thread: Thread<'gc>,
}

impl<'gc, 'a> LuaRegisters<'gc, 'a> {
    pub(super) fn open_upvalue(&mut self, mc: &Mutation<'gc>, reg: RegisterIndex) -> UpValue<'gc> {
        let ind = self.base + reg.0 as usize;
        match self
            .open_upvalues
            .binary_search_by(|&u| open_upvalue_ind(u).cmp(&ind))
        {
            Ok(i) => self.open_upvalues[i],
            Err(i) => {
                let uv = UpValue(Gc::new(mc, Lock::new(UpValueState::Open(self.thread, ind))));
                self.open_upvalues.insert(i, uv);
                uv
            }
        }
    }

    pub(super) fn get_upvalue(&self, upvalue: UpValue<'gc>) -> Value<'gc> {
        match upvalue.0.get() {
            UpValueState::Open(upvalue_thread, ind) => {
                if upvalue_thread == self.thread {
                    assert!(
                        ind < self.bottom,
                        "upvalues must be above the current Lua frame"
                    );
                    self.upper_stack[ind]
                } else {
                    upvalue_thread.0.borrow().stack[ind]
                }
            }
            UpValueState::Closed(v) => v,
        }
    }

    pub(super) fn set_upvalue(
        &mut self,
        mc: &Mutation<'gc>,
        upvalue: UpValue<'gc>,
        value: Value<'gc>,
    ) {
        match upvalue.0.get() {
            UpValueState::Open(upvalue_thread, ind) => {
                if upvalue_thread == self.thread {
                    assert!(
                        ind < self.bottom,
                        "upvalues must be above the current Lua frame"
                    );
                    self.upper_stack[ind] = value;
                } else {
                    upvalue_thread.0.borrow_mut(mc).stack[ind] = value;
                }
            }
            UpValueState::Closed(v) => {
                upvalue.0.set(mc, UpValueState::Closed(v));
            }
        }
    }

    pub(super) fn close_upvalues(&mut self, mc: &Mutation<'gc>, bottom_register: RegisterIndex) {
        let bottom = self.base + bottom_register.0 as usize;
        let start = match self
            .open_upvalues
            .binary_search_by(|&u| open_upvalue_ind(u).cmp(&bottom))
        {
            Ok(i) => i,
            Err(i) => i,
        };

        for &upval in &self.open_upvalues[start..] {
            match upval.0.get() {
                UpValueState::Open(upvalue_thread, ind) => {
                    assert!(upvalue_thread == self.thread);
                    upval.0.set(
                        mc,
                        UpValueState::Closed(if ind < self.base {
                            self.upper_stack[ind]
                        } else {
                            self.stack_frame[ind - self.base]
                        }),
                    );
                }
                UpValueState::Closed(_) => panic!("upvalue is not open"),
            }
        }

        self.open_upvalues.truncate(start);
    }
}

#[derive(Debug, Collect)]
#[collect(require_static)]
enum LuaReturn {
    // Normal function call, place return values at the bottom of the returning function's stack,
    // as normal.
    Normal(VarCount),
    // Synthetic metamethod call, place an optional single return value at an index relative to the
    // returned to function's bottom.
    Meta(Option<RegisterIndex>),
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
enum Frame<'gc> {
    // A running Lua frame.
    Lua {
        bottom: usize,
        base: usize,
        is_variable: bool,
        pc: usize,
        stack_size: usize,
        expected_return: Option<LuaReturn>,
    },
    // A suspended function call that has not yet been run. Must be the only frame in the stack.
    Start(Function<'gc>),
    // Thread has yielded and is waiting resume. Must be the top frame of the stack or immediately
    // below a results frame.
    Yielded,
    // A callback that has been queued but not called yet. Must be the top frame of the stack.
    Callback {
        bottom: usize,
        callback: AnyCallback<'gc>,
    },
    // A frame for a running sequence. When it is the top frame, either the `poll` or `error` method
    // will be called on the next call to `Thread::step`, depending on whether there is a pending
    // error.
    Sequence {
        bottom: usize,
        sequence: AnySequence<'gc>,
        // Will be set when unwinding has stopped at this frame. If set, this must be the top frame
        // of the stack.
        pending_error: Option<Error<'gc>>,
    },
    // We are waiting on an upper thread to finish. Must be the top frame of the stack.
    WaitThread,
    // Results are waiting to be taken. Must be the top frame of the stack.
    Result {
        bottom: usize,
    },
    // An error is currently unwinding. Must be the top frame of the stack.
    Error(Error<'gc>),
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
struct ThreadState<'gc> {
    frames: vec::Vec<Frame<'gc>, MetricsAlloc<'gc>>,
    stack: vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
    open_upvalues: vec::Vec<UpValue<'gc>, MetricsAlloc<'gc>>,
}

impl<'gc> ThreadState<'gc> {
    fn mode(&self) -> ThreadMode {
        match self.frames.last() {
            None => {
                debug_assert!(self.stack.is_empty() && self.open_upvalues.is_empty());
                ThreadMode::Stopped
            }
            Some(frame) => match frame {
                Frame::Lua { .. } | Frame::Callback { .. } | Frame::Sequence { .. } => {
                    ThreadMode::Normal
                }
                Frame::Start(_) | Frame::Yielded => ThreadMode::Suspended,
                Frame::WaitThread => ThreadMode::Waiting,
                Frame::Result { .. } => ThreadMode::Result,
                Frame::Error(_) => {
                    if self.frames.len() == 1 {
                        ThreadMode::Result
                    } else {
                        ThreadMode::Normal
                    }
                }
            },
        }
    }

    // Pushes a function call frame, arguments start at the given stack bottom.
    fn push_call(&mut self, bottom: usize, function: Function<'gc>) {
        match function {
            Function::Closure(closure) => {
                let fixed_params = closure.0.proto.fixed_params as usize;
                let stack_size = closure.0.proto.stack_size as usize;
                let given_params = self.stack.len() - bottom;

                let var_params = if given_params > fixed_params {
                    given_params - fixed_params
                } else {
                    0
                };
                self.stack.insert(bottom, closure.into());
                self.stack[bottom + 1..].rotate_right(var_params);
                let base = bottom + 1 + var_params;

                self.stack.resize(base + stack_size, Value::Nil);

                self.frames.push(Frame::Lua {
                    bottom,
                    base,
                    is_variable: false,
                    pc: 0,
                    stack_size,
                    expected_return: None,
                });
            }
            Function::Callback(callback) => {
                self.frames.push(Frame::Callback { bottom, callback });
            }
        }
    }

    // Return to the current top frame from a popped frame. The current top frame must be a
    // sequence, lua frame, or there must be no frames at all.
    fn return_to(&mut self, bottom: usize) {
        match self.frames.last_mut() {
            Some(Frame::Sequence {
                bottom: seq_bottom, ..
            }) => assert_eq!(bottom, *seq_bottom),
            Some(Frame::Lua {
                expected_return,
                is_variable,
                base,
                stack_size,
                ..
            }) => {
                let return_len = self.stack.len() - bottom;
                match expected_return {
                    Some(LuaReturn::Normal(ret_count)) => {
                        let return_len = ret_count
                            .to_constant()
                            .map(|c| c as usize)
                            .unwrap_or(return_len);

                        self.stack.truncate(bottom + return_len);

                        *is_variable = ret_count.is_variable();
                        if !ret_count.is_variable() {
                            self.stack.resize(*base + *stack_size, Value::Nil);
                        }
                    }
                    Some(LuaReturn::Meta(meta_ind)) => {
                        let meta_ret = self.stack.get(bottom).copied().unwrap_or_default();
                        self.stack.truncate(bottom);
                        self.stack.resize(*base + *stack_size, Value::Nil);
                        *is_variable = false;
                        if let Some(meta_ind) = meta_ind {
                            self.stack[*base + meta_ind.0 as usize] = meta_ret;
                        }
                    }
                    None => panic!("no expected return set for returned to lua frame"),
                }
            }
            None => {
                self.frames.push(Frame::Result { bottom });
            }
            _ => panic!("return frame must be sequence or lua frame"),
        }
    }

    fn take_result(&mut self) -> Result<impl Iterator<Item = Value<'gc>> + '_, Error<'gc>> {
        match self.frames.pop() {
            Some(Frame::Result { bottom }) => Ok(self.stack.drain(bottom..)),
            Some(Frame::Error(err)) => {
                assert!(self.stack.is_empty());
                assert!(self.frames.is_empty());
                assert!(self.open_upvalues.is_empty());
                Err(err)
            }
            _ => panic!("no results available to take"),
        }
    }

    fn close_upvalues(&mut self, mc: &Mutation<'gc>, bottom: usize) {
        let start = match self
            .open_upvalues
            .binary_search_by(|&u| open_upvalue_ind(u).cmp(&bottom))
        {
            Ok(i) => i,
            Err(i) => i,
        };

        let this_ptr = self as *mut _;
        for &upval in &self.open_upvalues[start..] {
            match upval.0.get() {
                UpValueState::Open(upvalue_thread, ind) => {
                    assert!(upvalue_thread.0.as_ptr() == this_ptr);
                    upval.0.set(mc, UpValueState::Closed(self.stack[ind]));
                }
                UpValueState::Closed(_) => panic!("upvalue is not open"),
            }
        }

        self.open_upvalues.truncate(start);
    }
}

fn count_fuel(per_item: i32, len: usize) -> i32 {
    i32::try_from(len)
        .unwrap_or(i32::MAX)
        .saturating_mul(per_item)
}

fn open_upvalue_ind<'gc>(u: UpValue<'gc>) -> usize {
    match u.0.get() {
        UpValueState::Open(_, ind) => ind,
        UpValueState::Closed(_) => panic!("upvalue is not open"),
    }
}
