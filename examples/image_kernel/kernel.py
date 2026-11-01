"""A kernel that becomes an image: ``moruna build kernel:add_doubled`` (SDD 30).

The function takes a batch of ``id`` and gives it back with a column ``doubled``. The image
holds this file, the dependencies of ``requirements.lock`` and the shim that reads and writes
Arrow batches, on top of the Python runtime; ``image_job.rs`` writes the job that runs it.

    moruna build kernel:add_doubled --out image --tag add_doubled:1
"""

import moruna
import pyarrow as pa
import pyarrow.compute as pc


@moruna.kernel(input_schema={"id": "int64"}, output_schema={"adds": {"doubled": "int64"}})
def add_doubled(batch: pa.RecordBatch) -> pa.RecordBatch:
    return batch.append_column("doubled", pc.multiply(batch.column("id"), 2))
