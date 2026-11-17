"""The root-cause log's format (system design 6C), for architecture/benchmark-log.md.

Each miss of the bar gets one entry. The harness writes it with what a run knows (the job, the
step it missed, the stage of Moruna's split that loses most) and the rest marked `TODO`; item 6.3
fills it in. An entry is complete when every field holds something other than `TODO`:

    #### Miss: <job>, <step>
    - Run: <date>, plan <name>
    - Gap: <the verdict's sentence>
    - Stage: <the stage that loses most, from the report's time split>
    - Profile: <the profile of that stage, and the place in Moruna's code it names>
    - Defect: <the general defect>
    - Beyond the benchmark: <why it hurts workloads other than this one>
    - Test: <a test that shows the defect without the benchmark>
    - Work item: <architecture/work/<id>-<name>.md>
    - Not refused: <why the fix is none of 6C's refused kinds: no constant tuned to a dataset, no
      path special to a job, nothing that changes what is measured>
"""
import re

import table

FIELDS = ("Run", "Gap", "Stage", "Profile", "Defect", "Beyond the benchmark", "Test", "Work item", "Not refused")
TODO = "TODO"
HEAD = re.compile(r"^#### Miss: (?P<job>[a-z_]+), (?P<step>.+)$")


def misses(summary: dict) -> list:
    """Every step of the bar Moruna missed, per job: (job, step, sentence, stage)."""
    out = []
    for job, j in summary["jobs"].items():
        vs = table.verdicts(j["engines"])
        where = next((v["text"] for v in vs if v["step"] == "where"), None)
        for v in vs:
            if v["passed"] is False:
                out.append({"job": job, "step": v["step"], "gap": v["text"].replace("**", ""),
                            "stage": where.split(": ", 1)[1] if where else TODO})
    return out


def entry(miss: dict, run: str, **filled) -> str:
    values = {"Run": run, "Gap": miss["gap"], "Stage": miss.get("stage") or TODO}
    for k in FIELDS:
        values.setdefault(k, filled.get(k.lower().replace(" ", "_"), TODO))
    return "\n".join([f"#### Miss: {miss['job']}, {miss['step']}"] + [f"- {k}: {values[k]}" for k in FIELDS]) + "\n"


def parse(text: str) -> list:
    """The entries in `text`: each as {job, step, fields}, in order."""
    out, cur = [], None
    for line in text.splitlines():
        m = HEAD.match(line)
        if m:
            cur = {"job": m["job"], "step": m["step"], "fields": {}}
            out.append(cur)
        elif cur is not None and line.startswith("- "):
            k, sep, v = line[2:].partition(": ")
            if sep and k in FIELDS:
                cur["fields"][k] = v.strip()
        elif cur is not None and line.startswith("#"):
            cur = None
    return out


def problems(e: dict) -> list:
    """What keeps an entry from being complete: missing fields, `TODO`s, and a work item that is
    not a work item's path."""
    out = [f"{k}: missing" for k in FIELDS if k not in e["fields"]]
    out += [f"{k}: {TODO}" for k in FIELDS if k in e["fields"] and e["fields"][k].strip() in ("", TODO)]
    item = e["fields"].get("Work item", "")
    if item not in ("", TODO) and not re.fullmatch(r"architecture/work/\d+\.\d+[a-z]?-[a-z0-9-]+\.md", item):
        out.append("Work item: not a path under architecture/work/")
    return out
