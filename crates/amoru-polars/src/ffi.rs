//! Moving one column between Polars and Amoru through the Arrow C Data Interface.
//!
//! Polars 0.55 stores its data in `polars-arrow`, a different crate from the `arrow` the
//! contracts crate is written against, so a `Series` and a `RecordBatch` column are not the same
//! Rust type even though they are the same bytes. The C Data Interface is the bridge the Arrow
//! project provides for exactly this, and it is a pointer hand off: no column bytes are copied
//! in either direction.
//!
//! `unsafe` lives here and nowhere else in this crate. The two `#[repr(C)]` structs each side
//! declares are the Arrow C Data Interface's `ArrowArray` and `ArrowSchema`, whose layout the
//! Arrow specification fixes; the static assertions below check that this build's two
//! declarations agree in size and alignment, and the `// SAFETY:` comment on each block names the
//! ownership rule it relies on.
//!
//! 05-adapters section l permits `unsafe` in `amoru-adapters`'s `cross.rs` and nowhere else in
//! these three crates. This module is therefore an E9 item, reported: the alternative to it is
//! copying every column element by element, which would put a CPU copy in the one place S7 asks
//! for a zero copy hand off.

use amoru_kernel::arrow::array::{ArrayRef, make_array};
use amoru_kernel::arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use polars::prelude::{ArrowField, CompatLevel, PlSmallStr, PolarsError, PolarsResult, Series};

const _: () = {
    assert!(size_of::<FFI_ArrowArray>() == size_of::<polars_arrow::ffi::ArrowArray>());
    assert!(align_of::<FFI_ArrowArray>() == align_of::<polars_arrow::ffi::ArrowArray>());
    assert!(size_of::<FFI_ArrowSchema>() == size_of::<polars_arrow::ffi::ArrowSchema>());
    assert!(align_of::<FFI_ArrowSchema>() == align_of::<polars_arrow::ffi::ArrowSchema>());
};

/// One `Series` as one arrow-rs array, by pointer.
pub fn series_to_arrow(series: &Series) -> PolarsResult<ArrayRef> {
    let one_chunk = series.rechunk();
    let field = ArrowField::new(
        series.name().clone(),
        series.dtype().to_arrow(CompatLevel::newest()),
        true,
    );
    let array = one_chunk.to_arrow(0, CompatLevel::newest());
    let exported_array = polars_arrow::ffi::export_array_to_c(array);
    let exported_schema = polars_arrow::ffi::export_field_to_c(&field);
    // SAFETY: both types are the Arrow C Data Interface's `ArrowArray` and `ArrowSchema`, whose
    // field order and layout the specification fixes and whose size and alignment the static
    // assertions above check. Ownership moves with the value: after the transmute the release
    // callback is arrow-rs's to call, and it calls the one Polars installed.
    let array: FFI_ArrowArray = unsafe { std::mem::transmute(exported_array) };
    // SAFETY: as above, for the schema.
    let schema: FFI_ArrowSchema = unsafe { std::mem::transmute(exported_schema) };
    // SAFETY: `array` and `schema` were produced together by `polars-arrow` for one array and
    // describe the same buffers; both are valid, neither has been released, and `from_ffi` takes
    // ownership of the array while borrowing the schema, which is dropped (and so released) here.
    let data = unsafe { amoru_kernel::arrow::ffi::from_ffi(array, &schema) }
        .map_err(|e| PolarsError::ComputeError(e.to_string().into()))?;
    Ok(make_array(data))
}

/// One arrow-rs array as a `Series` named `name`, by pointer.
pub fn arrow_to_series(name: PlSmallStr, array: &ArrayRef) -> PolarsResult<Series> {
    let (exported_array, exported_schema) = amoru_kernel::arrow::ffi::to_ffi(&array.to_data())
        .map_err(|e| PolarsError::ComputeError(e.to_string().into()))?;
    // SAFETY: as in `series_to_arrow`, in the other direction: the two structs are the C Data
    // Interface's, and ownership moves with the value, so the release callback arrow-rs installed
    // is the one `polars-arrow` will call.
    let imported_array: polars_arrow::ffi::ArrowArray =
        unsafe { std::mem::transmute(exported_array) };
    // SAFETY: as above, for the schema.
    let imported_schema: polars_arrow::ffi::ArrowSchema =
        unsafe { std::mem::transmute(exported_schema) };
    // SAFETY: the schema was produced by arrow-rs for this array, is valid and has not been
    // released; `import_field_from_c` only reads it.
    let field = unsafe { polars_arrow::ffi::import_field_from_c(&imported_schema) }?;
    // SAFETY: the array and the field describe the same data, the array has not been released,
    // and `import_array_from_c` takes ownership of it.
    let imported =
        unsafe { polars_arrow::ffi::import_array_from_c(imported_array, field.dtype().clone()) }?;
    Series::from_arrow(name, imported)
}
