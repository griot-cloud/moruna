//! libmoruna_function: the six calls of `moruna_function.h` (architecture/moruna-os-system.md,
//! 5B), over moruna-function (5A).
//!
//! Each call is a call of the SDK's task driven a call at a time (`moruna_function::task`),
//! which keeps the ring protocol, the record check and the exit codes as `serve` does. Batches
//! cross by the Arrow C Data Interface through arrow-rs's FFI: a batch from `moruna_next` is the
//! record read in place and exported, its buffers the engine's memory, no copy made; a batch
//! given to `moruna_emit` is imported, its buffers the function's, and copied once into a window.
//!
//! A function hands this library structures it cannot check in full. Before arrow-rs reads one,
//! each structure's pointers and counts are checked against its type, what arrow-rs would
//! otherwise assert on; the imported array is then validated in full. A pointer to memory that
//! is not what the structure says it is remains the function's own fault.

use std::ffi::{CStr, c_char, c_int};
use std::marker::{PhantomData, PhantomPinned};
use std::sync::Arc;

use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema, from_ffi_and_data_type};
use arrow_array::{Array, RecordBatch, RecordBatchOptions, StructArray};
use arrow_data::layout;
use arrow_schema::{DataType, Schema};
use sdk::task::Task;
use sdk::{FunctionError, code};

/// The header's `struct moruna_task`: a task the function holds a pointer to, and only that.
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct moruna_task {
    _opaque: [u8; 0],
    _pinned: PhantomData<(*mut u8, PhantomPinned)>,
}

/// How deep a schema or an array may nest before it is refused: the checks, and arrow-rs after
/// them, recurse.
const DEPTH: usize = 64;

/// An exit code as `moruna_finish` returns it.
fn exit_code(code: i64) -> c_int {
    c_int::try_from(code).unwrap_or(c_int::MAX)
}

/// An exit code as a failed call returns it.
fn negated(code: i64) -> c_int {
    -exit_code(code)
}

/// The task behind `task`, `None` for a null pointer.
///
/// # Safety
///
/// `task` is null or came from `moruna_open` and was not given to `moruna_finish` since.
unsafe fn task_mut<'a>(task: *mut moruna_task) -> Option<&'a mut Task> {
    // SAFETY: as the function's contract says.
    unsafe { task.cast::<Task>().as_mut() }
}

/// Attaches the program to its task's rings, once.
///
/// # Safety
///
/// `task` is null or valid for a write of a pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn moruna_open(task: *mut *mut moruna_task) -> c_int {
    if task.is_null() {
        sdk::log("moruna_open: task is NULL, and the task is written there");
        return negated(code::REFUSED);
    }
    let (opened, r) = match Task::open() {
        Ok(t) => (Box::into_raw(Box::new(t)).cast::<moruna_task>(), 0),
        Err(code) => (std::ptr::null_mut(), negated(code)),
    };
    // SAFETY: the caller's contract.
    unsafe { task.write(opened) };
    r
}

/// The batch's struct array and schema, exported.
fn export(batch: RecordBatch) -> Result<(FFI_ArrowArray, FFI_ArrowSchema), FunctionError> {
    let schema = FFI_ArrowSchema::try_from(batch.schema_ref().as_ref())
        .map_err(|e| FunctionError::new(format!("moruna_next: the batch's schema: {e}")))?;
    let array = FFI_ArrowArray::new(&StructArray::from(batch).into_data());
    Ok((array, schema))
}

/// The next batch: 1 with it in `*out` and `*schema`, 0 at the engine's end, the exit code
/// negated once the task has failed.
///
/// # Safety
///
/// `task` is null or from `moruna_open`; `out` and `schema` are each null or valid for a write
/// of their structure, whose old contents are not released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn moruna_next(
    task: *mut moruna_task,
    out: *mut FFI_ArrowArray,
    schema: *mut FFI_ArrowSchema,
) -> c_int {
    // Released, so a function that reads them after an error finds nothing to release.
    if !out.is_null() {
        // SAFETY: the caller's contract.
        unsafe { out.write(FFI_ArrowArray::empty()) };
    }
    if !schema.is_null() {
        // SAFETY: the caller's contract.
        unsafe { schema.write(FFI_ArrowSchema::empty()) };
    }
    // SAFETY: the caller's contract.
    let Some(task) = (unsafe { task_mut(task) }) else {
        return negated(code::NO_PLANE);
    };
    if let Some(code) = task.failed() {
        return negated(code);
    }
    if out.is_null() || schema.is_null() {
        let e = FunctionError::new("moruna_next: out or schema is NULL, and the batch goes there");
        return negated(task.fail(&e));
    }
    let batch = match task.next_batch() {
        Ok(Some(batch)) => batch,
        Ok(None) => return 0,
        Err(code) => return negated(code),
    };
    match export(batch) {
        Ok((a, s)) => {
            // SAFETY: checked non-null above; the caller's contract.
            unsafe {
                out.write(a);
                schema.write(s);
            }
            1
        }
        Err(e) => negated(task.fail(&e)),
    }
}

/// What arrow-rs asserts on or reads blindly in a schema, checked: each format and child pointer.
///
/// # Safety
///
/// Every pointer the structure holds is null or points to what the C Data Interface says.
unsafe fn check_schema(s: &FFI_ArrowSchema, depth: usize) -> Result<(), String> {
    if depth > DEPTH {
        return Err(format!("the schema nests deeper than {DEPTH}"));
    }
    if s.format.is_null() {
        return Err(String::from("a schema has no format"));
    }
    let n = usize::try_from(s.n_children)
        .map_err(|_| format!("a schema has {} children", s.n_children))?;
    if n > 0 && s.children.is_null() {
        return Err(String::from("a schema's children are NULL"));
    }
    for i in 0..n {
        // SAFETY: `children` holds `n` pointers, by the caller's contract.
        let child = unsafe { *s.children.add(i) };
        // SAFETY: null or a schema, by the caller's contract.
        let Some(child) = (unsafe { child.as_ref() }) else {
            return Err(format!("a schema's child {i} is NULL"));
        };
        // SAFETY: the caller's contract, for the child.
        unsafe { check_schema(child, depth + 1) }?;
    }
    // SAFETY: null or a schema, by the caller's contract.
    if let Some(d) = unsafe { s.dictionary.as_ref() } {
        // SAFETY: the caller's contract, for the dictionary.
        unsafe { check_schema(d, depth + 1) }?;
    }
    Ok(())
}

/// The child types an array of `t` has.
fn child_types(t: &DataType) -> Vec<&DataType> {
    match t {
        DataType::List(f)
        | DataType::LargeList(f)
        | DataType::ListView(f)
        | DataType::LargeListView(f)
        | DataType::FixedSizeList(f, _)
        | DataType::Map(f, _) => vec![f.data_type()],
        DataType::Struct(fields) => fields.iter().map(|f| f.data_type()).collect(),
        DataType::Union(fields, _) => fields.iter().map(|(_, f)| f.data_type()).collect(),
        DataType::RunEndEncoded(ends, values) => vec![ends.data_type(), values.data_type()],
        _ => Vec::new(),
    }
}

/// What arrow-rs asserts on in an array of `t`, checked: its counts, buffers, children and
/// dictionary against the type.
///
/// # Safety
///
/// Every pointer the structure holds is null or points to what the C Data Interface says.
unsafe fn check_array(a: &FFI_ArrowArray, t: &DataType, depth: usize) -> Result<(), String> {
    if depth > DEPTH {
        return Err(format!("the array nests deeper than {DEPTH}"));
    }
    if a.length < 0 || a.offset < 0 || a.null_count < -1 {
        return Err(format!(
            "an array of {t} has length {}, offset {} and null count {}",
            a.length, a.offset, a.null_count
        ));
    }
    let l = layout(t);
    let want = l.buffers.len() + usize::from(l.can_contain_null_mask);
    let have = usize::try_from(a.n_buffers).unwrap_or(usize::MAX);
    if if l.variadic {
        have <= want
    } else {
        have != want
    } {
        return Err(format!(
            "an array of {t} has {} buffers, where it has {}{want}",
            a.n_buffers,
            if l.variadic { "more than " } else { "" }
        ));
    }
    if have > 0 && a.buffers.is_null() {
        return Err(format!("an array of {t} has its buffers NULL"));
    }
    let children = child_types(t);
    if usize::try_from(a.n_children).ok() != Some(children.len()) {
        return Err(format!(
            "an array of {t} has {} children, where it has {}",
            a.n_children,
            children.len()
        ));
    }
    if !children.is_empty() && a.children.is_null() {
        return Err(format!("an array of {t} has its children NULL"));
    }
    for (i, ct) in children.into_iter().enumerate() {
        // SAFETY: `children` holds `n_children` pointers, by the caller's contract.
        let child = unsafe { *a.children.add(i) };
        // SAFETY: null or an array, by the caller's contract.
        let Some(child) = (unsafe { child.as_ref() }) else {
            return Err(format!("an array of {t} has child {i} NULL"));
        };
        // SAFETY: the caller's contract, for the child.
        unsafe { check_array(child, ct, depth + 1) }?;
    }
    if let DataType::Dictionary(_, values) = t {
        // SAFETY: null or an array, by the caller's contract.
        let Some(d) = (unsafe { a.dictionary.as_ref() }) else {
            return Err(format!("an array of {t} has no dictionary"));
        };
        // SAFETY: the caller's contract, for the dictionary.
        unsafe { check_array(d, values, depth + 1) }?;
    }
    Ok(())
}

/// The batch `array` holds, its fields `schema`'s.
///
/// # Safety
///
/// Every pointer the structures hold is null or points to what the C Data Interface says.
unsafe fn import(array: FFI_ArrowArray, schema: &FFI_ArrowSchema) -> Result<RecordBatch, String> {
    // SAFETY: the caller's contract.
    unsafe { check_schema(schema, 0) }?;
    let schema = Schema::try_from(schema).map_err(|e| format!("the schema: {e}"))?;
    let t = DataType::Struct(schema.fields().clone());
    // SAFETY: the caller's contract.
    unsafe { check_array(&array, &t, 0) }?;
    // SAFETY: checked against its type above, and the caller's contract for the rest.
    let data = unsafe { from_ffi_and_data_type(array, t) }.map_err(|e| e.to_string())?;
    data.validate_full().map_err(|e| e.to_string())?;
    let rows = data.len();
    let (_, columns, nulls) = StructArray::from(data).into_parts();
    if nulls.is_some_and(|n| n.null_count() > 0) {
        return Err(String::from(
            "the struct array has nulls, and every row of a batch is there",
        ));
    }
    let options = RecordBatchOptions::new().with_row_count(Some(rows));
    RecordBatch::try_new_with_options(Arc::new(schema), columns, &options)
        .map_err(|e| e.to_string())
}

/// `*p` moved out, `None` when `p` is null or the structure is released.
///
/// # Safety
///
/// `p` is null or valid for reads and writes of an initialized array.
unsafe fn take_array(p: *mut FFI_ArrowArray) -> Option<FFI_ArrowArray> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's contract; the source is left released.
    let a = unsafe { FFI_ArrowArray::from_raw(p) };
    (!a.is_released()).then_some(a)
}

/// `*p` moved out, `None` when `p` is null or the structure is released.
///
/// # Safety
///
/// `p` is null or valid for reads and writes of an initialized schema.
unsafe fn take_schema(p: *mut FFI_ArrowSchema) -> Option<FFI_ArrowSchema> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's contract; the source is left released.
    let s = unsafe { FFI_ArrowSchema::from_raw(p) };
    s.release.is_some().then_some(s)
}

/// Writes a batch as the answer to the oldest batch held (or to none, with none held), taking
/// `*batch` and `*schema`.
///
/// # Safety
///
/// `task` is null or from `moruna_open`; `batch` and `schema` are each null or valid for reads
/// and writes of an initialized structure of the C Data Interface.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn moruna_emit(
    task: *mut moruna_task,
    batch: *mut FFI_ArrowArray,
    schema: *mut FFI_ArrowSchema,
) -> c_int {
    // Taken before anything else, so both are released by the time this returns.
    // SAFETY: the caller's contract.
    let (array, ffi_schema) = unsafe { (take_array(batch), take_schema(schema)) };
    // SAFETY: the caller's contract.
    let Some(task) = (unsafe { task_mut(task) }) else {
        return negated(code::NO_PLANE);
    };
    if let Some(code) = task.failed() {
        return negated(code);
    }
    let (Some(array), Some(ffi_schema)) = (array, ffi_schema) else {
        let e = FunctionError::new("moruna_emit: the batch or its schema is NULL or released");
        return negated(task.fail(&e));
    };
    // SAFETY: the caller's contract.
    match unsafe { import(array, &ffi_schema) } {
        Ok(b) => match task.emit(&b) {
            Ok(()) => 0,
            Err(code) => negated(code),
        },
        Err(why) => negated(task.fail(&FunctionError::new(format!(
            "moruna_emit: the batch: {why}"
        )))),
    }
}

/// `p` as text, `None` for a null pointer.
///
/// # Safety
///
/// `p` is null or a NUL-terminated string.
unsafe fn text(p: *const c_char) -> Option<String> {
    // SAFETY: the caller's contract.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Writes `line` to the task's log; a null `line` writes nothing.
///
/// # Safety
///
/// `task` is null or from `moruna_open`; `line` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn moruna_log(task: *mut moruna_task, line: *const c_char) {
    // SAFETY: the caller's contract.
    let Some(line) = (unsafe { text(line) }) else {
        return;
    };
    // SAFETY: the caller's contract.
    match unsafe { task_mut(task) } {
        Some(task) => task.log(&line),
        None => sdk::log(line),
    }
}

/// Ends the task with `message`, the function's refusal.
///
/// # Safety
///
/// `task` is null or from `moruna_open`; `message` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn moruna_error(task: *mut moruna_task, message: *const c_char) {
    // SAFETY: the caller's contract.
    let message = unsafe { text(message) }
        .unwrap_or_else(|| String::from("the function ended the task with no message"));
    // SAFETY: the caller's contract.
    match unsafe { task_mut(task) } {
        Some(task) => {
            task.fail(&FunctionError::new(message));
        }
        None => sdk::log(format!("moruna_error with no task: {message}")),
    }
}

/// Ends the task, frees it and returns its exit code.
///
/// # Safety
///
/// `task` is null or from `moruna_open`, and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn moruna_finish(task: *mut moruna_task) -> c_int {
    if task.is_null() {
        return exit_code(code::NO_PLANE);
    }
    // SAFETY: the caller's contract: the box `moruna_open` made, given back once.
    let task = unsafe { Box::from_raw(task.cast::<Task>()) };
    exit_code(task.finish())
}

/// The checks against structures arrow-rs made and then damaged; the calls themselves are
/// driven from C in tests/header.rs.
#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::types::Int32Type;
    use arrow_array::{ArrayRef, Int32Array, ListArray};
    use arrow_buffer::OffsetBuffer;
    use arrow_schema::{Field, UnionFields, UnionMode};

    /// An Int32 column wrapped in `levels` lists.
    fn nested(levels: usize) -> ArrayRef {
        let mut a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2]));
        for _ in 0..levels {
            let field = Arc::new(Field::new("item", a.data_type().clone(), true));
            let offsets = OffsetBuffer::from_lengths([a.len()]);
            a = Arc::new(ListArray::try_new(field, offsets, a, None).unwrap());
        }
        a
    }

    fn exported(a: &ArrayRef) -> (FFI_ArrowArray, FFI_ArrowSchema) {
        let schema = FFI_ArrowSchema::try_from(a.data_type()).unwrap();
        (FFI_ArrowArray::new(&a.to_data()), schema)
    }

    #[test]
    fn each_type_has_its_children() {
        let f = Arc::new(Field::new("x", DataType::Int8, true));
        for t in [
            DataType::List(f.clone()),
            DataType::LargeList(f.clone()),
            DataType::ListView(f.clone()),
            DataType::LargeListView(f.clone()),
            DataType::FixedSizeList(f.clone(), 2),
            DataType::Map(f.clone(), false),
        ] {
            assert_eq!(child_types(&t), [&DataType::Int8], "{t}");
        }
        let union = DataType::Union(
            UnionFields::try_new(
                [0, 1],
                [Field::new("a", DataType::Utf8, true), (*f).clone()],
            )
            .unwrap(),
            UnionMode::Sparse,
        );
        assert_eq!(child_types(&union), [&DataType::Utf8, &DataType::Int8]);
        let ree =
            DataType::RunEndEncoded(Arc::new(Field::new("e", DataType::Int32, false)), f.clone());
        assert_eq!(child_types(&ree), [&DataType::Int32, &DataType::Int8]);
        assert!(child_types(&DataType::Int64).is_empty());
    }

    #[test]
    fn a_well_made_array_and_schema_pass_and_a_deep_one_is_refused() {
        let (a, s) = exported(&nested(3));
        unsafe {
            assert_eq!(check_schema(&s, 0), Ok(()));
            assert_eq!(check_array(&a, nested(3).data_type(), 0), Ok(()));
        }
        let deep = nested(DEPTH + 1);
        let (a, s) = exported(&deep);
        unsafe {
            let e = check_schema(&s, 0).unwrap_err();
            assert_eq!(e, format!("the schema nests deeper than {DEPTH}"));
            let e = check_array(&a, deep.data_type(), 0).unwrap_err();
            assert_eq!(e, format!("the array nests deeper than {DEPTH}"));
        }
    }

    #[test]
    fn damaged_arrays_are_refused_by_what_is_wrong() {
        let list = nested(1);
        let t = list.data_type().clone();
        type Damage = (fn(&mut FFI_ArrowArray), &'static str);
        let damage: [Damage; 5] = [
            (
                |a| a.length = -1,
                "has length -1, offset 0 and null count 0",
            ),
            (|a| a.buffers = std::ptr::null_mut(), "has its buffers NULL"),
            (
                |a| a.children = std::ptr::null_mut(),
                "has its children NULL",
            ),
            (
                |a| unsafe { *a.children = std::ptr::null_mut() },
                "has child 0 NULL",
            ),
            (|a| a.n_children = 2, "has 2 children, where it has 1"),
        ];
        for (hurt, said) in damage {
            let (mut a, _) = exported(&list);
            let (children, child) = (a.children, unsafe { *a.children });
            hurt(&mut a);
            let e = unsafe { check_array(&a, &t, 0) }.unwrap_err();
            assert!(e.ends_with(said), "{e}");
            // As it was, for its release.
            a.children = children;
            unsafe { *a.children = child };
        }
        let keys = Int32Array::from(vec![0]);
        let values: ArrayRef = Arc::new(Int32Array::from(vec![7]));
        let dictionary: ArrayRef =
            Arc::new(arrow_array::DictionaryArray::<Int32Type>::try_new(keys, values).unwrap());
        let (mut a, _) = exported(&dictionary);
        let d = a.dictionary;
        a.dictionary = std::ptr::null_mut();
        let e = unsafe { check_array(&a, dictionary.data_type(), 0) }.unwrap_err();
        assert!(e.ends_with("has no dictionary"), "{e}");
        a.dictionary = d;
    }

    #[test]
    fn a_schema_with_a_child_missing_is_refused() {
        let (_, mut s) = exported(&nested(1));
        let child = unsafe { *s.children };
        unsafe { *s.children = std::ptr::null_mut() };
        assert_eq!(
            unsafe { check_schema(&s, 0) },
            Err(String::from("a schema's child 0 is NULL"))
        );
        unsafe { *s.children = child };
        s.n_children = -1;
        assert_eq!(
            unsafe { check_schema(&s, 0) },
            Err(String::from("a schema has -1 children"))
        );
        s.n_children = 1;
    }
}
