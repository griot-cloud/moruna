"""KD2's transform as a batch function for the shim: the same rows as kernel/linux/programs/kd2,
`y` replaced by `3 * x + y` and `x` as read, with the batch read in place and no IPC stream.

    python3 -m moruna_shim kd2_task:transform
"""

import pyarrow as pa
import pyarrow.compute as pc


def transform(batch):
    x = batch.column("x")
    y = pc.add(pc.multiply(x, 3), batch.column("y"))
    return pa.record_batch([x, y], schema=batch.schema)
