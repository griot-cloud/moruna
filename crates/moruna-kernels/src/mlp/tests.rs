//! The CPU reference against a pass written the plain way, one row at a time.

use super::*;
use moruna_kernel::arrow::array::{Float64Builder, Int32Array, Int64Array, ListArray};

/// A deterministic source of numbers in [-1, 1).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }

    fn vec(&mut self, n: usize) -> Vec<f64> {
        (0..n).map(|_| self.next()).collect()
    }
}

fn model(seed: u64, widths: &[usize], activations: &[Activation], bias: bool) -> Model {
    let mut rng = Lcg(seed);
    let layers = widths
        .windows(2)
        .zip(activations)
        .map(|(w, &a)| {
            Layer::new(
                w[0],
                w[1],
                rng.vec(w[0] * w[1]),
                bias.then(|| rng.vec(w[1])),
                a,
            )
            .unwrap()
        })
        .collect();
    Model::new(layers).unwrap()
}

/// The forward pass one row at a time, in f64, multiply then add in input order.
fn naive(model: &Model, row: &[f64]) -> Vec<f64> {
    let mut h = row.to_vec();
    for l in model.layers() {
        let mut out = vec![0.0; l.outputs()];
        for (j, o) in out.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (i, x) in h.iter().enumerate() {
                acc += x * l.weights()[i * l.outputs() + j];
            }
            if let Some(b) = l.bias() {
                acc += b[j];
            }
            *o = match l.activation() {
                Activation::Identity => acc,
                Activation::Relu => acc.relu(),
                Activation::Sigmoid => acc.sigmoid(),
                Activation::Tanh => acc.tanh(),
            };
        }
        h = out;
    }
    h
}

fn columns(rows: &[Vec<f64>], k: usize) -> Vec<ArrayRef> {
    (0..k)
        .map(|c| Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r[c]))) as ArrayRef)
        .collect()
}

fn values(out: &ArrayRef) -> Vec<f64> {
    match out.data_type() {
        DataType::Float64 => out
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .values()
            .to_vec(),
        DataType::Float32 => out
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap()
            .values()
            .iter()
            .map(|&v| v as f64)
            .collect(),
        DataType::FixedSizeList(..) => {
            let list = out.as_any().downcast_ref::<FixedSizeListArray>().unwrap();
            values(list.values())
        }
        other => panic!("unexpected output {other}"),
    }
}

fn close(got: &[f64], want: &[f64], tol: f64) {
    assert_eq!(got.len(), want.len());
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            (g - w).abs() <= tol * (1.0 + w.abs()),
            "value {i}: {g} against {w}"
        );
    }
}

fn rows(seed: u64, n: usize, k: usize) -> Vec<Vec<f64>> {
    let mut rng = Lcg(seed);
    (0..n)
        .map(|_| rng.vec(k).iter().map(|v| v * 3.0).collect())
        .collect()
}

fn list(x: &[Vec<f64>], k: usize) -> FixedSizeListArray {
    let flat = Float64Array::from_iter_values(x.iter().flatten().copied());
    FixedSizeListArray::new(
        Arc::new(Field::new_list_field(DataType::Float64, true)),
        k as i32,
        Arc::new(flat),
        None,
    )
}

const NONLINEAR: [Activation; 3] = [Activation::Relu, Activation::Tanh, Activation::Sigmoid];

#[test]
fn names_parse_back() {
    for a in Activation::ALL {
        assert_eq!(Activation::parse(a.name()).unwrap(), a);
    }
    assert!(
        Activation::parse("gelu")
            .unwrap_err()
            .to_string()
            .contains("gelu")
    );
    for p in [Precision::Float32, Precision::Float64] {
        assert_eq!(Precision::parse(p.name()).unwrap(), p);
    }
    assert_eq!(Precision::Float32.data_type(), DataType::Float32);
    assert_eq!(Precision::Float64.width(), 8);
    assert!(Precision::parse("bfloat16").is_err());
}

#[test]
fn layers_and_models_refuse_wrong_shapes() {
    let relu = Activation::Relu;
    assert!(Layer::new(0, 1, vec![], None, relu).is_err());
    assert!(Layer::new(2, 2, vec![1.0; 3], None, relu).is_err());
    assert!(Layer::new(2, 2, vec![1.0; 4], Some(vec![0.0; 3]), relu).is_err());
    assert!(Layer::new(1, 1, vec![f64::NAN], None, relu).is_err());
    assert!(Layer::new(1, 1, vec![1.0], Some(vec![f64::INFINITY]), relu).is_err());
    assert!(Model::new(vec![]).is_err());
    let a = Layer::new(2, 3, vec![1.0; 6], None, relu).unwrap();
    let b = Layer::new(2, 1, vec![1.0; 2], Some(vec![0.5]), relu).unwrap();
    let err = Model::new(vec![a.clone(), b]).unwrap_err().to_string();
    assert!(err.contains("layer 1 gives 3 outputs"), "{err}");
    let m = Model::new(vec![a]).unwrap();
    assert_eq!((m.inputs(), m.outputs(), m.parameters()), (2, 3, 6));
    assert_eq!(m.widest_hidden(), 0);
}

#[test]
fn json_round_trips_and_refuses_what_is_not_a_model() {
    let m = model(
        3,
        &[3, 4, 2],
        &[Activation::Tanh, Activation::Identity],
        true,
    );
    assert_eq!(Model::from_json(&m.to_json()).unwrap(), m);
    let nb = model(4, &[2, 1], &[Activation::Sigmoid], false);
    let j = nb.to_json();
    assert!(j["layers"][0].get("bias").is_none());
    assert_eq!(Model::from_json(&j).unwrap(), nb);
    let defaults = json!({"layers": [{"weights": [[1.0], [2.0]], "bias": null}]});
    let d = Model::from_json(&defaults).unwrap();
    assert_eq!(d.layers()[0].activation(), Activation::Identity);
    assert!(d.layers()[0].bias().is_none());
    for bad in [
        json!([]),
        json!({"layers": [], "extra": 1}),
        json!({"layers": {}}),
        json!({"layers": []}),
        json!({"layers": [7]}),
        json!({"layers": [{"weights": [[1.0]], "act": "relu"}]}),
        json!({"layers": [{"weights": 1.0}]}),
        json!({"layers": [{"weights": []}]}),
        json!({"layers": [{"weights": [[1.0, 2.0], [3.0]]}]}),
        json!({"layers": [{"weights": [["a"]]}]}),
        json!({"layers": [{"weights": [[1.0]], "bias": [true]}]}),
        json!({"layers": [{"weights": [[1.0]], "bias": 1.0}]}),
        json!({"layers": [{"weights": [[1.0]], "activation": 3}]}),
        json!({"layers": [{"weights": [[1.0]], "activation": "softmax"}]}),
    ] {
        assert!(Model::from_json(&bad).is_err(), "{bad} was accepted");
    }
}

#[test]
fn float64_matches_the_naive_pass_across_block_edges() {
    let m = model(
        11,
        &[5, 16, 7, 1],
        &[Activation::Relu, Activation::Tanh, Activation::Sigmoid],
        true,
    );
    let kernel = MlpForward::configure(m.clone(), Precision::Float64);
    assert_eq!(kernel.output_type(), DataType::Float64);
    let mut scratch = Scratch::default();
    for n in [
        0,
        1,
        BLOCK_ROWS - 1,
        BLOCK_ROWS,
        BLOCK_ROWS + 1,
        3 * BLOCK_ROWS + 7,
    ] {
        let x = rows(n as u64, n, 5);
        let out = kernel.forward(&columns(&x, 5), &mut scratch).unwrap();
        assert_eq!(out.len(), n);
        assert_eq!(out.null_count(), 0);
        let want: Vec<f64> = x.iter().flat_map(|r| naive(&m, r)).collect();
        close(&values(&out), &want, 1e-13);
    }
    assert!(scratch.bytes() > 0);
}

#[test]
fn every_activation_and_a_list_output() {
    for (i, a) in NONLINEAR.into_iter().enumerate() {
        let m = model(20 + i as u64, &[4, 6, 3], &[a, a], i % 2 == 0);
        let x = rows(5, 300, 4);
        let want: Vec<f64> = x.iter().flat_map(|r| naive(&m, r)).collect();
        let kernel = MlpForward::configure(m, Precision::Float64);
        assert!(matches!(
            kernel.output_type(),
            DataType::FixedSizeList(_, 3)
        ));
        let out = kernel
            .forward(&columns(&x, 4), &mut Scratch::default())
            .unwrap();
        assert_eq!(out.data_type(), &kernel.output_type());
        close(&values(&out), &want, 1e-13);
    }
}

#[test]
fn float32_is_within_float32_of_float64() {
    let m = model(
        31,
        &[23, 32, 32, 1],
        &[Activation::Relu, Activation::Relu, Activation::Identity],
        true,
    );
    let x = rows(9, 600, 23);
    let want: Vec<f64> = x.iter().flat_map(|r| naive(&m, r)).collect();
    let kernel = MlpForward::configure(m.clone(), Precision::Float32);
    assert_eq!(kernel.output_type(), DataType::Float32);
    assert_eq!(kernel.state_bytes(), m.parameters() as u64 * 4);
    assert_eq!(kernel.precision(), Precision::Float32);
    assert_eq!(kernel.model(), &m);
    let out = kernel
        .forward(&columns(&x, 23), &mut Scratch::default())
        .unwrap();
    assert_eq!(out.data_type(), &DataType::Float32);
    close(&values(&out), &want, 1e-4);
}

#[test]
fn a_list_input_gives_what_columns_give_in_every_precision_and_slice() {
    let m = model(
        41,
        &[3, 8, 1],
        &[Activation::Relu, Activation::Identity],
        true,
    );
    let x = rows(2, 2 * BLOCK_ROWS + 3, 3);
    for p in [Precision::Float64, Precision::Float32] {
        let kernel = MlpForward::configure(m.clone(), p);
        let mut scratch = Scratch::default();
        let by_cols = kernel.forward(&columns(&x, 3), &mut scratch).unwrap();
        let l = list(&x, 3);
        let by_list = kernel
            .forward(&[Arc::new(l.clone()) as ArrayRef], &mut scratch)
            .unwrap();
        assert_eq!(values(&by_cols), values(&by_list));
        // A slice gives the rows it names, from its offset.
        let sliced = kernel
            .forward(&[Arc::new(l.slice(5, 300)) as ArrayRef], &mut scratch)
            .unwrap();
        assert_eq!(values(&sliced), values(&by_cols)[5..305].to_vec());
        let cols: Vec<ArrayRef> = columns(&x, 3).iter().map(|c| c.slice(5, 300)).collect();
        assert_eq!(
            values(&kernel.forward(&cols, &mut scratch).unwrap()),
            values(&sliced)
        );
    }
}

#[test]
fn narrower_inputs_are_widened_exactly() {
    let m = model(
        51,
        &[3, 5, 1],
        &[Activation::Tanh, Activation::Identity],
        true,
    );
    let x: Vec<Vec<f64>> = rows(3, 100, 3)
        .into_iter()
        .map(|r| r.into_iter().map(|v| (v as f32) as f64).collect())
        .collect();
    let kernel = MlpForward::configure(m, Precision::Float64);
    let mut scratch = Scratch::default();
    let want = values(&kernel.forward(&columns(&x, 3), &mut scratch).unwrap());
    let f32s: Vec<ArrayRef> = (0..3)
        .map(|c| {
            Arc::new(Float32Array::from_iter_values(
                x.iter().map(|r| r[c] as f32),
            )) as ArrayRef
        })
        .collect();
    assert_eq!(values(&kernel.forward(&f32s, &mut scratch).unwrap()), want);
    // Float16 columns.
    let h: Vec<Vec<f64>> = x
        .iter()
        .map(|r| r.iter().map(|&v| F16::from_f64(v).to_f64()).collect())
        .collect();
    let want_h = values(&kernel.forward(&columns(&h, 3), &mut scratch).unwrap());
    let f16s: Vec<ArrayRef> = (0..3)
        .map(|c| {
            Arc::new(Float16Array::from_iter_values(
                h.iter().map(|r| F16::from_f64(r[c])),
            )) as ArrayRef
        })
        .collect();
    assert_eq!(
        values(&kernel.forward(&f16s, &mut scratch).unwrap()),
        want_h
    );
    // A list of Float32 values, gathered and widened.
    let flat = Float32Array::from_iter_values(x.iter().flatten().map(|&v| v as f32));
    let l = FixedSizeListArray::new(
        Arc::new(Field::new_list_field(DataType::Float32, false)),
        3,
        Arc::new(flat),
        None,
    );
    assert_eq!(
        values(
            &kernel
                .forward(&[Arc::new(l) as ArrayRef], &mut scratch)
                .unwrap()
        ),
        want
    );
}

#[test]
fn a_null_input_gives_a_null_row_of_zeros_and_leaves_the_others() {
    let m = model(
        61,
        &[2, 4, 2],
        &[Activation::Sigmoid, Activation::Identity],
        true,
    );
    let x = rows(4, 10, 2);
    let kernel = MlpForward::configure(m, Precision::Float64);
    let mut scratch = Scratch::default();
    let clean = values(&kernel.forward(&columns(&x, 2), &mut scratch).unwrap());
    let mut c0 = Float64Builder::new();
    for (i, r) in x.iter().enumerate() {
        if i == 3 {
            c0.append_null()
        } else {
            c0.append_value(r[0])
        }
    }
    let mut cols = columns(&x, 2);
    cols[0] = Arc::new(c0.finish());
    let out = kernel.forward(&cols, &mut scratch).unwrap();
    assert_eq!(out.null_count(), 1);
    assert!(out.is_null(3));
    let got = values(&out);
    assert_eq!(&got[6..8], &[0.0, 0.0]);
    assert_eq!(&got[..6], &clean[..6]);
    assert_eq!(&got[8..], &clean[8..]);

    // A list: a null row, and a row with a null value inside it.
    let mut flat = Float64Builder::new();
    for (i, r) in x.iter().enumerate() {
        for (j, v) in r.iter().enumerate() {
            if i == 7 && j == 1 {
                flat.append_null()
            } else {
                flat.append_value(*v)
            }
        }
    }
    let nulls = NullBuffer::from_iter((0..10).map(|i| i != 2));
    let l = FixedSizeListArray::new(
        Arc::new(Field::new_list_field(DataType::Float64, true)),
        2,
        Arc::new(flat.finish()),
        Some(nulls),
    );
    let out = kernel
        .forward(&[Arc::new(l) as ArrayRef], &mut scratch)
        .unwrap();
    assert_eq!(out.null_count(), 2);
    assert!(out.is_null(2) && out.is_null(7));
    let got = values(&out);
    assert_eq!(&got[4..6], &[0.0, 0.0]);
    assert_eq!(&got[14..16], &[0.0, 0.0]);
    assert_eq!(&got[..4], &clean[..4]);
    // A one-wide model's null row is a null Float.
    let one = MlpForward::configure(
        model(62, &[2, 1], &[Activation::Relu], true),
        Precision::Float32,
    );
    let out = one.forward(&cols, &mut scratch).unwrap();
    assert!(out.is_null(3));
    assert_eq!(values(&out)[3], 0.0);
}

#[test]
fn relu_sends_negatives_and_negative_zero_to_zero_and_keeps_nan() {
    assert_eq!(f64::relu(-0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(f64::relu(-2.0), 0.0);
    assert_eq!(f64::relu(2.0), 2.0);
    assert!(f64::relu(f64::NAN).is_nan());
    assert!(f32::relu(f32::NAN).is_nan());
    assert_eq!(f64::sigmoid(-1000.0), 0.0);
    assert_eq!(f64::sigmoid(1000.0), 1.0);
    // NaN goes through the whole pass, and only through its own row.
    let m = model(
        71,
        &[2, 3, 1],
        &[Activation::Relu, Activation::Identity],
        true,
    );
    let kernel = MlpForward::configure(m, Precision::Float64);
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Float64Array::from(vec![f64::NAN, 1.0])),
        Arc::new(Float64Array::from(vec![1.0, 1.0])),
    ];
    let out = values(&kernel.forward(&cols, &mut Scratch::default()).unwrap());
    assert!(out[0].is_nan() && !out[1].is_nan());
}

#[test]
fn inputs_of_the_wrong_shape_are_refused() {
    let kernel = MlpForward::configure(
        model(81, &[2, 1], &[Activation::Relu], true),
        Precision::Float64,
    );
    let f = |v: Vec<f64>| Arc::new(Float64Array::from(v)) as ArrayRef;
    let err = |inputs: &[ArrayRef]| {
        kernel
            .forward(inputs, &mut Scratch::default())
            .unwrap_err()
            .to_string()
    };
    assert!(err(&[f(vec![1.0])]).contains("takes 2 Float columns"));
    assert!(err(&[f(vec![1.0]), Arc::new(Int64Array::from(vec![1]))]).contains("Int64"));
    assert!(err(&[f(vec![1.0]), f(vec![1.0, 2.0])]).contains("lengths 1, 2"));
    let wrong = list(&[vec![1.0, 2.0, 3.0]], 3);
    assert!(err(&[Arc::new(wrong)]).contains("FixedSizeList<Float64, 3>"));
    let ints = FixedSizeListArray::new(
        Arc::new(Field::new_list_field(DataType::Int32, true)),
        2,
        Arc::new(Int32Array::from(vec![1, 2])),
        None,
    );
    assert!(err(&[Arc::new(ints)]).contains("Int32"));
    let lists =
        ListArray::from_iter_primitive::<Float64Type, _, _>(vec![Some(vec![Some(1.0), Some(2.0)])]);
    assert!(err(&[Arc::new(lists)]).contains("1 column(s)"));
    assert!(
        kernel
            .check_inputs(&[DataType::Float16, DataType::Float32])
            .is_ok()
    );
    let empty = kernel
        .forward(&[f(vec![]), f(vec![])], &mut Scratch::default())
        .unwrap();
    assert!(empty.is_empty());
}

#[test]
#[should_panic(expected = "A lies inside its slice")]
fn a_product_outside_its_slices_panics_before_it_is_computed() {
    let a = [1.0f64; 3];
    let b = [1.0f64; 2];
    let mut c = [0.0f64; 4];
    f64::gemm(
        2,
        2,
        1,
        Mat {
            data: &a,
            rs: 2,
            cs: 1,
        },
        Mat {
            data: &b,
            rs: 1,
            cs: 1,
        },
        &mut c,
    );
}
