# Using Moruna

You can use Moruna from Python, where a job is a call to `moruna.run`, or from the command line, where a job is a JSON document. Both run the same job the same way; the command line suits schedulers, containers and virtual machines, where there is no Python program to write.

::::{grid} 1 1 2 2
:gutter: 3

:::{grid-item-card} Running jobs from Python
:link: python
:link-type: doc

Read Parquet, tensor files or your own iterator, write the results, and set the budget and other options.
:::

:::{grid-item-card} Writing kernels
:link: kernels
:link-type: doc

Write Python and Polars kernels, keep state between batches, use the standard kernels, and check a kernel before you run it.
:::

:::{grid-item-card} Running jobs from the command line
:link: command-line
:link-type: doc

Describe a job in a JSON document and run it with the `moruna` command.
:::

:::{grid-item-card} Where Moruna runs
:link: hosting
:link-type: doc

Run on a laptop, in a container or Kubernetes pod, or in an isolated virtual machine, and choose where Moruna spills to disk.
:::
::::

```{toctree}
:hidden:

python
kernels
command-line
hosting
```
