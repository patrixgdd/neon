//! Abstractions representing the JavaScript virtual machine and its control flow.

use std::cell::RefCell;
use std::mem;
use std::any::TypeId;
use std::convert::Into;
use std::error::Error;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::marker::PhantomData;
use std::collections::HashMap;
use std::os::raw::c_void;
use std::panic::UnwindSafe;
use neon_runtime;
use neon_runtime::raw;
use neon_runtime::call::CCallback;
use js::{JsValue, Value, Object, JsObject, JsArray, JsFunction, JsBoolean, JsNumber, JsString, StringResult, JsNull, JsUndefined, Ref, RefMut, Borrow, BorrowMut};
use js::binary::{JsArrayBuffer, JsBuffer};
use js::class::internal::ClassMetadata;
use js::class::Class;
use js::error::{JsError, Kind};
use mem::{Handle, Managed};
use self::internal::{Ledger, ContextInternal, Scope, ScopeMetadata};

pub(crate) mod internal {
    use std::cell::Cell;
    use std::mem;
    use std::collections::HashSet;
    use std::os::raw::c_void;
    use neon_runtime;
    use neon_runtime::raw;
    use neon_runtime::scope::Root;
    use mem::Handle;
    use vm::VmResult;
    use js::{JsObject, LoanError};
    use super::{ClassMap, ModuleContext};

    pub unsafe trait Pointer {
        unsafe fn as_ptr(&self) -> *const c_void;
        unsafe fn as_mut(&mut self) -> *mut c_void;
    }

    unsafe impl<T> Pointer for *mut T {
        unsafe fn as_ptr(&self) -> *const c_void {
            *self as *const c_void
        }

        unsafe fn as_mut(&mut self) -> *mut c_void {
            *self as *mut c_void
        }
    }
    unsafe impl<'a, T> Pointer for &'a mut T {
        unsafe fn as_ptr(&self) -> *const c_void {
            let r: &T = &**self;
            mem::transmute(r)
        }

        unsafe fn as_mut(&mut self) -> *mut c_void {
            let r: &mut T = &mut **self;
            mem::transmute(r)
        }
    }

    pub struct Ledger {
        immutable_loans: HashSet<*const c_void>,
        mutable_loans: HashSet<*const c_void>
    }

    impl Ledger {
        pub fn new() -> Self {
            Ledger {
                immutable_loans: HashSet::new(),
                mutable_loans: HashSet::new()
            }
        }

        pub fn try_borrow<T>(&mut self, p: *const T) -> Result<(), LoanError> {
            let p = p as *const c_void;
            if self.mutable_loans.contains(&p) {
                return Err(LoanError::Mutating(p));
            }
            self.immutable_loans.insert(p);
            Ok(())
        }

        pub fn settle<T>(&mut self, p: *const T) {
            let p = p as *const c_void;
            self.immutable_loans.remove(&p);
        }

        pub fn try_borrow_mut<T>(&mut self, p: *mut T) -> Result<(), LoanError> {
            let p = p as *const c_void;
            if self.mutable_loans.contains(&p) {
                return Err(LoanError::Mutating(p));
            } else if self.immutable_loans.contains(&p) {
                return Err(LoanError::Frozen(p));
            }
            self.mutable_loans.insert(p);
            Ok(())
        }

        pub fn settle_mut<T>(&mut self, p: *mut T) {
            let p = p as *const c_void;
            self.mutable_loans.remove(&p);
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Isolate(*mut raw::Isolate);

    extern "C" fn drop_class_map(map: Box<ClassMap>) {
        mem::drop(map);
    }

    impl Isolate {
        pub(crate) fn to_raw(self) -> *mut raw::Isolate {
            let Isolate(ptr) = self;
            ptr
        }

        pub(crate) fn class_map(&mut self) -> &mut ClassMap {
            let mut ptr: *mut c_void = unsafe { neon_runtime::class::get_class_map(self.to_raw()) };
            if ptr.is_null() {
                let b: Box<ClassMap> = Box::new(ClassMap::new());
                let raw = Box::into_raw(b);
                ptr = unsafe { mem::transmute(raw) };
                let free_map: *mut c_void = unsafe { mem::transmute(drop_class_map as usize) };
                unsafe {
                    neon_runtime::class::set_class_map(self.to_raw(), ptr, free_map);
                }
            }
            unsafe { mem::transmute(ptr) }
        }

        pub(crate) fn current() -> Isolate {
            unsafe {
                mem::transmute(neon_runtime::call::current_isolate())
            }
        }
    }

    pub struct ScopeMetadata {
        isolate: Isolate,
        active: Cell<bool>
    }

    pub struct Scope<'a, R: Root + 'static> {
        pub metadata: ScopeMetadata,
        pub handle_scope: &'a mut R
    }

    impl<'a, R: Root + 'static> Scope<'a, R> {
        pub fn with<T, F: for<'b> FnOnce(Scope<'b, R>) -> T>(f: F) -> T {
            let mut handle_scope: R = unsafe { R::allocate() };
            let isolate = Isolate::current();
            unsafe {
                handle_scope.enter(isolate.to_raw());
            }
            let result = {
                let scope = Scope {
                    metadata: ScopeMetadata {
                        isolate,
                        active: Cell::new(true)
                    },
                    handle_scope: &mut handle_scope
                };
                f(scope)
            };
            unsafe {
                handle_scope.exit();
            }
            result
        }
    }

    pub trait ContextInternal<'a>: Sized {
        fn scope_metadata(&self) -> &ScopeMetadata;

        fn isolate(&self) -> Isolate {
            self.scope_metadata().isolate
        }

        fn is_active(&self) -> bool {
            self.scope_metadata().active.get()
        }

        fn check_active(&self) {
            if !self.is_active() {
                panic!("VM context is inactive");
            }
        }

        fn activate(&self) { self.scope_metadata().active.set(true); }
        fn deactivate(&self) { self.scope_metadata().active.set(false); }
    }

    pub fn initialize_module(exports: Handle<JsObject>, init: fn(ModuleContext) -> VmResult<()>) {
        ModuleContext::with(exports, |cx| {
            let _ = init(cx);
        });
    }
}

/// An error sentinel type used by `VmResult` (and `JsResult`) to indicate that the JS VM has entered into a throwing state.
#[derive(Debug)]
pub struct Throw;

impl Display for Throw {
    fn fmt(&self, fmt: &mut Formatter) -> FmtResult {
        fmt.write_str("JavaScript Error")
    }
}

impl Error for Throw {
    fn description(&self) -> &str {
        "javascript error"
    }
}

/// The result of a computation that might send the JS VM into a throwing state.
pub type VmResult<T> = Result<T, Throw>;

/// The result of a computation that produces a JS value and might send the JS VM into a throwing state.
pub type JsResult<'b, T> = VmResult<Handle<'b, T>>;

/// An extension trait for `Result` values that can be converted into `JsResult` values by throwing a JavaScript
/// exception in the error case.
pub trait JsResultExt<'a, V: Value> {
    fn unwrap_or_throw<'b, C: Context<'b>>(self, cx: &mut C) -> JsResult<'a, V>;
}

pub(crate) struct ClassMap {
    map: HashMap<TypeId, ClassMetadata>
}

impl ClassMap {
    fn new() -> ClassMap {
        ClassMap {
            map: HashMap::new()
        }
    }

    pub fn get(&self, key: &TypeId) -> Option<&ClassMetadata> {
        self.map.get(key)
    }

    pub fn set(&mut self, key: TypeId, val: ClassMetadata) {
        self.map.insert(key, val);
    }
}

#[repr(C)]
pub(crate) struct CallbackInfo {
    info: raw::FunctionCallbackInfo
}

impl CallbackInfo {
    pub fn data<'a>(&self) -> Handle<'a, JsValue> {
        unsafe {
            let mut local: raw::Local = mem::zeroed();
            neon_runtime::call::data(&self.info, &mut local);
            Handle::new_internal(JsValue::from_raw(local))
        }
    }

    pub unsafe fn with_cx<T: This, U, F: for<'a> FnOnce(CallContext<'a, T>) -> U>(&self, f: F) -> U {
        CallContext::<T>::with(self, f)
    }

    pub fn set_return<'a, 'b, T: Value>(&'a self, value: Handle<'b, T>) {
        unsafe {
            neon_runtime::call::set_return(&self.info, value.to_raw())
        }
    }

    fn kind(&self) -> CallKind {
        if unsafe { neon_runtime::call::is_construct(mem::transmute(self)) } {
            CallKind::Construct
        } else {
            CallKind::Call
        }
    }

    pub fn len(&self) -> i32 {
        unsafe {
            neon_runtime::call::len(&self.info)
        }
    }

    pub fn get<'b, C: Context<'b>>(&self, _: &mut C, i: i32) -> Option<Handle<'b, JsValue>> {
        if i < 0 || i >= self.len() {
            return None;
        }
        unsafe {
            let mut local: raw::Local = mem::zeroed();
            neon_runtime::call::get(&self.info, i, &mut local);
            Some(Handle::new_internal(JsValue::from_raw(local)))
        }
    }

    pub fn require<'b, C: Context<'b>>(&self, cx: &mut C, i: i32) -> JsResult<'b, JsValue> {
        if i < 0 || i >= self.len() {
            return JsError::throw(cx, Kind::TypeError, "not enough arguments");
        }
        unsafe {
            let mut local: raw::Local = mem::zeroed();
            neon_runtime::call::get(&self.info, i, &mut local);
            Ok(Handle::new_internal(JsValue::from_raw(local)))
        }
    }

    pub fn this<'b, V: Context<'b>>(&self, _: &mut V) -> raw::Local {
        unsafe {
            let mut local: raw::Local = mem::zeroed();
            neon_runtime::call::this(mem::transmute(&self.info), &mut local);
            local
        }
    }
}

/// The trait of types that can be a function's `this` binding.
pub unsafe trait This: Managed {
    fn as_this(h: raw::Local) -> Self;
}

/// Indicates whether a function call was called with JavaScript's `[[Call]]` or `[[Construct]]` semantics.
#[derive(Clone, Copy, Debug)]
pub enum CallKind {
    Construct,
    Call
}

/// An RAII implementation of a "scoped lock" of the JS VM. When this structure is dropped (falls out of scope), the VM will be unlocked.
///
/// Types of JS values that support the `Borrow` and `BorrowMut` traits can be inspected while the VM is locked by passing a reference to a `VmGuard` to their methods.
pub struct VmGuard<'a> {
    pub(crate) ledger: RefCell<Ledger>,
    phantom: PhantomData<&'a ()>
}

impl<'a> VmGuard<'a> {
    fn new() -> Self {
        VmGuard {
            ledger: RefCell::new(Ledger::new()),
            phantom: PhantomData
        }
    }
}

/// A contextual view of the JS VM. Most operations that interact with the VM require passing a reference to a VM context.
/// 
/// A VM context has a lifetime `'a`, which tracks the rooting of handles managed by the JS garbage collector. All handles created during the lifetime of a context are rooted for that duration and cannot outlive the context.
pub trait Context<'a>: ContextInternal<'a> {

    /// Lock the JS VM, returning an RAII guard that keeps the lock active as long as the guard is alive.
    /// 
    /// If this is not the currently active context (for example, if it was used to spawn a scoped context with `execute_scoped` or `compute_scoped`), this method will panic.
    fn lock(&self) -> VmGuard {
        self.check_active();
        VmGuard::new()
    }

    /// Convenience method for locking the VM and borrowing a single JS value's internals.
    /// 
    /// # Example:
    /// 
    /// ```no_run
    /// use neon::js::{JsNumber, Borrow, Ref};
    /// use neon::js::binary::JsArrayBuffer;
    /// # use neon::vm::{JsResult, FunctionContext};
    /// use neon::vm::Context;
    /// use neon::mem::Handle;
    /// 
    /// # fn my_neon_function(mut cx: FunctionContext) -> JsResult<JsNumber> {
    /// let b: Handle<JsArrayBuffer> = cx.argument(0)?;
    /// let x: u32 = cx.borrow(&b, |data| { data.as_slice()[0] });
    /// let n: Handle<JsNumber> = cx.number(x);
    /// # Ok(n)
    /// # }
    /// ```
    /// 
    /// Note: the borrowed value is required to be a reference to a handle instead of a handle
    /// as a workaround for a [Rust compiler bug](https://github.com/rust-lang/rust/issues/29997).
    /// We may be able to generalize this compatibly in the future when the Rust bug is fixed,
    /// but while the extra `&` is a small ergonomics regression, this API is still a nice
    /// convenience.
    fn borrow<'c, V, T, F>(&self, v: &'c Handle<V>, f: F) -> T
        where V: Value,
              &'c V: Borrow,
              F: for<'b> FnOnce(Ref<'b, <&'c V as Borrow>::Target>) -> T
    {
        let guard = self.lock();
        let contents = v.borrow(&guard);
        f(contents)
    }

    /// Convenience method for locking the VM and mutably borrowing a single JS value's internals.
    /// 
    /// # Example:
    /// 
    /// ```no_run
    /// use neon::js::{BorrowMut, RefMut};
    /// # use neon::js::JsUndefined;
    /// use neon::js::binary::JsArrayBuffer;
    /// # use neon::vm::{JsResult, FunctionContext};
    /// use neon::vm::Context;
    /// use neon::mem::Handle;
    /// 
    /// # fn my_neon_function(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    /// let mut b: Handle<JsArrayBuffer> = cx.argument(0)?;
    /// cx.borrow_mut(&mut b, |data| {
    ///     let slice = data.as_mut_slice::<u32>();
    ///     slice[0] += 1;
    /// });
    /// # Ok(cx.undefined())
    /// # }
    /// ```
    /// 
    /// Note: the borrowed value is required to be a reference to a handle instead of a handle
    /// as a workaround for a [Rust compiler bug](https://github.com/rust-lang/rust/issues/29997).
    /// We may be able to generalize this compatibly in the future when the Rust bug is fixed,
    /// but while the extra `&mut` is a small ergonomics regression, this API is still a nice
    /// convenience.
    fn borrow_mut<'c, V, T, F>(&self, v: &'c mut Handle<V>, f: F) -> T
        where V: Value,
              &'c mut V: BorrowMut,
              F: for<'b> FnOnce(RefMut<'b, <&'c mut V as Borrow>::Target>) -> T
    {
        let guard = self.lock();
        let contents = v.borrow_mut(&guard);
        f(contents)
    }

    /// Executes a computation in a new memory management scope.
    /// 
    /// Handles created in the new scope are rooted for the duration of the computation and cannot escape.
    /// 
    /// This method can be useful for limiting the life of temporary values created during long-running computations, to prevent leaks.
    fn execute_scoped<T, F>(&self, f: F) -> T
        where F: for<'b> FnOnce(ExecuteContext<'b>) -> T
    {
        self.check_active();
        self.deactivate();
        let result = ExecuteContext::with(f);
        self.activate();
        result
    }

    /// Executes a computation in a new memory management scope and computes a single result value that outlives the computation.
    /// 
    /// Handles created in the new scope are rooted for the duration of the computation and cannot escape, with the exception of the result value, which is rooted in the current context.
    /// 
    /// This method can be useful for limiting the life of temporary values created during long-running computations, to prevent leaks.
    fn compute_scoped<V, F>(&self, f: F) -> JsResult<'a, V>
        where V: Value,
              F: for<'b, 'c> FnOnce(ComputeContext<'b, 'c>) -> JsResult<'b, V>
    {
        self.check_active();
        self.deactivate();
        let result = ComputeContext::with(|cx| {
            unsafe {
                let escapable_handle_scope = cx.scope.handle_scope as *mut raw::EscapableHandleScope;
                let escapee = f(cx)?;
                let mut result_local: raw::Local = mem::zeroed();
                neon_runtime::scope::escape(&mut result_local, escapable_handle_scope, escapee.to_raw());
                Ok(Handle::new_internal(V::from_raw(result_local)))
            }
        });
        self.activate();
        result
    }

    /// Convenience method for creating a `JsBoolean` value.
    fn boolean(&mut self, b: bool) -> Handle<'a, JsBoolean> {
        JsBoolean::new(self, b)
    }

    /// Convenience method for creating a `JsNumber` value.
    fn number<T: Into<f64>>(&mut self, x: T) -> Handle<'a, JsNumber> {
        JsNumber::new(self, x.into())
    }

    /// Convenience method for creating a `JsString` value.
    /// 
    /// If the string exceeds the limits of the JS VM, this method panics.
    fn string<S: AsRef<str>>(&mut self, s: S) -> Handle<'a, JsString> {
        JsString::new(self, s)
    }

    /// Convenience method for creating a `JsString` value.
    /// 
    /// If the string exceeds the limits of the JS VM, this method returns an `Err` value.
    fn try_string<S: AsRef<str>>(&mut self, s: S) -> StringResult<'a> {
        JsString::try_new(self, s)
    }

    /// Convenience method for creating a `JsNull` value.
    fn null(&mut self) -> Handle<'a, JsNull> {
        JsNull::new()
    }

    /// Convenience method for creating a `JsUndefined` value.
    fn undefined(&mut self) -> Handle<'a, JsUndefined> {
        JsUndefined::new()
    }

    /// Convenience method for creating an empty `JsObject` value.
    fn empty_object(&mut self) -> Handle<'a, JsObject> {
        JsObject::new(self)
    }

    /// Convenience method for creating an empty `JsArray` value.
    fn empty_array(&mut self) -> Handle<'a, JsArray> {
        JsArray::new(self, 0)
    }

    /// Convenience method for creating an empty `JsArrayBuffer` value.
    fn array_buffer(&mut self, size: u32) -> JsResult<'a, JsArrayBuffer> {
        JsArrayBuffer::new(self, size)
    }

    /// Convenience method for creating an empty `JsBuffer` value.
    fn buffer(&mut self, size: u32) -> JsResult<'a, JsBuffer> {
        JsBuffer::new(self, size)
    }

    /// Produces a handle to the JavaScript global object.
    fn global(&mut self) -> Handle<'a, JsObject> {
        JsObject::build(|out| {
            unsafe {
                neon_runtime::scope::get_global(self.isolate().to_raw(), out);
            }
        })
    }
}

/// A view of the JS VM in the context of top-level initialization of a Neon module.
pub struct ModuleContext<'a> {
    scope: Scope<'a, raw::HandleScope>,
    exports: Handle<'a, JsObject>
}

impl<'a> UnwindSafe for ModuleContext<'a> { }

impl<'a> ModuleContext<'a> {
    pub(crate) fn with<T, F: for<'b> FnOnce(ModuleContext<'b>) -> T>(exports: Handle<'a, JsObject>, f: F) -> T {
        debug_assert!(unsafe { neon_runtime::scope::size() } <= mem::size_of::<raw::HandleScope>());
        debug_assert!(unsafe { neon_runtime::scope::alignment() } <= mem::align_of::<raw::HandleScope>());
        Scope::with(|scope| {
            f(ModuleContext {
                scope,
                exports
            })
        })
    }

    /// Convenience method for exporting a Neon function from a module.
    pub fn export_function<T: Value>(&mut self, key: &str, f: fn(FunctionContext) -> JsResult<T>) -> VmResult<()> {
        let value = JsFunction::new(self, f)?.upcast::<JsValue>();
        self.exports.set(self, key, value)?;
        Ok(())
    }

    /// Convenience method for exporting a Neon class constructor from a module.
    pub fn export_class<T: Class>(&mut self, key: &str) -> VmResult<()> {
        let constructor = T::constructor(self)?;
        self.exports.set(self, key, constructor)?;
        Ok(())
    }

    /// Exports a JavaScript value from a Neon module.
    pub fn export_value<T: Value>(&mut self, key: &str, val: Handle<T>) -> VmResult<()> {
        self.exports.set(self, key, val)?;
        Ok(())
    }

    /// Produces a handle to a module's exports object.
    pub fn exports_object(&mut self) -> JsResult<'a, JsObject> {
        Ok(self.exports)
    }
}

impl<'a> ContextInternal<'a> for ModuleContext<'a> {
    fn scope_metadata(&self) -> &ScopeMetadata {
        &self.scope.metadata
    }
}

impl<'a> Context<'a> for ModuleContext<'a> { }

/// A view of the JS VM in the context of a scoped computation started by `Context::execute_scoped()`.
pub struct ExecuteContext<'a> {
    scope: Scope<'a, raw::HandleScope>
}

impl<'a> ExecuteContext<'a> {
    pub(crate) fn with<T, F: for<'b> FnOnce(ExecuteContext<'b>) -> T>(f: F) -> T {
        Scope::with(|scope| {
            f(ExecuteContext { scope })
        })
    }
}

impl<'a> ContextInternal<'a> for ExecuteContext<'a> {
    fn scope_metadata(&self) -> &ScopeMetadata {
        &self.scope.metadata
    }
}

impl<'a> Context<'a> for ExecuteContext<'a> { }

/// A view of the JS VM in the context of a scoped computation started by `Context::compute_scoped()`.
pub struct ComputeContext<'a, 'outer> {
    scope: Scope<'a, raw::EscapableHandleScope>,
    phantom_inner: PhantomData<&'a ()>,
    phantom_outer: PhantomData<&'outer ()>
}

impl<'a, 'b> ComputeContext<'a, 'b> {
    pub(crate) fn with<T, F: for<'c, 'd> FnOnce(ComputeContext<'c, 'd>) -> T>(f: F) -> T {
        Scope::with(|scope| {
            f(ComputeContext {
                scope,
                phantom_inner: PhantomData,
                phantom_outer: PhantomData
            })
        })
    }
}

impl<'a, 'b> ContextInternal<'a> for ComputeContext<'a, 'b> {
    fn scope_metadata(&self) -> &ScopeMetadata {
        &self.scope.metadata
    }
}

impl<'a, 'b> Context<'a> for ComputeContext<'a, 'b> { }

/// A view of the JS VM in the context of a function call.
/// 
/// The type parameter `T` is the type of the `this`-binding.
pub struct CallContext<'a, T: This> {
    scope: Scope<'a, raw::HandleScope>,
    info: &'a CallbackInfo,
    phantom_type: PhantomData<T>
}

impl<'a, T: This> UnwindSafe for CallContext<'a, T> { }

impl<'a, T: This> CallContext<'a, T> {
    /// Indicates whether the function was called via the JavaScript `[[Call]]` or `[[Construct]]` semantics.
    pub fn kind(&self) -> CallKind { self.info.kind() }

    pub(crate) fn with<U, F: for<'b> FnOnce(CallContext<'b, T>) -> U>(info: &'a CallbackInfo, f: F) -> U {
        Scope::with(|scope| {
            f(CallContext {
                scope,
                info,
                phantom_type: PhantomData
            })
        })
    }

    /// Indicates the number of arguments that were passed to the function.
    pub fn len(&self) -> i32 { self.info.len() }

    /// Produces the `i`th argument, or `None` if `i` is greater than or equal to `self.len()`.
    pub fn argument_opt(&mut self, i: i32) -> Option<Handle<'a, JsValue>> {
        self.info.get(self, i)
    }

    /// Produces the `i`th argument and casts it to the type `V`, or throws an exception if `i` is greater than or equal to `self.len()` or cannot be cast to `V`.
    pub fn argument<V: Value>(&mut self, i: i32) -> JsResult<'a, V> {
        let a = self.info.require(self, i)?;
        a.downcast().unwrap_or_throw(self)
    }

    /// Produces a handle to the `this`-binding.
    pub fn this(&mut self) -> Handle<'a, T> {
        Handle::new_internal(T::as_this(self.info.this(self)))
    }
}

impl<'a, T: This> ContextInternal<'a> for CallContext<'a, T> {
    fn scope_metadata(&self) -> &ScopeMetadata {
        &self.scope.metadata
    }
}

impl<'a, T: This> Context<'a> for CallContext<'a, T> { }

/// A shorthand for a `CallContext` with `this`-type `JsObject`.
pub type FunctionContext<'a> = CallContext<'a, JsObject>;

/// An alias for `CallContext`, useful for indicating that the function is a method of a class.
pub type MethodContext<'a, T> = CallContext<'a, T>;

/// A view of the JS VM in the context of a task completion callback.
pub struct TaskContext<'a> {
    /// We use an "inherited HandleScope" here because the C++ `neon::Task::complete`
    /// method sets up and tears down a `HandleScope` for us.
    scope: Scope<'a, raw::InheritedHandleScope>
}

impl<'a> TaskContext<'a> {
    pub(crate) fn with<T, F: for<'b> FnOnce(TaskContext<'b>) -> T>(f: F) -> T {
        Scope::with(|scope| {
            f(TaskContext { scope })
        })
    }
}

impl<'a> ContextInternal<'a> for TaskContext<'a> {
    fn scope_metadata(&self) -> &ScopeMetadata {
        &self.scope.metadata
    }
}

impl<'a> Context<'a> for TaskContext<'a> { }

/// A dynamically computed callback that can be passed through C to the JS VM.
/// This type makes it possible to export a dynamically computed Rust function
/// as a pair of 1) a raw pointer to the dynamically computed function, and 2)
/// a static function that knows how to transmute that raw pointer and call it.
pub(crate) trait Callback<T: Clone + Copy + Sized>: Sized {

    /// Extracts the computed Rust function and invokes it. The Neon runtime
    /// ensures that the computed function is provided as the extra data field,
    /// wrapped as a V8 External, in the `CallbackInfo` argument.
    extern "C" fn invoke(info: &CallbackInfo) -> T;

    /// Converts the callback to a raw void pointer.
    fn as_ptr(self) -> *mut c_void;

    /// Exports the callback as a pair consisting of the static `Self::invoke`
    /// method and the computed callback, both converted to raw void pointers.
    fn into_c_callback(self) -> CCallback {
        CCallback {
            static_callback: unsafe { mem::transmute(Self::invoke as usize) },
            dynamic_callback: self.as_ptr()
        }
    }
}
