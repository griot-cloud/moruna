---
layout: landing
content_max_width: 68rem
---

<div class="griot-home">

# Moruna

<p class="home-lead">Moruna is a batch runtime that runs your Python functions over datasets larger than memory, inside a memory limit you set, and sizes the work itself so the job neither runs out of memory nor leaves the machine idle.</p>

<section class="home-usecases" aria-labelledby="usecases-title">
  <h2 id="usecases-title" class="home-section-title">Use cases</h2>
  <div class="home-usecase"><h3>Run a model over a large dataset</h3><p>Apply a tokenizer, a scoring model or any Python function to every row of a Parquet dataset that does not fit in memory, and write the results as Parquet.</p></div>
  <div class="home-usecase"><h3>Stay inside a memory limit</h3><p>Give a job a budget, or let Moruna read its container's limit. Moruna measures what your function uses and adjusts batch sizes and workers to stay under it.</p></div>
  <div class="home-usecase"><h3>Resume a job that was stopped</h3><p>Restart a long job from its last checkpoint instead of from the beginning. The output is the same as a run that was never interrupted.</p></div>
  <div class="home-usecase"><h3>Run a job described in a file</h3><p>Describe the input, functions, output and budget in a job document, then run it with the <code>moruna</code> command from a scheduler, a container or a virtual machine.</p></div>
</section>

<h2 class="home-section-title">Documentation</h2>
<nav class="home-cards" aria-label="Documentation sections">
  <a class="home-card" href="getting-started.html"><span class="card-number">01</span><h3>Getting started</h3><p>Learn the concepts, then run your first job.</p><span class="card-arrow" aria-hidden="true">↗</span></a>
  <a class="home-card" href="using.html"><span class="card-number">02</span><h3>Using Moruna</h3><p>Write kernels, run jobs from Python or the command line, and choose where they run.</p><span class="card-arrow" aria-hidden="true">↗</span></a>
  <a class="home-card" href="execution.html"><span class="card-number">03</span><h3>How it works</h3><p>Understand how Moruna sizes work, keeps memory under the limit and resumes.</p><span class="card-arrow" aria-hidden="true">↗</span></a>
  <a class="home-card" href="reference.html"><span class="card-number">04</span><h3>Reference</h3><p>Look up functions, the job document, commands and the run report.</p><span class="card-arrow" aria-hidden="true">↗</span></a>
</nav>
<a class="home-next" href="getting-started.html"><span><small>Start here</small>Getting started</span><span aria-hidden="true">→</span></a>
</div>

```{toctree}
:maxdepth: 2
:hidden:

getting-started
using
execution
reference
```
