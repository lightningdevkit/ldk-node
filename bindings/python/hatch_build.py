import os

from hatchling.builders.hooks.plugin.interface import BuildHookInterface
from hatchling.metadata.plugin.interface import MetadataHookInterface


class CustomMetadataHook(MetadataHookInterface):
    # Hatchling refuses a `readme` path outside the project directory, so we read the
    # repository README ourselves and hand it over as literal text instead.
    def update(self, metadata):
        readme_path = os.path.join(self.root, os.pardir, os.pardir, "README.md")
        with open(readme_path, encoding="utf-8") as f:
            metadata["readme"] = {"content-type": "text/markdown", "text": f.read()}


class CustomBuildHook(BuildHookInterface):
    def initialize(self, version, build_data):
        build_data["pure_python"] = False
        build_data["infer_tag"] = True
