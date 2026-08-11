import os
import sysconfig

from hatchling.builders.hooks.plugin.interface import BuildHookInterface
from hatchling.metadata.plugin.interface import MetadataHookInterface


def readme_path(root):
    local_readme_path = os.path.join(root, "README.md")
    if os.path.isfile(local_readme_path):
        return local_readme_path

    return os.path.normpath(os.path.join(root, "..", "..", "README.md"))


class CustomMetadataHook(MetadataHookInterface):
    def update(self, metadata):
        with open(readme_path(self.root), encoding="utf-8") as readme_file:
            metadata["readme"] = {"content-type": "text/markdown", "text": readme_file.read()}


class CustomBuildHook(BuildHookInterface):
    def initialize(self, version, build_data):
        platform_tag = sysconfig.get_platform().replace("-", "_").replace(".", "_")
        build_data["pure_python"] = False
        build_data["tag"] = f"py3-none-{platform_tag}"
        if self.target_name == "sdist":
            build_data["force_include"][readme_path(self.root)] = "README.md"
