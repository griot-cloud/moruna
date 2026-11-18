// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! An array imported from the Arrow C Data Interface: arrow-array 59.3.0's `ffi` import
//! (`src/ffi.rs`: `bit_width`, `create_buffer`, `from_ffi_and_data_type`, `ImportedArrowArray`),
//! taken here so the C library needs no `ffi` feature of arrow-array, whose C stream interface
//! takes its errno values from the `libc` crate, which knows no Moruna (item 5.2b). Changed from
//! arrow-array's: errors are this library's sentences, and the asserts on a child count are
//! gone, since `crate::check_array` refuses such an array before it gets here. The export side
//! (`FFI_ArrowArray::new`, `FFI_ArrowSchema::try_from`) is arrow-data's and arrow-schema's.

use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::Arc;

use arrow_buffer::{Buffer, MutableBuffer, bit_util};
use arrow_data::ffi::FFI_ArrowArray;
use arrow_data::{ArrayData, layout};
use arrow_schema::{DataType, UnionMode};

type Result<T> = std::result::Result<T, String>;

fn buffers_expected(data_type: &DataType, n: usize, i: usize) -> String {
    format!("an array of {data_type} has {n} buffers, and buffer {i} was asked for")
}

/// The bits each slot of buffer `i` (indexed as the C Data Interface indexes them) holds.
fn bit_width(data_type: &DataType, i: usize) -> Result<usize> {
    if let Some(primitive) = data_type.primitive_width() {
        return match i {
            1 => Ok(primitive * 8),
            i => Err(buffers_expected(data_type, 2, i)),
        };
    }
    Ok(match (data_type, i) {
        (DataType::Boolean, 1) => 1,
        (DataType::FixedSizeBinary(num_bytes), 1) => {
            usize::try_from(*num_bytes).map_err(|_| format!("a FixedSizeBinary({num_bytes})"))?
                * u8::BITS as usize
        }
        (DataType::FixedSizeList(f, num_elems), 1) => {
            bit_width(f.data_type(), 1)? * (*num_elems as usize)
        }
        (DataType::Utf8, 1)
        | (DataType::Binary, 1)
        | (DataType::List(_), 1)
        | (DataType::Map(_, _), 1) => i32::BITS as _,
        (DataType::Utf8, 2) | (DataType::Binary, 2) => u8::BITS as _,
        (DataType::ListView(_), 1) | (DataType::ListView(_), 2) => i32::BITS as _,
        (DataType::LargeListView(_), 1) | (DataType::LargeListView(_), 2) => i64::BITS as _,
        (DataType::LargeUtf8, 1) | (DataType::LargeBinary, 1) | (DataType::LargeList(_), 1) => {
            i64::BITS as _
        }
        (DataType::LargeUtf8, 2) | (DataType::LargeBinary, 2) | (DataType::LargeList(_), 2) => {
            u8::BITS as _
        }
        (DataType::Utf8View, 1) | (DataType::BinaryView, 1) => u128::BITS as _,
        (DataType::Utf8View, _) | (DataType::BinaryView, _) => u8::BITS as _,
        (DataType::Union(_, _), 0) => i8::BITS as _,
        (DataType::Union(_, UnionMode::Dense), 1) => i32::BITS as _,
        _ => return Err(format!("an array of {data_type} has no buffer {i}")),
    })
}

/// Buffer `index` of `array`, `len` bytes, held by `owner`; `None` for a null pointer.
///
/// # Safety
///
/// The buffer holds `len` bytes, valid while `owner` lives.
unsafe fn create_buffer(
    owner: Arc<FFI_ArrowArray>,
    array: &FFI_ArrowArray,
    index: usize,
    len: usize,
) -> Option<Buffer> {
    if array.num_buffers() == 0 {
        return None;
    }
    NonNull::new(array.buffer(index) as _)
        // SAFETY: the caller's contract.
        .map(|ptr| unsafe { Buffer::from_custom_allocation(ptr, len, owner) })
}

/// `array` imported as an array of `data_type`, its buffers held by it until the last is
/// dropped, which releases it.
///
/// # Safety
///
/// `array` is what the C Data Interface says an array of `data_type` is, its counts checked
/// (`crate::check_array`).
pub(crate) unsafe fn from_ffi_and_data_type(
    array: FFI_ArrowArray,
    data_type: DataType,
) -> Result<ArrayData> {
    let array = Arc::new(array);
    let tmp = ImportedArrowArray {
        array: &array,
        data_type,
        owner: &array,
    };
    let mut data = tmp.consume()?;
    // arrow-rs aligns buffers more strictly than the C Data Interface: a copy when one is not.
    data.align_buffers();
    Ok(data)
}

struct ImportedArrowArray<'a> {
    array: &'a FFI_ArrowArray,
    data_type: DataType,
    owner: &'a Arc<FFI_ArrowArray>,
}

impl ImportedArrowArray<'_> {
    fn consume(self) -> Result<ArrayData> {
        let len = self.array.len();
        let offset = self.array.offset();
        let null_count = match &self.data_type {
            DataType::Null => Some(0),
            _ => self.array.null_count_opt(),
        };
        let data_layout = layout(&self.data_type);
        let buffers = self.buffers(data_layout.can_contain_null_mask, data_layout.variadic)?;
        let null_bit_buffer = if data_layout.can_contain_null_mask {
            self.null_bit_buffer()
        } else {
            None
        };
        let mut child_data = self.consume_children()?;
        if let Some(d) = self.dictionary()? {
            child_data.push(d.consume()?);
        }
        // SAFETY: the caller validates the data in full (`ArrayData::validate_full`).
        Ok(unsafe {
            ArrayData::new_unchecked(
                self.data_type,
                len,
                null_count,
                null_bit_buffer,
                offset,
                buffers,
                child_data,
            )
        })
    }

    fn consume_children(&self) -> Result<Vec<ArrayData>> {
        match &self.data_type {
            DataType::List(field)
            | DataType::FixedSizeList(field, _)
            | DataType::LargeList(field)
            | DataType::ListView(field)
            | DataType::LargeListView(field)
            | DataType::Map(field, _) => Ok([self.consume_child(0, field.data_type())?].to_vec()),
            DataType::Struct(fields) => fields
                .iter()
                .enumerate()
                .map(|(i, field)| self.consume_child(i, field.data_type()))
                .collect(),
            DataType::Union(union_fields, _) => union_fields
                .iter()
                .enumerate()
                .map(|(i, (_, field))| self.consume_child(i, field.data_type()))
                .collect(),
            DataType::RunEndEncoded(run_ends_field, values_field) => Ok([
                self.consume_child(0, run_ends_field.data_type())?,
                self.consume_child(1, values_field.data_type())?,
            ]
            .to_vec()),
            _ => Ok(Vec::new()),
        }
    }

    fn consume_child(&self, index: usize, child_type: &DataType) -> Result<ArrayData> {
        ImportedArrowArray {
            array: self.array.child(index),
            data_type: child_type.clone(),
            owner: self.owner,
        }
        .consume()
    }

    /// Every buffer, as arrow-rs keeps them: the null buffer, which the C Data Interface
    /// counts, apart.
    fn buffers(&self, can_contain_null_mask: bool, variadic: bool) -> Result<Vec<Buffer>> {
        let buffer_begin = usize::from(can_contain_null_mask);
        let buffer_end = self.array.num_buffers() - usize::from(variadic);
        let variadic_buffer_lens = if variadic {
            // A views array: its null buffer, its views, its data buffers, their lengths.
            let num_variadic_buffers =
                self.array.num_buffers() - (2 + usize::from(can_contain_null_mask));
            if num_variadic_buffers == 0 {
                &[]
            } else {
                let lengths = self.array.buffer(self.array.num_buffers() - 1);
                // SAFETY: the lengths buffer holds one length per data buffer.
                unsafe { std::slice::from_raw_parts(lengths.cast::<i64>(), num_variadic_buffers) }
            }
        } else {
            &[]
        };
        (buffer_begin..buffer_end)
            .map(|index| {
                let len = self.buffer_len(index, variadic_buffer_lens, &self.data_type)?;
                // SAFETY: the buffer's length is the type's, from the array's length.
                match unsafe { create_buffer(self.owner.clone(), self.array, index, len) } {
                    // A dangling pointer for a buffer of no bytes is the C Data Interface's.
                    Some(buf) if buf.is_empty() => Ok(MutableBuffer::new(0).into()),
                    Some(buf) => Ok(buf),
                    None if len == 0 => Ok(MutableBuffer::new(0).into()),
                    None => Err(format!("buffer {index} of an array is null")),
                }
            })
            .collect()
    }

    /// The bytes of buffer `i` (indexed as the C Data Interface indexes them): from the array's
    /// length and offset, and for variable-sized data from the last offset.
    fn buffer_len(
        &self,
        i: usize,
        variadic_buffer_lengths: &[i64],
        dt: &DataType,
    ) -> Result<usize> {
        let data_type = match dt {
            DataType::Dictionary(key_data_type, _) => key_data_type.as_ref(),
            dt => dt,
        };
        let length = self.array.len() + self.array.offset();
        Ok(match (&data_type, i) {
            (DataType::Utf8, 1)
            | (DataType::LargeUtf8, 1)
            | (DataType::Binary, 1)
            | (DataType::LargeBinary, 1)
            | (DataType::List(_), 1)
            | (DataType::LargeList(_), 1)
            | (DataType::Map(_, _), 1) => (length + 1) * (bit_width(data_type, i)? / 8),
            (DataType::ListView(_), 1)
            | (DataType::ListView(_), 2)
            | (DataType::LargeListView(_), 1)
            | (DataType::LargeListView(_), 2) => length * (bit_width(data_type, i)? / 8),
            (DataType::Utf8, 2) | (DataType::Binary, 2) => {
                if self.array.is_empty() {
                    return Ok(0);
                }
                let len = self.buffer_len(1, variadic_buffer_lengths, dt)?;
                #[allow(clippy::cast_ptr_alignment)]
                let offset_buffer = self.array.buffer(1) as *const i32;
                // SAFETY: the offsets buffer holds `len` bytes; its last offset ends the data.
                (unsafe { *offset_buffer.add(len / size_of::<i32>() - 1) }) as usize
            }
            (DataType::LargeUtf8, 2) | (DataType::LargeBinary, 2) => {
                if self.array.is_empty() {
                    return Ok(0);
                }
                let len = self.buffer_len(1, variadic_buffer_lengths, dt)?;
                #[allow(clippy::cast_ptr_alignment)]
                let offset_buffer = self.array.buffer(1) as *const i64;
                // SAFETY: as above, for 64-bit offsets.
                (unsafe { *offset_buffer.add(len / size_of::<i64>() - 1) }) as usize
            }
            (DataType::Utf8View, 1) | (DataType::BinaryView, 1) => size_of::<u128>() * length,
            (DataType::Utf8View, i) | (DataType::BinaryView, i) => {
                variadic_buffer_lengths[i - 2] as usize
            }
            _ => bit_util::ceil(length * bit_width(data_type, i)?, 8),
        })
    }

    /// The null buffer, which arrow-rs keeps apart from the others.
    fn null_bit_buffer(&self) -> Option<Buffer> {
        let length = self.array.len() + self.array.offset();
        let buffer_len = bit_util::ceil(length, 8);
        // SAFETY: a null buffer holds a bit for every slot from the array's start.
        unsafe { create_buffer(self.owner.clone(), self.array, 0, buffer_len) }
    }

    fn dictionary(&self) -> Result<Option<ImportedArrowArray<'_>>> {
        match (self.array.dictionary(), &self.data_type) {
            (Some(array), DataType::Dictionary(_, value_type)) => Ok(Some(ImportedArrowArray {
                array,
                data_type: value_type.as_ref().clone(),
                owner: self.owner,
            })),
            (Some(_), _) => Err(String::from(
                "a dictionary for an array of no dictionary type",
            )),
            (None, DataType::Dictionary(_, _)) => Err(String::from(
                "no dictionary for an array of a dictionary type",
            )),
            (_, _) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::types::{Int8Type, Int32Type};
    use arrow_array::{
        Array, ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, DictionaryArray,
        FixedSizeBinaryArray, FixedSizeListArray, Float64Array, Int8Array, Int32Array,
        LargeBinaryArray, LargeListArray, LargeStringArray, ListArray, MapArray, NullArray,
        RunArray, StringArray, StringViewArray, StructArray, UnionArray, make_array,
    };
    use arrow_buffer::ScalarBuffer;
    use arrow_schema::{Field, UnionFields};

    /// `a` exported by arrow-data and imported here, sliced and whole.
    fn round_trip(a: ArrayRef) {
        for a in [a.clone(), a.slice(1, a.len() - 2)] {
            let exported = FFI_ArrowArray::new(&a.to_data());
            // SAFETY: arrow-data's own export, of this type.
            let data = unsafe { from_ffi_and_data_type(exported, a.data_type().clone()) }.unwrap();
            data.validate_full().unwrap();
            assert_eq!(make_array(data).as_ref(), a.as_ref(), "{}", a.data_type());
        }
    }

    #[test]
    fn every_kind_of_array_comes_back_as_it_went() {
        let ints: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), None, Some(3), Some(4)]));
        let strings = vec![Some("a"), None, Some("ccc"), Some("")];
        let item = Arc::new(Field::new("item", DataType::Int32, true));
        let mut arrays: Vec<ArrayRef> = vec![
            ints.clone(),
            Arc::new(Float64Array::from(vec![1.5, 2.5, 3.5, 4.5])),
            Arc::new(BooleanArray::from(vec![
                Some(true),
                None,
                Some(false),
                Some(true),
            ])),
            Arc::new(StringArray::from(strings.clone())),
            Arc::new(LargeStringArray::from(strings.clone())),
            Arc::new(BinaryArray::from_opt_vec(vec![
                Some(b"x"),
                None,
                Some(b"yz"),
                Some(b""),
            ])),
            Arc::new(LargeBinaryArray::from_opt_vec(vec![
                Some(b"x"),
                None,
                Some(b"y"),
                None,
            ])),
            Arc::new(StringViewArray::from(vec![
                "short",
                "a string longer than twelve bytes",
                "x",
                "",
            ])),
            Arc::new(BinaryViewArray::from(vec![&b"ab"[..], b"", b"cd", b"ef"])),
            Arc::new(
                FixedSizeBinaryArray::try_from_iter([b"ab", b"cd", b"ef", b"gh"].into_iter())
                    .unwrap(),
            ),
            Arc::new(ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
                Some(vec![Some(1), None]),
                None,
                Some(vec![]),
                Some(vec![Some(4)]),
            ])),
            Arc::new(LargeListArray::from_iter_primitive::<Int32Type, _, _>(
                vec![
                    Some(vec![Some(1)]),
                    Some(vec![]),
                    None,
                    Some(vec![Some(4), Some(5)]),
                ],
            )),
            Arc::new(
                FixedSizeListArray::try_new(
                    item.clone(),
                    2,
                    Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5, 6, 7, 8])),
                    None,
                )
                .unwrap(),
            ),
            Arc::new(StructArray::from(vec![(
                Arc::new(Field::new("x", DataType::Int32, true)),
                ints.clone(),
            )])),
            Arc::new(
                vec![Some("a"), None, Some("b"), Some("a")]
                    .into_iter()
                    .collect::<DictionaryArray<Int8Type>>(),
            ),
            Arc::new(NullArray::new(4)),
            Arc::new(
                RunArray::<Int32Type>::try_new(
                    &Int32Array::from(vec![2, 4]),
                    &Int32Array::from(vec![7, 8]),
                )
                .unwrap(),
            ),
        ];
        let values = Int32Array::from(vec![1, 2, 3, 4]);
        arrays.push(Arc::new(
            MapArray::new_from_strings(
                ["k1", "k2", "k3", "k4"].into_iter(),
                &values,
                &[0, 1, 2, 3, 4],
            )
            .unwrap(),
        ));
        let fields = UnionFields::try_new(
            [0, 1],
            [
                Field::new("i", DataType::Int32, true),
                Field::new("b", DataType::Int8, true),
            ],
        )
        .unwrap();
        let type_ids = ScalarBuffer::from(vec![0i8, 1, 0, 1]);
        arrays.push(Arc::new(
            UnionArray::try_new(
                fields.clone(),
                type_ids.clone(),
                None,
                vec![
                    Arc::new(Int32Array::from(vec![1, 2, 3, 4])) as ArrayRef,
                    Arc::new(Int8Array::from(vec![5, 6, 7, 8])),
                ],
            )
            .unwrap(),
        ));
        arrays.push(Arc::new(
            UnionArray::try_new(
                fields,
                type_ids,
                Some(ScalarBuffer::from(vec![0i32, 0, 1, 1])),
                vec![
                    Arc::new(Int32Array::from(vec![1, 3])) as ArrayRef,
                    Arc::new(Int8Array::from(vec![6, 8])),
                ],
            )
            .unwrap(),
        ));
        for a in arrays {
            round_trip(a);
        }
    }

    #[test]
    fn what_cannot_be_read_is_refused() {
        assert!(bit_width(&DataType::Int32, 2).is_err());
        assert!(bit_width(&DataType::Utf8, 3).is_err());
        assert!(bit_width(&DataType::FixedSizeBinary(-1), 1).is_err());
        assert!(bit_width(&DataType::Union(UnionFields::empty(), UnionMode::Sparse), 1).is_err());
        assert!(
            bit_width(
                &DataType::FixedSizeList(Arc::new(Field::new("i", DataType::Int32, true)), 3),
                1
            )
            .is_ok()
        );
        // A dictionary column's array without its dictionary, and a dictionary where none is.
        let d: ArrayRef = Arc::new(
            vec!["a", "b"]
                .into_iter()
                .collect::<DictionaryArray<Int8Type>>(),
        );
        let mut exported = FFI_ArrowArray::new(&d.to_data());
        // arrow-data's release frees the dictionary from its own record, not from this field.
        exported.dictionary = std::ptr::null_mut();
        // SAFETY: arrow-data's export, damaged only in its dictionary pointer.
        let e = unsafe { from_ffi_and_data_type(exported, d.data_type().clone()) };
        assert_eq!(
            e.err().as_deref(),
            Some("no dictionary for an array of a dictionary type")
        );
        // The keys alone, read as a plain Int8 array: their dictionary is not one's.
        let exported = FFI_ArrowArray::new(&d.to_data());
        // SAFETY: arrow-data's export of an Int8-keyed dictionary, whose keys are Int8.
        let e = unsafe { from_ffi_and_data_type(exported, DataType::Int8) };
        assert_eq!(
            e.err().as_deref(),
            Some("a dictionary for an array of no dictionary type")
        );
    }
}
