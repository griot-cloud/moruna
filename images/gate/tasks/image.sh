#!/usr/bin/env bash
# The gate's image: the seven native tasks at the image's root (/gate-JOB), its environment
# declaring MORUNA_PROCESSES=N (the task processes a stage runs, stream P's layout.rs), written as
# a layer stack `moruna run --image-stack` attaches, its manifest digest in OUT/image.digest.
#
#   images/gate/tasks/image.sh BIN_DIR BASE_LAYOUT PROCESSES OUT MORUNA_LAYER ENGINE
#
# ENGINE is the engine's program, /moruna/bin/moruna-engine in the image: on the Moruna kernel the
# engine runs from the job's image layers (as a Linux program of the personality, the node kit's
# linux/programs/moruna/bin/moruna-engine), so an image that replaces the kit's carries it.
#
# BIN_DIR holds gate-* (tasks/build.sh's output), stripped of their debug information here;
# BASE_LAYOUT is an OCI layout to derive from (kernel/tools/python-layout.sh's, as
# kernel/tools/build.sh derives the node's image), or `scratch` for none: the tasks need no file
# but their own; MORUNA_LAYER is crates/moruna-image's moruna-layer. Run from the repository's
# root.
set -euo pipefail
bin="$1"; base="$2"; n="$3"; out="$4"; layer="$5"; engine="$6"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir "$work/root"
for job in enrich audit occupancy score fw_minhash ov_shape ov_attrs; do
  install -m 0755 "$bin/gate-$job" "$work/root/gate-$job"
  strip --strip-debug "$work/root/gate-$job"
done
install -D -m 0755 "$engine" "$work/root/moruna/bin/moruna-engine"
if [ "$base" = scratch ]; then
  base="$work/base"
  python3 -I "$here/image_env.py" --empty "$base"
fi
manifest="$(python3 kernel/tools/oci-manifest.py "$base" linux amd64)"
"$layer" derive "$base" "$manifest" "$work/root" gate "$work/image" /gate-score
python3 -I "$here/image_env.py" "$work/image" "MORUNA_PROCESSES=$n"
rm -rf "$out"
"$layer" stack "$work/image" gate "$out" 2
cat "$out/image.digest"
