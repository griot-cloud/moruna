"""Sphinx configuration for the Moruna documentation site."""

from datetime import date

project = "Moruna"
copyright = f"{date.today().year}, Griot Data Technologies"
author = "Griot Data Technologies"
release = "0.2.5"

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
