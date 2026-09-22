//! The `ScalarUDF` a kernel becomes (f.6).

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use amoru_kernel::arrow::datatypes::{DataType, Field, Schema};
use amoru_kernel::arrow::record_batch::RecordBatch;
use amoru_kernel::{AmoruError, Kernel, NoState, Payload, SourceSchema};
use datafusion::error::{DataFusionError, Result as DataFusionResult};
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};

/// A kernel as a DataFusion scalar function.
///
/// The kernel is held behind `Arc<dyn Kernel>` rather than as a type parameter: DataFusion
/// stores the function as a trait object anyway, and one implementation for every kernel is one
/// thing to read and one thing to test.
///
/// The function takes the kernel's input columns and returns its first output column, which is
/// the shape DataFusion's scalar contract allows; a kernel with more than one output column is
/// not exposed this way, and the documentation says so.
pub struct KernelUdf {
    kernel: Arc<dyn Kernel>,
    name: String,
    signature: Signature,
}

impl KernelUdf {
    /// The bridge's own type, for a caller that wants it without DataFusion's `ScalarUDF`
    /// wrapper around it (the tests, and a planner building its own registry).
    pub fn new<K: Kernel>(kernel: K, name: &str) -> KernelUdf {
        KernelUdf {
            kernel: Arc::new(kernel),
            name: name.to_string(),
            signature: Signature::variadic_any(Volatility::Immutable),
        }
    }
}

impl std::fmt::Debug for KernelUdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelUdf")
            .field("name", &self.name)
            .finish()
    }
}

/// Host `kernel` inside DataFusion as a `ScalarUDF` called `name` (d.1).
pub fn datafusion_udf<K: Kernel>(kernel: K, name: &str) -> ScalarUDF {
    ScalarUDF::new_from_impl(KernelUdf::new(kernel, name))
}

fn external(error: AmoruError) -> DataFusionError {
    DataFusionError::External(Box::new(error))
}

/// The Amoru schema for a list of DataFusion argument types.
fn input_schema(arg_types: &[DataType]) -> SourceSchema {
    let fields: Vec<Field> = arg_types
        .iter()
        .enumerate()
        .map(|(index, data_type)| Field::new(format!("arg{index}"), data_type.clone(), true))
        .collect();
    SourceSchema::Table(Arc::new(Schema::new(fields)))
}

/// Two bridged kernels are the same function when they carry the same name, which is what
/// DataFusion compares function instances for: a `Kernel` is opaque and has no equality of its
/// own, and the fingerprint it does have is not part of the plan.
impl PartialEq for KernelUdf {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for KernelUdf {}

impl Hash for KernelUdf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl ScalarUDFImpl for KernelUdf {
    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    /// The kernel's own `output_schema`, narrowed to its first column, which is what this
    /// function returns.
    fn return_type(&self, arg_types: &[DataType]) -> DataFusionResult<DataType> {
        let output = self
            .kernel
            .output_schema(&input_schema(arg_types))
            .map_err(external)?;
        match output {
            SourceSchema::Table(schema) => match schema.fields().first() {
                Some(field) => Ok(field.data_type().clone()),
                None => Err(DataFusionError::Plan(format!(
                    "the kernel behind {} produces no columns",
                    self.name
                ))),
            },
            SourceSchema::Tensor { .. } => Err(DataFusionError::Plan(format!(
                "the kernel behind {} produces a tensor, which a scalar function cannot return",
                self.name
            ))),
        }
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DataFusionResult<ColumnarValue> {
        let rows = args.number_rows;
        let mut fields = Vec::with_capacity(args.args.len());
        let mut columns = Vec::with_capacity(args.args.len());
        for (index, value) in args.args.iter().enumerate() {
            let array = value.to_array(rows)?;
            let name = match args.arg_fields.get(index) {
                Some(field) => field.name().to_string(),
                None => format!("arg{index}"),
            };
            fields.push(Field::new(
                name,
                array.data_type().clone(),
                array.null_count() > 0,
            ));
            columns.push(array);
        }
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)?;
        let payload = Payload::table(batch).map_err(external)?;
        let mut state = NoState;
        let output = self.kernel.apply(&mut state, payload).map_err(external)?;
        let Payload::Table(out, _) = output else {
            return Err(DataFusionError::Execution(format!(
                "the kernel behind {} returned a tensor, which a scalar function cannot return",
                self.name
            )));
        };
        match out.columns().first() {
            Some(column) => Ok(ColumnarValue::Array(Arc::clone(column))),
            None => Err(DataFusionError::Execution(format!(
                "the kernel behind {} returned no columns",
                self.name
            ))),
        }
    }
}
