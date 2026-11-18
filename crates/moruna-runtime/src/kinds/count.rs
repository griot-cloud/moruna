//! A sink that keeps nothing and counts what reaches it, as `/dev/null` does with a count: the
//! only code that knows the `count` kind. A job that ends in it writes no file, volume or object,
//! and its report's `rows_out` is the rows that came through, which is what a smoke test, a
//! measurement or a sizing run wants.
//!
//! `{"kind": "count"}` takes no settings.

use std::sync::atomic::{AtomicU64, Ordering};

use moruna_kernel::{
    BoxFuture, Payload, PayloadKind, PayloadSpec, Result, Seq, Sink, SinkSummary, SourceSchema,
    TierPref,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{BuiltSink, Context, Entry, SinkKind};
use crate::spec::SinkSpec;
use moruna_job::{Declaration, Fact, Flow, Isolation, Needs, Role, settings_schema};

/// The `count` kind, a sink.
pub struct Count;

/// A `count` sink's settings: none.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {}

impl SinkKind for Count {
    fn declaration(&self) -> Declaration {
        Declaration {
            name: "count".into(),
            role: Role::Sink,
            summary: "Keeps nothing and counts the rows and bytes that reach it, as /dev/null \
                      would: no output"
                .into(),
            settings: settings_schema::<Settings>("count", vec![json!({})]),
            inputs: Flow::Batches,
            outputs: Flow::Nothing,
            needs: Needs::nothing(),
            deterministic: Fact::Yes,
            preserves_columns: Fact::Yes,
            isolation: Isolation::Builtin,
            served: None,
            title: None,
            description: None,
            takes: None,
            gives: None,
            requires: Vec::new(),
            provenance: None,
        }
    }

    fn sink(&self, entry: &Entry<'_>, _cx: &mut Context) -> Result<BuiltSink> {
        let Settings {} = entry.settings()?;
        Ok(BuiltSink {
            spec: SinkSpec::Built(Box::new(CountSink::default())),
            target: String::new(),
        })
    }
}

/// The sink the `count` kind builds: the rows and bytes written to it.
#[derive(Debug, Default)]
pub struct CountSink {
    rows: AtomicU64,
    bytes: AtomicU64,
}

impl Sink for CountSink {
    fn open(&mut self, _schema: &SourceSchema) -> Result<()> {
        Ok(())
    }

    fn accepts(&self) -> PayloadSpec {
        PayloadSpec {
            kind: PayloadKind::Either,
            tier: TierPref::Any,
        }
    }

    fn write(&self, _seq: Seq, payload: Payload) -> BoxFuture<'_, Result<()>> {
        self.rows.fetch_add(payload.rows(), Ordering::Relaxed);
        self.bytes.fetch_add(payload.bytes(), Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }

    fn finish(&mut self) -> Result<SinkSummary> {
        Ok(SinkSummary {
            rows: self.rows.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            files: Vec::new(),
            commit: None,
        })
    }
}
