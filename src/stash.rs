use std::fmt;

use gc_arena::{DynamicRoot, DynamicRootSet, Mutation, Rootable};

use crate::{
    Callback, Closure, Error, Executor, Function, RuntimeError, String, Table, Thread, UserData,
    Value,
};

pub trait Stashable<'gc> {
    type Stashed;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed;
}

pub trait Fetchable<'gc> {
    type Fetched;

    fn fetch(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched;
}

#[derive(Clone)]
pub struct StashedTable(pub DynamicRoot<Rootable![Table<'_>]>);

impl fmt::Debug for StashedTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedTable")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Clone)]
pub struct StashedClosure(pub DynamicRoot<Rootable![Closure<'_>]>);

impl fmt::Debug for StashedClosure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedClosure")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Clone)]
pub struct StashedCallback(pub DynamicRoot<Rootable![Callback<'_>]>);

impl fmt::Debug for StashedCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedCallback")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Clone)]
pub struct StashedThread(pub DynamicRoot<Rootable![Thread<'_>]>);

impl fmt::Debug for StashedThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedThread")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Clone)]
pub struct StashedUserData(pub DynamicRoot<Rootable![UserData<'_>]>);

impl fmt::Debug for StashedUserData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedUserData")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Clone)]
pub struct StashedString(pub DynamicRoot<Rootable![String<'_>]>);

impl fmt::Debug for StashedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedString")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Clone)]
pub struct StashedExecutor(pub DynamicRoot<Rootable![Executor<'_>]>);

impl fmt::Debug for StashedExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StashedExecutor")
            .field(&self.0.as_ptr())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum StashedFunction {
    Closure(StashedClosure),
    Callback(StashedCallback),
}

impl From<StashedClosure> for StashedFunction {
    fn from(closure: StashedClosure) -> Self {
        Self::Closure(closure)
    }
}

impl From<StashedCallback> for StashedFunction {
    fn from(callback: StashedCallback) -> Self {
        Self::Callback(callback)
    }
}

#[derive(Debug, Clone)]
pub enum StashedValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StashedString),
    Table(StashedTable),
    Function(StashedFunction),
    Thread(StashedThread),
    UserData(StashedUserData),
}

impl StashedValue {
    pub fn to_bool(self) -> bool {
        match self {
            StashedValue::Nil => false,
            StashedValue::Boolean(false) => false,
            _ => true,
        }
    }

    pub fn as_primitive<'gc>(&self) -> Option<Value<'gc>> {
        match self {
            StashedValue::Nil => Some(Value::Nil),
            StashedValue::Boolean(b) => Some(Value::Boolean(*b)),
            StashedValue::Integer(i) => Some(Value::Integer(*i)),
            StashedValue::Number(n) => Some(Value::Number(*n)),
            _ => None,
        }
    }
}

impl From<bool> for StashedValue {
    fn from(v: bool) -> StashedValue {
        StashedValue::Boolean(v)
    }
}

impl From<i64> for StashedValue {
    fn from(v: i64) -> StashedValue {
        StashedValue::Integer(v)
    }
}

impl From<f64> for StashedValue {
    fn from(v: f64) -> StashedValue {
        StashedValue::Number(v)
    }
}

impl From<StashedString> for StashedValue {
    fn from(v: StashedString) -> StashedValue {
        StashedValue::String(v)
    }
}

impl From<StashedTable> for StashedValue {
    fn from(v: StashedTable) -> StashedValue {
        StashedValue::Table(v)
    }
}

impl From<StashedFunction> for StashedValue {
    fn from(v: StashedFunction) -> StashedValue {
        StashedValue::Function(v)
    }
}

impl From<StashedClosure> for StashedValue {
    fn from(v: StashedClosure) -> StashedValue {
        StashedValue::Function(StashedFunction::Closure(v))
    }
}

impl From<StashedCallback> for StashedValue {
    fn from(v: StashedCallback) -> StashedValue {
        StashedValue::Function(StashedFunction::Callback(v))
    }
}

impl From<StashedUserData> for StashedValue {
    fn from(v: StashedUserData) -> StashedValue {
        StashedValue::UserData(v)
    }
}

pub enum StashedError {
    Lua(StashedValue),
    Runtime(RuntimeError),
}

impl From<StashedValue> for StashedError {
    fn from(error: StashedValue) -> Self {
        Self::Lua(error)
    }
}

impl From<RuntimeError> for StashedError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

macro_rules! stash_type {
    ($t:ident, $r:ident) => {
        impl<'gc> Stashable<'gc> for $t<'gc> {
            type Stashed = $r;

            fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
                $r(roots.stash::<Rootable![$t<'_>]>(mc, self))
            }
        }
    };
}

stash_type!(String, StashedString);
stash_type!(Table, StashedTable);
stash_type!(Closure, StashedClosure);
stash_type!(Callback, StashedCallback);
stash_type!(Thread, StashedThread);
stash_type!(UserData, StashedUserData);
stash_type!(Executor, StashedExecutor);

macro_rules! fetch_type {
    ($r:ident, $t:ident) => {
        impl<'gc> Fetchable<'gc> for $r {
            type Fetched = $t<'gc>;

            fn fetch(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched {
                *roots.fetch::<Rootable![$t<'_>]>(&self.0)
            }
        }
    };
}

fetch_type!(StashedString, String);
fetch_type!(StashedTable, Table);
fetch_type!(StashedClosure, Closure);
fetch_type!(StashedCallback, Callback);
fetch_type!(StashedThread, Thread);
fetch_type!(StashedUserData, UserData);
fetch_type!(StashedExecutor, Executor);

impl<'gc> Stashable<'gc> for Function<'gc> {
    type Stashed = StashedFunction;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
        match self {
            Function::Closure(closure) => StashedFunction::Closure(closure.stash(mc, roots)),
            Function::Callback(callback) => StashedFunction::Callback(callback.stash(mc, roots)),
        }
    }
}

impl<'gc> Fetchable<'gc> for StashedFunction {
    type Fetched = Function<'gc>;

    fn fetch(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched {
        match self {
            StashedFunction::Closure(closure) => Function::Closure(closure.fetch(roots)),
            StashedFunction::Callback(callback) => Function::Callback(callback.fetch(roots)),
        }
    }
}

impl<'gc> Stashable<'gc> for Value<'gc> {
    type Stashed = StashedValue;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
        match self {
            Value::Nil => StashedValue::Nil,
            Value::Boolean(b) => StashedValue::Boolean(b),
            Value::Integer(i) => StashedValue::Integer(i),
            Value::Number(n) => StashedValue::Number(n),
            Value::String(s) => StashedValue::String(s.stash(mc, roots)),
            Value::Table(t) => StashedValue::Table(t.stash(mc, roots)),
            Value::Function(f) => StashedValue::Function(f.stash(mc, roots)),
            Value::Thread(t) => StashedValue::Thread(t.stash(mc, roots)),
            Value::UserData(u) => StashedValue::UserData(u.stash(mc, roots)),
        }
    }
}

impl<'gc> Fetchable<'gc> for StashedValue {
    type Fetched = Value<'gc>;

    fn fetch(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched {
        match self {
            StashedValue::Nil => Value::Nil,
            StashedValue::Boolean(b) => Value::Boolean(*b),
            StashedValue::Integer(i) => Value::Integer(*i),
            StashedValue::Number(n) => Value::Number(*n),
            StashedValue::String(s) => Value::String(s.fetch(roots)),
            StashedValue::Table(t) => Value::Table(t.fetch(roots)),
            StashedValue::Function(f) => Value::Function(f.fetch(roots)),
            StashedValue::Thread(t) => Value::Thread(t.fetch(roots)),
            StashedValue::UserData(u) => Value::UserData(u.fetch(roots)),
        }
    }
}

impl<'gc> Stashable<'gc> for Error<'gc> {
    type Stashed = StashedError;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
        match self {
            Error::Lua(err) => StashedError::Lua(err.0.stash(mc, roots)),
            Error::Runtime(err) => StashedError::Runtime(err),
        }
    }
}

impl<'gc> Fetchable<'gc> for StashedError {
    type Fetched = Error<'gc>;

    fn fetch(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched {
        match self {
            StashedError::Lua(err) => Error::from_value(err.fetch(roots)),
            StashedError::Runtime(err) => err.clone().into(),
        }
    }
}
