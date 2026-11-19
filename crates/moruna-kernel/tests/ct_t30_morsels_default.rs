//! CT-T30 morsels_default (item 6.3m): a kernel that is not pipelined gives each morsel in the
//! call that brought it. `apply_morsels`' default is `apply`'s output under the unit's own number,
//! timed as the unit; `finish_morsels`' default is `finish`'s payload under `after`, or nothing;
//! and either passes the kernel's error through.

use std::sync::Arc;

use moruna_kernel::arrow::array::Int64Array;
use moruna_kernel::arrow::datatypes::{DataType, Field, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::{
    Completed, Fingerprint, InitCtx, Kernel, KernelKind, KernelState, MorselTime, MorunaError,
    NoState, Payload, PayloadKind, PayloadSpec, SourceSchema, Tier, TierPref,
};

fn batch(values: &[i64]) -> Payload {
    let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
    let batch = RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(values.to_vec()))])
        .expect("a batch");
    Payload::Table(batch, Tier::Host)
}

/// Doubles each value; gives `[-1]` at its finish when `gives` says so; fails when told to.
struct Doubling {
    gives: bool,
    fails: bool,
}

impl Kernel for Doubling {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::compute("ct-t30::Doubling", b"")
    }
    fn kind(&self) -> KernelKind {
        KernelKind::Stateless
    }
    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Table,
            tier: TierPref::Any,
        }
    }
    fn output_schema(&self, input: &SourceSchema) -> moruna_kernel::Result<SourceSchema> {
        Ok(input.clone())
    }
    fn init(&self, _ctx: &InitCtx) -> moruna_kernel::Result<Box<dyn KernelState>> {
        Ok(Box::new(NoState))
    }
    fn apply(
        &self,
        _state: &mut dyn KernelState,
        input: Payload,
    ) -> moruna_kernel::Result<Payload> {
        if self.fails {
            return Err(MorunaError::Plan("told to fail".into()));
        }
        let Payload::Table(b, _) = input else {
            return Err(MorunaError::Plan("a table".into()));
        };
        let column = b
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        let doubled: Vec<i64> = column.values().iter().map(|v| v * 2).collect();
        Ok(batch(&doubled))
    }
    fn finish(&self, _state: &mut dyn KernelState) -> moruna_kernel::Result<Option<Payload>> {
        if self.fails {
            return Err(MorunaError::Plan("told to fail at the end".into()));
        }
        Ok(self.gives.then(|| batch(&[-1])))
    }
}

fn values(c: &Completed) -> Vec<i64> {
    let Payload::Table(b, _) = &c.output else {
        panic!("a table");
    };
    b.column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("int64")
        .values()
        .to_vec()
}

#[test]
fn ct_t30_a_kernel_that_is_not_pipelined_gives_its_own_morsel_in_its_unit() {
    let kernel = Doubling {
        gives: false,
        fails: false,
    };
    let mut state = NoState;
    let given = kernel
        .apply_morsels(&mut state, batch(&[1, 2, 3]), 41)
        .expect("applies");
    assert_eq!(given.len(), 1, "one morsel, the unit's own");
    assert_eq!(given[0].seq, 41);
    assert_eq!(given[0].time, MorselTime::Unit, "timed as the unit");
    assert_eq!(values(&given[0]), vec![2, 4, 6], "apply's output");
}

#[test]
fn ct_t30_finish_morsels_gives_finish_s_payload_under_after_or_nothing() {
    let mut state = NoState;
    let silent = Doubling {
        gives: false,
        fails: false,
    };
    assert!(
        silent
            .finish_morsels(&mut state, 100)
            .expect("finishes")
            .is_empty(),
        "a finish that gives nothing gives no morsel"
    );
    let giving = Doubling {
        gives: true,
        fails: false,
    };
    let given = giving.finish_morsels(&mut state, 100).expect("finishes");
    assert_eq!(given.len(), 1);
    assert_eq!(
        (given[0].seq, given[0].time),
        (100, MorselTime::Unit),
        "numbered after every input number (d.16)"
    );
    assert_eq!(values(&given[0]), vec![-1]);
}

#[test]
fn ct_t30_the_defaults_pass_the_kernel_s_error_through() {
    let failing = Doubling {
        gives: true,
        fails: true,
    };
    let mut state = NoState;
    let applied = failing.apply_morsels(&mut state, batch(&[1]), 0);
    assert!(
        matches!(applied, Err(MorunaError::Plan(ref m)) if m == "told to fail"),
        "{applied:?}"
    );
    let finished = failing.finish_morsels(&mut state, 0);
    assert!(
        matches!(finished, Err(MorunaError::Plan(ref m)) if m == "told to fail at the end"),
        "{finished:?}"
    );
}

#[test]
fn ct_t30_a_morsel_given_in_its_unit_is_timed_as_the_unit() {
    let c = Completed::unit(7, batch(&[5]));
    assert_eq!((c.seq, c.time), (7, MorselTime::Unit));
    let given = MorselTime::Given {
        wall_ns: 3,
        cpu_ns: 2,
    };
    assert_ne!(given, MorselTime::Untimed);
    assert!(format!("{c:?}").contains("Unit"));
}
