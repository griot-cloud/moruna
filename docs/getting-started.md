# Getting started

Moruna runs a Python function over a dataset that is larger than the memory you give it. You describe where the data comes from, the functions to apply and where the results go, and Moruna decides how much data to process at a time.

Start with Concepts if these ideas are new to you, or follow the Quickstart to score half a million rows. You only need basic Python for the example.

::::{grid} 1 1 2 2
:gutter: 3

:::{grid-item-card} Concepts
:link: concepts
:link-type: doc

Understand jobs, kernels, budgets and morsels, and what Moruna decides for you.
:::

:::{grid-item-card} Quickstart
:link: quickstart
:link-type: doc

Install Moruna, write a kernel and run it over a Parquet dataset inside a memory budget.
:::
::::

```{toctree}
:hidden:

concepts
quickstart
```
