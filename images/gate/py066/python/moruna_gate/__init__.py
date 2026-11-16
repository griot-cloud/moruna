"""The gate's Rust functions for Moruna 0.6.6 (images/gate/README.md): each a Python function, so
`moruna.function` takes it as it takes any, calling the Rust port in `moruna_gate._native`."""
from moruna_gate import _native


def score(batch):
    return _native.score(batch)


def occupancy(batch):
    return _native.occupancy(batch)


def enrich(batch):
    return _native.enrich(batch)


def audit(batch):
    return _native.audit(batch)


def ov_shape(batch):
    return _native.ov_shape(batch)


def ov_attrs(batch):
    return _native.ov_attrs(batch)


def fw_minhash(batch):
    return _native.fw_minhash(batch)
