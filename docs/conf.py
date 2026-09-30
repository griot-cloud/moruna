"""Sphinx configuration for the Moruna documentation site."""

import tomllib
from datetime import date
from pathlib import Path

project = "Moruna"
copyright = f"{date.today().year}, Griot Data Technologies"
author = "Griot Data Technologies"
# The version is the workspace's, read from Cargo.toml, so the site cannot fall behind a release.
with (Path(__file__).resolve().parent.parent / "Cargo.toml").open("rb") as manifest:
    release = tomllib.load(manifest)["workspace"]["package"]["version"]

extensions = ["myst_parser", "sphinx_design"]
source_suffix = {".md": "markdown"}
exclude_patterns = ["_build"]

myst_enable_extensions = ["colon_fence", "deflist", "fieldlist"]
myst_heading_anchors = 3

html_theme = "shibuya"
html_title = "Moruna: out-of-core batch runtime"
html_static_path = ["_static"]
html_css_files = ["griot.css"]
html_theme_options = {
    "accent_color": "blue",
    "github_url": "https://github.com/griot-cloud/moruna",
    "nav_links": [
        {"title": "parcel", "url": "https://griot-cloud.github.io/parcel/", "external": True},
        {"title": "peQL", "url": "https://griot-cloud.github.io/peQL/", "external": True},
        {"title": "Moruna", "url": "https://griot-cloud.github.io/moruna/", "external": True},
        {"title": "GitHub", "url": "https://github.com/griot-cloud/moruna", "external": True},
    ],
}
